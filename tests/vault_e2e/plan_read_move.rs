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
fn plan_cli_json_flag_order_and_full_compact_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("N.md"), "# N\n").unwrap();

    let run = |args: &[&str]| {
        isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap()
    };

    let c1 = run(&["--json", "notes", "read", "N", "--compact"]);
    let c2 = run(&["notes", "read", "N", "--compact", "--json"]);
    let c3 = run(&["--json", "notes", "read", "N"]);
    for (idx, out) in [(1, &c1), (2, &c2), (3, &c3)] {
        assert!(
            out.status.success(),
            "case {idx} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(c1.stdout, c2.stdout);
    assert_eq!(c1.stdout, c3.stdout);
    let compact_val: Value = serde_json::from_slice(&c1.stdout).unwrap();
    let expected_keys: std::collections::BTreeSet<String> = [
        "hint",
        "mtime_ms",
        "omitted",
        "raw_markdown",
        "rel_path",
        "title",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(plan_json_keys(&compact_val), expected_keys);

    let c4 = run(&["notes", "read", "N", "--json", "--full"]);
    assert!(
        c4.status.success(),
        "{}",
        String::from_utf8_lossy(&c4.stderr)
    );
    let full_val: Value = serde_json::from_slice(&c4.stdout).unwrap();
    assert!(full_val.get("summary").is_some());
    assert!(full_val.get("parsed").is_some());

    let c5 = run(&["notes", "read", "N", "--json", "--compact", "--full"]);
    assert_eq!(c5.status.code(), Some(2));
    assert!(c5.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c5.stderr).contains("cannot be used with"));

    let c6 = run(&["notes", "read", "N", "--full"]);
    assert_eq!(c6.status.code(), Some(2));
    assert!(c6.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c6.stderr).contains("--compact and --full require --json"));

    let c7 = run(&["notes", "read", "N"]);
    assert!(c7.status.success());
    assert_eq!(c7.stdout, b"# N\n");

    let snap_before_daily = plan_snapshot_dir(&root);
    let c8 = run(&["daily", "--full"]);
    assert_eq!(c8.status.code(), Some(2));
    assert!(c8.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c8.stderr).contains("--compact and --full require --json"));
    assert_eq!(plan_snapshot_dir(&root), snap_before_daily);
}

