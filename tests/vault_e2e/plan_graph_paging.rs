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
fn plan_graph_degrees_depth_and_order_are_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("A.md"),
        "[[Ghost]]\n[[C]]\n[[C]]\n[[C]]\n![[pic.png]]\n[[A]]\n",
    )
    .unwrap();
    fs::write(root.join("B.md"), "[[Ghost]]\n").unwrap();
    fs::write(root.join("C.md"), "# C\n").unwrap();

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
        out.stdout
    };

    let g1_bytes = run(&["graph", "--json"]);
    let g2_bytes = run(&["graph", "--json"]);
    assert_eq!(
        g1_bytes, g2_bytes,
        "global graph output must be byte-identical"
    );

    let g1: Value = serde_json::from_slice(&g1_bytes).unwrap();
    let find_node = |id: &str| -> Value {
        g1["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("missing node {id} in {g1}"))
    };
    assert_eq!(find_node("unresolved:Ghost")["in_degree"], 2);
    assert_eq!(find_node("unresolved:Ghost")["status"], "missing");
    assert_eq!(find_node("A.md")["out_degree"], 2);
    assert_eq!(find_node("A.md")["status"], "resolved");
    assert_eq!(find_node("C.md")["in_degree"], 1);

    let g_hi: Value =
        serde_json::from_slice(&run(&["graph", "A", "--depth", "9", "--json"])).unwrap();
    assert_eq!(g_hi["depth"], 5);
    let g_lo: Value =
        serde_json::from_slice(&run(&["graph", "A", "--depth", "0", "--json"])).unwrap();
    assert_eq!(g_lo["depth"], 1);

    let mcp = plan_mcp_server(&root);
    let mcp_g = mcp
        .dispatch_tool("get_local_graph", &json!({ "note": "A", "depth": 9 }))
        .unwrap();
    assert_eq!(mcp_g["depth"], 5);

    let list_json: Value = serde_json::from_slice(&run(&["notes", "list", "--json"])).unwrap();
    let a_note = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "A.md")
        .unwrap();
    assert_eq!(a_note["outgoing_count"], 6);
}

