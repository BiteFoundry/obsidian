use super::tx::compute_removed_paths;
use super::*;
use std::{collections::HashSet, fs, path::Path, time::Instant};
use rusqlite::params;
use crate::models::TaskFilter;
use crate::vault::fs::file_mtime_and_size;
use crate::vault::vault_from_dir;

#[test]
fn backlinks_keep_first_link_order_and_context_for_matching_titles() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("A")).unwrap();
    fs::create_dir(root.join("B")).unwrap();
    for (path, body) in [
        ("Target.md", "---\naliases: [Destination]\n---\n# Target\n\n[[Target]]\n"),
        ("A/Same.md", "See ![[Destination#First|Destination]].\n\n[[Target#Second]]\n\n[[Target#Second]]\n"),
        ("B/Same.md", "See [[Target#Third|Target]].\n\n![[Destination#Fourth]]\n"),
        ("later.md", "See [[Target]].\n"),
    ] {
        fs::write(root.join(path), body).unwrap();
    }
    let vault = vault_from_dir(root, None);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let links = storage.get_backlinks(&vault.id, "Target.md").unwrap();
    let fields: Vec<_> = links
        .iter()
        .map(|link| {
            (
                link.source_path.as_str(),
                link.source_title.as_str(),
                link.context_snippet.as_str(),
                link.heading.as_deref(),
                link.is_embed,
            )
        })
        .collect();
    assert_eq!(
        fields,
        [
            ("later.md", "later", "See Target.", None, false),
            ("A/Same.md", "Same", "See Destination.", Some("First"), true),
            ("B/Same.md", "Same", "See Target.", Some("Third"), false),
        ]
    );
}

#[test]
fn first_index_clears_fts_once_and_keeps_other_vault_searchable() {
    use rusqlite::ffi;
    use std::{
        cell::Cell,
        ffi::{c_int, c_uint, c_void, CStr},
        ptr,
    };

    unsafe extern "C" fn count_deletes(
        _: c_uint,
        context: *mut c_void,
        statement: *mut c_void,
        _: *mut c_void,
    ) -> c_int {
        // SQLITE_TRACE_STMT supplies a live statement and the registered context.
        unsafe {
            let sql = ffi::sqlite3_sql(statement.cast());
            if !sql.is_null()
                && CStr::from_ptr(sql)
                    .to_bytes()
                    .starts_with(b"DELETE FROM notes_fts WHERE ")
            {
                let deletes = &*context.cast::<Cell<usize>>();
                deletes.set(deletes.get() + 1);
            }
        }
        0
    }

    let first = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let vault = vault_from_dir(first.path(), None);
    let other_vault = vault_from_dir(other.path(), None);
    for i in 0..2048 {
        let body = if i == 0 {
            "Anchor firstword"
        } else {
            "Index filler"
        };
        fs::write(first.path().join(format!("Note{i:04}.md")), body).unwrap();
    }
    fs::write(other.path().join("Note0000.md"), "secondvault").unwrap();

    let deletes = Cell::new(0usize);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&other_vault).unwrap();
    {
        let conn = storage.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO notes_fts (vault_id, rel_path, body_plain) VALUES (?1, 'Orphan.md', 'stalefts')",
            params![vault.id],
        ).unwrap();
        // The callback is used synchronously by this connection, which drops before `deletes`.
        assert_eq!(
            unsafe {
                ffi::sqlite3_trace_v2(
                    conn.handle(),
                    ffi::SQLITE_TRACE_STMT as c_uint,
                    Some(count_deletes),
                    ptr::from_ref(&deletes).cast_mut().cast(),
                )
            },
            ffi::SQLITE_OK
        );
    }
    storage.sync_vault(&vault).unwrap();
    assert_eq!(deletes.get(), 1);

    let fts_count = |vault_id: &str, query: &str| -> usize {
        storage
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE vault_id = ?1 AND notes_fts MATCH ?2",
                params![vault_id, query],
                |r| r.get(0),
            )
            .unwrap()
    };
    let total: usize = storage
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM notes_fts WHERE vault_id = ?1",
            params![vault.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 2048);
    assert_eq!(fts_count(&vault.id, "stalefts"), 0);
    assert_eq!(fts_count(&vault.id, "Anchor"), 1);
    assert_eq!(fts_count(&other_vault.id, "secondvault"), 1);

    let anchor = first.path().join("Note0000.md");
    fs::write(&anchor, "New indexed replacement").unwrap();
    storage.sync_vault(&vault).unwrap();
    assert_eq!(fts_count(&vault.id, "firstword"), 0);
    assert_eq!(fts_count(&vault.id, "replacement"), 1);
    fs::remove_file(anchor).unwrap();
    storage.sync_vault(&vault).unwrap();
    assert_eq!(fts_count(&vault.id, "replacement"), 0);
    assert_eq!(fts_count(&other_vault.id, "secondvault"), 1);
}

