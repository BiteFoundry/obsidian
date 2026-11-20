//! Incremental background vault watcher & re-indexer.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::{
    config::VaultProfile,
    models::CoreEvent,
    storage::Storage,
    vault::fs::{content_sha256_hex, scan_vault_html_files},
};

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

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    cmd_tx: mpsc::UnboundedSender<SyncCommand>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        cmd_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self { focus_tx, cmd_tx }
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
}

/// Spawns an incremental filesystem watcher task polling `(mtime_ms, size_bytes)` on `poll_ms`.
pub fn spawn_vault_watcher(
    initial_vault: VaultProfile,
    storage: Storage,
    poll_ms: u64,
    events_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let interval_ms = poll_ms.clamp(500, 60_000);
    tokio::spawn(async move {
        let mut vault = initial_vault;
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_fingerprint: Option<String> = None;

        // Initial scan (stat-based, no full-vault hashing)
        run_sync_once(&vault, &storage, &events_tx, true, false, &mut last_fingerprint).await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    run_sync_once(&vault, &storage, &events_tx, false, false, &mut last_fingerprint).await;
                }
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(SyncCommand::ReindexNow) => {
                            run_sync_once(&vault, &storage, &events_tx, true, true, &mut last_fingerprint).await;
                        }
                        Some(SyncCommand::SwitchVault { vault: next }) => {
                            vault = next;
                            last_fingerprint = None;
                            run_sync_once(&vault, &storage, &events_tx, true, false, &mut last_fingerprint).await;
                        }
                        None => break,
                    }
                }
            }
        }
    });

    SyncController { focus_tx, cmd_tx }
}

async fn run_sync_once(
    vault: &VaultProfile,
    storage: &Storage,
    events_tx: &mpsc::UnboundedSender<CoreEvent>,
    always_emit: bool,
    force_hash_check: bool,
    last_fingerprint: &mut Option<String>,
) {
    let vault_clone = vault.clone();
    let storage_clone = storage.clone();
    let res = tokio::task::spawn_blocking(move || {
        let stats = if force_hash_check {
            storage_clone.reindex_vault(&vault_clone)?
        } else {
            storage_clone.sync_vault(&vault_clone)?
        };
        let db_fp = storage_clone
            .vault_index_fingerprint(&vault_clone.id)
            .unwrap_or_default();
        let html_fp = html_files_fingerprint(&vault_clone.path);
        Ok::<_, anyhow::Error>((stats, format!("{db_fp}|{html_fp}")))
    })
    .await;
    match res {
        Ok(Ok((stats, fp))) => {
            let fp_changed = last_fingerprint.as_deref() != Some(fp.as_str());
            *last_fingerprint = Some(fp);
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

fn html_files_fingerprint(vault_root: &std::path::Path) -> String {
    let Ok(files) = scan_vault_html_files(vault_root) else {
        return String::new();
    };
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
