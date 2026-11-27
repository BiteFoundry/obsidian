#[allow(unused_imports)]
use std::{fs, sync::Arc};

#[allow(unused_imports)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
#[allow(unused_imports)]
use obsidian_cli::{
    config::AppConfig,
    mcp::{tool_definitions, McpServer},
    models::SidebarTab,
    serve::{build_router, ServeState},
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        action::Action,
        app::{AppState, CanvasMode, UiState},
    },
    vault::{
        discover_native_vaults_from,
        fs::{ensure_daily_note, toggle_task_at_line},
        init_starter_vault, vault_from_dir,
    },
};
#[allow(unused_imports)]
use ratatui::{backend::TestBackend, Terminal};
#[allow(unused_imports)]
use serde_json::{json, Value};
#[allow(unused_imports)]
use tokio::sync::{mpsc, watch};

#[allow(unused_imports)]
use crate::common::*;

// ===========================================================================
// v0.4.1 regression tests
// ===========================================================================

struct ObsServer {
    base: String,
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ObsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn obs_serve(
    cfg: AppConfig,
    vault: &obsidian_cli::config::VaultProfile,
    storage: Storage,
    policy: Option<obsidian_cli::serve::ServePolicy>,
) -> ObsServer {
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let app = match policy {
        Some(policy) => obsidian_cli::serve::build_router_with_policy(state, policy),
        None => build_router(state),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    ObsServer {
        base: format!("http://{addr}"),
        port: addr.port(),
        task,
    }
}

fn obs_count_files(dir: &std::path::Path) -> usize {
    let mut count = 0;
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += obs_count_files(&path);
        } else {
            count += 1;
        }
    }
    count
}

/// Registered vault `vault` plus an unregistered sibling directory `outside` holding `secret.md`.
fn obs01_fixture(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = tmp.join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Home.md"), "# Home\n\nRegistered note.\n").unwrap();
    let outside = tmp.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.md"), "# Secret\n\nTOPSECRET-PAYLOAD\n").unwrap();
    (root, outside)
}