#[test]
fn test_incremental_sync_backlinks_graph_and_korean_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::write(
        root.join("Projects/RFC.md"),
        "---\ntags: [architecture, rust]\naliases: [RFC-1]\n---\n\n# Architecture RFC\n\nLinks to [[Storage Engine]] and [[Missing Note]].\n\n- [ ] Ship graph\n",
    )
    .unwrap();
    fs::write(
        root.join("Storage Engine.md"),
        "---\ntags: [storage]\n---\n\n# Storage Engine\n\nBack-referenced by [[RFC-1]]. Contains 회의 notes.\n",
    )
    .unwrap();
    fs::write(
        root.join("Orphan.md"),
        "# Lone Note\n\nNothing links here.\n",
    )
    .unwrap();

    let vault = vault_from_dir(root, Some("TestVault"));
    let storage = Storage::open_in_memory().unwrap();

    let s1 = storage.sync_vault(&vault).unwrap();
    assert_eq!(s1.total_notes, 3);
    assert_eq!(s1.updated, 3);

    // Second sync without disk edits should update 0 files
    let s2 = storage.sync_vault(&vault).unwrap();
    assert_eq!(s2.updated, 0);
    assert_eq!(s2.total_notes, 3);

    // Backlinks via alias (`[[RFC-1]]`)
    let rfc_backlinks = storage.get_backlinks(&vault.id, "Projects/RFC.md").unwrap();
    assert_eq!(rfc_backlinks.len(), 1);
    assert_eq!(rfc_backlinks[0].source_path, "Storage Engine.md");

    // Local graph
    let graph = storage
        .get_graph(&vault, Some("Projects/RFC.md"), 2)
        .unwrap();
    assert!(graph.nodes.len() >= 3); // RFC, Storage Engine, Missing Note (unresolved)

    // Orphans
    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].rel_path, "Orphan.md");

    // Short 2-char Korean LIKE fallback
    let kr_hits = storage.search_notes(&vault.id, "회의", 10).unwrap();
    assert_eq!(kr_hits.len(), 1);
    assert_eq!(kr_hits[0].rel_path, "Storage Engine.md");
}

#[test]
fn test_upsert_single_note_updates_all_tables_preserves_bookmark_and_prevents_redundant_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("Projects")).unwrap();
    let rfc_abs = root.join("Projects/RFC.md");
    fs::write(&rfc_abs, "# Initial Title\n\nOld body.\n").unwrap();
    fs::write(root.join("Target.md"), "# Target\n\nTarget body.\n").unwrap();

    let vault = vault_from_dir(root, Some("FastVault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // Bookmark the note before editing
    assert!(storage
        .toggle_bookmark(&vault.id, "Projects/RFC.md")
        .unwrap());

    // Write updated content to disk and call upsert_single_note (NO sync_vault!)
    let new_md = "# Fresh Heading\n\nUniqueSearchToken links to [[Target]] with #instanttag\n\n- [ ] Instant task\n";
    fs::write(&rfc_abs, new_md).unwrap();
    let (mtime_ms, size_bytes) = file_mtime_and_size(&rfc_abs);

    storage
        .upsert_single_note(&vault, "Projects/RFC.md", new_md, mtime_ms, size_bytes)
        .unwrap();

    // Verify bookmark preserved + summary updated
    let summary = storage
        .get_note_summary(&vault.id, "Projects/RFC.md")
        .unwrap()
        .unwrap();
    assert!(
        summary.is_bookmarked,
        "upsert_single_note must preserve is_bookmarked"
    );
    assert!(summary.tags.iter().any(|t| t == "instanttag"));
    assert_eq!(summary.mtime_ms, mtime_ms);
    assert_eq!(summary.size_bytes, size_bytes);

    // Verify FTS5, headings, tags, tasks, and resolved links all updated
    let hits = storage
        .search_notes(&vault.id, "UniqueSearchToken", 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rel_path, "Projects/RFC.md");

    let headings = storage.get_headings(&vault.id, "Projects/RFC.md").unwrap();
    assert_eq!(headings.len(), 1);
    assert_eq!(headings[0].text, "Fresh Heading");

    let tasks = storage
        .list_tasks(&vault.id, TaskFilter::Open, Some("Projects/RFC.md"))
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].text, "Instant task");

    let outgoing = storage
        .get_outgoing_links(&vault, "Projects/RFC.md")
        .unwrap();
    assert_eq!(outgoing.len(), 1);
    assert!(outgoing[0].resolved);
    assert_eq!(
        outgoing[0].target_resolved_path.as_deref(),
        Some("Target.md")
    );

    // Verify subsequent sync_vault sees 0 changed files (suppressing redundant watcher scans!)
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.updated, 0);
    assert_eq!(stats.removed, 0);
}

