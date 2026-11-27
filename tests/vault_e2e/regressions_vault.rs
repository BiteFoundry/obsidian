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

#[test]
fn regression_obsidian_obs_01_hash_filename_resolution() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs01Vault");
    fs::create_dir_all(&root).unwrap();

    let meeting_orig = "# Meeting\n\n## Agenda\n\nOriginal meeting body.\n";
    let meeting_1_orig = "# Meeting #1\n\nFirst numbered meeting body.\n";
    let referrer_orig = "# Index\n\nSee [[Meeting#Agenda]].\n";
    fs::write(root.join("Meeting.md"), meeting_orig).unwrap();
    fs::write(root.join("Meeting #1.md"), meeting_1_orig).unwrap();
    fs::write(root.join("Index.md"), referrer_orig).unwrap();
    let meeting_hash_before = content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap());

    // 1. CLI create/read/append/replace/delete for `#` filenames
    let create_cs = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "create", "C# Guide"])
        .output()
        .unwrap();
    assert!(
        create_cs.status.success(),
        "create C# Guide failed: {}",
        String::from_utf8_lossy(&create_cs.stderr)
    );
    assert!(root.join("C# Guide.md").is_file());

    for ident in ["C# Guide", "C# Guide.md"] {
        let out = isolated_cli(tmp.path(), &root)
            .args(["--json", "notes", "read", ident])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "read '{ident}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let payload: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(payload["rel_path"], "C# Guide.md");
    }

    // Read Meeting #1.md via CLI -> must open Meeting #1.md, NOT Meeting.md
    let read_m1 = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", "Meeting #1.md"])
        .output()
        .unwrap();
    assert!(
        read_m1.status.success(),
        "read Meeting #1.md failed: {}",
        String::from_utf8_lossy(&read_m1.stderr)
    );
    let read_m1_json: Value = serde_json::from_slice(&read_m1.stdout).unwrap();
    assert_eq!(read_m1_json["rel_path"], "Meeting #1.md");
    assert!(read_m1_json["raw_markdown"]
        .as_str()
        .unwrap()
        .contains("First numbered meeting body."));

    // Append to Meeting #1.md -> only Meeting #1.md changes; Meeting.md hash unchanged
    let app_m1 = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "append",
            "Meeting #1.md",
            "ONLY_FOR_MEETING_1",
        ])
        .output()
        .unwrap();
    assert!(
        app_m1.status.success(),
        "append Meeting #1.md failed: {}",
        String::from_utf8_lossy(&app_m1.stderr)
    );
    let app_m1_json: Value = serde_json::from_slice(&app_m1.stdout).unwrap();
    assert_eq!(app_m1_json["rel_path"], "Meeting #1.md");
    assert!(fs::read_to_string(root.join("Meeting #1.md"))
        .unwrap()
        .contains("ONLY_FOR_MEETING_1"));
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before,
        "Meeting.md must not be modified when appending to Meeting #1.md"
    );

    // Control: writing to nonexistent Meeting #2.md must fail with 0 changed files
    let meeting_1_hash_before_bad =
        content_sha256_hex(&fs::read(root.join("Meeting #1.md")).unwrap());
    for args in [
        vec!["notes", "append", "Meeting #2.md", "SHOULD_NOT_WRITE"],
        vec!["notes", "replace", "Meeting #2.md", "Original", "Corrupted"],
        vec!["notes", "delete", "--yes", "Meeting #2.md"],
        vec!["notes", "move", "Meeting #2.md", "Moved.md"],
    ] {
        let bad = isolated_cli(tmp.path(), &root)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            !bad.status.success(),
            "nonexistent Meeting #2.md must fail for {args:?}, stdout={}",
            String::from_utf8_lossy(&bad.stdout)
        );
    }
    assert!(root.join("Meeting.md").is_file());
    assert!(root.join("Meeting #1.md").is_file());
    assert!(!root.join("Meeting #2.md").exists());
    assert!(!root.join("Moved.md").exists());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting #1.md")).unwrap()),
        meeting_1_hash_before_bad
    );

    // Delete Meeting #1.md -> moves Meeting #1.md to .trash, preserves Meeting.md
    let del_m1 = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "delete", "--yes", "Meeting #1.md"])
        .output()
        .unwrap();
    assert!(
        del_m1.status.success(),
        "delete Meeting #1.md failed: {}",
        String::from_utf8_lossy(&del_m1.stderr)
    );
    let del_m1_json: Value = serde_json::from_slice(&del_m1.stdout).unwrap();
    assert_eq!(del_m1_json["rel_path"], "Meeting #1.md");
    assert!(!root.join("Meeting #1.md").exists());
    assert!(root.join(".trash/Meeting #1.md").is_file());
    assert!(root.join("Meeting.md").is_file());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    // Control: wikilink [[Meeting#Agenda]] resolves to Meeting.md in link context
    let vault = vault_from_dir(&root, Some("Obs01Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    let index_links = storage.get_outgoing_links(&vault, "Index.md").unwrap();
    assert!(
        index_links.iter().any(|l| l.target_raw == "Meeting"
            && l.heading.as_deref() == Some("Agenda")
            && l.resolved
            && l.target_resolved_path.as_deref() == Some("Meeting.md")),
        "[[Meeting#Agenda]] must resolve as section link to Meeting.md, got {index_links:?}"
    );
    let meeting_bl = storage.get_backlinks(&vault.id, "Meeting.md").unwrap();
    assert!(meeting_bl.iter().any(|b| b.source_path == "Index.md"));
}

#[tokio::test]
async fn regression_obsidian_obs_01_mcp_and_http_hash_filenames() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs01ApiVault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Meeting.md"), "# Meeting\n\nBase.\n").unwrap();
    fs::write(root.join("Meeting #1.md"), "# Meeting #1\n\nOne.\n").unwrap();
    fs::write(root.join("C# Guide.md"), "# C# Guide\n\nSharp.\n").unwrap();
    let meeting_hash_before = content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap());

    let vault = vault_from_dir(&root, Some("Obs01ApiVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // MCP read/append/replace/move + nonexistent Meeting #2.md guard
    let mcp = McpServer::new(cfg.clone(), vault.clone(), storage.clone());
    let cs = mcp
        .dispatch_tool("read_note", &json!({ "note": "C# Guide" }))
        .unwrap();
    assert_eq!(cs["rel_path"], "C# Guide.md");
    let cs_ext = mcp
        .dispatch_tool("read_note", &json!({ "note": "C# Guide.md" }))
        .unwrap();
    assert_eq!(cs_ext["rel_path"], "C# Guide.md");
    assert!(mcp
        .dispatch_tool(
            "append_note",
            &json!({ "note": "Meeting #2.md", "text": "NOPE" })
        )
        .is_err());
    let app = mcp
        .dispatch_tool(
            "append_note",
            &json!({ "note": "Meeting #1.md", "text": "MCP_LINE" }),
        )
        .unwrap();
    assert_eq!(app["rel_path"], "Meeting #1.md");
    let rep = mcp
        .dispatch_tool(
            "replace_note_text",
            &json!({ "note": "Meeting #1.md", "find": "One.", "replace": "Two." }),
        )
        .unwrap();
    assert_eq!(rep["rel_path"], "Meeting #1.md");
    assert_eq!(rep["replaced"], true);
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    // HTTP read/append for # filename
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let r = client
        .get(format!("{base}/api/v1/notes/C%23%20Guide.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r_json: Value = r.json().await.unwrap();
    assert_eq!(r_json["note"]["summary"]["rel_path"], "C# Guide.md");

    let bad_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "Meeting #2.md", "text": "NOPE" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ap.status(), StatusCode::NOT_FOUND);

    let ok_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "Meeting #1.md", "text": "HTTP_LINE" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_ap.status(), StatusCode::OK);
    let ok_ap_json: Value = ok_ap.json().await.unwrap();
    assert_eq!(ok_ap_json["rel_path"], "Meeting #1.md");
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    server.abort();
}

#[test]
fn regression_obsidian_obs_13_vault_selection_conflicts_and_daemon() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use obsidian_cli::vault::fs::content_sha256_hex;

    fn run_cli_bounded(
        cmd: &mut std::process::Command,
        scratch: &std::path::Path,
        label: &str,
        timeout: Duration,
    ) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
        let out_path = scratch.join(format!("{label}.stdout"));
        let err_path = scratch.join(format!("{label}.stderr"));
        let out_file = fs::File::create(&out_path).unwrap();
        let err_file = fs::File::create(&err_path).unwrap();
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = fs::read_to_string(&out_path).unwrap_or_default();
                let stderr = fs::read_to_string(&err_path).unwrap_or_default();
                panic!("CLI command '{label}' timed out after {timeout:?}\nstdout={stdout}\nstderr={stderr}");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let stdout = fs::read(&out_path).unwrap_or_default();
        let stderr = fs::read(&err_path).unwrap_or_default();
        (status, stdout, stderr)
    }

    struct DaemonStopGuard(std::path::PathBuf);
    impl Drop for DaemonStopGuard {
        fn drop(&mut self) {
            if let Ok(mut child) = isolated_cli_base(&self.0)
                .env_remove("OBSIDIAN_VAULT")
                .args(["daemon", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let vault_a = tmp.path().join("VaultA");
    let vault_b = tmp.path().join("VaultB");
    let vault_c = tmp.path().join("VaultC");
    fs::create_dir_all(&vault_a).unwrap();
    fs::create_dir_all(&vault_b).unwrap();
    fs::create_dir_all(&vault_c).unwrap();

    fs::write(vault_a.join("NoteA.md"), "# NoteA\n\nAlpha body.\n").unwrap();
    fs::write(vault_b.join("NoteB.md"), "# NoteB\n\nBeta body.\n").unwrap();
    fs::write(vault_c.join("NoteC.md"), "# NoteC\n\nGamma body.\n").unwrap();

    let hash_a = content_sha256_hex(&fs::read(vault_a.join("NoteA.md")).unwrap());
    let hash_b = content_sha256_hex(&fs::read(vault_b.join("NoteB.md")).unwrap());

    // Control 1: First run with zero selection args keeps default native discovery (obsidian.json)
    let discovery_tmp = tempfile::tempdir().unwrap();
    fs::write(
        discovery_tmp.path().join("obsidian.json"),
        json!({
            "vaults": {
                "nat1": { "path": vault_a.to_string_lossy(), "ts": 1732000000i64, "open": true }
            }
        })
        .to_string(),
    )
    .unwrap();
    let first_run = isolated_cli_base(discovery_tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(
        first_run.status.success(),
        "default discovery failed: {}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first_notes: Value = serde_json::from_slice(&first_run.stdout).unwrap();
    assert_eq!(first_notes["notes"].as_array().unwrap().len(), 1);
    assert_eq!(first_notes["notes"][0]["rel_path"], "NoteA.md");

    // Register VaultA so A exists in config.toml
    let add_a = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "vaults", "add"])
        .arg(&vault_a)
        .output()
        .unwrap();
    assert!(
        add_a.status.success(),
        "add VaultA failed: {}",
        String::from_utf8_lossy(&add_a.stderr)
    );
    let config_path = tmp.path().join("config").join("config.toml");
    let cfg_hash_before = content_sha256_hex(&fs::read(&config_path).unwrap());

    // 1. `vaults use <unregistered C>` must fail, advise `vaults add`, and leave config & files untouched
    let use_c = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use"])
        .arg(&vault_c)
        .output()
        .unwrap();
    assert!(
        !use_c.status.success(),
        "vaults use on unregistered path must fail, stdout={}",
        String::from_utf8_lossy(&use_c.stdout)
    );
    let use_c_err = String::from_utf8_lossy(&use_c.stderr);
    assert!(
        use_c_err.contains("vaults add"),
        "expected vaults add guidance in stderr, got: {use_c_err}"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(&config_path).unwrap()),
        cfg_hash_before,
        "config.toml must remain unchanged after rejected vaults use"
    );

    // 2. Invalid OBSIDIAN_VAULT and invalid --vault must abort append with zero writes
    let bad_env_append = isolated_cli_base(tmp.path())
        .env("OBSIDIAN_VAULT", "NonExistentVaultXYZ")
        .args(["notes", "append", "NoteA", "SHOULD_NOT_APPEND"])
        .output()
        .unwrap();
    assert!(
        !bad_env_append.status.success(),
        "invalid OBSIDIAN_VAULT must fail, stdout={}",
        String::from_utf8_lossy(&bad_env_append.stdout)
    );

    let bad_flag_append = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args([
            "--vault",
            "NonExistentVaultXYZ",
            "notes",
            "append",
            "NoteA",
            "SHOULD_NOT_APPEND",
        ])
        .output()
        .unwrap();
    assert!(!bad_flag_append.status.success());

    assert_eq!(
        content_sha256_hex(&fs::read(&config_path).unwrap()),
        cfg_hash_before
    );
    assert_eq!(
        content_sha256_hex(&fs::read(vault_a.join("NoteA.md")).unwrap()),
        hash_a
    );
    assert_eq!(
        content_sha256_hex(&fs::read(vault_b.join("NoteB.md")).unwrap()),
        hash_b
    );

    // 3. Explicitly add B then use A -> use B; next process opens B
    let add_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "vaults", "add"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(
        add_b.status.success(),
        "{}",
        String::from_utf8_lossy(&add_b.stderr)
    );
    let add_b_json: Value = serde_json::from_slice(&add_b.stdout).unwrap();
    let vault_b_id = add_b_json["vault"]["id"].as_str().unwrap().to_string();

    let use_a = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use", "VaultA"])
        .output()
        .unwrap();
    assert!(
        use_a.status.success(),
        "{}",
        String::from_utf8_lossy(&use_a.stderr)
    );

    let use_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(
        use_b.status.success(),
        "{}",
        String::from_utf8_lossy(&use_b.stderr)
    );

    let list_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(
        list_b.status.success(),
        "{}",
        String::from_utf8_lossy(&list_b.stderr)
    );
    let notes_b: Value = serde_json::from_slice(&list_b.stdout).unwrap();
    assert_eq!(notes_b["notes"].as_array().unwrap().len(), 1);
    assert_eq!(notes_b["notes"][0]["rel_path"], "NoteB.md");

    // 4. Same name + different path -> error; same name + same path -> idempotent
    let dup_name_diff_path = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_c)
        .args(["--name", "VaultA"])
        .output()
        .unwrap();
    assert!(
        !dup_name_diff_path.status.success(),
        "registering duplicate name for different path must fail, stdout={}",
        String::from_utf8_lossy(&dup_name_diff_path.stdout)
    );
    let cfg_after_dup = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(
        obsidian_cli::config::normalize_path_key(&cfg_after_dup.find_vault("VaultA").unwrap().path),
        obsidian_cli::config::normalize_path_key(&vault_a),
        "existing VaultA path must not be overwritten"
    );

    let dup_name_same_path = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_a)
        .args(["--name", "VaultA"])
        .output()
        .unwrap();
    assert!(
        dup_name_same_path.status.success(),
        "idempotent re-add of same name + same path must succeed: {}",
        String::from_utf8_lossy(&dup_name_same_path.stderr)
    );
    let cfg_after_idem = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(cfg_after_idem.vaults.len(), 2);

    // 5. Removing active vault by ID clears active_vault selection
    let use_b_again = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use", "VaultB"])
        .output()
        .unwrap();
    assert!(use_b_again.status.success());
    let rm_b_by_id = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "remove", &vault_b_id])
        .output()
        .unwrap();
    assert!(rm_b_by_id.status.success());
    let cfg_after_rm_id = AppConfig::load_from(&config_path).unwrap();
    assert!(
        cfg_after_rm_id.active_vault.is_empty(),
        "removing active vault by ID must clear active_vault, got '{}'",
        cfg_after_rm_id.active_vault
    );

    // Re-add B for daemon conflict test
    let readd_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(readd_b.status.success());

    // 6. Daemon start for VaultA, idempotent same-vault start, error on different-vault start preserving running daemon
    let daemon_io = tmp.path().join("daemon_io");
    fs::create_dir_all(&daemon_io).unwrap();
    let _guard = DaemonStopGuard(tmp.path().to_path_buf());
    let (start_a_status, start_a_out, start_a_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultA", "daemon", "start"]),
        &daemon_io,
        "start_a",
        Duration::from_secs(15),
    );
    assert!(
        start_a_status.success(),
        "daemon start VaultA failed: {}",
        String::from_utf8_lossy(&start_a_err)
    );
    let st_a: Value = serde_json::from_slice(&start_a_out).unwrap();
    assert_eq!(st_a["running"], true);
    assert_eq!(st_a["vault_name"], "VaultA");
    let pid_a = st_a["pid"].as_u64().unwrap();

    // Control: same vault daemon start returns existing running status
    let (start_a2_status, start_a2_out, start_a2_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultA", "daemon", "start"]),
        &daemon_io,
        "start_a_again",
        Duration::from_secs(10),
    );
    assert!(
        start_a2_status.success(),
        "same-vault daemon start failed: {}",
        String::from_utf8_lossy(&start_a2_err)
    );
    let st_a2: Value = serde_json::from_slice(&start_a2_out).unwrap();
    assert_eq!(st_a2["pid"].as_u64(), Some(pid_a));
    assert_eq!(st_a2["vault_name"], "VaultA");

    // Conflict: different vault daemon start must fail naming both VaultA and VaultB and preserve VaultA daemon
    let (start_b_status, start_b_out, start_b_err_bytes) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultB", "daemon", "start"]),
        &daemon_io,
        "start_b",
        Duration::from_secs(10),
    );
    assert!(
        !start_b_status.success(),
        "daemon start for VaultB while VaultA daemon runs must fail, stdout={}",
        String::from_utf8_lossy(&start_b_out)
    );
    assert!(
        start_b_err_bytes.is_empty(),
        "expected empty stderr on --json error, got: {}",
        String::from_utf8_lossy(&start_b_err_bytes)
    );
    let start_b_json: Value = serde_json::from_slice(&start_b_out).unwrap();
    assert_eq!(start_b_json["ok"], false);
    let start_b_err = start_b_json["error"]["message"].as_str().unwrap_or("");
    assert!(
        start_b_err.contains("VaultA") && start_b_err.contains("VaultB"),
        "daemon conflict error must include both running and requested vault names, got: {start_b_err}"
    );

    let (status_after_ok, status_after_out, status_after_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "daemon", "status"]),
        &daemon_io,
        "status_after",
        Duration::from_secs(10),
    );
    assert!(
        status_after_ok.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&status_after_err)
    );
    let st_after: Value = serde_json::from_slice(&status_after_out).unwrap();
    assert_eq!(st_after["running"], true);
    assert_eq!(st_after["pid"].as_u64(), Some(pid_a));
    assert_eq!(st_after["vault_name"], "VaultA");

    let (stop_ok, _, stop_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["daemon", "stop"]),
        &daemon_io,
        "stop_a",
        Duration::from_secs(10),
    );
    assert!(
        stop_ok.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop_err)
    );
}