#[test]
fn plan_compact_note_object_reports_exact_omissions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("T.md"),
        "---\naliases: [Tee]\ntags: [x]\n---\n\n# Heading One\n\n[[A]]\n[[A]]\n[[Nope]]\n- [ ] one\n- [x] two\n",
    )
    .unwrap();
    fs::write(root.join("A.md"), "[[T]] and [[T]]\n").unwrap();
    fs::write(root.join("B.md"), "[[T]]\n").unwrap();
    fs::write(root.join("Lonely.md"), "# Lonely\n\nNothing here.\n").unwrap();

    let run = |args: &[&str]| {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };

    let cli_compact = run(&["notes", "read", "T", "--json"]);
    let cli_full = run(&["notes", "read", "T", "--json", "--full"]);
    assert_eq!(
        cli_compact["omitted"],
        json!({
            "backlinks": 2,
            "outgoing_links": 3,
            "images": Value::Null,
            "fields": ["summary", "parsed"],
        })
    );
    assert_eq!(
        cli_compact["omitted"]["backlinks"].as_u64().unwrap() as usize,
        cli_full["backlinks"].as_array().unwrap().len()
    );
    assert_eq!(
        cli_compact["omitted"]["outgoing_links"].as_u64().unwrap() as usize,
        cli_full["outgoing_links"].as_array().unwrap().len()
    );
    let cli_hint = cli_compact["hint"].as_str().unwrap();
    assert!(cli_hint.ends_with("--json --full"), "got hint: {cli_hint}");
    assert!(cli_hint.contains("\"T.md\""), "got hint: {cli_hint}");

    let mcp = plan_mcp_server(&root);
    let mcp_compact = mcp
        .dispatch_tool("read_note", &json!({ "note": "T" }))
        .unwrap();
    let mcp_full = mcp
        .dispatch_tool("read_note", &json!({ "note": "T", "compact": false }))
        .unwrap();
    assert_eq!(
        mcp_compact["omitted"],
        json!({
            "backlinks": 2,
            "outgoing_links": 3,
            "tags": 1,
            "aliases": 1,
            "headings": 1,
            "tasks": 2,
            "images": Value::Null,
            "frontmatter_keys": ["aliases", "tags"],
            "fields": ["folder", "word_count", "size_bytes", "is_bookmarked", "md_links"],
        })
    );
    for key in [
        "backlinks",
        "outgoing_links",
        "tags",
        "aliases",
        "headings",
        "tasks",
    ] {
        assert_eq!(
            mcp_compact["omitted"][key].as_u64().unwrap() as usize,
            mcp_full[key].as_array().unwrap().len(),
            "mismatch for omitted.{key}"
        );
    }
    assert_eq!(
        mcp_compact["hint"],
        r#"read_note {"compact":false,"note":"T.md"}"#
    );

    let lonely = run(&["notes", "read", "Lonely", "--json"]);
    assert_eq!(lonely["omitted"]["backlinks"], json!(0));
    assert_eq!(lonely["omitted"]["outgoing_links"], json!(0));
    assert!(lonely["omitted"]["images"].is_null());

    let daily_compact = run(&["daily", "--date", "2026-11-21", "--json"]);
    let expected_note_keys: std::collections::BTreeSet<String> = [
        "hint",
        "mtime_ms",
        "omitted",
        "raw_markdown",
        "rel_path",
        "title",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(plan_json_keys(&daily_compact["note"]), expected_note_keys);

    let daily_full = run(&["daily", "--date", "2026-11-21", "--json", "--full"]);
    assert!(daily_full["note"].get("summary").is_some());
}