fn obs10_vault(
    notes: &[(&str, &str)],
) -> (tempfile::TempDir, crate::config::VaultProfile, Storage) {
    let tmp = tempfile::tempdir().unwrap();
    for (path, body) in notes {
        fs::write(tmp.path().join(path), body).unwrap();
    }
    let vault = vault_from_dir(tmp.path(), Some("obs10"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    (tmp, vault, storage)
}

const OBS10_KOREAN: &[(&str, &str)] = &[
    ("a-minutes.md", "회의록 작성\n"),
    ("b-weekly.md", "주간회의록\n"),
    ("c-team.md", "팀회의록\n"),
    ("d-other.md", "관계없는 메모\n"),
];

fn obs10_paths(hits: &[crate::models::SearchHit]) -> Vec<&str> {
    hits.iter().map(|h| h.rel_path.as_str()).collect()
}

#[test]
fn obs10_korean_three_char_query_recalls_3_of_3() {
    let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
    for query in ["회의록", "회의"] {
        let hits = storage.search_notes(&vault.id, query, 25).unwrap();
        let mut paths = obs10_paths(&hits);
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["a-minutes.md", "b-weekly.md", "c-team.md"],
            "{query}"
        );
    }
}

#[test]
fn obs10_cjk_merge_reports_has_more() {
    let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
    let page = storage.search_notes_page(&vault.id, "회의록", 2).unwrap();
    assert_eq!(page.hits.len(), 2);
    assert!(page.has_more);
    let page = storage.search_notes_page(&vault.id, "회의록", 3).unwrap();
    assert_eq!(page.hits.len(), 3);
    assert!(!page.has_more);
    // The limit bounds the merged list, never more than asked for.
    assert_eq!(
        storage.search_notes(&vault.id, "회의록", 1).unwrap().len(),
        1
    );
}

#[test]
fn obs10_cjk_merge_keeps_fts_hits_first() {
    let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
    let hits = storage.search_notes(&vault.id, "회의록", 25).unwrap();
    assert_eq!(hits[0].rel_path, "a-minutes.md");
    let limited = storage.search_notes(&vault.id, "회의록", 1).unwrap();
    assert_eq!(obs10_paths(&limited), ["a-minutes.md"]);
}

#[test]
fn obs10_japanese_and_han_substring() {
    let (_tmp, vault, storage) = obs10_vault(&[
        ("weekly.md", "週次議事録\n"),
        ("kana.md", "まいにちのメモ帳です\n"),
        ("other.md", "unrelated\n"),
    ]);
    let hits = storage.search_notes(&vault.id, "議事録", 25).unwrap();
    assert_eq!(obs10_paths(&hits), ["weekly.md"]);
    let hits = storage.search_notes(&vault.id, "メモ帳", 25).unwrap();
    assert_eq!(obs10_paths(&hits), ["kana.md"]);
    // LIKE wildcards in a CJK query stay literal.
    assert!(storage
        .search_notes(&vault.id, "議%録", 25)
        .unwrap()
        .is_empty());
}

#[test]
fn obs10_ascii_query_path_unchanged() {
    let (_tmp, vault, storage) = obs10_vault(&[
        ("first.md", "meeting notes\n"),
        ("second.md", "premeeting\n"),
        ("third.md", "ÜBER uns\n"),
    ]);
    // The FTS hit suppresses the substring scan for non-CJK queries, as before.
    let hits = storage.search_notes(&vault.id, "meeting", 25).unwrap();
    assert_eq!(obs10_paths(&hits), ["first.md"]);
    let hits = storage.search_notes(&vault.id, "über", 25).unwrap();
    assert_eq!(obs10_paths(&hits), ["third.md"]);
    assert!(!should_use_short_cjk_like("meeting"));
    assert!(should_use_short_cjk_like("회의"));
    assert!(!should_use_short_cjk_like("회의록"));
}

#[test]
fn obs05_compute_removed_paths_protects_unreadable_prefixes() {
    let existing: Vec<String> = ["A/x.md", "A/B/y.md", "AB/x.md", "C/z.md", "r.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let disk: HashSet<String> = ["r.md".to_string()].into_iter().collect();
    let removed = |unreadable: &[&str]| {
        let dirs: Vec<String> = unreadable.iter().map(|s| s.to_string()).collect();
        compute_removed_paths(existing.iter(), &disk, &dirs)
    };

    // Nothing unreadable: every indexed path missing from disk is removed.
    assert_eq!(removed(&[]), ["A/B/y.md", "A/x.md", "AB/x.md", "C/z.md"]);
    // `A` protects `A/...` at any depth, but not the sibling `AB`.
    assert_eq!(removed(&["A"]), ["AB/x.md", "C/z.md"]);
    assert_eq!(removed(&["A/B"]), ["A/x.md", "AB/x.md", "C/z.md"]);
    // An unreadable vault root protects everything.
    assert!(removed(&[""]).is_empty());
    if cfg!(any(windows, target_os = "macos")) {
        assert_eq!(removed(&["a"]), ["AB/x.md", "C/z.md"]);
    } else {
        assert_eq!(removed(&["a"]), ["A/B/y.md", "A/x.md", "AB/x.md", "C/z.md"]);
    }
}

fn obs05_locked_vault() -> (tempfile::TempDir, crate::config::VaultProfile, Storage) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("Locked/Deep")).unwrap();
    fs::write(root.join("Locked/n.md"), "# n\n\n[[Open]]\n").unwrap();
    fs::write(root.join("Locked/Deep/d.md"), "# d\n").unwrap();
    fs::write(root.join("Open.md"), "# Open\n").unwrap();
    let vault = vault_from_dir(root, Some("obs05"));
    let storage = Storage::open_in_memory().unwrap();
    let first = storage.sync_vault(&vault).unwrap();
    assert_eq!(first.total_notes, 3);
    assert_eq!(first.unreadable_dirs, 0);
    assert!(!serde_json::to_string(&first)
        .unwrap()
        .contains("unreadable_dirs"));
    assert!(storage.toggle_bookmark(&vault.id, "Locked/n.md").unwrap());
    (tmp, vault, storage)
}

