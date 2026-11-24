//! Incremental background vault watcher & re-indexer.
//!
//! Uses `notify::recommended_watcher` (ReadDirectoryChangesW / FSEvents / inotify) to
//! react immediately to local Markdown, HTML, and directory mutations with short bounded
//! debouncing and path-scoped SQLite indexing, while keeping 0 repeated full vault scans
//! while idle. Falls back automatically to `watch_poll_ms` polling on unsupported volumes
//! and performs a single full reconciliation on watcher overflow/rescan notifications.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use notify::{
    event::{MetadataKind, ModifyKind},
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch, Notify},
    time::Instant,
};

use crate::{
    config::VaultProfile,
    models::CoreEvent,
    storage::Storage,
    vault::fs::{
        content_sha256_hex, has_symlink_component_in_vault, is_html_file_path, is_ignored_dir_name,
        is_markdown_file_path, rel_path_in_vault, scan_vault_html_files, scan_vault_html_subdir,
        scan_vault_markdown_subdir,
    },
};

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const MAX_PENDING_PATHS: usize = 256;
const DEFAULT_DEBOUNCE_MS: u64 = 55;
const DEFAULT_MAX_COALESCE_MS: u64 = 180;
const LOCKED_FILE_RETRY_MS: u64 = 120;
const MAX_LOCKED_RETRIES: u8 = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub active_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncCommand {
    ReindexNow,
    SwitchVault { vault: VaultProfile },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherMode {
    Native,
    PollingFallback,
}

impl WatcherMode {
    fn as_u8(self) -> u8 {
        match self {
            Self::Native => 1,
            Self::PollingFallback => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Native,
            _ => Self::PollingFallback,
        }
    }
}

/// Live atomic diagnostics for the background vault watcher (used in tests and benchmarks).
#[derive(Debug, Default)]
pub struct WatcherDiagnostics {
    mode: AtomicU8,
    pub initial_scans: AtomicU64,
    pub post_init_full_scans: AtomicU64,
    pub incremental_batches: AtomicU64,
    pub rescan_events: AtomicU64,
    pub overflow_rescans: AtomicU64,
    pub db_change_reloads: AtomicU64,
    pub locked_retries: AtomicU64,
    pub pending_queue_len: AtomicU64,
    pub pending_queue_high_water: AtomicU64,
}

impl WatcherDiagnostics {
    pub fn mode(&self) -> WatcherMode {
        WatcherMode::from_u8(self.mode.load(Ordering::Acquire))
    }

    fn set_mode(&self, mode: WatcherMode) {
        self.mode.store(mode.as_u8(), Ordering::Release);
    }

    fn record_pending_len(&self, len: usize) {
        let v = len as u64;
        self.pending_queue_len.store(v, Ordering::Release);
        let mut prev = self.pending_queue_high_water.load(Ordering::Relaxed);
        while v > prev {
            match self.pending_queue_high_water.compare_exchange_weak(
                prev,
                v,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatcherOptions {
    pub force_polling_fallback: bool,
    pub debounce_ms: u64,
    pub max_coalesce_ms: u64,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        Self {
            force_polling_fallback: false,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            max_coalesce_ms: DEFAULT_MAX_COALESCE_MS,
        }
    }
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    cmd_tx: mpsc::UnboundedSender<SyncCommand>,
    rescan_flag: Arc<AtomicBool>,
    rescan_notify: Arc<Notify>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        cmd_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self {
            focus_tx,
            cmd_tx,
            rescan_flag: Arc::new(AtomicBool::new(false)),
            rescan_notify: Arc::new(Notify::new()),
        }
    }

    pub fn set_focus(&self, target: FocusTarget) {
        let _ = self.focus_tx.send(target);
    }

    pub fn reindex_now(&self) {
        let _ = self.cmd_tx.send(SyncCommand::ReindexNow);
    }

    pub fn switch_vault(&self, vault: VaultProfile) {
        let _ = self.cmd_tx.send(SyncCommand::SwitchVault { vault });
    }

    /// Signals that filesystem events may have been dropped, triggering a single full reconciliation.
    pub fn request_rescan(&self) {
        self.rescan_flag.store(true, Ordering::Release);
        self.rescan_notify.notify_one();
    }
}

#[derive(Debug)]
enum FsSignal {
    Batch(Vec<ClassifiedChange>),
    Rescan,
}

#[derive(Debug, Clone)]
enum ClassifiedChange {
    Md(String),
    Html(String),
    DirCreatedOrRenamed(String),
    DirRemovedOrRenamed(String),
}

struct NativeWatchBinding {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<FsSignal>,
    overflow_flag: Arc<AtomicBool>,
    overflow_notify: Arc<Notify>,
}

#[derive(Default)]
struct PendingBatch {
    md_paths: HashSet<String>,
    html_paths: HashSet<String>,
    dir_created_or_renamed: HashSet<String>,
    dir_removed_or_renamed: HashSet<String>,
    html_changed: bool,
    need_full_rescan: bool,
    first_event_at: Option<Instant>,
    flush_at: Option<Instant>,
}

impl PendingBatch {
    fn is_empty(&self) -> bool {
        !self.need_full_rescan
            && !self.html_changed
            && self.md_paths.is_empty()
            && self.html_paths.is_empty()
            && self.dir_created_or_renamed.is_empty()
            && self.dir_removed_or_renamed.is_empty()
    }

    fn tracked_len(&self) -> usize {
        self.md_paths.len()
            + self.html_paths.len()
            + self.dir_created_or_renamed.len()
            + self.dir_removed_or_renamed.len()
    }

    fn bump_deadline(&mut self, now: Instant, debounce: Duration, max_wait: Duration) {
        let first = *self.first_event_at.get_or_insert(now);
        let desired = now + debounce;
        let cap = first + max_wait;
        let next = desired.min(cap);
        self.flush_at = Some(match self.flush_at {
            Some(prev) => prev.max(next).min(cap),
            None => next,
        });
    }

    fn mark_full_rescan(&mut self, now: Instant, debounce: Duration, max_wait: Duration) {
        self.need_full_rescan = true;
        self.md_paths.clear();
        self.html_paths.clear();
        self.dir_created_or_renamed.clear();
        self.dir_removed_or_renamed.clear();
        self.shrink_if_oversized();
        self.bump_deadline(now, debounce, max_wait);
    }

    fn push_signal(
        &mut self,
        sig: FsSignal,
        now: Instant,
        debounce: Duration,
        max_wait: Duration,
        diag: &WatcherDiagnostics,
    ) {
        match sig {
            FsSignal::Rescan => {
                diag.rescan_events.fetch_add(1, Ordering::Relaxed);
                self.mark_full_rescan(now, debounce, max_wait);
            }
            FsSignal::Batch(changes) => {
                if self.need_full_rescan {
                    self.bump_deadline(now, debounce, max_wait);
                    return;
                }
                for ch in changes {
                    match ch {
                        ClassifiedChange::Md(p) => {
                            self.md_paths.insert(p);
                        }
                        ClassifiedChange::Html(p) => {
                            self.html_changed = true;
                            self.html_paths.insert(p);
                        }
                        ClassifiedChange::DirCreatedOrRenamed(p) => {
                            self.dir_created_or_renamed.insert(p);
                        }
                        ClassifiedChange::DirRemovedOrRenamed(p) => {
                            self.dir_removed_or_renamed.insert(p);
                        }
                    }
                    if self.tracked_len() > MAX_PENDING_PATHS {
                        diag.overflow_rescans.fetch_add(1, Ordering::Relaxed);
                        self.mark_full_rescan(now, debounce, max_wait);
                        break;
                    }
                }
                self.bump_deadline(now, debounce, max_wait);
            }
        }
        diag.record_pending_len(self.tracked_len());
    }

    fn take(&mut self, diag: &WatcherDiagnostics) -> PendingBatch {
        let taken = std::mem::take(self);
        self.shrink_if_oversized();
        diag.record_pending_len(0);
        taken
    }

    fn clear_all(&mut self, diag: &WatcherDiagnostics) {
        *self = Self::default();
        diag.record_pending_len(0);
    }

    fn shrink_if_oversized(&mut self) {
        if self.md_paths.capacity() > 64 {
            self.md_paths.shrink_to_fit();
        }
        if self.html_paths.capacity() > 64 {
            self.html_paths.shrink_to_fit();
        }
        if self.dir_created_or_renamed.capacity() > 32 {
            self.dir_created_or_renamed.shrink_to_fit();
        }
        if self.dir_removed_or_renamed.capacity() > 32 {
            self.dir_removed_or_renamed.shrink_to_fit();
        }
    }
}

/// Spawns an event-driven filesystem watcher for `initial_vault`, falling back to
/// `poll_ms` polling only when the OS watcher cannot monitor the vault volume.
pub fn spawn_vault_watcher(
    initial_vault: VaultProfile,
    storage: Storage,
    poll_ms: u64,
    events_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (ctrl, _diag) = spawn_vault_watcher_with_options(
        initial_vault,
        storage,
        poll_ms,
        events_tx,
        WatcherOptions::default(),
    );
    ctrl
}

/// Spawns the vault watcher with explicit options and returns live diagnostics counters.
pub fn spawn_vault_watcher_with_options(
    initial_vault: VaultProfile,
    storage: Storage,
    poll_ms: u64,
    events_tx: mpsc::UnboundedSender<CoreEvent>,
    options: WatcherOptions,
) -> (SyncController, Arc<WatcherDiagnostics>) {
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let diag = Arc::new(WatcherDiagnostics::default());
    let rescan_flag = Arc::new(AtomicBool::new(false));
    let rescan_notify = Arc::new(Notify::new());

    let interval_ms = poll_ms.clamp(500, 60_000);
    let debounce = Duration::from_millis(options.debounce_ms.max(10));
    let max_wait = Duration::from_millis(options.max_coalesce_ms.max(options.debounce_ms.max(20)));

    // Register native OS watcher synchronously BEFORE the initial sync so no mutations
    // during startup are missed.
    let initial_binding = if options.force_polling_fallback {
        None
    } else {
        register_native_watcher(&initial_vault.path)
    };
    diag.set_mode(if initial_binding.is_some() {
        WatcherMode::Native
    } else {
        WatcherMode::PollingFallback
    });

    let diag_bg = diag.clone();
    let rescan_flag_bg = rescan_flag.clone();
    let rescan_notify_bg = rescan_notify.clone();

    tokio::spawn(async move {
        let mut vault = initial_vault;
        let mut binding = initial_binding;
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate first tick so periodic checks start after one interval.
        ticker.tick().await;

        let mut last_poll_fingerprint: Option<String> = None;
        let mut last_data_version: Option<i64> = None;
        let mut known_html_paths: HashSet<String> = HashSet::new();
        let mut pending = PendingBatch::default();
        let mut locked_retries: HashMap<String, u8> = HashMap::new();
        let mut retry_at: Option<Instant> = None;

        // 1. Initial reconciliation scan (after watcher registration!)
        run_full_sync(
            &vault,
            &storage,
            &events_tx,
            true,
            false,
            binding.is_none(),
            &mut last_poll_fingerprint,
            &mut last_data_version,
            &mut known_html_paths,
        )
        .await;
        diag_bg.initial_scans.fetch_add(1, Ordering::Relaxed);

        // 2. Drain any filesystem events queued while initial sync ran
        drain_pending_native_signals(binding.as_mut(), &mut pending, debounce, max_wait, &diag_bg);

        loop {
            let flush_deadline = pending.flush_at;
            let retry_deadline = if locked_retries.is_empty() {
                None
            } else {
                retry_at
            };
            let overflow_notify = binding.as_ref().map(|b| b.overflow_notify.clone());

            tokio::select! {
                biased;

                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(SyncCommand::ReindexNow) => {
                            pending.clear_all(&diag_bg);
                            locked_retries.clear();
                            retry_at = None;
                            run_full_sync(
                                &vault,
                                &storage,
                                &events_tx,
                                true,
                                true,
                                binding.is_none(),
                                &mut last_poll_fingerprint,
                                &mut last_data_version,
                                &mut known_html_paths,
                            )
                            .await;
                            diag_bg.post_init_full_scans.fetch_add(1, Ordering::Relaxed);
                        }
                        Some(SyncCommand::SwitchVault { vault: next }) => {
                            pending.clear_all(&diag_bg);
                            locked_retries.clear();
                            retry_at = None;
                            last_poll_fingerprint = None;
                            last_data_version = None;
                            known_html_paths.clear();
                            vault = next;

                            // Register watcher on the new vault BEFORE initial sync
                            binding = if options.force_polling_fallback {
                                None
                            } else {
                                register_native_watcher(&vault.path)
                            };
                            diag_bg.set_mode(if binding.is_some() {
                                WatcherMode::Native
                            } else {
                                WatcherMode::PollingFallback
                            });

                            run_full_sync(
                                &vault,
                                &storage,
                                &events_tx,
                                true,
                                false,
                                binding.is_none(),
                                &mut last_poll_fingerprint,
                                &mut last_data_version,
                                &mut known_html_paths,
                            )
                            .await;
                            diag_bg.initial_scans.fetch_add(1, Ordering::Relaxed);
                            drain_pending_native_signals(
                                binding.as_mut(),
                                &mut pending,
                                debounce,
                                max_wait,
                                &diag_bg,
                            );
                        }
                        None => break,
                    }
                }

                _ = rescan_notify_bg.notified() => {
                    if rescan_flag_bg.swap(false, Ordering::AcqRel) {
                        diag_bg.rescan_events.fetch_add(1, Ordering::Relaxed);
                        pending.mark_full_rescan(Instant::now(), debounce, max_wait);
                    }
                }

                _ = wait_overflow_notify(overflow_notify.as_deref()), if overflow_notify.is_some() => {
                    if let Some(ref b) = binding {
                        if b.overflow_flag.swap(false, Ordering::AcqRel) {
                            diag_bg.overflow_rescans.fetch_add(1, Ordering::Relaxed);
                            pending.mark_full_rescan(Instant::now(), debounce, max_wait);
                        }
                    }
                }

                maybe_sig = recv_native_signal(binding.as_mut()), if binding.is_some() => {
                    match maybe_sig {
                        Some(sig) => {
                            let now = Instant::now();
                            pending.push_signal(sig, now, debounce, max_wait, &diag_bg);
                            drain_pending_native_signals(
                                binding.as_mut(),
                                &mut pending,
                                debounce,
                                max_wait,
                                &diag_bg,
                            );
                        }
                        None => {
                            // Watcher thread terminated unexpectedly; switch cleanly to polling fallback.
                            binding = None;
                            diag_bg.set_mode(WatcherMode::PollingFallback);
                            pending.mark_full_rescan(Instant::now(), debounce, max_wait);
                        }
                    }
                }

                _ = sleep_until_opt(flush_deadline), if flush_deadline.is_some() => {
                    drain_pending_native_signals(
                        binding.as_mut(),
                        &mut pending,
                        debounce,
                        max_wait,
                        &diag_bg,
                    );
                    if rescan_flag_bg.swap(false, Ordering::AcqRel) {
                        diag_bg.rescan_events.fetch_add(1, Ordering::Relaxed);
                        pending.need_full_rescan = true;
                    }
                    if !pending.is_empty() {
                        let batch = pending.take(&diag_bg);
                        process_batch(
                            batch,
                            &vault,
                            &storage,
                            &events_tx,
                            binding.is_none(),
                            &mut last_poll_fingerprint,
                            &mut last_data_version,
                            &mut known_html_paths,
                            &mut locked_retries,
                            &mut retry_at,
                            &diag_bg,
                        )
                        .await;
                    }
                }

                _ = sleep_until_opt(retry_deadline), if retry_deadline.is_some() => {
                    retry_at = None;
                    if !locked_retries.is_empty() {
                        let paths: Vec<String> = locked_retries.keys().cloned().collect();
                        let now = Instant::now();
                        for p in paths {
                            pending.md_paths.insert(p);
                        }
                        pending.bump_deadline(now, Duration::from_millis(10), Duration::from_millis(20));
                    }
                }

                _ = ticker.tick() => {
                    if binding.is_none() {
                        // Fallback polling mode for unsupported volumes.
                        run_full_sync(
                            &vault,
                            &storage,
                            &events_tx,
                            false,
                            false,
                            true,
                            &mut last_poll_fingerprint,
                            &mut last_data_version,
                            &mut known_html_paths,
                        )
                        .await;
                        diag_bg.post_init_full_scans.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // Normal native watch mode: 0 filesystem scans; O(1) PRAGMA data_version check
                        // catches DB-only cross-process mutations (e.g. bookmarks).
                        check_external_db_changes(
                            &vault,
                            &storage,
                            &events_tx,
                            &mut last_data_version,
                            &diag_bg,
                        )
                        .await;
                    }
                }
            }
        }
    });

    (
        SyncController {
            focus_tx,
            cmd_tx,
            rescan_flag,
            rescan_notify,
        },
        diag,
    )
}

async fn wait_overflow_notify(notify: Option<&Notify>) {
    match notify {
        Some(n) => n.notified().await,
        None => std::future::pending::<()>().await,
    }
}

async fn recv_native_signal(binding: Option<&mut NativeWatchBinding>) -> Option<FsSignal> {
    match binding {
        Some(b) => b.rx.recv().await,
        None => std::future::pending::<Option<FsSignal>>().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

fn drain_pending_native_signals(
    binding: Option<&mut NativeWatchBinding>,
    pending: &mut PendingBatch,
    debounce: Duration,
    max_wait: Duration,
    diag: &WatcherDiagnostics,
) {
    let Some(b) = binding else {
        return;
    };
    let now = Instant::now();
    while let Ok(sig) = b.rx.try_recv() {
        pending.push_signal(sig, now, debounce, max_wait, diag);
    }
    if b.overflow_flag.swap(false, Ordering::AcqRel) {
        diag.overflow_rescans.fetch_add(1, Ordering::Relaxed);
        pending.mark_full_rescan(now, debounce, max_wait);
    }
}

fn watcher_config() -> notify::Config {
    notify::Config::default().with_follow_symlinks(false)
}

fn register_native_watcher(vault_root: &Path) -> Option<NativeWatchBinding> {
    if !vault_root.is_dir() {
        return None;
    }
    let root_owned = vault_root.to_path_buf();
    let canonical_root = fs::canonicalize(vault_root).ok();
    let watch_root = canonical_root
        .as_deref()
        .unwrap_or(vault_root)
        .to_path_buf();
    let (tx, rx) = mpsc::channel::<FsSignal>(EVENT_CHANNEL_CAPACITY);
    let overflow_flag = Arc::new(AtomicBool::new(false));
    let overflow_notify = Arc::new(Notify::new());

    let overflow_cb = overflow_flag.clone();
    let notify_cb = overflow_notify.clone();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                if ev.need_rescan() {
                    if tx.try_send(FsSignal::Rescan).is_err() {
                        overflow_cb.store(true, Ordering::Release);
                        notify_cb.notify_one();
                    }
                    return;
                }
                let changes = classify_notify_event(&root_owned, canonical_root.as_deref(), &ev);
                if changes.is_empty() {
                    return;
                }
                if tx.try_send(FsSignal::Batch(changes)).is_err() {
                    overflow_cb.store(true, Ordering::Release);
                    notify_cb.notify_one();
                }
            }
            Err(_) => {
                if tx.try_send(FsSignal::Rescan).is_err() {
                    overflow_cb.store(true, Ordering::Release);
                    notify_cb.notify_one();
                }
            }
        },
        watcher_config(),
    )
    .ok()?;

    if watcher.watch(&watch_root, RecursiveMode::Recursive).is_err() {
        return None;
    }

    Some(NativeWatchBinding {
        _watcher: watcher,
        rx,
        overflow_flag,
        overflow_notify,
    })
}

