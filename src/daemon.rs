//! Lightweight single-instance background vault watcher & IPC daemon (`obsidian daemon`).
//!
//! Uses `obsidian-cli` loopback/IPC metadata (`obsidian-daemon.json` + `obsidian-daemon.lock`),
//! keeps SQLite WAL + FTS5 index fresh in the background, and lets TUI/CLI attach or nudge re-indexing.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, watch},
};

use crate::{
    config::{ensure_parent, write_atomic_0600, AppConfig, IPC_SERVICE_NAME},
    models::CoreEvent,
    storage::Storage,
    sync::{self, FocusTarget, SyncCommand, SyncController},
    vault::resolve_active_vault,
};

const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const STARTUP_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(60);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_millis(1500);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub service: String,
    pub transport: Option<String>,
    pub endpoint: Option<String>,
    pub vault_name: Option<String>,
    pub uptime_secs: Option<u64>,
    pub attached_clients: usize,
    pub last_sync_at: Option<String>,
    pub total_notes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonRuntimeInfo {
    pid: u32,
    version: String,
    service: String,
    transport: String,
    endpoint: String,
    secret: String,
    vault_name: String,
    started_at_unix: u64,
}

#[derive(Debug, Clone)]
struct DaemonPaths {
    state_file: PathBuf,
    lock_file: PathBuf,
}

impl DaemonPaths {
    fn from_dir(dir: &Path) -> Self {
        Self {
            state_file: dir.join("obsidian-daemon.json"),
            lock_file: dir.join("obsidian-daemon.lock"),
        }
    }

    fn current() -> Self {
        Self::from_dir(&crate::config::config_dir())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClientFrame {
    secret: String,
    op: ClientOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientOp {
    Status,
    Reindex,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerReply {
    Status { status: DaemonStatus },
    Ok { message: String },
    Error { message: String },
}

pub struct TuiDaemonConnection {
    pub sync_ctrl: SyncController,
    pub events_rx: mpsc::UnboundedReceiver<CoreEvent>,
    pub status: DaemonStatus,
}

/// Starts the background daemon process detached if not already running.
pub async fn start_daemon(vault_override: Option<&str>) -> Result<DaemonStatus> {
    let paths = DaemonPaths::current();
    if let Ok(st) = query_status_at(&paths).await {
        if st.running {
            return Ok(st);
        }
    }

    let exe = std::env::current_exe().context("Unable to determine obsidian executable path")?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(v) = vault_override {
        cmd.args(["--vault", v]);
    }
    cmd.args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }

    cmd.spawn().context("Failed to spawn obsidian background daemon")?;

    let deadline = Instant::now() + STARTUP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(st) = query_status_at(&paths).await {
            if st.running {
                return Ok(st);
            }
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }

    bail!("Timed out waiting for obsidian daemon to start")
}

/// Stops the running background daemon gracefully.
pub async fn stop_daemon() -> Result<bool> {
    let paths = DaemonPaths::current();
    let Ok(info) = read_runtime_info(&paths.state_file) else {
        return Ok(false);
    };
    let _ = send_rpc(&info, ClientOp::Shutdown).await;
    let deadline = Instant::now() + STOP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if !paths.state_file.exists() {
            return Ok(true);
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
    let _ = fs::remove_file(&paths.state_file);
    let _ = fs::remove_file(&paths.lock_file);
    Ok(true)
}

/// Queries current daemon status.
pub async fn daemon_status() -> Result<DaemonStatus> {
    query_status_at(&DaemonPaths::current()).await
}

async fn query_status_at(paths: &DaemonPaths) -> Result<DaemonStatus> {
    let Ok(info) = read_runtime_info(&paths.state_file) else {
        return Ok(DaemonStatus {
            running: false,
            service: IPC_SERVICE_NAME.to_string(),
            ..Default::default()
        });
    };

    match send_rpc(&info, ClientOp::Status).await {
        Ok(ServerReply::Status { status }) => Ok(status),
        _ => {
            let _ = fs::remove_file(&paths.state_file);
            Ok(DaemonStatus {
                running: false,
                service: IPC_SERVICE_NAME.to_string(),
                ..Default::default()
            })
        }
    }
}

/// Runs the foreground daemon event loop (invoked by `obsidian daemon run`).
pub async fn run_daemon_foreground(
    cfg: AppConfig,
    vault_override: Option<String>,
) -> Result<()> {
    let paths = DaemonPaths::current();
    ensure_parent(&paths.lock_file)?;
    let _lock = acquire_lock(&paths.lock_file)?;

    let vault = resolve_active_vault(&cfg, vault_override.as_deref())?;
    let storage = Storage::open_default()?;
    let initial_stats = storage.sync_vault(&vault)?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind daemon IPC loopback listener")?;
    let addr: SocketAddr = listener.local_addr()?;

    let mut secret_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let secret = hex::encode(secret_bytes);
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let info = DaemonRuntimeInfo {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        service: IPC_SERVICE_NAME.to_string(),
        transport: "tcp-loopback".to_string(),
        endpoint: addr.to_string(),
        secret: secret.clone(),
        vault_name: vault.display_name().to_string(),
        started_at_unix: started_unix,
    };
    write_atomic_0600(&paths.state_file, &serde_json::to_vec_pretty(&info)?)?;

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = sync::spawn_vault_watcher(vault.clone(), storage.clone(), cfg.watch_poll_ms, events_tx);

    let last_sync = Arc::new(Mutex::new((
        Some(chrono::Utc::now().to_rfc3339()),
        initial_stats.total_notes,
    )));
    let last_sync_bg = last_sync.clone();
    tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            if let CoreEvent::VaultIndexed { stats } = ev {
                if let Ok(mut guard) = last_sync_bg.lock() {
                    *guard = (Some(chrono::Utc::now().to_rfc3339()), stats.total_notes);
                }
            }
        }
    });

    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(4);
    let attached = Arc::new(AtomicUsize::new(0));

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            accept_res = listener.accept() => {
                let Ok((stream, _)) = accept_res else { continue };
                let info_c = info.clone();
                let sync_c = sync_ctrl.clone();
                let last_c = last_sync.clone();
                let shut_c = shutdown_tx.clone();
                let att_c = attached.clone();
                tokio::spawn(async move {
                    let _ = handle_client(stream, info_c, sync_c, last_c, shut_c, att_c).await;
                });
            }
        }
    }

    let _ = fs::remove_file(&paths.state_file);
    let _ = fs::remove_file(&paths.lock_file);
    Ok(())
}