#[test]
fn plan_note_and_task_pages_cover_everything_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    seed_plan_paging_vault(&root);

    let run_json = |args: &[&str]| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let first_page = run_json(&["notes", "list", "--json"]);
    assert_eq!(first_page["returned"], 50);
    assert_eq!(first_page["total"], 120);
    assert_eq!(first_page["has_more"], true);
    assert!(first_page["next_cursor"].is_string());

    let all_notes = run_json(&["notes", "list", "--json", "--all"]);
    let all_paths: Vec<String> = all_notes["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["rel_path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(all_paths.len(), 120);

    let mut paged_paths = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let mut args = vec!["notes", "list", "--json", "--limit", "50"];
        if let Some(ref c) = cursor {
            args.push("--cursor");
            args.push(c.as_str());
        }
        let p = run_json(&args);
        pages += 1;
        for n in p["notes"].as_array().unwrap() {
            paged_paths.push(n["rel_path"].as_str().unwrap().to_string());
        }
        if p["has_more"].as_bool().unwrap() {
            cursor = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(p["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(paged_paths, all_paths);

    let lim1 = run_json(&["notes", "list", "--json", "--limit", "1"]);
    assert_eq!(lim1["returned"], 1);
    assert_eq!(lim1["has_more"], true);

    let lim500 = run_json(&["notes", "list", "--json", "--limit", "500"]);
    assert_eq!(lim500["returned"], 120);
    assert_eq!(lim500["has_more"], false);
    assert!(lim500["next_cursor"].is_null());

    let folder_page = run_json(&["notes", "list", "--json", "--folder", "Alpha"]);
    assert_eq!(folder_page["total"], 40);
    assert_eq!(folder_page["returned"], 40);
    assert_eq!(folder_page["has_more"], false);

    let past_end = run_json(&["notes", "list", "--json", "--cursor", "zzzz/PastEnd.md"]);
    assert_eq!(past_end["returned"], 0);
    assert_eq!(past_end["has_more"], false);
    assert!(past_end["next_cursor"].is_null());

    // Tasks paging
    let all_tasks = run_json(&["tasks", "--json", "--all"]);
    let all_task_keys: Vec<(String, u64, bool)> = all_tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["note_path"].as_str().unwrap().to_string(),
                t["line_no"].as_u64().unwrap(),
                t["checked"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(all_task_keys.len(), 240);
    assert!(all_task_keys[..120].iter().all(|(_, _, checked)| !checked));
    assert!(all_task_keys[120..].iter().all(|(_, _, checked)| *checked));

    let mut paged_tasks = Vec::new();
    let mut tcursor: Option<String> = None;
    loop {
        let mut args = vec!["tasks", "--json", "--limit", "100"];
        if let Some(ref c) = tcursor {
            args.push("--cursor");
            args.push(c.as_str());
        }
        let p = run_json(&args);
        for t in p["tasks"].as_array().unwrap() {
            paged_tasks.push((
                t["note_path"].as_str().unwrap().to_string(),
                t["line_no"].as_u64().unwrap(),
                t["checked"].as_bool().unwrap(),
            ));
        }
        if p["has_more"].as_bool().unwrap() {
            tcursor = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(p["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(paged_tasks, all_task_keys);

    let open_all = run_json(&["tasks", "--open", "--json", "--all"]);
    assert_eq!(open_all["returned"], 120);
    assert_eq!(open_all["total"], 120);

    // MCP paging
    let mcp = plan_mcp_server(&root);
    let m1 = mcp.dispatch_tool("list_notes", &json!({})).unwrap();
    assert_eq!(m1["count"], 50);
    assert_eq!(m1["returned"], 50);
    assert_eq!(m1["total"], 120);
    let mut mcp_paths = Vec::new();
    let mut mcur: Option<String> = None;
    loop {
        let mut arg = json!({ "limit": 50 });
        if let Some(ref c) = mcur {
            arg["cursor"] = json!(c);
        }
        let p = mcp.dispatch_tool("list_notes", &arg).unwrap();
        assert_eq!(p["count"], p["returned"]);
        for n in p["notes"].as_array().unwrap() {
            mcp_paths.push(n["rel_path"].as_str().unwrap().to_string());
        }
        if p["has_more"].as_bool().unwrap() {
            mcur = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            break;
        }
    }
    assert_eq!(mcp_paths, all_paths);

    let mt = mcp
        .dispatch_tool("list_tasks", &json!({ "limit": 5 }))
        .unwrap();
    assert_eq!(mt["count"], 5);
    assert_eq!(mt["returned"], 5);
    assert_eq!(mt["total"], 240);
    assert_eq!(mt["has_more"], true);

    // Text `notes list` remains unbounded
    let text_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "list"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert_eq!(text.lines().count(), 120);
}

#[test]
fn plan_paging_survives_deletion_of_cursor_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    seed_plan_paging_vault(&root);

    let run_json = |args: &[&str]| -> Value {
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
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let p1 = run_json(&["notes", "list", "--json", "--limit", "50"]);
    let deleted_cursor = p1["next_cursor"].as_str().unwrap().to_string();
    fs::remove_file(root.join(&deleted_cursor)).unwrap();

    let mut remaining = Vec::new();
    let mut cursor = Some(deleted_cursor.clone());
    while let Some(c) = cursor {
        let p = run_json(&["notes", "list", "--json", "--limit", "50", "--cursor", &c]);
        for n in p["notes"].as_array().unwrap() {
            remaining.push(n["rel_path"].as_str().unwrap().to_string());
        }
        cursor = if p["has_more"].as_bool().unwrap() {
            Some(p["next_cursor"].as_str().unwrap().to_string())
        } else {
            None
        };
    }

    assert_eq!(remaining.len(), 70);
    assert!(!remaining.contains(&deleted_cursor));
    let unique: std::collections::HashSet<&String> = remaining.iter().collect();
    assert_eq!(unique.len(), 70);
}

#[test]
fn plan_paging_rejects_bad_limits_and_cursors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("One.md"), "# One\n- [ ] task\n").unwrap();

    for bad_args in [
        &["notes", "list", "--limit", "0"][..],
        &["notes", "list", "--limit", "501"][..],
        &["notes", "list", "--all", "--limit", "5"][..],
    ] {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(bad_args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit 2 for {bad_args:?}"
        );
        assert!(out.stdout.is_empty());
    }

    let bad_cur = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["tasks", "--cursor", "abc"])
        .output()
        .unwrap();
    assert_eq!(bad_cur.status.code(), Some(1));
    assert!(bad_cur.stdout.is_empty());
    assert!(String::from_utf8_lossy(&bad_cur.stderr).contains("Invalid cursor: 'abc'"));

    let mcp = plan_mcp_server(&root);
    let e1 = mcp
        .dispatch_tool("list_notes", &json!({ "limit": 0 }))
        .unwrap_err()
        .to_string();
    assert!(
        e1.contains("Invalid 'limit': expected an integer from 1 to 500"),
        "{e1}"
    );
    let e2 = mcp
        .dispatch_tool("list_notes", &json!({ "limit": "5" }))
        .unwrap_err()
        .to_string();
    assert!(
        e2.contains("Invalid 'limit': expected an integer from 1 to 500"),
        "{e2}"
    );
    let e3 = mcp
        .dispatch_tool("list_notes", &json!({ "cursor": 5 }))
        .unwrap_err()
        .to_string();
    assert!(e3.contains("Invalid 'cursor': expected a string"), "{e3}");
    let e4 = mcp
        .dispatch_tool("list_tasks", &json!({ "cursor": "abc" }))
        .unwrap_err()
        .to_string();
    assert!(e4.contains("Invalid cursor: 'abc'"), "{e4}");
}

#[test]
fn plan_search_reports_applied_limit_and_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    for i in 0..25usize {
        fs::write(
            root.join(format!("Hit-{i:02}.md")),
            format!("# Hit {i:02}\n\nContains needle token {i}.\n"),
        )
        .unwrap();
    }

    let run_json = |args: &[&str]| -> Value {
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
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let def = run_json(&["search", "needle", "--json"]);
    assert_eq!(def["returned"], 20);
    assert_eq!(def["limit"], 20);
    assert_eq!(def["has_more"], true);
    assert!(def["next_cursor"].is_null());

    let exact = run_json(&["search", "needle", "--json", "--limit", "25"]);
    assert_eq!(exact["returned"], 25);
    assert_eq!(exact["limit"], 25);
    assert_eq!(exact["has_more"], false);

    let hi = run_json(&["search", "needle", "--json", "--limit", "500"]);
    assert_eq!(hi["limit"], 200);
    assert_eq!(hi["returned"], 25);
    assert_eq!(hi["has_more"], false);

    let lo = run_json(&["search", "needle", "--json", "--limit", "0"]);
    assert_eq!(lo["limit"], 1);
    assert_eq!(lo["returned"], 1);
    assert_eq!(lo["has_more"], true);

    let mcp = plan_mcp_server(&root);
    let m_def = mcp
        .dispatch_tool("search_notes", &json!({ "query": "needle" }))
        .unwrap();
    assert_eq!(m_def["limit"], 20);
    assert_eq!(m_def["returned"], 20);
    assert_eq!(m_def["count"], 20);
    assert_eq!(m_def["has_more"], true);
    assert!(m_def["next_cursor"].is_null());

    let m_25 = mcp
        .dispatch_tool("search_notes", &json!({ "query": "needle", "limit": 25 }))
        .unwrap();
    assert_eq!(m_25["limit"], 25);
    assert_eq!(m_25["returned"], 25);
    assert_eq!(m_25["count"], 25);
    assert_eq!(m_25["has_more"], false);
}

#[test]
fn plan_daemon_status_reports_unknown_note_total_when_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let out = isolated_cli_base(tmp.path())
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["--json", "daemon", "status"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let st: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(st["running"], false);
    assert!(
        st["total_notes"].is_null(),
        "expected null total_notes, got {st}"
    );
}

#[test]
fn move_destination_spelling_does_not_change_link_rewrites() {
    for use_mcp in [false, true] {
        let mut control_ref_bytes: Option<Vec<u8>> = None;
        for dst in [
            "Sub/TargetMoved.md",
            "./Sub/TargetMoved.md",
            "Sub//TargetMoved.md",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("vault");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("Target.md"), "# Target\n").unwrap();
            fs::write(root.join("Ref.md"), "See [[Target]] and [Doc](Target.md)\n").unwrap();

            let (moved, links) = if use_mcp {
                let mcp = plan_mcp_server(&root);
                let moved = mcp
                    .dispatch_tool("move_note", &json!({ "src": "Target.md", "dst": dst }))
                    .unwrap();
                let links = mcp
                    .dispatch_tool("get_outgoing_links", &json!({ "note": "Ref.md" }))
                    .unwrap()["outgoing_links"]
                    .clone();
                (moved, links)
            } else {
                let out = isolated_cli(tmp.path(), &root)
                    .env("OBSIDIAN_AUTO_DAEMON", "0")
                    .args(["notes", "move", "Target.md", dst, "--json"])
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "move to {dst} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let moved: Value = serde_json::from_slice(&out.stdout).unwrap();

                let links_out = isolated_cli(tmp.path(), &root)
                    .env("OBSIDIAN_AUTO_DAEMON", "0")
                    .args(["links", "Ref.md", "--json"])
                    .output()
                    .unwrap();
                assert!(
                    links_out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&links_out.stderr)
                );
                let links: Value = serde_json::from_slice(&links_out.stdout).unwrap();
                (moved, links)
            };

            assert_eq!(moved["to"], "Sub/TargetMoved.md", "dst={dst}");
            let ref_bytes = fs::read(root.join("Ref.md")).unwrap();
            let ref_text = String::from_utf8(ref_bytes.clone()).unwrap();
            if let Some(expected) = &control_ref_bytes {
                assert_eq!(
                    &ref_bytes, expected,
                    "Ref.md bytes diverged for dst={dst} (use_mcp={use_mcp})"
                );
            } else {
                assert!(
                    ref_text.contains("](Sub/TargetMoved.md)"),
                    "expected rewritten markdown link in control Ref.md, got: {ref_text}"
                );
                control_ref_bytes = Some(ref_bytes);
            }

            let arr = links.as_array().unwrap();
            assert!(!arr.is_empty());
            for entry in arr {
                assert_eq!(
                    entry["status"], "resolved",
                    "unresolved link after move to {dst}: {entry}"
                );
            }
        }

        // Index-excluded destinations (bare '.md' and dot-folders) must be rejected without moving.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("ToVanish.md"), "# Vanish\n").unwrap();
        fs::write(root.join("ToDotDir.md"), "# DotDir\n").unwrap();

        for (src, bad_dst) in [("ToVanish.md", ".md"), ("ToDotDir.md", ".hidden/Lost.md")] {
            if use_mcp {
                let mcp = plan_mcp_server(&root);
                assert!(
                    mcp.dispatch_tool("move_note", &json!({ "src": src, "dst": bad_dst }))
                        .is_err(),
                    "expected MCP move to {bad_dst} to fail"
                );
                assert!(root.join(src).is_file());
                assert!(mcp
                    .dispatch_tool("read_note", &json!({ "note": src }))
                    .is_ok());
            } else {
                let out = isolated_cli(tmp.path(), &root)
                    .env("OBSIDIAN_AUTO_DAEMON", "0")
                    .args(["notes", "move", src, bad_dst, "--json"])
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(1), "expected exit 1 for {bad_dst}");
                assert!(out.stderr.is_empty());
                let err_json: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(err_json["ok"], false);
                assert_eq!(err_json["error"]["code"], "invalid_destination");
                assert!(root.join(src).is_file());

                let read_out = isolated_cli(tmp.path(), &root)
                    .env("OBSIDIAN_AUTO_DAEMON", "0")
                    .args(["notes", "read", src, "--json"])
                    .output()
                    .unwrap();
                assert!(read_out.status.success());
            }
        }
    }
}

#[test]
fn move_keeps_non_image_wikilink_embeds_pointing_at_same_file() {
    for use_mcp in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        fs::create_dir_all(root.join("TeamA/Specs")).unwrap();
        fs::write(
            root.join("TeamA/Specs/Hub.md"),
            "![[./local.pdf]]\n[Local PDF](./local.pdf)\n",
        )
        .unwrap();
        fs::write(root.join("TeamA/Specs/local.pdf"), b"%PDF-1.4").unwrap();

        if use_mcp {
            let mcp = plan_mcp_server(&root);
            mcp.dispatch_tool(
                "move_note",
                &json!({ "src": "TeamA/Specs/Hub.md", "dst": "TeamB/Specs/Hub.md" }),
            )
            .unwrap();
        } else {
            let out = isolated_cli(tmp.path(), &root)
                .env("OBSIDIAN_AUTO_DAEMON", "0")
                .args([
                    "notes",
                    "move",
                    "TeamA/Specs/Hub.md",
                    "TeamB/Specs/Hub.md",
                    "--json",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        assert_eq!(
            fs::read_to_string(root.join("TeamB/Specs/Hub.md")).unwrap(),
            "![[TeamA/Specs/local.pdf]]\n[Local PDF](../../TeamA/Specs/local.pdf)\n"
        );
    }
}

#[test]
fn create_note_keeps_tags_and_reports_real_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();

    let run_cli = |args: &[&str]| {
        isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap()
    };

    // 1. Frontmatter content + --tags merges tags into existing frontmatter
    let fm_out = run_cli(&[
        "notes",
        "create",
        "FMWithTags",
        "--content",
        "---\nstatus: draft\n---\n\n# FMWithTags\nBody\n",
        "--tags",
        "urgent,ops",
        "--json",
    ]);
    assert!(
        fm_out.status.success(),
        "{}",
        String::from_utf8_lossy(&fm_out.stderr)
    );
    let fm_disk = fs::read_to_string(root.join("FMWithTags.md")).unwrap();
    assert!(fm_disk.contains("status: draft"), "got:\n{fm_disk}");
    assert!(fm_disk.contains("tags: [urgent, ops]"), "got:\n{fm_disk}");
    let tag_out = run_cli(&["tags", "urgent", "--json"]);
    assert!(tag_out.status.success());
    let tagged: Value = serde_json::from_slice(&tag_out.stdout).unwrap();
    assert!(tagged
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["rel_path"] == "FMWithTags.md"));

    // Conflicting tags in both frontmatter and --tags must fail without writing
    let conflict_out = run_cli(&[
        "notes",
        "create",
        "ConflictTags",
        "--content",
        "---\ntags: [existing]\n---\nBody\n",
        "--tags",
        "extra",
        "--json",
    ]);
    assert_eq!(conflict_out.status.code(), Some(1));
    assert!(conflict_out.stderr.is_empty());
    let conflict_json: Value = serde_json::from_slice(&conflict_out.stdout).unwrap();
    assert_eq!(conflict_json["ok"], false);
    assert_eq!(conflict_json["error"]["code"], "invalid_argument");
    assert!(!root.join("ConflictTags.md").exists());

    // 2. Horizontal rule `---` without closing frontmatter is not mistaken for frontmatter
    let hr_out = run_cli(&[
        "notes",
        "create",
        "HRWithTags",
        "--content",
        "---\nParagraph under HR\n",
        "--tags",
        "hrtag",
        "--json",
    ]);
    assert!(
        hr_out.status.success(),
        "{}",
        String::from_utf8_lossy(&hr_out.stderr)
    );
    let hr_disk = fs::read_to_string(root.join("HRWithTags.md")).unwrap();
    assert!(hr_disk.contains("created: "), "got:\n{hr_disk}");
    assert!(hr_disk.contains("tags: [hrtag]\n"), "got:\n{hr_disk}");
    assert!(hr_disk.contains("# HRWithTags\n"), "got:\n{hr_disk}");
    assert!(
        hr_disk.contains("---\nParagraph under HR\n"),
        "got:\n{hr_disk}"
    );

    // 3. Leading slash in title returns normalized rel_path matching disk/index
    let slash_out = run_cli(&[
        "notes",
        "create",
        "/LeadingSlash",
        "--content",
        "Body",
        "--json",
    ]);
    assert!(
        slash_out.status.success(),
        "{}",
        String::from_utf8_lossy(&slash_out.stderr)
    );
    let slash_json: Value = serde_json::from_slice(&slash_out.stdout).unwrap();
    assert_eq!(slash_json["rel_path"], "LeadingSlash.md");
    assert!(root.join("LeadingSlash.md").is_file());

    // 4. MCP string tags rejected (isError: true, file not created)
    let mcp = plan_mcp_server(&root);
    let rpc_err = mcp
        .handle_rpc(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "create_note",
                "arguments": {
                    "title": "McpStringTags",
                    "tags": "tag1,tag2",
                    "content": "Hello"
                }
            }
        }))
        .unwrap();
    assert_eq!(rpc_err["result"]["isError"], true);
    assert!(!root.join("McpStringTags.md").exists());

    // 5. YAML-special tag characters are safely quoted and round-trip as strings
    mcp.dispatch_tool(
        "create_note",
        &json!({
            "title": "BrokenYaml",
            "tags": ["normal", "key: value"],
            "content": "Hello"
        }),
    )
    .unwrap();
    let yaml_disk = fs::read_to_string(root.join("BrokenYaml.md")).unwrap();
    assert!(
        yaml_disk.contains("tags: [normal, \"key: value\"]\n"),
        "got:\n{yaml_disk}"
    );
    let read_back = mcp
        .dispatch_tool(
            "read_note",
            &json!({ "note": "BrokenYaml.md", "compact": false }),
        )
        .unwrap();
    let tags = read_back["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2, "expected 2 string tags, got: {read_back}");
    assert!(tags.iter().any(|t| t == "normal"));
    assert!(tags.iter().any(|t| t == "key: value"));

    // Control: plain tags format unchanged
    let plain_out = run_cli(&["notes", "create", "Plain", "--tags", "a,b", "--json"]);
    assert!(plain_out.status.success());
    let plain_disk = fs::read_to_string(root.join("Plain.md")).unwrap();
    assert!(plain_disk.contains("tags: [a, b]\n"), "got:\n{plain_disk}");
}

