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
fn regression_obsidian_obs_06_local_markdown_note_links_in_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs06Vault");
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::create_dir_all(root.join("Sub")).unwrap();

    fs::write(root.join("Attachments/pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(
        root.join("Projects/Plan.md"),
        "# Plan\n\n## Goals\n\nDeliver local markdown links.\n",
    )
    .unwrap();
    fs::write(root.join("Projects/Space Note.md"), "# Space Note\n").unwrap();
    fs::write(
        root.join("R.md"),
        "# Referrer\n\nSee [The Plan](Projects/Plan.md#Goals \"Plan Title\"), [Spaced](Projects/Space%20Note.md), [Ext](https://example.com/Projects/Plan.md), and ![img](Attachments/pic.png).\n",
    )
    .unwrap();
    fs::write(
        root.join("OrphanOnlyExt.md"),
        "# External Only\n\n[Site](https://example.com) and ![img](Attachments/pic.png)\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs06Vault"));
    let db_path = tmp.path().join("obs06.db");
    let storage = Storage::open(&db_path).unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Outgoing links from R.md reflect only the 2 local markdown note targets (no HTTPS/image edges)
    let r_links = storage.get_outgoing_links(&vault, "R.md").unwrap();
    assert_eq!(
        r_links.len(),
        2,
        "R.md should have exactly 2 local note links (Plan + Space Note), got {r_links:?}"
    );
    assert!(
        r_links.iter().any(
            |l| l.target_resolved_path.as_deref() == Some("Projects/Plan.md")
                && l.heading.as_deref() == Some("Goals")
        ),
        "expected Projects/Plan.md#Goals link, got {r_links:?}"
    );
    assert!(
        r_links
            .iter()
            .any(|l| l.target_resolved_path.as_deref() == Some("Projects/Space Note.md")),
        "expected Projects/Space Note.md link, got {r_links:?}"
    );

    // 2. Backlinks, graph, and orphans agree; external HTTPS + image-only note stays orphan
    let plan_bl = storage
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap();
    assert!(
        plan_bl.iter().any(|b| b.source_path == "R.md"),
        "Projects/Plan.md must have R.md as backlink: {plan_bl:?}"
    );
    let graph = storage.get_graph(&vault, Some("R.md"), 1).unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "R.md" && e.target == "Projects/Plan.md"));
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "R.md" && e.target == "Projects/Space Note.md"));
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.target.contains("https://") || e.target.contains("pic.png")),
        "external URLs and images must not be note graph edges: {:?}",
        graph.edges
    );

    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert!(!orphans.iter().any(|o| o.rel_path == "R.md"), "{orphans:?}");
    assert!(
        !orphans.iter().any(|o| o.rel_path == "Projects/Plan.md"),
        "{orphans:?}"
    );
    assert!(
        orphans.iter().any(|o| o.rel_path == "OrphanOnlyExt.md"),
        "note with only HTTPS link and image must remain orphan: {orphans:?}"
    );

    // 3. Upgrade from old user_version=1 DB (without markdown link rows) automatically reindexes once
    drop(storage);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DELETE FROM links WHERE vault_id = ?1", [&vault.id])
            .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }
    let upgraded = Storage::open(&db_path).unwrap();
    let up_stats = upgraded.sync_vault(&vault).unwrap();
    assert!(
        up_stats.updated >= 1,
        "v1 DB must trigger one-time reindex on upgrade"
    );
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .iter()
        .any(|b| b.source_path == "R.md"));

    // 4. Incremental sync_paths computes local markdown links identically
    fs::write(
        root.join("Sub/IncRef.md"),
        "# IncRef\n\nRelative [Plan](../Projects/Plan.md#Goals).\n",
    )
    .unwrap();
    upgraded
        .sync_paths(&vault, &["Sub/IncRef.md".to_string()], &[])
        .unwrap();
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .iter()
        .any(|b| b.source_path == "Sub/IncRef.md"));

    // 5. Move target -> preserves label/title/anchor and only new path remains indexed
    let moved = upgraded
        .move_note_with_links(&vault, "Projects/Plan.md", "Archive/New Plan.md")
        .unwrap();
    assert_eq!(moved.to, "Archive/New Plan.md");
    let r_after = fs::read_to_string(root.join("R.md")).unwrap();
    assert!(
        r_after.contains("[The Plan](Archive/New%20Plan.md#Goals \"Plan Title\")"),
        "move must preserve label, anchor, and title: {r_after}"
    );
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .is_empty());
    let new_bl = upgraded
        .get_backlinks(&vault.id, "Archive/New Plan.md")
        .unwrap();
    assert!(new_bl.iter().any(|b| b.source_path == "R.md"));
    assert!(new_bl.iter().any(|b| b.source_path == "Sub/IncRef.md"));
}