fn obs05_assert_rows_survive_unreadable_dir(
    vault: &crate::config::VaultProfile,
    storage: &Storage,
) {
    let stats = storage.sync_vault(vault).unwrap();
    assert_eq!(stats.unreadable_dirs, 1);
    assert_eq!(stats.removed, 0);
    assert_eq!(stats.total_notes, 3);
    assert!(serde_json::to_string(&stats)
        .unwrap()
        .contains("\"unreadable_dirs\":1"));
    let notes = storage.list_notes(&vault.id, None).unwrap();
    let locked = notes
        .iter()
        .find(|n| n.rel_path == "Locked/n.md")
        .expect("row under the unreadable directory is kept");
    assert!(locked.is_bookmarked);
    assert!(notes.iter().any(|n| n.rel_path == "Locked/Deep/d.md"));
    // The explicit reindex honours the same protection.
    let reindexed = storage.reindex_vault(vault).unwrap();
    assert_eq!(reindexed.removed, 0);
    assert_eq!(reindexed.total_notes, 3);
}

#[cfg(unix)]
#[test]
fn obs05_permission_denied_dir_keeps_rows_and_bookmark() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let (tmp, vault, storage) = obs05_locked_vault();
    let locked = tmp.path().join("Locked");
    let _restore = Restore(locked.clone());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        eprintln!(
            "obs05_permission_denied_dir_keeps_rows_and_bookmark: skipped (running as root)"
        );
        return;
    }

    obs05_assert_rows_survive_unreadable_dir(&vault, &storage);

    // Once the directory is readable again, a real deletion is reconciled.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(locked.join("n.md")).unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.unreadable_dirs, 0);
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total_notes, 2);
}

#[cfg(windows)]
#[test]
fn obs05_access_denied_dir_keeps_rows_and_bookmark() {
    // Deny "list folder" to Everyone (S-1-1-0) on the directory itself.
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::process::Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", "*S-1-1-0"])
                .output();
        }
    }

    let (tmp, vault, storage) = obs05_locked_vault();
    let locked = tmp.path().join("Locked");
    let _restore = Restore(locked.clone());
    let denied = std::process::Command::new("icacls")
        .arg(&locked)
        .args(["/deny", "*S-1-1-0:(RD)"])
        .output();
    if !denied.is_ok_and(|out| out.status.success()) || fs::read_dir(&locked).is_ok() {
        eprintln!("obs05_access_denied_dir_keeps_rows_and_bookmark: skipped (cannot deny directory listing here)");
        return;
    }

    obs05_assert_rows_survive_unreadable_dir(&vault, &storage);

    // Once the directory is readable again, a real deletion is reconciled.
    drop(_restore);
    fs::remove_file(locked.join("n.md")).unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.unreadable_dirs, 0);
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total_notes, 2);
}