#[test]
fn cli_json_runtime_errors_are_one_stdout_object() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("a/Dup.md"), "# A\n").unwrap();
    fs::write(root.join("b/Dup.md"), "# B\n").unwrap();
    fs::write(root.join("N.md"), "# N\n").unwrap();

    let run_cli = |args: &[&str]| {
        isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap()
    };

    // 1. not_found
    let not_found = run_cli(&["notes", "read", "Nope", "--json"]);
    assert_eq!(not_found.status.code(), Some(1));
    assert!(not_found.stderr.is_empty());
    let nf_json: Value = serde_json::from_slice(&not_found.stdout).unwrap();
    assert_eq!(nf_json["ok"], false);
    assert_eq!(nf_json["error"]["code"], "not_found");
    assert!(nf_json["error"].get("candidates").is_none());

    // 2. ambiguous with candidates
    let ambig = run_cli(&["notes", "read", "Dup", "--json"]);
    assert_eq!(ambig.status.code(), Some(1));
    assert!(ambig.stderr.is_empty());
    let ambig_json: Value = serde_json::from_slice(&ambig.stdout).unwrap();
    assert_eq!(ambig_json["ok"], false);
    assert_eq!(ambig_json["error"]["code"], "ambiguous");
    assert_eq!(
        ambig_json["error"]["candidates"],
        json!(["a/Dup.md", "b/Dup.md"])
    );

    // 3. confirmation_required
    let del = run_cli(&["notes", "delete", "N", "--json"]);
    assert_eq!(del.status.code(), Some(1));
    assert!(del.stderr.is_empty());
    let del_json: Value = serde_json::from_slice(&del.stdout).unwrap();
    assert_eq!(del_json["ok"], false);
    assert_eq!(del_json["error"]["code"], "confirmation_required");

    // 4. Text mode control unchanged
    let text_err = run_cli(&["notes", "read", "Nope"]);
    assert_eq!(text_err.status.code(), Some(1));
    assert!(text_err.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&text_err.stderr).starts_with("Error: "),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&text_err.stderr)
    );

    // 5. N.md was not deleted
    assert!(root.join("N.md").is_file());
}