async fn handle_client(
    stream: TcpStream,
    info: DaemonRuntimeInfo,
    sync_ctrl: SyncController,
    last_sync: Arc<Mutex<(Option<String>, usize)>>,
    shutdown_tx: broadcast::Sender<()>,
    attached: Arc<AtomicUsize>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }
    let frame: ClientFrame = match serde_json::from_str(line.trim()) {
        Ok(f) => f,
        Err(err) => {
            let reply = ServerReply::Error {
                message: format!("Invalid IPC frame: {err}"),
            };
            let _ = write_reply(&mut write_half, &reply).await;
            return Ok(());
        }
    };
    if frame.secret != info.secret {
        let reply = ServerReply::Error {
            message: "Unauthorized IPC secret".to_string(),
        };
        let _ = write_reply(&mut write_half, &reply).await;
        return Ok(());
    }

    let reply = match frame.op {
        ClientOp::Status => {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(info.started_at_unix);
            let (last_at, total_notes) = last_sync
                .lock()
                .map(|g| g.clone())
                .unwrap_or((None, 0));
            ServerReply::Status {
                status: DaemonStatus {
                    running: true,
                    pid: Some(info.pid),
                    version: Some(info.version.clone()),
                    service: info.service.clone(),
                    transport: Some(info.transport.clone()),
                    endpoint: Some(info.endpoint.clone()),
                    vault_name: Some(info.vault_name.clone()),
                    uptime_secs: Some(now_unix.saturating_sub(info.started_at_unix)),
                    attached_clients: attached.load(Ordering::Relaxed),
                    last_sync_at: last_at,
                    total_notes,
                },
            }
        }
        ClientOp::Reindex => {
            sync_ctrl.reindex_now();
            ServerReply::Ok {
                message: "Reindex scheduled".to_string(),
            }
        }
        ClientOp::Shutdown => {
            let _ = shutdown_tx.send(());
            ServerReply::Ok {
                message: "Daemon shutting down".to_string(),
            }
        }
    };

    write_reply(&mut write_half, &reply).await
}

async fn write_reply<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, reply: &ServerReply) -> Result<()> {
    let mut payload = serde_json::to_vec(reply)?;
    payload.push(b'\n');
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

async fn send_rpc(info: &DaemonRuntimeInfo, op: ClientOp) -> Result<ServerReply> {
    let stream = tokio::time::timeout(IPC_CONNECT_TIMEOUT, TcpStream::connect(&info.endpoint))
        .await
        .map_err(|_| anyhow!("IPC connect timeout"))??;
    let (read_half, mut write_half) = stream.into_split();
    let frame = ClientFrame {
        secret: info.secret.clone(),
        op,
    };
    let mut payload = serde_json::to_vec(&frame)?;
    payload.push(b'\n');
    write_half.write_all(&payload).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(IPC_CONNECT_TIMEOUT, reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("IPC read timeout"))??;
    serde_json::from_str(line.trim()).context("Invalid daemon IPC reply")
}

fn read_runtime_info(path: &Path) -> Result<DaemonRuntimeInfo> {
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire_lock(path: &Path) -> Result<LockGuard> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("Failed to acquire daemon lock: {}", path.display()))?;
    writeln!(file, "{}", std::process::id())?;
    Ok(LockGuard(path.to_path_buf()))
}

/// Convenience helper for creating a disconnected mock handle in tests.
pub fn dummy_connection() -> TuiDaemonConnection {
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (_ev_tx, ev_rx) = mpsc::unbounded_channel::<CoreEvent>();
    TuiDaemonConnection {
        sync_ctrl: SyncController::from_channels(focus_tx, cmd_tx),
        events_rx: ev_rx,
        status: DaemonStatus::default(),
    }
}