#[test]
fn plan_daily_note_receipt_appends_once_and_points_to_read_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();

    let mcp = plan_mcp_server(&root);
    let first = mcp
        .dispatch_tool(
            "daily_note",
            &json!({ "date": "2026-11-21", "append": "- [ ] once" }),
        )
        .unwrap();
    let expected_keys: std::collections::BTreeSet<String> =
        ["appended", "created", "hint", "rel_path"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(plan_json_keys(&first), expected_keys);
    assert_eq!(first["created"], true);
    assert_eq!(first["appended"], true);

    let rel_path = first["rel_path"].as_str().unwrap().to_string();
    let hint = first["hint"].as_str().unwrap();
    assert_eq!(
        hint,
        format!(
            "read_note {}",
            json!({ "note": rel_path, "compact": false })
        )
    );
    assert!(!hint.contains("daily_note"));

    let full = mcp
        .dispatch_tool("read_note", &json!({ "note": rel_path, "compact": false }))
        .unwrap();
    let compact = mcp
        .dispatch_tool("read_note", &json!({ "note": rel_path }))
        .unwrap();

    let cli_daily_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["daily", "--date", "2026-11-21", "--json"])
        .output()
        .unwrap();
    assert!(
        cli_daily_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_daily_out.stderr)
    );
    let cli_daily: Value = serde_json::from_slice(&cli_daily_out.stdout).unwrap();

    let disk_after_reads = fs::read_to_string(root.join(&rel_path)).unwrap();
    assert_eq!(disk_after_reads.matches("- [ ] once").count(), 1);
    assert_eq!(full["raw_markdown"].as_str().unwrap(), disk_after_reads);
    assert_eq!(compact["raw_markdown"].as_str().unwrap(), disk_after_reads);
    assert_eq!(
        cli_daily["note"]["raw_markdown"].as_str().unwrap(),
        disk_after_reads
    );

    let second = mcp
        .dispatch_tool("daily_note", &json!({ "date": "2026-11-21" }))
        .unwrap();
    assert_eq!(second["created"], false);
    assert_eq!(
        second["omitted"]["fields"],
        json!(["title", "mtime_ms", "folder", "frontmatter", "word_count"])
    );
    for key in [
        "backlinks",
        "outgoing_links",
        "tags",
        "aliases",
        "headings",
        "tasks",
    ] {
        assert!(
            second["omitted"][key].is_u64(),
            "expected numeric omitted.{key}, got {:?}",
            second["omitted"][key]
        );
        assert_eq!(
            second["omitted"][key].as_u64().unwrap() as usize,
            full[key].as_array().unwrap().len(),
            "omitted.{key} must equal read_note compact:false length"
        );
    }
    assert!(second["omitted"]["tasks"].as_u64().unwrap() >= 1);
    assert_eq!(
        fs::read_to_string(root.join(&rel_path)).unwrap(),
        disk_after_reads
    );

    // CLI daily --append --json returns minimal receipt; --full preserves full note detail
    let cli_append_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args([
            "daily",
            "--date",
            "2026-11-22",
            "--append",
            "- [ ] receipt",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        cli_append_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_append_out.stderr)
    );
    let cli_append: Value = serde_json::from_slice(&cli_append_out.stdout).unwrap();
    assert_eq!(
        plan_json_keys(&cli_append),
        ["appended", "created", "rel_path"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert_eq!(cli_append["appended"], true);
    assert_eq!(cli_append["created"], true);

    let cli_append_full_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args([
            "daily",
            "--date",
            "2026-11-22",
            "--append",
            "- [ ] full",
            "--json",
            "--full",
        ])
        .output()
        .unwrap();
    assert!(
        cli_append_full_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_append_full_out.stderr)
    );
    let cli_append_full: Value = serde_json::from_slice(&cli_append_full_out.stdout).unwrap();
    assert!(cli_append_full["note"].get("summary").is_some());
    assert!(cli_append_full["note"]["parsed"].get("blocks").is_some());

    // Control (F-05a): CLI notes read --json --full keeps parsed.blocks, body_markdown, body_plain
    let read_full_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", &rel_path, "--json", "--full"])
        .output()
        .unwrap();
    assert!(
        read_full_out.status.success(),
        "{}",
        String::from_utf8_lossy(&read_full_out.stderr)
    );
    let read_full: Value = serde_json::from_slice(&read_full_out.stdout).unwrap();
    assert!(read_full["parsed"].get("blocks").is_some());
    assert!(read_full["parsed"].get("body_markdown").is_some());
    assert!(read_full["parsed"].get("body_plain").is_some());
}

#[test]
fn plan_read_note_mtime_round_trips_into_toggle_task() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("K.md"), "- [ ] one\n").unwrap();

    let mcp = plan_mcp_server(&root);
    let read = mcp
        .dispatch_tool("read_note", &json!({ "note": "K.md" }))
        .unwrap();
    let mtime_ms = read["mtime_ms"].as_i64().unwrap();

    let toggled = mcp
        .dispatch_tool(
            "toggle_task",
            &json!({ "note": "K.md", "line": 1, "expected_mtime_ms": mtime_ms }),
        )
        .unwrap();
    assert_eq!(toggled["checked"], true);
    assert_eq!(
        fs::read_to_string(root.join("K.md")).unwrap(),
        "- [x] one\n"
    );

    let stale = mcp.dispatch_tool(
        "toggle_task",
        &json!({ "note": "K.md", "line": 1, "expected_mtime_ms": 1 }),
    );
    assert!(stale.is_err());
    assert_eq!(
        fs::read_to_string(root.join("K.md")).unwrap(),
        "- [x] one\n"
    );

    let full = mcp
        .dispatch_tool("read_note", &json!({ "note": "K.md", "compact": false }))
        .unwrap();
    let listed = mcp.dispatch_tool("list_notes", &json!({})).unwrap();
    let list_mtime = listed["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "K.md")
        .unwrap()["mtime_ms"]
        .as_i64()
        .unwrap();
    assert_eq!(full["mtime_ms"].as_i64().unwrap(), list_mtime);
}