#[test]
fn obs05_deleted_dir_is_still_pruned() {
    let (tmp, vault, storage) = obs05_locked_vault();
    fs::remove_dir_all(tmp.path().join("Locked")).unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.unreadable_dirs, 0);
    assert_eq!(stats.removed, 2);
    assert_eq!(stats.total_notes, 1);
    let notes = storage.list_notes(&vault.id, None).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].rel_path, "Open.md");
}

fn obs12_link_snapshot(
    storage: &Storage,
    vault_id: &str,
) -> Vec<(String, String, Option<String>)> {
    let conn = storage.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT source_path, target_raw, target_resolved_path FROM links
             WHERE vault_id = ?1 ORDER BY source_path, target_raw, id",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![vault_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn obs12_incremental_matches_full_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for dir in ["A", "A/Deep", "B", "C"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let write = |rel: &str, body: &str| {
        fs::write(root.join(rel), body).unwrap();
    };
    // 48 filler notes spread over four locations, each linking to neighbours in every style.
    let folders = ["", "A/", "B/", "C/"];
    for i in 0..48usize {
        let folder = folders[i % 4];
        let next = (i + 1) % 48;
        let far = (i + 7) % 48;
        let next_folder = folders[next % 4];
        let mut body = String::new();
        if i % 6 == 0 {
            body.push_str(&format!("---\naliases: [alias-{i:02}]\n---\n"));
        }
        body.push_str(&format!("# N{i:02}\n\n"));
        body.push_str(&format!(
            "Wikilink [[N{next:02}]] and [[{next_folder}N{next:02}|shown]].\n"
        ));
        body.push_str(&format!(
            "Alias [[alias-{:02}]] and far [[N{far:02}#Heading]].\n",
            (i / 6) * 6
        ));
        body.push_str(&format!(
            "Markdown [rooted](/{next_folder}N{next:02}.md) and [bare](N{far:02}.md).\n"
        ));
        body.push_str(
            "Shared [[Dup]], [[Solo]], [[Foo]], [[Ghost]], [[Renamed]] and [[Deep/Solo]].\n",
        );
        write(&format!("{folder}N{i:02}.md"), &body);
    }
    write("A/Dup.md", "# Dup A\n");
    write("B/Dup.md", "# Dup B\n\n[[Dup]] and [[A/Dup]].\n");
    write("A/Deep/Solo.md", "# Solo\n\n[up](../../N00.md) [[N01]]\n");
    write(
        "C/Holder.md",
        "---\naliases: [Foo, Bar]\n---\n# Holder\n\n[[Bar]]\n",
    );
    write("Renamed.md", "# Renamed\n\n[[Holder]]\n");

    let vault = vault_from_dir(root, Some("obs12"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // Each step changes one or two files (well under a quarter of the vault), syncs
    // incrementally, then compares against the full re-resolution of a reindex.
    type Step<'a> = (&'a str, Box<dyn Fn() + 'a>);
    let steps: Vec<Step> = vec![
        (
            "edit a body",
            Box::new(|| {
                write(
                    "N05.md",
                    "# N05\n\n[[N06]] [[Ghost]] [[Dup]] [new](B/N02.md)\n",
                )
            }),
        ),
        (
            "add a note that resolves a dangling link",
            Box::new(|| write("B/Ghost.md", "# Ghost\n")),
        ),
        (
            "add a shorter path for an existing stem",
            Box::new(|| write("Solo.md", "# Solo root\n")),
        ),
        (
            "add Foo.md while [[Foo]] resolves through an alias",
            Box::new(|| write("Foo.md", "# Foo\n")),
        ),
        (
            "add a second holder of an alias",
            Box::new(|| {
                write(
                    "B/Second.md",
                    "---\naliases: [alias-06, Bar]\n---\n# Second\n",
                )
            }),
        ),
        (
            "remove an alias",
            Box::new(|| {
                write(
                    "C/Holder.md",
                    "---\naliases: [Bar]\n---\n# Holder\n\n[[Bar]]\n",
                )
            }),
        ),
        (
            "give an existing note a colliding alias",
            Box::new(|| {
                write(
                    "N10.md",
                    "---\naliases: [alias-12, N11]\n---\n# N10\n\n[[N11]] [[alias-12]]\n",
                )
            }),
        ),
        (
            "delete a note",
            Box::new(|| fs::remove_file(root.join("Foo.md")).unwrap()),
        ),
        (
            "delete one of two same-stem notes",
            Box::new(|| fs::remove_file(root.join("A/Dup.md")).unwrap()),
        ),
        (
            "rename via delete plus create",
            Box::new(|| {
                fs::remove_file(root.join("Renamed.md")).unwrap();
                write("C/Renamed.md", "# Renamed\n\n[[Holder]] [[Solo]]\n");
            }),
        ),
        (
            "add a same-folder note for a bare link",
            Box::new(|| write("A/Dup.md", "# Dup A again\n")),
        ),
        (
            "restore the removed alias",
            Box::new(|| {
                write(
                    "C/Holder.md",
                    "---\naliases: [Foo, Bar]\n---\n# Holder\n\n[[Bar]] [[Second]]\n",
                )
            }),
        ),
        (
            "delete the second alias holder",
            Box::new(|| fs::remove_file(root.join("B/Second.md")).unwrap()),
        ),
    ];
    for (label, apply) in &steps {
        apply();
        let stats = storage.sync_vault(&vault).unwrap();
        assert!(
            stats.updated + stats.removed >= 1,
            "{label}: change not detected"
        );
        assert!(
            (stats.updated + stats.removed) * 4 <= stats.total_notes,
            "{label}: step must stay on the incremental route"
        );
        let incremental = obs12_link_snapshot(&storage, &vault.id);
        storage.reindex_vault(&vault).unwrap();
        let full = obs12_link_snapshot(&storage, &vault.id);
        assert!(!full.is_empty());
        let diff: Vec<_> = incremental
            .iter()
            .zip(full.iter())
            .filter(|(a, b)| a != b)
            .take(5)
            .collect();
        assert!(
            incremental.len() == full.len() && diff.is_empty(),
            "{label}: incremental resolution differs from full: {diff:?}"
        );
    }
}

fn obs12_seed_ring(root: &Path, notes: usize, links: usize) {
    for i in 0..notes {
        let mut body = format!("# Ring{i:05}\n\n");
        for k in 1..=links {
            body.push_str(&format!("[[Ring{:05}]]\n", (i + k) % notes));
        }
        fs::write(root.join(format!("Ring{i:05}.md")), body).unwrap();
    }
}

#[test]
fn obs12_single_edit_resolves_only_its_links() {
    use rusqlite::ffi;
    use std::{
        cell::Cell,
        ffi::{c_int, c_uint, c_void, CStr},
        ptr,
    };

    unsafe extern "C" fn count_link_updates(
        _: c_uint,
        context: *mut c_void,
        statement: *mut c_void,
        _: *mut c_void,
    ) -> c_int {
        // SQLITE_TRACE_STMT supplies a live statement and the registered context.
        unsafe {
            let sql = ffi::sqlite3_sql(statement.cast());
            if !sql.is_null()
                && CStr::from_ptr(sql)
                    .to_bytes()
                    .starts_with(b"UPDATE links SET target_resolved_path")
            {
                let updates = &*context.cast::<Cell<usize>>();
                updates.set(updates.get() + 1);
            }
        }
        0
    }

    const NOTES: usize = 2000;
    const LINKS: usize = 10;
    let tmp = tempfile::tempdir().unwrap();
    obs12_seed_ring(tmp.path(), NOTES, LINKS);
    let vault = vault_from_dir(tmp.path(), Some("obs12-ring"));
    let updates = Cell::new(0usize);
    let storage = Storage::open_in_memory().unwrap();
    let first = storage.sync_vault(&vault).unwrap();
    assert_eq!(first.total_notes, NOTES);
    assert_eq!(first.total_links, NOTES * LINKS);
    {
        let conn = storage.conn.lock().unwrap();
        // The callback is used synchronously by this connection, which drops before `updates`.
        assert_eq!(
            unsafe {
                ffi::sqlite3_trace_v2(
                    conn.handle(),
                    ffi::SQLITE_TRACE_STMT as c_uint,
                    Some(count_link_updates),
                    ptr::from_ref(&updates).cast_mut().cast(),
                )
            },
            ffi::SQLITE_OK
        );
    }

    // Edit one note without touching its aliases or its links.
    let edited = tmp.path().join("Ring00000.md");
    let mut body = fs::read_to_string(&edited).unwrap();
    body.push_str("\nOne more paragraph.\n");
    fs::write(&edited, body).unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.updated, 1);
    assert!(
        updates.get() >= 1 && updates.get() <= LINKS,
        "one edit re-resolved {} links (vault has {})",
        updates.get(),
        NOTES * LINKS
    );
    let unresolved: usize = storage
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM links WHERE vault_id = ?1 AND target_resolved_path IS NULL",
            params![vault.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unresolved, 0);

    // The explicit reindex stays the full repair path.
    updates.set(0);
    storage.reindex_vault(&vault).unwrap();
    assert_eq!(updates.get(), NOTES * LINKS);
}