#[test]
fn regression_obsidian_obs_08_windows_case_only_rename() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs08Vault");
    fs::create_dir_all(root.join("notes")).unwrap();

    let plan_bytes = b"# plan\n\nOriginal exact bytes.\r\n";
    let existing_other_bytes = b"# Existing\n\nDo not clobber.\n";
    fs::write(root.join("notes/plan.md"), plan_bytes).unwrap();
    fs::write(root.join("notes/Existing.md"), existing_other_bytes).unwrap();
    fs::write(
        root.join("Index.md"),
        "# Index\n\nSee [[notes/plan]] and [md](notes/plan.md).\n",
    )
    .unwrap();
    let orig_hash = content_sha256_hex(plan_bytes);
    let other_hash = content_sha256_hex(existing_other_bytes);

    // 1. Case-only rename notes/plan.md -> notes/Plan.md via CLI
    let mv = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", "notes/plan.md", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        mv.status.success(),
        "case-only move notes/plan.md -> notes/Plan.md failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    let mv_json: Value = serde_json::from_slice(&mv.stdout).unwrap();
    assert_eq!(mv_json["from"], "notes/plan.md");
    assert_eq!(mv_json["to"], "notes/Plan.md");

    // Enumerate actual directory entries on disk to verify exact on-disk casing
    let mut disk_names: Vec<String> = fs::read_dir(root.join("notes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    disk_names.sort();
    assert_eq!(
        disk_names,
        vec!["Existing.md".to_string(), "Plan.md".to_string()],
        "on-disk directory entries must show exact Plan.md casing, got {disk_names:?}"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );

    // CLI notes list & SQLite show exactly one note with new casing, and incoming links point to notes/Plan.md
    let list_out = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let list_json: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    let plan_entries: Vec<&str> = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["rel_path"].as_str())
        .filter(|p| p.eq_ignore_ascii_case("notes/plan.md"))
        .collect();
    assert_eq!(
        plan_entries,
        vec!["notes/Plan.md"],
        "must not duplicate note under old and new casing"
    );

    let index_after = fs::read_to_string(root.join("Index.md")).unwrap();
    assert!(
        index_after.contains("notes/Plan") || index_after.contains("[[Plan]]"),
        "Index.md should reference updated casing or valid link: {index_after}"
    );
    let bl_out = isolated_cli(tmp.path(), &root)
        .args(["--json", "backlinks", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        bl_out.status.success(),
        "{}",
        String::from_utf8_lossy(&bl_out.stderr)
    );
    let bl_json: Value = serde_json::from_slice(&bl_out.stdout).unwrap();
    assert!(
        bl_json
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["source_path"] == "Index.md"),
        "Index.md must remain a backlink to notes/Plan.md: {bl_json}"
    );

    // 2. Control: exact same path move retains existing conflict/error behavior
    let same_path = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", "notes/Plan.md", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        !same_path.status.success(),
        "identical path move must retain existing already-exists error"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );

    // 3. Control: moving onto a DIFFERENT existing file fails and preserves both files
    let collide = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "move",
            "notes/Plan.md",
            "notes/Existing.md",
        ])
        .output()
        .unwrap();
    assert!(!collide.status.success());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Existing.md")).unwrap()),
        other_hash
    );

    // 4. Control: normal move to new folder updates links cleanly
    let normal_mv = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "move",
            "notes/Plan.md",
            "Archive/FinalPlan.md",
        ])
        .output()
        .unwrap();
    assert!(
        normal_mv.status.success(),
        "{}",
        String::from_utf8_lossy(&normal_mv.stderr)
    );
    assert!(root.join("Archive/FinalPlan.md").is_file());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Archive/FinalPlan.md")).unwrap()),
        orig_hash
    );
}