#[test]
fn regression_obsidian_obs_02_source_relative_wikilink_index_matches_move() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs02Vault");
    fs::create_dir_all(root.join("Alpha")).unwrap();
    fs::create_dir_all(root.join("LongBetaFolder")).unwrap();
    fs::create_dir_all(root.join("Aliased")).unwrap();
    fs::create_dir_all(root.join("Sub")).unwrap();

    fs::write(root.join("Alpha/Spec.md"), "# Alpha Spec\n").unwrap();
    fs::write(root.join("LongBetaFolder/Spec.md"), "# Beta Spec\n").unwrap();
    fs::write(
        root.join("LongBetaFolder/Guide.md"),
        "# Guide\n\nSame folder: [[Spec]]\nDot relative: [[./Spec]]\nParent relative: [[../Alpha/Spec]]\n",
    )
    .unwrap();
    fs::write(
        root.join("Aliased/Target.md"),
        "---\naliases: [UniqueAlias]\n---\n# Target\n",
    )
    .unwrap();
    fs::write(
        root.join("RootAmbiguous.md"),
        "# Root\n\nAmbiguous bare: [[Spec]]\nFull path: [[LongBetaFolder/Spec]]\nAlias: [[UniqueAlias]]\n",
    )
    .unwrap();
    fs::write(root.join("Sub/UniqueRelTarget.md"), "# UniqueRelTarget\n").unwrap();
    fs::write(
        root.join("Sub/UniqueRelSource.md"),
        "# UniqueRelSource\n\nLink: [[./UniqueRelTarget]]\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs02Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Guide.md outgoing links resolve source-relatively: Beta, Beta, Alpha
    let guide_links = storage
        .get_outgoing_links(&vault, "LongBetaFolder/Guide.md")
        .unwrap();
    let resolved_for = |raw: &str| {
        guide_links
            .iter()
            .find(|l| l.target_raw == raw)
            .and_then(|l| l.target_resolved_path.as_deref())
    };
    assert_eq!(
        resolved_for("Spec"),
        Some("LongBetaFolder/Spec.md"),
        "[[Spec]] in LongBetaFolder/Guide.md must resolve to LongBetaFolder/Spec.md, got {guide_links:?}"
    );
    assert_eq!(
        resolved_for("./Spec"),
        Some("LongBetaFolder/Spec.md"),
        "[[./Spec]] in LongBetaFolder/Guide.md must resolve to LongBetaFolder/Spec.md, got {guide_links:?}"
    );
    assert_eq!(
        resolved_for("../Alpha/Spec"),
        Some("Alpha/Spec.md"),
        "[[../Alpha/Spec]] in LongBetaFolder/Guide.md must resolve to Alpha/Spec.md, got {guide_links:?}"
    );

    // 2. Control: RootAmbiguous.md leaves ambiguous [[Spec]] unresolved while resolving full path & unique alias
    let root_links = storage
        .get_outgoing_links(&vault, "RootAmbiguous.md")
        .unwrap();
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "Spec")
            .and_then(|l| l.target_resolved_path.as_deref()),
        None,
        "ambiguous bare [[Spec]] from root must remain unresolved, got {root_links:?}"
    );
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "LongBetaFolder/Spec")
            .and_then(|l| l.target_resolved_path.as_deref()),
        Some("LongBetaFolder/Spec.md")
    );
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "UniqueAlias")
            .and_then(|l| l.target_resolved_path.as_deref()),
        Some("Aliased/Target.md")
    );

    // 3. Backlinks & Graph agree with outgoing link resolution
    let beta_bl = storage
        .get_backlinks(&vault.id, "LongBetaFolder/Spec.md")
        .unwrap();
    assert!(beta_bl
        .iter()
        .any(|b| b.source_path == "LongBetaFolder/Guide.md"));
    let alpha_bl = storage.get_backlinks(&vault.id, "Alpha/Spec.md").unwrap();
    assert!(alpha_bl
        .iter()
        .any(|b| b.source_path == "LongBetaFolder/Guide.md"));
    assert!(
        !alpha_bl.iter().any(|b| b.source_path == "RootAmbiguous.md"),
        "ambiguous bare link in RootAmbiguous.md must not create backlink to Alpha/Spec.md"
    );

    let graph = storage
        .get_graph(&vault, Some("LongBetaFolder/Guide.md"), 1)
        .unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "LongBetaFolder/Guide.md" && e.target == "LongBetaFolder/Spec.md"));
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "LongBetaFolder/Guide.md" && e.target == "Alpha/Spec.md"));

    // 4. Unique relative link target is excluded from orphans
    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert!(
        !orphans
            .iter()
            .any(|o| o.rel_path == "Sub/UniqueRelTarget.md"),
        "Sub/UniqueRelTarget.md linked via [[./UniqueRelTarget]] must not be orphan: {orphans:?}"
    );

    // 5. Incremental sync (upsert_single_note / sync_paths) passes source_rel
    fs::write(root.join("Alpha/Ref.md"), "# Ref\n\n[[Spec]]\n").unwrap();
    storage
        .sync_paths(&vault, &["Alpha/Ref.md".to_string()], &[])
        .unwrap();
    let alpha_ref_links = storage.get_outgoing_links(&vault, "Alpha/Ref.md").unwrap();
    assert_eq!(
        alpha_ref_links[0].target_resolved_path.as_deref(),
        Some("Alpha/Spec.md"),
        "incremental sync_paths must resolve [[Spec]] in Alpha/Ref.md to Alpha/Spec.md"
    );

    // 6. Move LongBetaFolder/Spec.md -> LongBetaFolder/BetaSpec.md matches indexed targets and leaves ambiguous bare link verbatim
    let moved = storage
        .move_note_with_links(
            &vault,
            "LongBetaFolder/Spec.md",
            "LongBetaFolder/BetaSpec.md",
        )
        .unwrap();
    assert_eq!(moved.to, "LongBetaFolder/BetaSpec.md");

    let guide_after = fs::read_to_string(root.join("LongBetaFolder/Guide.md")).unwrap();
    assert!(
        guide_after.contains("Same folder: [[BetaSpec]]"),
        "{guide_after}"
    );
    assert!(
        guide_after.contains("Dot relative: [[BetaSpec]]"),
        "{guide_after}"
    );
    assert!(
        guide_after.contains("Parent relative: [[../Alpha/Spec]]"),
        "{guide_after}"
    );

    let root_after = fs::read_to_string(root.join("RootAmbiguous.md")).unwrap();
    assert!(
        root_after.contains("Ambiguous bare: [[Spec]]"),
        "ambiguous bare link must stay verbatim on move: {root_after}"
    );
    assert!(
        root_after.contains("Full path: [[BetaSpec]]"),
        "unambiguous full path link must update on move: {root_after}"
    );
}