/// Run by hand: `cargo test --release obs12_large -- --ignored --nocapture`.
#[test]
#[ignore]
fn obs12_large_vault_edit_sync_wall_clock() {
    let tmp = tempfile::tempdir().unwrap();
    obs12_seed_ring(tmp.path(), 4000, 10);
    let vault = vault_from_dir(tmp.path(), Some("obs12-large"));
    let storage = Storage::open_in_memory().unwrap();

    let started = Instant::now();
    storage.sync_vault(&vault).unwrap();
    let first_index = started.elapsed();

    let started = Instant::now();
    storage.sync_vault(&vault).unwrap();
    let no_change = started.elapsed();

    let edited = tmp.path().join("Ring00000.md");
    let mut body = fs::read_to_string(&edited).unwrap();
    body.push_str("\nOne more paragraph.\n");
    fs::write(&edited, body).unwrap();
    let started = Instant::now();
    let stats = storage.sync_vault(&vault).unwrap();
    let one_edit = started.elapsed();
    assert_eq!(stats.updated, 1);

    eprintln!(
        "obs12 4000x10: first index {first_index:?}, no-change sync {no_change:?}, one-edit sync {one_edit:?}"
    );
    assert!(
        one_edit < no_change * 5 + std::time::Duration::from_millis(250),
        "one-edit sync {one_edit:?} vs no-change sync {no_change:?}"
    );
}