#[test]
fn plan_ambiguous_note_reference_never_reads_or_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["a", "b", "x", "y", "Tasks", "Other"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();
    fs::write(root.join("Solo.md"), "SOLO").unwrap();
    fs::write(root.join("x/Al.md"), "---\naliases: [Shared]\n---\nX\n").unwrap();
    fs::write(root.join("y/Bl.md"), "---\naliases: [Shared]\n---\nY\n").unwrap();
    fs::write(root.join("Tasks/Dup2.md"), "- [ ] t\n").unwrap();
    fs::write(root.join("Other/Dup2.md"), "- [ ] t\n").unwrap();

    let snap_before = plan_snapshot_dir(&root);

    let dup_msg = "Ambiguous note reference 'Dup': 2 notes match (a/Dup.md, b/Dup.md). Use a vault-relative path.";
    let shared_msg = "Ambiguous note reference 'Shared': 2 notes match (x/Al.md, y/Bl.md). Use a vault-relative path.";
    let dup2_msg = "Ambiguous note reference 'Dup2': 2 notes match (Other/Dup2.md, Tasks/Dup2.md). Use a vault-relative path.";

    let cli_cases: &[(&[&str], &str)] = &[
        (&["notes", "read", "Dup"], dup_msg),
        (&["notes", "append", "Dup", "X"], dup_msg),
        (
            &["notes", "replace", "Dup", "--find", "A", "--replace", "Z"],
            dup_msg,
        ),
        (&["notes", "delete", "--yes", "Dup"], dup_msg),
        (&["notes", "move", "Dup", "c/Dup"], dup_msg),
        (&["toggle-task", "Dup2", "1"], dup2_msg),
        (&["links", "Dup"], dup_msg),
        (&["notes", "read", "Shared"], shared_msg),
    ];
    for (args, expected_err) in cli_cases {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
        assert!(out.stdout.is_empty(), "expected empty stdout for {args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected_err),
            "expected {expected_err:?} in stderr for {args:?}, got: {stderr}"
        );
    }

    let mcp = plan_mcp_server(&root);
    let mcp_cases: &[(&str, Value, &str)] = &[
        ("read_note", json!({ "note": "Dup" }), dup_msg),
        (
            "append_note",
            json!({ "note": "Dup", "text": "X" }),
            dup_msg,
        ),
        (
            "replace_note_text",
            json!({ "note": "Dup", "find": "A", "replace": "Z" }),
            dup_msg,
        ),
        (
            "update_frontmatter",
            json!({ "note": "Dup", "properties": { "k": "v" } }),
            dup_msg,
        ),
        (
            "delete_note",
            json!({ "note": "Dup", "confirm": true }),
            dup_msg,
        ),
        (
            "move_note",
            json!({ "src": "Dup", "dst": "c/Dup" }),
            dup_msg,
        ),
        (
            "toggle_task",
            json!({ "note": "Dup2", "line": 1 }),
            dup2_msg,
        ),
    ];
    for (tool, args, expected_err) in mcp_cases {
        let err = mcp
            .dispatch_tool(tool, args)
            .expect_err(&format!("expected Err for {tool} {args}"));
        assert!(
            err.to_string().contains(expected_err),
            "expected {expected_err:?} for {tool}, got: {err}"
        );
    }

    assert_eq!(plan_snapshot_dir(&root), snap_before);

    // Controls
    let read_a = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "a/Dup"])
        .output()
        .unwrap();
    assert!(read_a.status.success());
    assert_eq!(read_a.stdout, b"A");

    let read_b = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "b/Dup.md"])
        .output()
        .unwrap();
    assert!(read_b.status.success());
    assert_eq!(read_b.stdout, b"B");

    let app_b = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "append", "b/Dup.md", "X"])
        .output()
        .unwrap();
    assert!(app_b.status.success());
    assert_eq!(fs::read_to_string(root.join("a/Dup.md")).unwrap(), "A");
    assert!(fs::read_to_string(root.join("b/Dup.md"))
        .unwrap()
        .contains('X'));

    let read_solo = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "Solo"])
        .output()
        .unwrap();
    assert!(read_solo.status.success());
    assert_eq!(read_solo.stdout, b"SOLO");

    let read_ci = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "A/DUP.MD", "--json"])
        .output()
        .unwrap();
    assert!(read_ci.status.success());
    let ci_json: Value = serde_json::from_slice(&read_ci.stdout).unwrap();
    assert_eq!(ci_json["rel_path"], "a/Dup.md");
}