#[test]
fn regression_obsidian_obs_10_surgical_task_toggle_eol_fence_and_bom() {
    use obsidian_cli::vault::fs::toggle_task_at_line_verified;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs10Vault");
    fs::create_dir_all(&root).unwrap();

    // 1. Mixed CRLF/LF note: toggling task changes ONLY the single checkbox byte (' ' <-> 'x')
    let mixed_path = root.join("Mixed.md");
    let mixed_before = b"# Mixed\r\n\n- [ ] Toggle target\n- [x] Keep LF\r\nTail";
    fs::write(&mixed_path, mixed_before).unwrap();
    let checked = toggle_task_at_line(&mixed_path, 3).unwrap();
    assert!(checked);
    let mixed_after = fs::read(&mixed_path).unwrap();
    assert_eq!(mixed_after.len(), mixed_before.len());
    let diffs: Vec<(usize, u8, u8)> = mixed_before
        .iter()
        .zip(mixed_after.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(idx, (&a, &b))| (idx, a, b))
        .collect();
    assert_eq!(
        diffs.len(),
        1,
        "mixed CRLF/LF toggle must change exactly 1 byte, diffs={diffs:?}"
    );
    assert_eq!((diffs[0].1, diffs[0].2), (b' ', b'x'));
    // Toggle back x -> ' '
    assert!(!toggle_task_at_line(&mixed_path, 3).unwrap());
    assert_eq!(fs::read(&mixed_path).unwrap(), mixed_before);

    // 2. Task inside code fence must error with 0 disk writes
    let fence_path = root.join("CodeTask.md");
    let fence_before = b"# CodeTask\n\n```md\n- [ ] Inside fence\n```\n\n- [ ] Real task\n";
    fs::write(&fence_path, fence_before).unwrap();
    assert!(
        toggle_task_at_line(&fence_path, 4).is_err(),
        "toggling - [ ] inside a fenced code block must fail"
    );
    assert_eq!(
        fs::read(&fence_path).unwrap(),
        fence_before,
        "failed toggle inside fence must leave bytes untouched"
    );
    assert!(toggle_task_at_line(&fence_path, 7).unwrap());
    assert_eq!(
        fs::read(&fence_path).unwrap(),
        b"# CodeTask\n\n```md\n- [ ] Inside fence\n```\n\n- [x] Real task\n"
    );

    // 3. UTF-8 BOM on line 1 (heading AND first-line task without frontmatter)
    let bom_heading_path = root.join("BomHeading.md");
    let bom_heading_bytes = "\u{feff}# BOM Heading\n- [ ] Second line task\n".as_bytes();
    fs::write(&bom_heading_path, bom_heading_bytes).unwrap();

    let bom_task_path = root.join("BomFirstLineTask.md");
    let bom_task_bytes = "\u{feff}- [ ] First line BOM task\n- [ ] Second task".as_bytes();
    fs::write(&bom_task_path, bom_task_bytes).unwrap();

    let vault = vault_from_dir(&root, Some("Obs10Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let bom_h_detail = storage.load_note_detail(&vault, "BomHeading.md").unwrap();
    assert_eq!(
        bom_h_detail.parsed.headings.len(),
        1,
        "BOM line-1 heading must be indexed: {:?}",
        bom_h_detail.parsed.headings
    );
    assert_eq!(bom_h_detail.parsed.headings[0].text, "BOM Heading");
    assert_eq!(bom_h_detail.parsed.headings[0].line_no, 1);

    let bom_t_tasks = storage
        .list_tasks(
            &vault.id,
            obsidian_cli::models::TaskFilter::All,
            Some("BomFirstLineTask.md"),
        )
        .unwrap();
    assert_eq!(
        bom_t_tasks.len(),
        2,
        "BOM line-1 task must be indexed: {bom_t_tasks:?}"
    );
    assert_eq!(bom_t_tasks[0].line_no, 1);
    assert_eq!(bom_t_tasks[0].text, "First line BOM task");

    // Toggle BOM line-1 task -> preserves leading UTF-8 BOM and no-trailing-newline byte-for-byte
    assert!(toggle_task_at_line(&bom_task_path, 1).unwrap());
    let bom_task_after = fs::read(&bom_task_path).unwrap();
    assert_eq!(
        bom_task_after,
        "\u{feff}- [x] First line BOM task\n- [ ] Second task".as_bytes(),
        "BOM and non-checkbox bytes must be preserved exactly"
    );

    // 4. Controls: pure CRLF, BOM+frontmatter, no trailing newline, expected-text mismatch
    let crlf_path = root.join("PureCrlf.md");
    let crlf_before = b"# CRLF\r\n- [ ] One\r\n- [ ] Two";
    fs::write(&crlf_path, crlf_before).unwrap();
    assert!(toggle_task_at_line(&crlf_path, 3).unwrap());
    assert_eq!(
        fs::read(&crlf_path).unwrap(),
        b"# CRLF\r\n- [ ] One\r\n- [x] Two"
    );

    let bom_fm_path = root.join("BomFM.md");
    let bom_fm_before =
        "\u{feff}---\r\ntags: [t]\r\n---\r\n\r\n# Title\r\n- [ ] After FM".as_bytes();
    fs::write(&bom_fm_path, bom_fm_before).unwrap();
    assert!(toggle_task_at_line(&bom_fm_path, 6).unwrap());
    assert_eq!(
        fs::read(&bom_fm_path).unwrap(),
        "\u{feff}---\r\ntags: [t]\r\n---\r\n\r\n# Title\r\n- [x] After FM".as_bytes()
    );

    // Expected-text mismatch must fail with 0 byte changes
    let before_mismatch = fs::read(&bom_fm_path).unwrap();
    assert!(toggle_task_at_line_verified(&bom_fm_path, 6, Some("Wrong text"), None).is_err());
    assert_eq!(fs::read(&bom_fm_path).unwrap(), before_mismatch);
}

#[tokio::test]
async fn regression_obsidian_obs_12_http_get_append_note_and_literal_percent_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs12Vault");
    fs::create_dir_all(root.join("Sub Folder")).unwrap();

    fs::write(root.join("append.md"), "# append\n\nAppend note body.\n").unwrap();
    fs::write(
        root.join("Ratio 50%20Test.md"),
        "# Ratio 50%20Test\n\nLiteral percent body.\n",
    )
    .unwrap();
    fs::write(
        root.join("Sub Folder/C++ 한글 노트.md"),
        "# C++ 한글 노트\n\nNormal encoded path body.\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs12Vault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // 1. GET /api/v1/notes/append and /api/v1/notes/append.md return the same note
    let r_bare = client
        .get(format!("{base}/api/v1/notes/append"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r_bare.status(),
        StatusCode::OK,
        "GET /api/v1/notes/append must return 200 OK"
    );
    let bare_json: Value = r_bare.json().await.unwrap();
    let r_ext = client
        .get(format!("{base}/api/v1/notes/append.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r_ext.status(), StatusCode::OK);
    let ext_json: Value = r_ext.json().await.unwrap();
    assert_eq!(bare_json["note"]["summary"]["rel_path"], "append.md");
    assert_eq!(ext_json["note"]["summary"]["rel_path"], "append.md");
    assert_eq!(
        bare_json["note"]["raw_markdown"],
        ext_json["note"]["raw_markdown"]
    );

    // 2. Literal %20 in filename (`Ratio 50%20Test.md`) requested with % -> %25 returns exact file
    let r_pct = client
        .get(format!("{base}/api/v1/notes/Ratio%2050%2520Test.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r_pct.status(),
        StatusCode::OK,
        "GET literal percent filename must not double-decode"
    );
    let pct_json: Value = r_pct.json().await.unwrap();
    assert_eq!(
        pct_json["note"]["summary"]["rel_path"],
        "Ratio 50%20Test.md"
    );
    assert!(pct_json["note"]["raw_markdown"]
        .as_str()
        .unwrap()
        .contains("Literal percent body."));

    // 3. Control: POST /api/v1/notes/append still appends exactly once
    let post_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "append", "text": "APPENDED_ONCE_MARKER" }))
        .send()
        .await
        .unwrap();
    assert_eq!(post_ap.status(), StatusCode::OK);
    let on_disk = fs::read_to_string(root.join("append.md")).unwrap();
    assert_eq!(on_disk.matches("APPENDED_ONCE_MARKER").count(), 1);

    // 4. Control: spaces, '+', Korean, subfolder normal GET still works
    let r_sub = client
        .get(format!(
            "{base}/api/v1/notes/Sub%20Folder/C++%20%ED%95%9C%EA%B8%80%20%EB%85%B8%ED%8A%B8.md"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_sub.status(), StatusCode::OK);
    let sub_json: Value = r_sub.json().await.unwrap();
    assert_eq!(
        sub_json["note"]["summary"]["rel_path"],
        "Sub Folder/C++ 한글 노트.md"
    );

    server.abort();
}

#[test]
fn regression_obsidian_obs_14_hyphen_titles_and_toggle_task_expected_text() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs14Vault");
    fs::create_dir_all(&root).unwrap();

    // 1. Create `- 목록 항목`, then read, append, replace, move, and delete with the same spelling
    let title = "- 목록 항목";
    let cr = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "create", title])
        .output()
        .unwrap();
    assert!(
        cr.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&cr.stderr)
    );
    assert!(root.join("- 목록 항목.md").is_file());

    let rd = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", title])
        .output()
        .unwrap();
    assert!(
        rd.status.success(),
        "read failed: {}",
        String::from_utf8_lossy(&rd.stderr)
    );
    let rd_json: Value = serde_json::from_slice(&rd.stdout).unwrap();
    assert_eq!(rd_json["rel_path"], "- 목록 항목.md");

    let rd_compact = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", title, "--compact", "--json"])
        .output()
        .unwrap();
    assert!(
        rd_compact.status.success(),
        "read --compact --json failed: {}",
        String::from_utf8_lossy(&rd_compact.stderr)
    );
    let rd_compact_json: Value = serde_json::from_slice(&rd_compact.stdout).unwrap();
    assert_eq!(rd_compact_json["rel_path"], "- 목록 항목.md");

    let ap = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "append", title, "- appended bullet"])
        .output()
        .unwrap();
    assert!(
        ap.status.success(),
        "append failed: {}",
        String::from_utf8_lossy(&ap.stderr)
    );
    assert!(fs::read_to_string(root.join("- 목록 항목.md"))
        .unwrap()
        .contains("- appended bullet"));

    let rep = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "replace",
            title,
            "--find",
            "- appended bullet",
            "--replace",
            "- updated bullet",
        ])
        .output()
        .unwrap();
    assert!(
        rep.status.success(),
        "replace failed: {}",
        String::from_utf8_lossy(&rep.stderr)
    );
    assert!(fs::read_to_string(root.join("- 목록 항목.md"))
        .unwrap()
        .contains("- updated bullet"));

    let moved_title = "- 이동된 항목";
    let mv = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", title, moved_title])
        .output()
        .unwrap();
    assert!(
        mv.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    assert!(root.join("- 이동된 항목.md").is_file());

    // Control: `--` separator still works for hyphen note identifier
    let rd_sep = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", "--", moved_title])
        .output()
        .unwrap();
    assert!(
        rd_sep.status.success(),
        "read after -- failed: {}",
        String::from_utf8_lossy(&rd_sep.stderr)
    );

    let del = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "delete", "--yes", moved_title])
        .output()
        .unwrap();
    assert!(
        del.status.success(),
        "delete failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
    assert!(!root.join("- 이동된 항목.md").exists());
    assert!(root.join(".trash/- 이동된 항목.md").is_file());

    // 2. toggle-task --expected-text with separate argv `- item` and control `--expected-text=- item`
    fs::write(
        root.join("Tasks.md"),
        "# Tasks\n\n- [ ] - item\n- [ ] other task\n",
    )
    .unwrap();
    let tog_ok = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "toggle-task",
            "Tasks.md",
            "3",
            "--expected-text",
            "- item",
        ])
        .output()
        .unwrap();
    assert!(
        tog_ok.status.success(),
        "toggle-task --expected-text '- item' failed: {}",
        String::from_utf8_lossy(&tog_ok.stderr)
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [x] - item"));

    // Mismatched expected-text must run comparison and fail with Task conflict (not clap exit 2)
    let tog_mismatch = isolated_cli(tmp.path(), &root)
        .args(["toggle-task", "Tasks.md", "4", "--expected-text", "- item"])
        .output()
        .unwrap();
    assert!(!tog_mismatch.status.success());
    let mismatch_err = String::from_utf8_lossy(&tog_mismatch.stderr);
    assert!(
        mismatch_err.contains("Task conflict"),
        "expected Task conflict comparison error, got: {mismatch_err}"
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [ ] other task"));

    // Control: `--expected-text=- item` toggles back
    let tog_eq = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "toggle-task",
            "Tasks.md",
            "3",
            "--expected-text=- item",
        ])
        .output()
        .unwrap();
    assert!(
        tog_eq.status.success(),
        "toggle-task --expected-text=- item failed: {}",
        String::from_utf8_lossy(&tog_eq.stderr)
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [ ] - item"));

    // W6: empty --expected-text must run comparison and fail instead of silently skipping
    let before_empty_expected = fs::read(root.join("Tasks.md")).unwrap();
    let tog_empty = isolated_cli(tmp.path(), &root)
        .args(["toggle-task", "Tasks.md", "3", "--expected-text", ""])
        .output()
        .unwrap();
    assert!(!tog_empty.status.success());
    assert!(
        String::from_utf8_lossy(&tog_empty.stderr).contains("Task conflict"),
        "expected Task conflict for empty expected-text, got: {}",
        String::from_utf8_lossy(&tog_empty.stderr)
    );
    assert_eq!(
        fs::read(root.join("Tasks.md")).unwrap(),
        before_empty_expected
    );

    // 3. Controls: --help, --json, missing values, and option typo diagnostics remain intact
    let help_read = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", "--help"])
        .output()
        .unwrap();
    assert!(help_read.status.success());
    assert!(String::from_utf8_lossy(&help_read.stdout).contains("--compact"));

    let bad_flag_read = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", "--compct"])
        .output()
        .unwrap();
    assert!(!bad_flag_read.status.success());
    assert_eq!(
        bad_flag_read.status.code(),
        Some(2),
        "option typo `--compct` must still be rejected by clap as an unknown option, stderr={}",
        String::from_utf8_lossy(&bad_flag_read.stderr)
    );

    let missing_expected_val = isolated_cli(tmp.path(), &root)
        .args(["toggle-task", "Tasks.md", "3", "--expected-text", "--json"])
        .output()
        .unwrap();
    assert!(!missing_expected_val.status.success());
    assert_eq!(missing_expected_val.status.code(), Some(2));

    let missing_delete_note = isolated_cli(tmp.path(), &root)
        .args(["notes", "delete", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(!missing_delete_note.status.success());
    assert_eq!(missing_delete_note.status.code(), Some(2));
}