#[test]
fn upsert_single_note_reresolves_wikilinks_on_new_and_changed_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = vault_from_dir(tmp.path(), Some("AliasUpsertVault"));
    let storage = Storage::open_in_memory().unwrap();

    let source_md = "# Source\n\nLinks to [[CodenameAlpha]] and [[CodenameBeta]].\n";
    let target_a_md = "---\naliases: [CodenameAlpha]\n---\n\n# TargetA\n";
    fs::write(tmp.path().join("Source.md"), source_md).unwrap();
    fs::write(tmp.path().join("TargetA.md"), target_a_md).unwrap();
    storage.sync_vault(&vault).unwrap();

    let links0 = storage.get_outgoing_links(&vault, "Source.md").unwrap();
    assert_eq!(links0[0].target_resolved_path.as_deref(), Some("TargetA.md"));
    assert_eq!(links0[1].target_resolved_path, None);

    // 1. New note created with alias `CodenameBeta` AND conflicting alias `CodenameAlpha`:
    //    `upsert_single_note` must add aliases to `stem_candidates` so [[CodenameBeta]] resolves
    //    and [[CodenameAlpha]] immediately becomes ambiguous (unresolved) without a full reindex!
    let target_b_md = "---\naliases: [CodenameAlpha, CodenameBeta]\n---\n\n# TargetB\n";
    fs::write(tmp.path().join("TargetB.md"), target_b_md).unwrap();
    let (mt_b, sz_b) = file_mtime_and_size(&tmp.path().join("TargetB.md"));
    storage
        .upsert_single_note(&vault, "TargetB.md", target_b_md, mt_b, sz_b)
        .unwrap();

    let links1 = storage.get_outgoing_links(&vault, "Source.md").unwrap();
    assert_eq!(
        links1[0].target_resolved_path, None,
        "duplicate alias on new note must make [[CodenameAlpha]] ambiguous immediately"
    );
    assert_eq!(
        links1[1].target_resolved_path.as_deref(),
        Some("TargetB.md"),
        "new note alias must resolve [[CodenameBeta]] immediately"
    );

    // 2. Updating TargetB to drop `CodenameAlpha` must re-resolve [[CodenameAlpha]] back to TargetA!
    let target_b_updated = "---\naliases: [CodenameBeta]\n---\n\n# TargetB\n";
    fs::write(tmp.path().join("TargetB.md"), target_b_updated).unwrap();
    let (mt_b2, sz_b2) = file_mtime_and_size(&tmp.path().join("TargetB.md"));
    storage
        .upsert_single_note(&vault, "TargetB.md", target_b_updated, mt_b2, sz_b2)
        .unwrap();

    let links2 = storage.get_outgoing_links(&vault, "Source.md").unwrap();
    assert_eq!(
        links2[0].target_resolved_path.as_deref(),
        Some("TargetA.md"),
        "removing conflicting alias via upsert_single_note must re-resolve [[CodenameAlpha]] to TargetA"
    );
    assert_eq!(
        links2[1].target_resolved_path.as_deref(),
        Some("TargetB.md")
    );
}