#[test]
fn plan_exact_root_path_wins_over_same_named_nested_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("Dup.md"), "ROOT").unwrap();
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();

    let rd = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "Dup"])
        .output()
        .unwrap();
    assert!(rd.status.success());
    assert_eq!(rd.stdout, b"ROOT");

    let ap = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "append", "Dup", "X"])
        .output()
        .unwrap();
    assert!(ap.status.success());
    assert!(fs::read_to_string(root.join("Dup.md"))
        .unwrap()
        .contains('X'));
    assert_eq!(fs::read_to_string(root.join("a/Dup.md")).unwrap(), "A");
    assert_eq!(fs::read_to_string(root.join("b/Dup.md")).unwrap(), "B");
}

#[test]
fn plan_outgoing_links_classify_attachment_ambiguous_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["Attachments", "a", "b"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("Attachments/pic.png"), b"png").unwrap();
    fs::write(root.join("Attachments/doc.pdf"), b"%PDF-1.4").unwrap();
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();
    fs::write(root.join("Real.md"), "# Real\n").unwrap();
    fs::write(
        root.join("a/Src.md"),
        "![[pic.png]]\n![[doc.pdf]]\n![[nope.png]]\n[[Dup]]\n[[Ghost]]\n[[Real]]\n",
    )
    .unwrap();
    fs::write(root.join("Root.md"), "[[Dup]]\n").unwrap();

    let run = |args: &[&str]| {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    let src_json: Value =
        serde_json::from_slice(&run(&["links", "a/Src.md", "--json"]).stdout).unwrap();
    let find_entry = |arr: &Value, target: &str| -> Value {
        arr.as_array()
            .unwrap()
            .iter()
            .find(|e| e["target_raw"] == target)
            .cloned()
            .unwrap_or_else(|| panic!("missing {target} in {arr}"))
    };

    let pic = find_entry(&src_json, "pic.png");
    assert_eq!(pic["status"], "attachment");
    assert_eq!(pic["resolved"], true);
    assert_eq!(pic["target_resolved_path"], "Attachments/pic.png");

    let doc = find_entry(&src_json, "doc.pdf");
    assert_eq!(doc["status"], "attachment");
    assert_eq!(doc["resolved"], true);
    assert_eq!(doc["target_resolved_path"], "Attachments/doc.pdf");

    let nope = find_entry(&src_json, "nope.png");
    assert_eq!(nope["status"], "missing");
    assert_eq!(nope["resolved"], false);

    let local_dup = find_entry(&src_json, "Dup");
    assert_eq!(local_dup["status"], "resolved");
    assert_eq!(local_dup["resolved"], true);
    assert_eq!(local_dup["target_resolved_path"], "a/Dup.md");

    let ghost = find_entry(&src_json, "Ghost");
    assert_eq!(ghost["status"], "missing");
    assert!(ghost.get("candidates").is_none());

    let real = find_entry(&src_json, "Real");
    assert_eq!(real["status"], "resolved");
    assert_eq!(real["target_resolved_path"], "Real.md");

    let root_json: Value =
        serde_json::from_slice(&run(&["links", "Root.md", "--json"]).stdout).unwrap();
    let root_dup = find_entry(&root_json, "Dup");
    assert_eq!(root_dup["status"], "ambiguous");
    assert_eq!(root_dup["resolved"], false);
    assert_eq!(root_dup["candidates"], json!(["a/Dup.md", "b/Dup.md"]));

    let src_text = String::from_utf8(run(&["links", "a/Src.md"]).stdout).unwrap();
    let missing_lines: Vec<&str> = src_text
        .lines()
        .filter(|line| line.contains("(missing)"))
        .collect();
    assert_eq!(missing_lines.len(), 2, "got:\n{src_text}");
    assert!(missing_lines.iter().any(|l| l.contains("nope.png")));
    assert!(missing_lines.iter().any(|l| l.contains("Ghost")));
    assert!(src_text.contains("→ pic.png (Attachments/pic.png)"));

    let root_text = String::from_utf8(run(&["links", "Root.md"]).stdout).unwrap();
    assert!(root_text.contains("(ambiguous: a/Dup.md, b/Dup.md)"));

    let mcp = plan_mcp_server(&root);
    let mcp_src = mcp
        .dispatch_tool("get_outgoing_links", &json!({ "note": "a/Src.md" }))
        .unwrap();
    assert_eq!(
        find_entry(&mcp_src["outgoing_links"], "pic.png")["status"],
        "attachment"
    );
    let mcp_full = mcp
        .dispatch_tool(
            "read_note",
            &json!({ "note": "a/Src.md", "compact": false }),
        )
        .unwrap();
    assert_eq!(
        find_entry(&mcp_full["outgoing_links"], "pic.png")["status"],
        "attachment"
    );

    let list_json: Value =
        serde_json::from_slice(&run(&["notes", "list", "--json"]).stdout).unwrap();
    let src_summary = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "a/Src.md")
        .unwrap();
    assert_eq!(src_summary["outgoing_count"], 6);

    // W5: graph excludes existing non-image attachments and marks ambiguous targets
    let src_graph: Value =
        serde_json::from_slice(&run(&["graph", "a/Src.md", "--depth", "1", "--json"]).stdout)
            .unwrap();
    assert!(
        !src_graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"].as_str().unwrap_or("").contains("doc.pdf")),
        "existing doc.pdf attachment must not appear as a graph node: {src_graph}"
    );
    let root_graph: Value =
        serde_json::from_slice(&run(&["graph", "Root.md", "--depth", "1", "--json"]).stdout)
            .unwrap();
    let ambig_node = root_graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "unresolved:Dup")
        .unwrap_or_else(|| panic!("missing unresolved:Dup in {root_graph}"));
    assert_eq!(ambig_node["status"], "ambiguous");
    assert_eq!(ambig_node["resolved"], false);

    fs::remove_file(root.join("Attachments/pic.png")).unwrap();
    let after_del: Value =
        serde_json::from_slice(&run(&["links", "a/Src.md", "--json"]).stdout).unwrap();
    let pic_after = find_entry(&after_del, "pic.png");
    assert_eq!(pic_after["status"], "missing");
    assert_eq!(pic_after["resolved"], false);
    assert!(pic_after["target_resolved_path"].is_null());
}