#[test]
fn regression_obsidian_obs_05_formatting_table_inline_code_and_link_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs05Vault");
    fs::create_dir_all(&root).unwrap();

    fs::write(root.join("Dest.md"), "# Dest\n\n## Section\n\nBody.\n").unwrap();
    fs::write(
        root.join("BoldNote.md"),
        "# BoldNote\n\n**[[Dest]]** and **#boldtag**\n",
    )
    .unwrap();
    fs::write(
        root.join("ItalicNote.md"),
        "# ItalicNote\n\n*[[Dest]]* and *#italictag*\n",
    )
    .unwrap();
    fs::write(
        root.join("HighlightNote.md"),
        "# HighlightNote\n\n==[[Dest]]== and ==#hltag==\n",
    )
    .unwrap();
    fs::write(
        root.join("TableHeaderNote.md"),
        "# TableHeader\n\n| [[Dest]] #hdrtag | Value |\n| --- | --- |\n| [[Dest\\|TableAlias]] #celltag | ok |\n",
    )
    .unwrap();
    fs::write(
        root.join("InlineTripleTick.md"),
        "# InlineTripleTick\n\n```inline code```\nAfter tick [[Dest]] #aftertick\n",
    )
    .unwrap();
    fs::write(
        root.join("HeadingSelfLink.md"),
        "# HeadingSelfLink\n\nSee [[#Section]] and [[#Nested/Heading]].\n",
    )
    .unwrap();
    fs::write(
        root.join("ParenUrlNote.md"),
        "# ParenUrl\n\nRead [Wiki](https://en.wikipedia.org/wiki/Rust_(programming_language)).\n",
    )
    .unwrap();
    fs::write(
        root.join("UnclosedBracketNote.md"),
        "# Bracket\n\n[note] see [RealLink](https://example.com/path) and [unclosed see [Second](https://example.com/two)\n",
    )
    .unwrap();
    fs::write(
        root.join("ControlFenceCode.md"),
        "# Control\n\n```md\n[[Dest]] #fencetag\n```\n\nInline `[[Dest]] #codetag` and ``[[Dest]] #dblcodetag``.\n\nPlain [[Dest|PlainAlias]] #plaintag\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs05Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Emphasis / italic / highlight contain wikilinks and tags exactly once
    for (note, tag) in [
        ("BoldNote.md", "boldtag"),
        ("ItalicNote.md", "italictag"),
        ("HighlightNote.md", "hltag"),
    ] {
        let links = storage.get_outgoing_links(&vault, note).unwrap();
        assert_eq!(
            links.len(),
            1,
            "{note} should index [[Dest]] once, got {links:?}"
        );
        assert_eq!(links[0].target_resolved_path.as_deref(), Some("Dest.md"));
        let summary = storage.get_note_summary(&vault.id, note).unwrap().unwrap();
        assert_eq!(
            summary.tags,
            vec![tag.to_string()],
            "{note} should extract #{tag}, got {:?}",
            summary.tags
        );
    }

    // 2. Table header links & tags + escaped pipe alias in table cell
    let tbl_links = storage
        .get_outgoing_links(&vault, "TableHeaderNote.md")
        .unwrap();
    assert_eq!(
        tbl_links.len(),
        2,
        "TableHeaderNote.md should extract header + cell links once each, got {tbl_links:?}"
    );
    assert!(tbl_links
        .iter()
        .all(|l| l.target_resolved_path.as_deref() == Some("Dest.md")));
    assert!(
        tbl_links
            .iter()
            .any(|l| l.target_raw == "Dest" && l.alias.as_deref() == Some("TableAlias")),
        "escaped pipe [[Dest\\|TableAlias]] must parse target=Dest alias=TableAlias, got {tbl_links:?}"
    );
    let tbl_summary = storage
        .get_note_summary(&vault.id, "TableHeaderNote.md")
        .unwrap()
        .unwrap();
    assert!(
        tbl_summary.tags.contains(&"hdrtag".to_string()),
        "{:?}",
        tbl_summary.tags
    );
    assert!(
        tbl_summary.tags.contains(&"celltag".to_string()),
        "{:?}",
        tbl_summary.tags
    );

    // 3. Same-line closed triple-backtick inline code must NOT swallow subsequent lines
    let tick_links = storage
        .get_outgoing_links(&vault, "InlineTripleTick.md")
        .unwrap();
    assert_eq!(
        tick_links.len(),
        1,
        "line after ```inline code``` must be parsed, got {tick_links:?}"
    );
    assert_eq!(
        tick_links[0].target_resolved_path.as_deref(),
        Some("Dest.md")
    );
    let tick_summary = storage
        .get_note_summary(&vault.id, "InlineTripleTick.md")
        .unwrap()
        .unwrap();
    assert_eq!(tick_summary.tags, vec!["aftertick".to_string()]);

    // 4. [[#Section]] must NOT create a false `section` tag
    let head_summary = storage
        .get_note_summary(&vault.id, "HeadingSelfLink.md")
        .unwrap()
        .unwrap();
    assert!(
        head_summary.tags.is_empty(),
        "[[#Section]] must produce 0 tags, got {:?}",
        head_summary.tags
    );

    // 5. Balanced parens in Markdown link URL
    let paren_detail = storage.load_note_detail(&vault, "ParenUrlNote.md").unwrap();
    assert_eq!(paren_detail.parsed.md_links.len(), 1);
    assert_eq!(
        paren_detail.parsed.md_links[0].url,
        "https://en.wikipedia.org/wiki/Rust_(programming_language)"
    );

    // 6. Preceding [note] or unclosed `[` must not swallow independent [RealLink](...)
    let bracket_detail = storage
        .load_note_detail(&vault, "UnclosedBracketNote.md")
        .unwrap();
    assert_eq!(
        bracket_detail.parsed.md_links.len(),
        2,
        "got {:?}",
        bracket_detail.parsed.md_links
    );
    assert_eq!(bracket_detail.parsed.md_links[0].label, "RealLink");
    assert_eq!(
        bracket_detail.parsed.md_links[0].url,
        "https://example.com/path"
    );
    assert_eq!(bracket_detail.parsed.md_links[1].label, "Second");
    assert_eq!(
        bracket_detail.parsed.md_links[1].url,
        "https://example.com/two"
    );

    // 7. Control: real code fence & inline code stay excluded; move rewrites same-line tick note & table escaped alias cleanly
    let ctl_links = storage
        .get_outgoing_links(&vault, "ControlFenceCode.md")
        .unwrap();
    assert_eq!(
        ctl_links.len(),
        1,
        "fence/inline code must be excluded: {ctl_links:?}"
    );
    assert_eq!(ctl_links[0].alias.as_deref(), Some("PlainAlias"));
    let ctl_summary = storage
        .get_note_summary(&vault.id, "ControlFenceCode.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctl_summary.tags, vec!["plaintag".to_string()]);

    // Move Dest.md -> DestMoved.md: InlineTripleTick.md & TableHeaderNote.md & emphasis notes update and reindex identically
    let moved = storage
        .move_note_with_links(&vault, "Dest.md", "DestMoved.md")
        .unwrap();
    assert_eq!(moved.to, "DestMoved.md");
    let tick_after = fs::read_to_string(root.join("InlineTripleTick.md")).unwrap();
    assert!(
        tick_after.contains("After tick [[DestMoved]]"),
        "move must also treat ```inline code``` as inline code rather than multiline fence: {tick_after}"
    );
    let ctl_after = fs::read_to_string(root.join("ControlFenceCode.md")).unwrap();
    assert!(ctl_after.contains("```md\n[[Dest]] #fencetag\n```"));
    assert!(ctl_after.contains("Inline `[[Dest]] #codetag` and ``[[Dest]] #dblcodetag``."));
    assert!(ctl_after.contains("Plain [[DestMoved|PlainAlias]]"));
    let bl_after = storage.get_backlinks(&vault.id, "DestMoved.md").unwrap();
    for src in [
        "BoldNote.md",
        "ItalicNote.md",
        "HighlightNote.md",
        "TableHeaderNote.md",
        "InlineTripleTick.md",
        "ControlFenceCode.md",
    ] {
        assert!(
            bl_after.iter().any(|b| b.source_path == src),
            "after move, {src} must remain backlink of DestMoved.md: {bl_after:?}"
        );
    }
}