fn is_non_mutating_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(_)
            | EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::AccessTime | MetadataKind::Ownership | MetadataKind::Permissions
            ))
    )
}

fn is_temp_or_hidden_filename(rel_path: &str) -> bool {
    let fname = rel_path.rsplit('/').next().unwrap_or(rel_path);
    if fname.starts_with('.') {
        return true;
    }
    let lower = fname.to_ascii_lowercase();
    lower.ends_with(".tmp")
        || lower.ends_with(".bak")
        || lower.ends_with(".swp")
        || lower.ends_with(".swo")
        || lower.ends_with(".part")
        || lower.ends_with(".crdownload")
        || lower.ends_with('~')
}

fn classify_notify_event(
    vault_root: &Path,
    canonical_root: Option<&Path>,
    ev: &notify::Event,
) -> Vec<ClassifiedChange> {
    if is_non_mutating_event(&ev.kind) {
        return Vec::new();
    }
    let is_create_or_rename = matches!(
        ev.kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any
    );
    let is_remove_or_rename = matches!(
        ev.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any
    );

    let mut out = Vec::new();
    for path in &ev.paths {
        let Some(rel) = rel_path_in_vault(vault_root, canonical_root, path) else {
            continue;
        };
        if has_symlink_component_in_vault(vault_root, &rel) {
            continue;
        }
        if is_markdown_file_path(&rel) {
            out.push(ClassifiedChange::Md(rel));
            continue;
        }
        if is_html_file_path(&rel) {
            out.push(ClassifiedChange::Html(rel));
            continue;
        }

        // Ignore hidden/system directory names themselves (e.g., `.obsidian` created/modified).
        let last_seg = rel.rsplit('/').next().unwrap_or(&rel);
        if is_ignored_dir_name(last_seg) {
            continue;
        }

        // Handle directory create / move / rename / remove events cleanly without triggering
        // scans on unrelated temp or non-note files.
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_dir()
                    && !meta.file_type().is_symlink()
                    && is_create_or_rename
                {
                    out.push(ClassifiedChange::DirCreatedOrRenamed(rel));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if is_remove_or_rename && !is_temp_or_hidden_filename(&rel) {
                    out.push(ClassifiedChange::DirRemovedOrRenamed(rel));
                }
            }
            Err(_) => {}
        }
    }
    out
}