#[test]
fn task_filters_and_empty_append_are_rejected_not_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Tasks.md"), "- [ ] open item\n- [x] done item\n").unwrap();
    fs::write(root.join("Orphan.md"), "# Orphan\n").unwrap();

    // 1. MCP list_tasks rejects unknown status instead of silently falling back to all
    let mcp = plan_mcp_server(&root);
    assert!(mcp
        .dispatch_tool("list_tasks", &json!({ "status": "completed" }))
        .is_err());
    let done_only = mcp
        .dispatch_tool("list_tasks", &json!({ "status": "done" }))
        .unwrap();
    let done_tasks = done_only["tasks"].as_array().unwrap();
    assert_eq!(done_tasks.len(), 1);
    assert_eq!(done_tasks[0]["checked"], true);

    // 2. CLI tasks --open --done fails with clap exit 2
    let conflict = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["tasks", "--open", "--done"])
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));

    // 4. CLI notes append "" fails with exit 1 and leaves file untouched
    let before = fs::read(root.join("Orphan.md")).unwrap();
    let empty_append = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "append", "Orphan.md", "", "--json"])
        .output()
        .unwrap();
    assert_eq!(empty_append.status.code(), Some(1));
    assert!(empty_append.stderr.is_empty());
    let err_json: Value = serde_json::from_slice(&empty_append.stdout).unwrap();
    assert_eq!(err_json["ok"], false);
    assert_eq!(err_json["error"]["code"], "invalid_argument");
    assert_eq!(fs::read(root.join("Orphan.md")).unwrap(), before);
}

#[test]
fn search_like_fallback_treats_percent_and_underscore_literally() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("OnlyAXB.md"), "Contains aXb only.\n").unwrap();
    fs::write(root.join("Literal.md"), "Contains a_b and 100%.\n").unwrap();

    let run_search = |query: &str| -> Vec<String> {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(["search", query, "--json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let val: Value = serde_json::from_slice(&out.stdout).unwrap();
        val["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["rel_path"].as_str().unwrap().to_string())
            .collect()
    };

    assert_eq!(run_search("a_b"), vec!["Literal.md".to_string()]);
    assert_eq!(run_search("%"), vec!["Literal.md".to_string()]);
}