#[test]
fn plan_move_rewrites_moved_note_destinations_without_retargeting() {
    for use_mcp in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        for d in ["sub", "img", "deep/nested/img", "incoming"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("sub/Note.md"), "# Note\n").unwrap();
        fs::write(root.join("img/pic.png"), b"real").unwrap();
        fs::write(root.join("deep/nested/img/pic.png"), b"decoy").unwrap();

        let mover_before = concat!(
            "[plain](sub/Note.md)\n",
            "[dot](./sub/Note.md)\n",
            "![img](img/pic.png)\n",
            "![imgdot](./img/pic.png)\n",
            "[abs](/sub/Note.md)\n",
            "![a](/img/pic.png)\n",
            "[anchor](#local)\n",
            "[e](https://example.com/img/pic.png)\n",
            "[m](mailto:a@example.invalid)\n",
            "[p](//host/x.png)\n",
            "`[inline](sub/Note.md)`\n",
            "```md\n",
            "![fence](img/pic.png)\n",
            "```\n",
            "![embed](sub/Note.md)\n",
            "[shot](img/pic.png)\n",
            "![x](nope.png)\n",
        );
        let mover_expected = concat!(
            "[plain](../../sub/Note.md)\n",
            "[dot](../../sub/Note.md)\n",
            "![img](../../img/pic.png)\n",
            "![imgdot](../../img/pic.png)\n",
            "[abs](/sub/Note.md)\n",
            "![a](/img/pic.png)\n",
            "[anchor](#local)\n",
            "[e](https://example.com/img/pic.png)\n",
            "[m](mailto:a@example.invalid)\n",
            "[p](//host/x.png)\n",
            "`[inline](sub/Note.md)`\n",
            "```md\n",
            "![fence](img/pic.png)\n",
            "```\n",
            "![embed](../../sub/Note.md)\n",
            "[shot](../../img/pic.png)\n",
            "![x](nope.png)\n",
        );
        fs::write(root.join("Mover.md"), mover_before).unwrap();
        fs::write(
            root.join("Incoming.md"),
            "[a](Mover.md)\n[b](./Mover.md)\n[[Mover]]\n",
        )
        .unwrap();
        fs::write(
            root.join("incoming/Reader.md"),
            "[m](../Mover.md)\n![[Mover]]\n",
        )
        .unwrap();

        let res = if use_mcp {
            let mcp = plan_mcp_server(&root);
            mcp.dispatch_tool(
                "move_note",
                &json!({ "src": "Mover.md", "dst": "deep/nested/Mover.md" }),
            )
            .unwrap()
        } else {
            let out = isolated_cli(tmp.path(), &root)
                .env("OBSIDIAN_AUTO_DAEMON", "0")
                .args([
                    "--json",
                    "notes",
                    "move",
                    "Mover.md",
                    "deep/nested/Mover.md",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        };

        assert_eq!(res["updated_links_in_notes"], 3);
        assert_eq!(
            fs::read(root.join("deep/nested/Mover.md")).unwrap(),
            mover_expected.as_bytes()
        );
        assert_eq!(
            fs::read(root.join("deep/nested").join("../../img/pic.png")).unwrap(),
            b"real"
        );
        assert_eq!(
            fs::read_to_string(root.join("Incoming.md")).unwrap(),
            "[a](deep/nested/Mover.md)\n[b](deep/nested/Mover.md)\n[[Mover]]\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("incoming/Reader.md")).unwrap(),
            "[m](../deep/nested/Mover.md)\n![[Mover]]\n"
        );

        let full_out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(["notes", "read", "deep/nested/Mover.md", "--json", "--full"])
            .output()
            .unwrap();
        assert!(full_out.status.success());
        let full: Value = serde_json::from_slice(&full_out.stdout).unwrap();
        let img_entry = full["images"]
            .as_array()
            .unwrap()
            .iter()
            .find(|img| img["raw_target"] == "../../img/pic.png")
            .expect("expected ../../img/pic.png in images");
        let resolved_abs = std::path::PathBuf::from(img_entry["abs_path"].as_str().unwrap());
        assert_eq!(
            fs::canonicalize(&resolved_abs).unwrap(),
            fs::canonicalize(root.join("img/pic.png")).unwrap()
        );
    }
}

#[test]
fn plan_move_back_up_preserves_anchor_title_percent_and_angle_forms() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["deep/nested", "sub", "my files"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("sub/Note.md"), "# Note\n\n## Sec\n").unwrap();
    fs::write(root.join("my files/spec v1.pdf"), b"%PDF").unwrap();
    let before = concat!(
        "[up](../../sub/Note.md#Sec \"t\")\r\n",
        "[enc](../../my%20files/spec%20v1.pdf)\n",
        "[sp](<../../my files/spec v1.pdf>)",
    );
    let expected = concat!(
        "[up](sub/Note.md#Sec \"t\")\r\n",
        "[enc](my%20files/spec%20v1.pdf)\n",
        "[sp](<my files/spec v1.pdf>)",
    );
    fs::write(root.join("deep/nested/Mover.md"), before).unwrap();

    let out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args([
            "--json",
            "notes",
            "move",
            "deep/nested/Mover.md",
            "Mover.md",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(root.join("Mover.md")).unwrap(),
        expected.as_bytes()
    );
}

#[test]
fn plan_move_keeps_location_independent_destinations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["img", "x", "y/img", "y/z", "Attachments", "a", "b", "sub"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("img/pic.png"), b"root-img").unwrap();
    fs::write(root.join("y/img/pic.png"), b"shadow-img").unwrap();
    fs::write(root.join("Attachments/pic.png"), b"att-img").unwrap();
    fs::write(root.join("sub/pic.png"), b"sub-shadow").unwrap();
    fs::write(root.join("a/local.png"), b"local").unwrap();
    fs::write(root.join("a/Peer.md"), "# Peer\n").unwrap();

    let m6 = "![r](img/pic.png)\n";
    let m7 = "![r](img/pic.png)\n";
    let m17 = "![[pic.png]]\n";
    let m18 = "![[pic.png|200]]\n";
    let m19 = "[p](local.png)\n[n](Peer.md)\n";
    fs::write(root.join("x/Mover.md"), m6).unwrap();
    fs::write(root.join("x/M.md"), m7).unwrap();
    fs::write(root.join("a/W.md"), m17).unwrap();
    fs::write(root.join("a/W2.md"), m18).unwrap();
    fs::write(root.join("a/X.md"), m19).unwrap();

    let move_cli = |src: &str, dst: &str| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(["--json", "notes", "move", src, dst])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    // M6: vault-root-relative stays untouched when unshadowed
    let r6 = move_cli("x/Mover.md", "y/z/Mover.md");
    assert_eq!(r6["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("y/z/Mover.md")).unwrap(), m6.as_bytes());

    // M7: vault-root-relative rewritten when destination shadows it
    let r7 = move_cli("x/M.md", "y/M.md");
    assert_eq!(r7["updated_links_in_notes"], 1);
    assert_eq!(
        fs::read_to_string(root.join("y/M.md")).unwrap(),
        "![r](../img/pic.png)\n"
    );

    // M17: wikilink image embed stays untouched when unshadowed
    let r17 = move_cli("a/W.md", "b/W.md");
    assert_eq!(r17["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("b/W.md")).unwrap(), m17.as_bytes());

    // M18: wikilink image embed rewritten when destination folder shadows it
    let r18 = move_cli("a/W2.md", "sub/W2.md");
    assert_eq!(r18["updated_links_in_notes"], 1);
    assert_eq!(
        fs::read_to_string(root.join("sub/W2.md")).unwrap(),
        "![[Attachments/pic.png|200]]\n"
    );
    let links_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["links", "sub/W2.md", "--json"])
        .output()
        .unwrap();
    assert!(links_out.status.success());
    let links_json: Value = serde_json::from_slice(&links_out.stdout).unwrap();
    assert_eq!(links_json[0]["status"], "attachment");
    assert_eq!(links_json[0]["target_resolved_path"], "Attachments/pic.png");

    // M19: same-folder rename leaves own destinations untouched
    let r19 = move_cli("a/X.md", "a/Y.md");
    assert_eq!(r19["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("a/Y.md")).unwrap(), m19.as_bytes());
}