/// What one batch did to the locked-file retry map.
#[derive(Debug, Default, PartialEq, Eq)]
struct RetryPlan {
    /// Locked paths that got another retry scheduled by this batch.
    scheduled: usize,
    /// Locked paths dropped because they already used `MAX_LOCKED_RETRIES` retries.
    gave_up: usize,
}

/// Updates the bounded retry counters after an incremental batch.
///
/// - a path that was attempted and is no longer locked is removed (it synced or vanished);
/// - a path that was not attempted is left alone, so an unrelated batch cannot drop its retry;
/// - a locked path below `MAX_LOCKED_RETRIES` is counted and scheduled again;
/// - a locked path already at the limit is removed (give up).
///
/// When the batch escalated to a full sync (`was_full`) nothing counts as attempted, because a
/// full sync does not report which files were locked.
fn update_locked_retries(
    retries: &mut HashMap<String, u8>,
    attempted: &HashSet<String>,
    locked: Vec<String>,
    was_full: bool,
) -> RetryPlan {
    let mut plan = RetryPlan::default();
    if was_full {
        return plan;
    }
    let locked: HashSet<String> = locked.into_iter().collect();
    retries.retain(|path, _| !attempted.contains(path) || locked.contains(path));
    for path in locked {
        let count = retries.entry(path.clone()).or_insert(0);
        if *count < MAX_LOCKED_RETRIES {
            *count += 1;
            plan.scheduled += 1;
        } else {
            retries.remove(&path);
            plan.gave_up += 1;
        }
    }
    plan
}