#[test]
fn test_replace_and_move_update_only_affected_paths_without_full_vault_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("Alpha.md"), "# Alpha\n\nOLD_TOKEN\n").unwrap();
    fs::write(root.join("Referrer.md"), "# Referrer\n\nSee [[Alpha]].\n").unwrap();
    fs::write(root.join("Untouched.md"), "# Untouched\n\nSteady.\n").unwrap();

    let vault = vault_from_dir(root, Some("PathScopedWriteVault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // Mark Untouched.md stale in SQLite: a full sync_vault would overwrite mtime_ms != -1,
    // while path-scoped sync_paths leaves Untouched.md's row untouched.
    storage
        .invalidate_note_mtime(&vault.id, "Untouched.md")
        .unwrap();
    assert!(storage.toggle_bookmark(&vault.id, "Alpha.md").unwrap());

    let rep = storage
        .replace_note_text(&vault, "Alpha.md", "OLD_TOKEN", "NEW_TOKEN")
        .unwrap();
    assert!(rep.replaced);
    assert_eq!(
        storage
            .search_notes(&vault.id, "NEW_TOKEN", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        storage
            .get_note_summary(&vault.id, "Untouched.md")
            .unwrap()
            .unwrap()
            .mtime_ms,
        -1,
        "replace_note_text must use path-scoped sync_paths, not a full sync_vault"
    );

    let moved = storage
        .move_note_with_links(&vault, "Alpha.md", "Renamed/Beta.md")
        .unwrap();
    assert!(moved.moved);
    assert!(moved.bookmark_preserved);
    assert_eq!(moved.updated_links_in_notes, 1);
    assert!(
        storage
            .get_note_summary(&vault.id, "Renamed/Beta.md")
            .unwrap()
            .unwrap()
            .is_bookmarked
    );
    let bl = storage.get_backlinks(&vault.id, "Renamed/Beta.md").unwrap();
    assert_eq!(bl.len(), 1);
    assert_eq!(bl[0].source_path, "Referrer.md");
    assert_eq!(
        storage
            .get_note_summary(&vault.id, "Untouched.md")
            .unwrap()
            .unwrap()
            .mtime_ms,
        -1,
        "move_note_with_links must sync only moved and rewritten paths without a second full vault scan"
    );
}

#[test]
fn test_indexed_link_resolution_and_noop_sync_fast_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("Notes")).unwrap();
    for i in 0..150 {
        let next = (i + 1) % 150;
        fs::write(
            root.join(format!("Notes/N{i:03}.md")),
            format!(
                "---\naliases: [Alias{i:03}]\n---\n# N{i:03}\n\nNext [[Alias{next:03}]] and [md](./N{next:03}.md).\n"
            ),
        )
        .unwrap();
    }
    let vault = vault_from_dir(root, Some("BulkIndexVault"));
    let storage = Storage::open_in_memory().unwrap();

    let s1 = storage.sync_vault(&vault).unwrap();
    assert_eq!(s1.total_notes, 150);
    assert_eq!(s1.updated, 150);
    assert_eq!(s1.total_links, 300);

    // Every wikilink and local markdown link resolved accurately via VaultLinkIndex.
    let out = storage.get_outgoing_links(&vault, "Notes/N000.md").unwrap();
    assert_eq!(out.len(), 2);
    assert!(out
        .iter()
        .all(|l| l.target_resolved_path.as_deref() == Some("Notes/N001.md")));
    let bl = storage.get_backlinks(&vault.id, "Notes/N001.md").unwrap();
    assert_eq!(bl.len(), 1);
    assert_eq!(bl[0].source_path, "Notes/N000.md");

    // Second sync with zero disk changes takes the no-op fast path.
    let s2 = storage.sync_vault(&vault).unwrap();
    assert_eq!(s2.updated, 0);
    assert_eq!(s2.removed, 0);
    assert_eq!(s2.total_notes, 150);
}