#[tokio::test]
async fn obs01_http_rejects_unregistered_vault_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    let server = obs_serve(cfg, &vault, storage.clone(), None).await;
    let client = reqwest::Client::new();
    let outside_spec = outside.to_string_lossy().to_string();
    let outside_id = vault_from_dir(&outside, None).id;

    let read = client
        .get(format!("{}/api/v1/notes/secret.md", server.base))
        .query(&[("vault", outside_spec.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::NOT_FOUND);
    let body = read.text().await.unwrap();
    assert!(!body.contains("raw_markdown"), "{body}");
    assert!(!body.contains("TOPSECRET-PAYLOAD"), "{body}");

    for path in [
        "/api/v1/notes",
        "/api/v1/search?q=secret",
        "/api/v1/tags",
        "/api/v1/tasks",
    ] {
        let resp = client
            .get(format!("{}{path}", server.base))
            .query(&[("vault", outside_spec.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(storage.note_count(&outside_id).unwrap(), 0);

    let before = obs_count_files(&outside);
    let create = client
        .post(format!("{}/api/v1/notes", server.base))
        .json(&json!({ "title": "pwn", "content": "x", "vault": outside_spec }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::NOT_FOUND);
    let daily = client
        .post(format!("{}/api/v1/daily", server.base))
        .json(&json!({ "vault": outside_spec }))
        .send()
        .await
        .unwrap();
    assert_eq!(daily.status(), StatusCode::NOT_FOUND);
    let reindex = client
        .post(format!("{}/api/v1/reindex", server.base))
        .json(&json!({ "vault": outside_spec }))
        .send()
        .await
        .unwrap();
    assert_eq!(reindex.status(), StatusCode::NOT_FOUND);
    assert_eq!(obs_count_files(&outside), before);
    assert!(!outside.join("pwn.md").exists());
    assert_eq!(storage.note_count(&outside_id).unwrap(), 0);
}

#[tokio::test]
async fn obs01_http_unregistered_error_is_uniform() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let server = obs_serve(cfg, &vault, Storage::open_in_memory().unwrap(), None).await;
    let client = reqwest::Client::new();

    let existing = outside.to_string_lossy().to_string();
    let missing = tmp.path().join("no-such-dir").to_string_lossy().to_string();
    let mut messages = Vec::new();
    for spec in [&existing, &missing] {
        let resp = client
            .get(format!("{}/api/v1/notes", server.base))
            .query(&[("vault", spec.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: Value = resp.json().await.unwrap();
        let message = body["error"].as_str().unwrap().to_string();
        assert!(message.contains(spec.as_str()), "{message}");
        messages.push(message.replace(spec.as_str(), "<spec>"));
    }
    assert_eq!(messages[0], messages[1]);
}

#[tokio::test]
async fn obs01_http_default_unregistered_vault_still_served() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    // Launch vault is NOT registered in the config (`--vault <dir> serve`).
    let vault = vault_from_dir(&root, None);
    let server = obs_serve(
        AppConfig::default(),
        &vault,
        Storage::open_in_memory().unwrap(),
        None,
    )
    .await;
    let client = reqwest::Client::new();
    let path_spec = root.to_string_lossy().to_string();
    let specs: [Option<&str>; 4] = [
        None,
        Some(vault.id.as_str()),
        Some(path_spec.as_str()),
        Some(vault.name.as_str()),
    ];
    for spec in specs {
        let mut req = client.get(format!("{}/api/v1/notes", server.base));
        if let Some(spec) = spec {
            req = req.query(&[("vault", spec)]);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{spec:?}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["count"], 1, "{spec:?}");
    }
}

#[tokio::test]
async fn obs01_http_registered_second_vault_by_name_id_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let second_root = tmp.path().join("second");
    fs::create_dir_all(&second_root).unwrap();
    fs::write(second_root.join("Only Second.md"), "# Only Second\n").unwrap();
    let vault = vault_from_dir(&root, Some("obs01-first"));
    let second = vault_from_dir(&second_root, Some("obs01-second"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    cfg.upsert_vault(second.clone(), false);
    let server = obs_serve(cfg, &vault, Storage::open_in_memory().unwrap(), None).await;
    let client = reqwest::Client::new();
    let path_spec = second_root.to_string_lossy().to_string();
    for spec in ["obs01-second", second.id.as_str(), path_spec.as_str()] {
        let resp = client
            .get(format!("{}/api/v1/notes", server.base))
            .query(&[("vault", spec)])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{spec}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["count"], 1, "{spec}");
        assert_eq!(body["notes"][0]["rel_path"], "Only Second.md", "{spec}");
    }
}

fn obs_has_cors_headers(resp: &reqwest::Response) -> bool {
    resp.headers()
        .keys()
        .any(|name| name.as_str().starts_with("access-control-allow"))
}

#[tokio::test]
async fn obs01_http_cross_origin_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let server = obs_serve(cfg, &vault, Storage::open_in_memory().unwrap(), None).await;
    let client = reqwest::Client::new();

    for origin in [
        "https://evil.example",
        "null",
        "http://localhost.evil.example",
    ] {
        for path in ["/health", "/api/v1/vaults"] {
            let resp = client
                .get(format!("{}{path}", server.base))
                .header("Origin", origin)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{origin} {path}");
            assert!(!obs_has_cors_headers(&resp), "{origin} {path}");
        }
    }

    // Preflight: no CORS grant for a foreign origin, nor for a loopback origin by default.
    for origin in ["https://evil.example", "http://localhost:3000"] {
        let preflight = client
            .request(
                reqwest::Method::OPTIONS,
                format!("{}/api/v1/notes", server.base),
            )
            .header("Origin", origin)
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Headers", "content-type")
            .send()
            .await
            .unwrap();
        assert!(!obs_has_cors_headers(&preflight), "{origin}");
        assert!(!preflight.status().is_success(), "{origin}");
    }

    // A bodyless cross-site form POST is a "simple request" (no preflight): it must not write.
    let daily_before = obs_count_files(&root.join("Daily Notes"));
    let daily = client
        .post(format!("{}/api/v1/daily", server.base))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(daily.status(), StatusCode::FORBIDDEN);
    assert_eq!(obs_count_files(&root.join("Daily Notes")), daily_before);
}

#[tokio::test]
async fn obs01_http_loopback_origin_and_no_origin_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let server = obs_serve(cfg, &vault, Storage::open_in_memory().unwrap(), None).await;
    let client = reqwest::Client::new();

    let plain = client
        .get(format!("{}/api/v1/notes", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    for origin in [
        "http://localhost:3000",
        "http://127.0.0.1:5173",
        "http://[::1]:8080",
        "http://localhost",
    ] {
        let resp = client
            .get(format!("{}/api/v1/notes", server.base))
            .header("Origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{origin}");
        assert!(!obs_has_cors_headers(&resp), "{origin}");
    }
}

#[tokio::test]
async fn obs01_http_host_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let server = obs_serve(
        cfg.clone(),
        &vault,
        Storage::open_in_memory().unwrap(),
        None,
    )
    .await;
    let client = reqwest::Client::new();
    let port = server.port;

    let get_with_host = |base: String, host: String| {
        let client = client.clone();
        async move {
            client
                .get(format!("{base}/health"))
                .header("Host", host)
                .send()
                .await
                .unwrap()
                .status()
        }
    };

    for host in [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        "LOCALHOST".to_string(),
        format!("localhost.:{port}"),
    ] {
        assert_eq!(
            get_with_host(server.base.clone(), host.clone()).await,
            StatusCode::OK,
            "{host}"
        );
    }
    for host in [
        "evil.example:18792".to_string(),
        "evil.example".to_string(),
        format!("127.0.0.1.evil.example:{port}"),
        format!("localhost@evil.example:{port}"),
        "nas.local".to_string(),
    ] {
        assert_eq!(
            get_with_host(server.base.clone(), host.clone()).await,
            StatusCode::FORBIDDEN,
            "{host}"
        );
    }

    // Extra hosts are opt-in through the policy (`--allow-host` / the non-loopback bind address).
    let policy = obsidian_cli::serve::ServePolicy {
        extra_hosts: vec!["NAS.local".to_string()],
        allowed_origins: Vec::new(),
    };
    let extra = obs_serve(
        cfg,
        &vault,
        Storage::open_in_memory().unwrap(),
        Some(policy),
    )
    .await;
    assert_eq!(
        get_with_host(extra.base.clone(), format!("nas.local:{}", extra.port)).await,
        StatusCode::OK
    );
    assert_eq!(
        get_with_host(extra.base.clone(), "evil.example".to_string()).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn obs01_serve_refuses_non_loopback_bind_without_token() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));

    // Returns before binding, so no socket is ever opened on 0.0.0.0.
    let err = obsidian_cli::serve::run_http_server(
        AppConfig::default(),
        vault.clone(),
        Storage::open_in_memory().unwrap(),
        "0.0.0.0",
        0,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--token"), "{err}");

    // `--allow-keyless` does not lift the refusal, and a blank token counts as no token.
    let opts = obsidian_cli::serve::ServeOptions {
        token: Some("   ".to_string()),
        allow_keyless: true,
        ..Default::default()
    };
    let err = obsidian_cli::serve::run_http_server_with_options(
        AppConfig::default(),
        vault.clone(),
        Storage::open_in_memory().unwrap(),
        "0.0.0.0",
        0,
        opts,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--token"), "{err}");

    // A wildcard origin is refused at startup.
    let opts = obsidian_cli::serve::ServeOptions {
        allow_origins: vec!["*".to_string()],
        ..Default::default()
    };
    let err = obsidian_cli::serve::run_http_server_with_options(
        AppConfig::default(),
        vault,
        Storage::open_in_memory().unwrap(),
        "127.0.0.1",
        0,
        opts,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--allow-origin"), "{err}");
}

#[tokio::test]
async fn obs01_allow_origin_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let policy = obsidian_cli::serve::ServePolicy {
        extra_hosts: Vec::new(),
        allowed_origins: vec!["https://app.example.com".to_string()],
    };
    let server = obs_serve(
        cfg,
        &vault,
        Storage::open_in_memory().unwrap(),
        Some(policy),
    )
    .await;
    let client = reqwest::Client::new();

    let allowed = client
        .get(format!("{}/api/v1/notes", server.base))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );

    let preflight = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/v1/notes", server.base),
        )
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "authorization,content-type",
        )
        .send()
        .await
        .unwrap();
    assert!(preflight.status().is_success());
    assert_eq!(
        preflight.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    let methods = preflight.headers()["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(methods.contains("POST"), "{methods}");

    for origin in [
        "https://evil.example",
        "https://app.example.com.evil.example",
    ] {
        let other = client
            .get(format!("{}/api/v1/notes", server.base))
            .header("Origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::FORBIDDEN, "{origin}");
        assert!(!obs_has_cors_headers(&other), "{origin}");
    }
}

#[test]
fn obs01_mcp_rejects_unregistered_vault_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, outside) = obs01_fixture(tmp.path());
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    let mcp = McpServer::new(cfg, vault.clone(), storage.clone());
    let outside_spec = outside.to_string_lossy().to_string();
    let outside_id = vault_from_dir(&outside, None).id;

    let err = mcp
        .dispatch_tool(
            "read_note",
            &json!({ "note": "secret", "vault": outside_spec }),
        )
        .unwrap_err();
    assert!(err.to_string().contains("is not registered"), "{err}");
    assert!(!err.to_string().contains("TOPSECRET-PAYLOAD"));

    let before = obs_count_files(&outside);
    assert!(mcp
        .dispatch_tool(
            "create_note",
            &json!({ "title": "pwn", "content": "x", "vault": outside_spec })
        )
        .is_err());
    assert!(mcp
        .dispatch_tool("daily_note", &json!({ "vault": outside_spec }))
        .is_err());
    assert!(mcp
        .dispatch_tool("reindex_vault", &json!({ "vault": outside_spec }))
        .is_err());
    assert_eq!(obs_count_files(&outside), before);
    assert_eq!(storage.note_count(&outside_id).unwrap(), 0);

    // The launch vault and registered vaults keep working by name, ID, and path.
    let path_spec = root.to_string_lossy().to_string();
    for spec in ["fixture", vault.id.as_str(), path_spec.as_str()] {
        let ok = mcp
            .dispatch_tool("read_note", &json!({ "note": "Home", "vault": spec }))
            .unwrap_or_else(|e| panic!("{spec}: {e}"));
        assert_eq!(ok["rel_path"], "Home.md", "{spec}");
    }
}

/// Spawns `bitefoundry-obsidian serve --port 0` and collects its stderr until the listen line.
struct ObsServeProcess {
    child: std::process::Child,
    base: String,
    token: Option<String>,
}

impl Drop for ObsServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn obs_spawn_serve(
    tmp: &std::path::Path,
    root: &std::path::Path,
    extra: &[&str],
) -> ObsServeProcess {
    use std::io::BufRead;
    let mut child = isolated_cli(tmp, root)
        .args(["serve", "--port", "0"])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut base = None;
    let mut token = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // The listen line comes first; the token or keyless notice follows immediately.
    let mut lines_after_listen = 0;
    while std::time::Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(std::time::Duration::from_millis(500)) else {
            if base.is_some() {
                break;
            }
            continue;
        };
        if let Some(rest) = line.strip_prefix("obsidian HTTP API listening on ") {
            base = rest.split_whitespace().next().map(str::to_string);
        } else if let Some(rest) =
            line.strip_prefix("obsidian HTTP API token (generated for this run): ")
        {
            token = Some(rest.trim().to_string());
        }
        if base.is_some() {
            lines_after_listen += 1;
            if lines_after_listen >= 2 {
                break;
            }
        }
    }
    ObsServeProcess {
        child,
        base: base.expect("serve did not print its listen address"),
        token,
    }
}

#[tokio::test]
async fn obs01_serve_generates_token_on_loopback() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    let client = reqwest::Client::new();

    let first = obs_spawn_serve(tmp.path(), &root, &[]);
    let token = first.token.clone().expect("a generated token on stderr");
    assert_eq!(token.len(), 64, "32 random bytes as hex: {token}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));

    let health = client
        .get(format!("{}/health", first.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let anonymous = client
        .get(format!("{}/api/v1/notes", first.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong = client
        .get(format!("{}/api/v1/notes", first.base))
        .bearer_auth("0".repeat(64))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let authed = client
        .get(format!("{}/api/v1/notes", first.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
    drop(first);

    // A second run gets a different token.
    let second = obs_spawn_serve(tmp.path(), &root, &[]);
    assert_ne!(second.token.as_deref(), Some(token.as_str()));
    drop(second);

    // An explicit --token is used as given and never echoed.
    let explicit = obs_spawn_serve(tmp.path(), &root, &["--token", "my-local-secret"]);
    assert!(explicit.token.is_none());
    let authed = client
        .get(format!("{}/api/v1/notes", explicit.base))
        .bearer_auth("my-local-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
    drop(explicit);

    // --allow-keyless opts out on loopback.
    let keyless = obs_spawn_serve(tmp.path(), &root, &["--allow-keyless"]);
    assert!(keyless.token.is_none());
    let anonymous = client
        .get(format!("{}/api/v1/notes", keyless.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::OK);
}

#[test]
fn obs01_serve_cli_refuses_non_loopback_bind_without_token() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _outside) = obs01_fixture(tmp.path());
    for extra in [&[][..], &["--allow-keyless"][..]] {
        let out = isolated_cli(tmp.path(), &root)
            .args(["serve", "--bind", "0.0.0.0", "--port", "0"])
            .args(extra)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{extra:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("without --token"), "{stderr}");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn obs02_http_drive_prefix_cannot_escape_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let server = obs_serve(cfg, &vault, Storage::open_in_memory().unwrap(), None).await;
    let client = reqwest::Client::new();
    let cwd = std::env::current_dir().unwrap();
    let before = obs_count_files(&root);

    let cases = [
        (
            "/api/v1/notes",
            json!({ "title": "esc", "folder": "C:obs-esc-test", "content": "pwn" }),
        ),
        (
            "/api/v1/notes",
            json!({ "title": "esc", "folder": "D:outside", "content": "pwn" }),
        ),
        (
            "/api/v1/notes",
            json!({ "title": "esc", "folder": "sub/C:obs-esc-test", "content": "pwn" }),
        ),
        ("/api/v1/daily", json!({ "date": "C:daily-esc" })),
        ("/api/v1/daily", json!({ "date": "2026-11-23:stream" })),
    ];
    for (path, body) in cases {
        let resp = client
            .post(format!("{}{path}", server.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path} {body}");
        assert!(!cwd.join("obs-esc-test").exists(), "{body}");
        assert!(!cwd.join("daily-esc.md").exists(), "{body}");
        assert_eq!(obs_count_files(&root), before, "{body}");
    }
}

#[cfg(windows)]
#[test]
fn obs02_mcp_drive_prefix_cannot_escape_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);
    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let mcp = McpServer::new(cfg, vault, Storage::open_in_memory().unwrap());
    let cwd = std::env::current_dir().unwrap();
    let before = obs_count_files(&root);

    let cases = [
        (
            "create_note",
            json!({ "title": "esc", "folder": "C:obs-esc-test", "content": "pwn" }),
        ),
        (
            "create_note",
            json!({ "title": "esc", "folder": "D:outside", "content": "pwn" }),
        ),
        ("daily_note", json!({ "date": "C:daily-esc" })),
    ];
    for (tool, args) in cases {
        assert!(mcp.dispatch_tool(tool, &args).is_err(), "{tool} {args}");
        assert!(!cwd.join("obs-esc-test").exists(), "{args}");
        assert!(!cwd.join("daily-esc.md").exists(), "{args}");
        assert_eq!(obs_count_files(&root), before, "{args}");
    }
}

#[cfg(windows)]
#[test]
fn obs15_unc_vault_opens() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    // C:\path\to\vault -> \\localhost\C$\path\to\vault (the administrative share).
    let local = vault_from_dir(&root, None)
        .path
        .to_string_lossy()
        .to_string();
    let Some((drive, rest)) = local.split_once(':') else {
        eprintln!("obs15_unc_vault_opens: skipped (no drive letter in {local})");
        return;
    };
    let unc = std::path::PathBuf::from(format!(r"\\localhost\{drive}{}{rest}", '$'));
    if !unc.is_dir() {
        eprintln!(
            "obs15_unc_vault_opens: skipped (admin share not reachable: {})",
            unc.display()
        );
        return;
    }

    let vault = vault_from_dir(&unc, Some("unc-fixture"));
    let vault_path = vault.path.to_string_lossy().to_string();
    assert!(
        vault_path.to_ascii_lowercase().starts_with(r"\\localhost\"),
        "{vault_path}"
    );
    assert!(vault.path.is_absolute());

    let storage = Storage::open_in_memory().unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.total_notes, 3);

    let note_abs = fs::canonicalize(unc.join("Projects").join("Q4 Architecture RFC.md")).unwrap();
    let canonical_root = fs::canonicalize(&unc).unwrap();
    assert_eq!(
        obsidian_cli::vault::fs::rel_path_in_vault(&vault.path, Some(&canonical_root), &note_abs)
            .as_deref(),
        Some("Projects/Q4 Architecture RFC.md")
    );
}

fn obs08_run(test_root: &std::path::Path, args: &[&str]) -> std::process::Output {
    isolated_cli_base(test_root).args(args).output().unwrap()
}

#[test]
fn obs08_vaults_add_use_remove_refuse_unparsable_config() {
    let tmp = tempfile::tempdir().unwrap();
    let test_root = tmp.path();
    let vault_dir = test_root.join("existing-vault");
    fs::create_dir_all(&vault_dir).unwrap();
    fs::write(vault_dir.join("Note.md"), "# Note\n").unwrap();
    fs::write(test_root.join("obsidian.json"), "{\"vaults\":{}}").unwrap();

    let config_path = test_root.join("config/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    // A valid registered vault followed by a syntax error.
    let broken = format!(
        "active_vault = \"X\"\n\n[[vaults]]\nid = \"v-x\"\nname = \"X\"\npath = '{}'\n\nthis line is not toml ===\n",
        vault_dir.display()
    );
    fs::write(&config_path, &broken).unwrap();

    let new_vault = test_root.join("starter-vault");
    let vault_arg = vault_dir.to_str().unwrap();
    let new_vault_arg = new_vault.to_str().unwrap();
    let mutating: [&[&str]; 4] = [
        &["vaults", "add", vault_arg],
        &["vaults", "use", "X"],
        &["vaults", "remove", "X"],
        &["init", new_vault_arg],
    ];
    for args in mutating {
        let output = obs08_run(test_root, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} succeeded: {stderr}");
        assert!(stderr.contains("config.toml"), "{args:?}: {stderr}");
        assert!(stderr.contains("Nothing was changed"), "{args:?}: {stderr}");
        assert_eq!(
            fs::read(&config_path).unwrap(),
            broken.as_bytes(),
            "{args:?} changed the config file"
        );
    }
    // `init` stops before creating the starter vault.
    assert!(!new_vault.exists());

    // Read-only commands still run on defaults, with a warning, and leave the file alone.
    let output = isolated_cli(test_root, &vault_dir)
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("could not be parsed"), "{stderr}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Note.md"));
    assert_eq!(fs::read(&config_path).unwrap(), broken.as_bytes());
}

#[test]
fn obs08_missing_config_still_bootstraps() {
    let tmp = tempfile::tempdir().unwrap();
    let test_root = tmp.path();
    let vault_dir = test_root.join("first-vault");
    fs::create_dir_all(&vault_dir).unwrap();
    fs::write(vault_dir.join("Note.md"), "# Note\n").unwrap();
    fs::write(test_root.join("obsidian.json"), "{\"vaults\":{}}").unwrap();
    let config_path = test_root.join("config/config.toml");
    assert!(!config_path.exists());

    let output = obs08_run(test_root, &["vaults", "add", vault_dir.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());

    let saved = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(saved.vaults.len(), 1);
    assert_eq!(saved.active_vault, "first-vault");

    // `init` bootstraps the same way and keeps the earlier registration.
    let starter = test_root.join("starter-vault");
    let output = obs08_run(test_root, &["init", starter.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(saved.vaults.len(), 2);
}

fn obs09_daily_vault(
    root: &std::path::Path,
    name: &str,
    format: &str,
) -> obsidian_cli::config::VaultProfile {
    fs::create_dir_all(root.join(".obsidian")).unwrap();
    fs::write(
        root.join(".obsidian/daily-notes.json"),
        serde_json::to_string(&json!({ "folder": "", "format": format })).unwrap(),
    )
    .unwrap();
    vault_from_dir(root, Some(name))
}

#[tokio::test]
async fn obs09_daily_note_via_mcp_and_http_uses_vault_folder_and_format() {
    for (format, expected) in [
        ("YYYY-MM-DD dddd", "2026-11-23 Monday.md"),
        ("YYYY-MMM-DD", "2026-Nov-23.md"),
    ] {
        // MCP `daily_note`
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("McpDaily");
        let vault = obs09_daily_vault(&root, "McpDaily", format);
        let mut cfg = AppConfig::default();
        cfg.upsert_vault(vault.clone(), true);
        let mcp = McpServer::new(cfg, vault.clone(), Storage::open_in_memory().unwrap());
        let result = mcp
            .dispatch_tool("daily_note", &json!({ "date": "2026-11-23" }))
            .unwrap();
        assert_eq!(result["rel_path"], expected, "MCP, format {format}");
        assert_eq!(result["created"], true);
        assert!(root.join(expected).is_file());
        assert!(!root.join("Daily Notes").exists());
        let again = mcp
            .dispatch_tool(
                "daily_note",
                &json!({ "date": "2026-11-23", "append": "- [ ] once" }),
            )
            .unwrap();
        assert_eq!(again["rel_path"], expected);
        assert_eq!(again["created"], false);

        // HTTP `POST /api/v1/daily`
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("HttpDaily");
        let vault = obs09_daily_vault(&root, "HttpDaily", format);
        let mut cfg = AppConfig::default();
        cfg.upsert_vault(vault.clone(), true);
        let state = Arc::new(ServeState {
            cfg,
            default_vault: vault.clone(),
            storage: Storage::open_in_memory().unwrap(),
            token: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let daily: Value = reqwest::Client::new()
            .post(format!("http://{addr}/api/v1/daily"))
            .json(&json!({ "date": "2026-11-23" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        server.abort();
        assert_eq!(daily["ok"], true, "HTTP, format {format}: {daily}");
        assert_eq!(daily["rel_path"], expected);
        assert_eq!(daily["created"], true);
        assert!(root.join(expected).is_file());
        assert!(!root.join("Daily Notes").exists());
    }
}

#[tokio::test]
async fn obs09_daily_format_with_colon_is_rejected_on_windows_not_written_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ColonDaily");
    let vault = obs09_daily_vault(&root, "ColonDaily", "YYYY-MM-DD [T]HH:mm");
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage: Storage::open_in_memory().unwrap(),
        token: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/daily"))
        .json(&json!({ "date": "2026-11-23" }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    server.abort();

    let markdown_files = || {
        fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .count()
    };
    if cfg!(windows) {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(markdown_files(), 0);
    } else {
        assert_eq!(status, StatusCode::OK);
        assert!(root.join("2026-11-23 THH:mm.md").is_file());
    }
}

fn obs13_app(
    root: &std::path::Path,
    files: &[(&str, &str)],
) -> (
    AppState,
    UiState,
    obsidian_cli::config::VaultProfile,
    Storage,
) {
    for (rel, body) in files {
        let abs = root.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, body).unwrap();
    }
    let vault = vault_from_dir(root, Some("Obs13Vault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut app = AppState::new(cfg.clone(), vault.clone(), storage.clone(), sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    (app, ui, vault, storage)
}

fn obs13_markdown_files(root: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn obs13_active(app: &AppState) -> Option<String> {
    app.active_note.as_ref().map(|d| d.summary.rel_path.clone())
}

#[test]
fn obs13_ambiguous_reference_creates_nothing() {
    use obsidian_cli::ui::{app::OpenIntent, toast::ToastKind};

    let tmp = tempfile::tempdir().unwrap();
    let (mut app, mut ui, _vault, _storage) = obs13_app(
        tmp.path(),
        &[
            ("A/Note.md", "# Note A\n"),
            ("B/Note.md", "# Note B\n"),
            ("Start.md", "# Start\n\n[[Note]]\n"),
        ],
    );
    app.open_note(&mut ui, "Start.md", false);
    let before = obs13_markdown_files(tmp.path());

    for intent in [OpenIntent::Navigate, OpenIntent::FollowLink] {
        ui.toasts.clear();
        app.open_note_with(&mut ui, "Note", true, intent);

        assert_eq!(obs13_markdown_files(tmp.path()), before, "{intent:?}");
        assert_eq!(obs13_active(&app).as_deref(), Some("Start.md"));
        assert!(app.history.is_empty(), "{intent:?}: {:?}", app.history);
        let toast = ui
            .toasts
            .latest()
            .expect("a toast explains why nothing opened");
        assert_eq!(toast.kind, ToastKind::Warning);
        assert!(toast.message.contains("Ambiguous"), "{}", toast.message);
        assert!(toast.message.contains("A/Note.md"), "{}", toast.message);
    }
}

#[test]
fn obs13_history_back_to_deleted_note_creates_nothing() {
    use obsidian_cli::ui::toast::ToastKind;

    let tmp = tempfile::tempdir().unwrap();
    let (mut app, mut ui, vault, storage) = obs13_app(
        tmp.path(),
        &[("A.md", "# A\n"), ("B.md", "# B\n"), ("C.md", "# C\n")],
    );
    app.open_note(&mut ui, "A.md", false);
    app.open_note(&mut ui, "B.md", true);
    app.open_note(&mut ui, "C.md", true);
    assert_eq!(app.history, ["A.md", "B.md"]);

    // B is deleted and the index has not caught up yet: the read fails, nothing is recreated.
    fs::remove_file(tmp.path().join("B.md")).unwrap();
    app.dispatch(Action::NavigateBack, &mut ui);
    assert!(!tmp.path().join("B.md").exists());
    assert_eq!(obs13_active(&app).as_deref(), Some("C.md"));
    assert_eq!(ui.toasts.latest().unwrap().kind, ToastKind::Error);
    assert_eq!(app.history, ["A.md"]);

    // A is deleted and the index knows: not found, nothing is recreated, the entry is dropped.
    fs::remove_file(tmp.path().join("A.md")).unwrap();
    storage.sync_vault(&vault).unwrap();
    app.reload_from_storage(&mut ui);
    ui.toasts.clear();
    app.dispatch(Action::NavigateBack, &mut ui);
    assert!(!tmp.path().join("A.md").exists());
    assert_eq!(obs13_markdown_files(tmp.path()), ["C.md"]);
    assert_eq!(obs13_active(&app).as_deref(), Some("C.md"));
    let toast = ui.toasts.latest().unwrap();
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.contains("Note not found"),
        "{}",
        toast.message
    );
    assert!(app.history.is_empty());
}

#[test]
fn obs13_plain_missing_reference_creates_nothing() {
    use obsidian_cli::ui::toast::ToastKind;

    let tmp = tempfile::tempdir().unwrap();
    let (mut app, mut ui, _vault, _storage) = obs13_app(tmp.path(), &[("Start.md", "# Start\n")]);
    app.open_note(&mut ui, "Start.md", false);

    app.open_note(&mut ui, "Nope", true);
    assert_eq!(obs13_markdown_files(tmp.path()), ["Start.md"]);
    assert_eq!(obs13_active(&app).as_deref(), Some("Start.md"));
    assert!(app.history.is_empty());
    let toast = ui.toasts.latest().unwrap();
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Note not found: Nope");

    // The quick switcher and the sidebar go through the same action.
    app.dispatch(Action::OpenNote("Also Missing.md".into()), &mut ui);
    assert_eq!(obs13_markdown_files(tmp.path()), ["Start.md"]);

    // An unresolved target is not created by a backwards navigation either.
    app.open_note(&mut ui, "unresolved:Ghost", false);
    assert_eq!(obs13_markdown_files(tmp.path()), ["Start.md"]);
}

#[test]
fn obs13_unresolved_link_still_creates() {
    use obsidian_cli::ui::{app::OpenIntent, toast::ToastKind};

    let tmp = tempfile::tempdir().unwrap();
    let (mut app, mut ui, _vault, _storage) = obs13_app(
        tmp.path(),
        &[("Start.md", "# Start\n\n[[New Idea]] and [[Linked Idea]]\n")],
    );
    app.open_note(&mut ui, "Start.md", false);

    // The graph's node ID for a link target with no note.
    app.open_note(&mut ui, "unresolved:New Idea", true);
    assert!(tmp.path().join("New Idea.md").is_file());
    assert_eq!(obs13_active(&app).as_deref(), Some("New Idea.md"));
    assert_eq!(app.history, ["Start.md"]);
    let toast = ui.toasts.latest().unwrap();
    assert_eq!(toast.kind, ToastKind::Success);
    assert_eq!(toast.message, "Created note: New Idea.md");

    // Following a wikilink whose target does not exist.
    app.open_note_with(&mut ui, "Linked Idea", true, OpenIntent::FollowLink);
    assert!(tmp.path().join("Linked Idea.md").is_file());
    assert_eq!(obs13_active(&app).as_deref(), Some("Linked Idea.md"));
    assert_eq!(app.history, ["Start.md", "New Idea.md"]);

    // Once the note exists, the same reference opens it instead of creating a second file.
    app.open_note(&mut ui, "Start.md", true);
    app.open_note(&mut ui, "unresolved:New Idea", true);
    assert_eq!(obs13_active(&app).as_deref(), Some("New Idea.md"));
    assert_eq!(
        obs13_markdown_files(tmp.path()),
        ["Linked Idea.md", "New Idea.md", "Start.md"]
    );
}

#[test]
fn mcp_tool_descriptions_carry_cost_hints() {
    let tools = tool_definitions();
    let description = |name: &str| -> String {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} is missing"))["description"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let reindex = description("reindex_vault");
    assert!(reindex.contains("full"), "{reindex}");
    assert!(!reindex.to_lowercase().contains("incremental"), "{reindex}");
    assert!(reindex.contains("Slow on large vaults"), "{reindex}");

    for (name, hint) in [
        ("list_vaults", "slow"),
        ("find_orphans", "cost grows with vault size"),
        ("get_local_graph", "slow on big vaults"),
        ("list_tags", "whole vault"),
    ] {
        let text = description(name);
        assert!(text.contains(hint), "{name}: {text}");
    }

    // Names, count and schemas are untouched: only description strings moved.
    let names: std::collections::BTreeSet<&str> =
        tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(tools.len(), 20);
    assert_eq!(names.len(), 20);
    for tool in &tools {
        let text = tool["description"].as_str().unwrap();
        assert!(
            text.chars().count() <= 400,
            "{} description has {} characters",
            tool["name"],
            text.chars().count()
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
    }
}