/// The retry timer after a batch: restarted when a retry was scheduled, cleared when nothing is
/// pending, otherwise the existing deadline is kept (or armed if there was none), so the timer
/// is never armed with an empty map and never idle with a pending retry.
fn next_retry_deadline(
    retries_empty: bool,
    current: Option<Instant>,
    scheduled: bool,
    now: Instant,
) -> Option<Instant> {
    let fresh = now + Duration::from_millis(LOCKED_FILE_RETRY_MS);
    if scheduled {
        Some(fresh)
    } else if retries_empty {
        None
    } else {
        current.or(Some(fresh))
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_batch(
    batch: PendingBatch,
    vault: &VaultProfile,
    storage: &Storage,
    events_tx: &mpsc::UnboundedSender<CoreEvent>,
    track_poll_fingerprint: bool,
    last_poll_fingerprint: &mut Option<String>,
    last_data_version: &mut Option<i64>,
    known_html_paths: &mut HashSet<String>,
    locked_retries: &mut HashMap<String, u8>,
    retry_at: &mut Option<Instant>,
    diag: &WatcherDiagnostics,
) {
    if batch.need_full_rescan {
        run_full_sync(
            vault,
            storage,
            events_tx,
            true,
            false,
            track_poll_fingerprint,
            last_poll_fingerprint,
            last_data_version,
            known_html_paths,
        )
        .await;
        diag.post_init_full_scans.fetch_add(1, Ordering::Relaxed);
        // A full sync skips locked files without reporting them, so pending retries stay due.
        *retry_at =
            next_retry_deadline(locked_retries.is_empty(), *retry_at, false, Instant::now());
        return;
    }

    let vault_clone = vault.clone();
    let storage_clone = storage.clone();
    let mut md_paths: Vec<String> = batch.md_paths.into_iter().collect();
    // The paths this batch asks `sync_paths` to look at. A pending retry outside this set was
    // not attempted, so this batch says nothing about whether it is still locked.
    let attempted: HashSet<String> = md_paths.iter().cloned().collect();
    let html_paths: Vec<String> = batch.html_paths.into_iter().collect();
    let dir_created: Vec<String> = batch.dir_created_or_renamed.into_iter().collect();
    let dir_removed: Vec<String> = batch.dir_removed_or_renamed.into_iter().collect();
    let initial_html_changed = batch.html_changed;

    // Check if any removed directory prefix held known HTML files.
    let mut removed_html_in_dir = false;
    for prefix in &dir_removed {
        let p_norm = prefix.trim_matches('/');
        let p_slash = format!("{p_norm}/");
        let before = known_html_paths.len();
        known_html_paths.retain(|hp| {
            if cfg!(windows) {
                !hp.to_ascii_lowercase()
                    .starts_with(&p_slash.to_ascii_lowercase())
            } else {
                !hp.starts_with(&p_slash)
            }
        });
        if known_html_paths.len() != before {
            removed_html_in_dir = true;
        }
    }

    let res = tokio::task::spawn_blocking(move || {
        let mut escalated_full_rescan = false;
        let mut extra_html_paths: Vec<String> = Vec::new();
        let mut html_changed = initial_html_changed || removed_html_in_dir;

        // Expand newly created / moved-in directories ONLY (never full vault).
        for dir_rel in &dir_created {
            let dir_abs = vault_clone
                .path
                .join(dir_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Ok(meta) = fs::symlink_metadata(&dir_abs) {
                if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                    if let Ok(entries) = scan_vault_markdown_subdir(&vault_clone.path, &dir_abs) {
                        if md_paths.len() + entries.len() > MAX_PENDING_PATHS {
                            escalated_full_rescan = true;
                            break;
                        }
                        for e in entries {
                            md_paths.push(e.rel_path);
                        }
                    }
                    if let Ok(htmls) = scan_vault_html_subdir(&vault_clone.path, &dir_abs) {
                        if !htmls.is_empty() {
                            html_changed = true;
                            for h in htmls {
                                extra_html_paths.push(h.rel_path);
                            }
                        }
                    }
                }
            }
        }

        if escalated_full_rescan {
            let stats = storage_clone.sync_vault(&vault_clone)?;
            let dv = storage_clone.data_version().ok();
            let all_html = scan_vault_html_files(&vault_clone.path)
                .unwrap_or_default()
                .into_iter()
                .map(|h| h.rel_path)
                .collect::<Vec<_>>();
            return Ok::<_, anyhow::Error>((
                stats,
                Vec::new(),
                dv,
                true,
                true,
                true,
                Some(all_html),
                extra_html_paths,
            ));
        }

        let had_md_events = !md_paths.is_empty();
        let outcome = storage_clone.sync_paths(&vault_clone, &md_paths, &dir_removed)?;
        let dv = storage_clone.data_version().ok();
        let should_emit =
            outcome.stats.updated > 0 || outcome.stats.removed > 0 || html_changed || had_md_events;

        Ok((
            outcome.stats,
            outcome.locked_paths,
            dv,
            should_emit,
            false,
            html_changed,
            None,
            extra_html_paths,
        ))
    })
    .await;

    match res {
        Ok(Ok((
            stats,
            locked,
            dv,
            should_emit,
            was_full,
            _html_changed,
            replaced_html,
            extra_html,
        ))) => {
            if let Some(v) = dv {
                *last_data_version = Some(v);
            }
            if let Some(all_h) = replaced_html {
                *known_html_paths = all_h.into_iter().collect();
            } else {
                for hp in html_paths.into_iter().chain(extra_html) {
                    let abs = vault
                        .path
                        .join(hp.replace('/', std::path::MAIN_SEPARATOR_STR));
                    if abs.is_file() {
                        known_html_paths.insert(hp);
                    } else {
                        known_html_paths.remove(&hp);
                    }
                }
            }

            if was_full {
                diag.post_init_full_scans.fetch_add(1, Ordering::Relaxed);
            } else {
                diag.incremental_batches.fetch_add(1, Ordering::Relaxed);
            }

            // Update bounded retries for transiently locked files.
            let plan = update_locked_retries(locked_retries, &attempted, locked, was_full);
            diag.locked_retries
                .fetch_add(plan.scheduled as u64, Ordering::Relaxed);
            *retry_at = next_retry_deadline(
                locked_retries.is_empty(),
                *retry_at,
                plan.scheduled > 0,
                Instant::now(),
            );

            if should_emit {
                let _ = events_tx.send(CoreEvent::VaultIndexed { stats });
            }
        }
        Ok(Err(err)) => {
            let _ = events_tx.send(CoreEvent::SyncStatus {
                busy: false,
                summary: format!("Index error: {err}"),
            });
        }
        Err(_) => {}
    }
}

async fn check_external_db_changes(
    vault: &VaultProfile,
    storage: &Storage,
    events_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_data_version: &mut Option<i64>,
    diag: &WatcherDiagnostics,
) {
    let vault_clone = vault.clone();
    let storage_clone = storage.clone();
    let prev_dv = *last_data_version;

    let res = tokio::task::spawn_blocking(move || {
        let cur_dv = storage_clone.data_version()?;
        if prev_dv == Some(cur_dv) {
            return Ok::<_, anyhow::Error>(None);
        }
        let stats = storage_clone.index_stats(&vault_clone, 0, 0, 0, 0)?;
        Ok(Some((cur_dv, stats)))
    })
    .await;

    if let Ok(Ok(Some((cur_dv, stats)))) = res {
        *last_data_version = Some(cur_dv);
        diag.db_change_reloads.fetch_add(1, Ordering::Relaxed);
        let _ = events_tx.send(CoreEvent::VaultIndexed { stats });
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_full_sync(
    vault: &VaultProfile,
    storage: &Storage,
    events_tx: &mpsc::UnboundedSender<CoreEvent>,
    always_emit: bool,
    force_hash_check: bool,
    compute_poll_fingerprint: bool,
    last_poll_fingerprint: &mut Option<String>,
    last_data_version: &mut Option<i64>,
    known_html_paths: &mut HashSet<String>,
) {
    let vault_clone = vault.clone();
    let storage_clone = storage.clone();
    let res = tokio::task::spawn_blocking(move || {
        let stats = if force_hash_check {
            storage_clone.reindex_vault(&vault_clone)?
        } else {
            storage_clone.sync_vault(&vault_clone)?
        };
        let dv = storage_clone.data_version().ok();
        let html_files = scan_vault_html_files(&vault_clone.path).unwrap_or_default();
        let html_rel_paths: Vec<String> = html_files.iter().map(|h| h.rel_path.clone()).collect();
        let poll_fp = if compute_poll_fingerprint {
            let db_fp = storage_clone
                .vault_index_fingerprint(&vault_clone.id)
                .unwrap_or_default();
            let html_fp = html_files_fingerprint_from_list(&html_files);
            Some(format!("{db_fp}|{html_fp}"))
        } else {
            None
        };
        Ok::<_, anyhow::Error>((stats, dv, poll_fp, html_rel_paths))
    })
    .await;
    match res {
        Ok(Ok((stats, dv, poll_fp, html_rel_paths))) => {
            if let Some(v) = dv {
                *last_data_version = Some(v);
            }
            *known_html_paths = html_rel_paths.into_iter().collect();
            let fp_changed = match poll_fp {
                Some(fp) => {
                    let changed = last_poll_fingerprint.as_deref() != Some(fp.as_str());
                    *last_poll_fingerprint = Some(fp);
                    changed
                }
                None => false,
            };
            if always_emit || stats.updated > 0 || stats.removed > 0 || fp_changed {
                let _ = events_tx.send(CoreEvent::VaultIndexed { stats });
            }
        }
        Ok(Err(err)) => {
            let _ = events_tx.send(CoreEvent::SyncStatus {
                busy: false,
                summary: format!("Index error: {err}"),
            });
        }
        Err(_) => {}
    }
}

fn html_files_fingerprint_from_list(files: &[crate::vault::fs::VaultHtmlFile]) -> String {
    let mut acc = String::new();
    for f in files {
        acc.push_str(&f.rel_path);
        acc.push(':');
        acc.push_str(&f.mtime_ns.to_string());
        acc.push(':');
        acc.push_str(&f.size_bytes.to_string());
        acc.push('|');
    }
    content_sha256_hex(acc.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::write_atomic;
    use crate::vault::vault_from_dir;

    fn obs06_set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn obs06_locked(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn obs06_unrelated_success_keeps_pending_retry() {
        let now = Instant::now();
        let deadline = Some(now + Duration::from_millis(40));
        let mut retries = HashMap::from([("A.md".to_string(), 1u8)]);

        let plan = update_locked_retries(&mut retries, &obs06_set(&["B.md"]), Vec::new(), false);

        assert_eq!(plan, RetryPlan::default());
        assert_eq!(retries, HashMap::from([("A.md".to_string(), 1u8)]));
        // The existing deadline is kept, not pushed back and not cleared.
        assert_eq!(
            next_retry_deadline(retries.is_empty(), deadline, false, now),
            deadline
        );
        // With no deadline left (the timer already fired) one is armed again.
        assert_eq!(
            next_retry_deadline(retries.is_empty(), None, false, now),
            Some(now + Duration::from_millis(LOCKED_FILE_RETRY_MS))
        );
    }

    #[test]
    fn obs06_attempted_success_clears_retry() {
        let now = Instant::now();
        let mut retries = HashMap::from([("A.md".to_string(), 1u8)]);

        let plan = update_locked_retries(&mut retries, &obs06_set(&["A.md"]), Vec::new(), false);

        assert_eq!(plan, RetryPlan::default());
        assert!(retries.is_empty());
        let deadline = Some(now + Duration::from_millis(40));
        assert_eq!(
            next_retry_deadline(retries.is_empty(), deadline, false, now),
            None
        );
    }

    #[test]
    fn obs06_retry_gives_up_at_max() {
        let now = Instant::now();
        let mut retries: HashMap<String, u8> = HashMap::new();
        let attempted = obs06_set(&["A.md"]);
        let mut deadline = None;

        for attempt in 1..=MAX_LOCKED_RETRIES {
            let plan =
                update_locked_retries(&mut retries, &attempted, obs06_locked(&["A.md"]), false);
            assert_eq!(
                plan,
                RetryPlan {
                    scheduled: 1,
                    gave_up: 0
                }
            );
            assert_eq!(retries.get("A.md"), Some(&attempt));
            deadline = next_retry_deadline(retries.is_empty(), deadline, true, now);
            assert!(deadline.is_some());
        }

        // Locked once more after MAX_LOCKED_RETRIES retries: give up.
        let plan = update_locked_retries(&mut retries, &attempted, obs06_locked(&["A.md"]), false);
        assert_eq!(
            plan,
            RetryPlan {
                scheduled: 0,
                gave_up: 1
            }
        );
        assert!(retries.is_empty());
        assert_eq!(
            next_retry_deadline(retries.is_empty(), deadline, plan.scheduled > 0, now),
            None
        );
    }

    #[test]
    fn obs06_full_sync_does_not_drop_pending() {
        let now = Instant::now();
        let deadline = Some(now + Duration::from_millis(40));
        let mut retries = HashMap::from([("A.md".to_string(), 1u8)]);

        let plan = update_locked_retries(&mut retries, &obs06_set(&["A.md"]), Vec::new(), true);

        assert_eq!(plan, RetryPlan::default());
        assert_eq!(retries, HashMap::from([("A.md".to_string(), 1u8)]));
        assert_eq!(
            next_retry_deadline(retries.is_empty(), deadline, false, now),
            deadline
        );
    }

    #[test]
    fn obs06_mixed_batch_tracks_each_path_separately() {
        let mut retries = HashMap::from([
            ("A.md".to_string(), 1u8),
            ("Gone.md".to_string(), 2u8),
            ("Limit.md".to_string(), MAX_LOCKED_RETRIES),
        ]);
        // A is pending and untouched; Gone and Limit are retried; B is newly locked.
        let attempted = obs06_set(&["Gone.md", "Limit.md", "B.md"]);

        let plan = update_locked_retries(
            &mut retries,
            &attempted,
            obs06_locked(&["Limit.md", "B.md"]),
            false,
        );

        assert_eq!(
            plan,
            RetryPlan {
                scheduled: 1,
                gave_up: 1
            }
        );
        assert_eq!(
            retries,
            HashMap::from([("A.md".to_string(), 1u8), ("B.md".to_string(), 1u8)])
        );
    }

    #[tokio::test]
    async fn test_native_watcher_incremental_save_atomic_rename_dir_and_zero_idle_rescans() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("WatchVault");
        fs::create_dir_all(root.join("FolderA")).unwrap();
        fs::write(root.join("Alpha.md"), "# Alpha\n\nInitial alpha.\n").unwrap();
        fs::write(
            root.join("FolderA/Nested.md"),
            "# Nested\n\nLinks to [[Alpha]].\n",
        )
        .unwrap();

        let db_file = tmp.path().join("index.db");
        let vault = vault_from_dir(&root, Some("WatchVault"));
        let storage = Storage::open(&db_file).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (ctrl, diag) = spawn_vault_watcher_with_options(
            vault.clone(),
            storage.clone(),
            500,
            tx,
            WatcherOptions::default(),
        );
        assert_eq!(diag.mode(), WatcherMode::Native);

        // Initial startup event
        let ev0 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev0, CoreEvent::VaultIndexed { .. }));
        assert_eq!(diag.post_init_full_scans.load(Ordering::Acquire), 0);

        // 1. Atomic replace on Alpha.md -> incremental update, 0 full scans
        write_atomic(&root.join("Alpha.md"), b"# Alpha\n\nATOMIC_TOKEN_101\n").unwrap();
        let ev1 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, CoreEvent::VaultIndexed { .. }));
        assert_eq!(diag.post_init_full_scans.load(Ordering::Acquire), 0);
        assert!(diag.incremental_batches.load(Ordering::Acquire) >= 1);
        let hits = storage
            .search_notes(&vault.id, "ATOMIC_TOKEN_101", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);

        // 2. Directory rename FolderA -> FolderB removes old FolderA/Nested.md and indexes FolderB/Nested.md
        fs::rename(root.join("FolderA"), root.join("FolderB")).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(rem, rx.recv()).await.unwrap().unwrap();
            if matches!(ev, CoreEvent::VaultIndexed { .. }) {
                let notes = storage.list_notes(&vault.id, None).unwrap();
                if notes.iter().any(|n| n.rel_path == "FolderB/Nested.md")
                    && !notes.iter().any(|n| n.rel_path == "FolderA/Nested.md")
                {
                    break;
                }
            }
        }
        let bl = storage.get_backlinks(&vault.id, "Alpha.md").unwrap();
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].source_path, "FolderB/Nested.md");

        // 3. Idle for 1.2s (> 2x watch_poll_ms=500ms) -> 0 full scans and 0 spurious events
        let full_before = diag.post_init_full_scans.load(Ordering::Acquire);
        let spurious = tokio::time::timeout(Duration::from_millis(1200), rx.recv()).await;
        assert!(
            spurious.is_err(),
            "idle vault must not emit spurious events"
        );
        assert_eq!(
            diag.post_init_full_scans.load(Ordering::Acquire),
            full_before,
            "idle native watcher must run 0 repeated full scans"
        );

        // 4. DB-only bookmark toggle from another connection -> detected via PRAGMA data_version
        let storage_other = Storage::open(&db_file).unwrap();
        assert!(storage_other
            .toggle_bookmark(&vault.id, "Alpha.md")
            .unwrap());
        let ev_bm = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev_bm, CoreEvent::VaultIndexed { .. }));
        assert!(diag.db_change_reloads.load(Ordering::Acquire) >= 1);
        assert!(
            storage
                .get_note_summary(&vault.id, "Alpha.md")
                .unwrap()
                .unwrap()
                .is_bookmarked
        );

        // 5. Explicit rescan notification triggers exactly one full reconciliation
        let full_before_rescan = diag.post_init_full_scans.load(Ordering::Acquire);
        ctrl.request_rescan();
        let ev_rescan = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev_rescan, CoreEvent::VaultIndexed { .. }));
        assert_eq!(
            diag.post_init_full_scans.load(Ordering::Acquire),
            full_before_rescan + 1
        );
    }

    #[tokio::test]
    async fn test_polling_fallback_mode_and_burst_bounded_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("FallbackVault");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Seed.md"), "# Seed\n\nInitial.\n").unwrap();

        let vault = vault_from_dir(&root, Some("FallbackVault"));
        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (_ctrl, diag) = spawn_vault_watcher_with_options(
            vault.clone(),
            storage.clone(),
            500,
            tx,
            WatcherOptions {
                force_polling_fallback: true,
                ..Default::default()
            },
        );
        assert_eq!(diag.mode(), WatcherMode::PollingFallback);
        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();

        fs::write(root.join("Seed.md"), "# Seed\n\nFALLBACK_POLL_TOKEN_909\n").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, CoreEvent::VaultIndexed { .. }));
        assert_eq!(
            storage
                .search_notes(&vault.id, "FALLBACK_POLL_TOKEN_909", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(diag.post_init_full_scans.load(Ordering::Acquire) >= 1);
    }

    #[tokio::test]
    async fn test_burst_copy_stays_bounded_and_drains_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("BurstVault");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Anchor.md"), "# Anchor\n\nStart.\n").unwrap();

        let vault = vault_from_dir(&root, Some("BurstVault"));
        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (_ctrl, diag) = spawn_vault_watcher_with_options(
            vault.clone(),
            storage.clone(),
            2_500,
            tx,
            WatcherOptions::default(),
        );
        assert_eq!(diag.mode(), WatcherMode::Native);
        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Burst-create 320 notes (> MAX_PENDING_PATHS = 256)
        for i in 0..320 {
            fs::write(
                root.join(format!("Burst_{i:04}.md")),
                format!("# Burst {i}\n\nBURST_NOTE_{i:04}\n"),
            )
            .unwrap();
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            let _ = tokio::time::timeout(rem, rx.recv()).await.unwrap().unwrap();
            if storage.note_count(&vault.id).unwrap() == 321 {
                break;
            }
        }
        assert_eq!(diag.pending_queue_len.load(Ordering::Acquire), 0);
        assert!(
            diag.pending_queue_high_water.load(Ordering::Acquire) <= (MAX_PENDING_PATHS + 1) as u64,
            "pending queue high water must remain bounded"
        );
    }

    #[test]
    fn test_watcher_config_disables_follow_symlinks() {
        assert!(
            !watcher_config().follow_symlinks(),
            "native watcher config must disable follow_symlinks to match vault traversal"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_native_watcher_skips_symlink_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("SymlinkVault");
        let outside = tmp.path().join("OutsideTarget");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("Real.md"), "# Real\n\nREAL_INIT\n").unwrap();
        fs::write(outside.join("External.md"), "# External\n\nOUTSIDE_INIT\n").unwrap();
        symlink(&outside, root.join("LinkedDir")).unwrap();

        let vault = vault_from_dir(&root, Some("SymlinkVault"));
        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (_ctrl, diag) = spawn_vault_watcher_with_options(
            vault.clone(),
            storage.clone(),
            2_500,
            tx,
            WatcherOptions::default(),
        );
        assert_eq!(diag.mode(), WatcherMode::Native);

        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(storage.note_count(&vault.id).unwrap(), 1);
        assert!(scan_vault_markdown_subdir(&root, &root.join("LinkedDir"))
            .unwrap()
            .is_empty());

        // Mutate file inside the symlinked external directory: watcher must ignore it.
        fs::write(
            outside.join("External.md"),
            "# External\n\nOUTSIDE_MUTATED_999\n",
        )
        .unwrap();
        let ignored = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        assert!(
            ignored.is_err(),
            "mutations inside symlinked directory must not emit watcher events"
        );
        assert_eq!(storage.note_count(&vault.id).unwrap(), 1);
        assert!(storage
            .search_notes(&vault.id, "OUTSIDE_MUTATED_999", 10)
            .unwrap()
            .is_empty());

        // Real vault file mutations still propagate normally.
        fs::write(root.join("Real.md"), "# Real\n\nREAL_UPDATED_123\n").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, CoreEvent::VaultIndexed { .. }));
        assert_eq!(
            storage
                .search_notes(&vault.id, "REAL_UPDATED_123", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_symlinked_home_vault_scan_and_watcher_work_while_inner_symlinks_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let real_home = tmp.path().join("real_home");
        let linked_home = tmp.path().join("linked_home");
        let real_vault = real_home.join("Vault");
        let outside = tmp.path().join("OutsideSecret");
        fs::create_dir_all(&real_vault).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(real_vault.join("HomeNote.md"), "# HomeNote\n\nINIT_HOME\n").unwrap();
        fs::write(
            outside.join("External.md"),
            "# External\n\nOUTSIDE_SECRET_INIT\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&real_home, &linked_home).unwrap();
            symlink(&outside, real_vault.join("InnerLink")).unwrap();
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_dir;
            let is_unprivileged = |err: &std::io::Error| {
                err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(1314)
            };
            if let Err(err) = symlink_dir(&real_home, &linked_home) {
                if is_unprivileged(&err) {
                    return;
                }
                panic!("failed to create symlink_dir for home: {err}");
            }
            if let Err(err) = symlink_dir(&outside, real_vault.join("InnerLink")) {
                if is_unprivileged(&err) {
                    return;
                }
                panic!("failed to create symlink_dir inside vault: {err}");
            }
        }

        let linked_vault = linked_home.join("Vault");
        let scanned = crate::vault::fs::scan_vault_markdown_files(&linked_vault).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].rel_path, "HomeNote.md");

        let mut vault = vault_from_dir(&linked_vault, Some("LinkedHomeVault"));
        // Keep the uncanonicalized symlinked-HOME path in VaultProfile, matching config.toml/obsidian.json.
        vault.path = linked_vault.clone();

        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (_ctrl, diag) = spawn_vault_watcher_with_options(
            vault.clone(),
            storage.clone(),
            2_500,
            tx,
            WatcherOptions::default(),
        );
        assert_eq!(diag.mode(), WatcherMode::Native);

        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(storage.note_count(&vault.id).unwrap(), 1);

        // Mutations inside the inner symlinked directory must still be ignored.
        fs::write(
            outside.join("External.md"),
            "# External\n\nOUTSIDE_MUTATED_SYMLINK_HOME\n",
        )
        .unwrap();
        let ignored = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
        assert!(
            ignored.is_err(),
            "mutations inside inner symlinked directory must be ignored"
        );
        assert_eq!(storage.note_count(&vault.id).unwrap(), 1);
        assert!(storage
            .search_notes(&vault.id, "OUTSIDE_MUTATED_SYMLINK_HOME", 10)
            .unwrap()
            .is_empty());

        // Mutations to real notes under the symlinked $HOME vault must propagate cleanly.
        fs::write(
            linked_vault.join("HomeNote.md"),
            "# HomeNote\n\nSYMLINK_HOME_UPDATED_777\n",
        )
        .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, CoreEvent::VaultIndexed { .. }));
        assert_eq!(
            storage
                .search_notes(&vault.id, "SYMLINK_HOME_UPDATED_777", 10)
                .unwrap()
                .len(),
            1
        );
    }
}
