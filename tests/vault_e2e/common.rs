//! Shared test vault fixtures and CLI/MCP test harnesses.

use std::fs;

use obsidian_cli::{
    config::AppConfig,
    mcp::McpServer,
    storage::Storage,
    vault::vault_from_dir,
};
use serde_json::Value;

pub(crate) fn seed_vault(root: &std::path::Path) {
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::create_dir_all(root.join("Daily Notes")).unwrap();
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::create_dir_all(root.join(".obsidian")).unwrap();
    fs::write(
        root.join(".obsidian/daily-notes.json"),
        "{\"folder\":\"Daily Notes\",\"format\":\"YYYY-MM-DD\"}",
    )
    .unwrap();
    // 1x1 valid PNG in Attachments/diagram.png
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60,
        0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0xE2, 0x26, 0x05, 0x9B, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    fs::write(root.join("Attachments/diagram.png"), png_bytes).unwrap();

    fs::write(
        root.join("Projects/Q4 Architecture RFC.md"),
        r#"---
status: active
aliases: [RFC-042]
tags: [architecture, rust]
---

# Q4 Architecture RFC

> [!tip] Fast Index
> Incremental scan with SQLite WAL + FTS5.

Links to [[Storage Engine|SQLite Engine]] and [[Unwritten Spec]].
Inline tag #project/q4 and code `#ignored`.

## Checklist

- [ ] Build Braille knowledge graph
- [x] Wire 18 MCP tools

![[diagram.png|400]]
"#,
    )
    .unwrap();

    fs::write(
        root.join("Storage Engine.md"),
        r#"---
tags: [storage, rust]
---

# Storage Engine

Supports [[RFC-042]] and Korean 검색 메모.

- [ ] Benchmark 10k notes
"#,
    )
    .unwrap();

    fs::write(
        root.join("Orphan Note.md"),
        "# Orphan Note\n\nCompletely isolated note.\n",
    )
    .unwrap();
}

pub(crate) fn isolated_cli(test_root: &std::path::Path, vault: &std::path::Path) -> std::process::Command {
    let mut command = isolated_cli_base(test_root);
    command.arg("--vault").arg(vault);
    command
}

pub(crate) fn isolated_cli_base(test_root: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bitefoundry-obsidian"));
    command
        .env("OBSIDIAN_CONFIG_DIR", test_root.join("config"))
        .env("OBSIDIAN_DATA_DIR", test_root.join("data"))
        .env("OBSIDIAN_APP_CONFIG_PATH", test_root.join("obsidian.json"));
    command
}


pub(crate) fn plan_mcp_server(root: &std::path::Path) -> McpServer {
    let vault = vault_from_dir(root, Some("PlanVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    McpServer::new(cfg, vault, Storage::open_in_memory().unwrap())
}

pub(crate) fn plan_json_keys(v: &Value) -> std::collections::BTreeSet<String> {
    v.as_object()
        .expect("expected JSON object")
        .keys()
        .cloned()
        .collect()
}

pub(crate) fn plan_snapshot_dir(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut map = std::collections::BTreeMap::new();
    walk(root, root, &mut map);
    map
}

pub(crate) fn seed_plan_paging_vault(root: &std::path::Path) {
    for d in ["Alpha", "Beta", "Gamma", "x", "y"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    let folders = ["Alpha", "Beta", "Gamma"];
    for i in 0..118usize {
        let folder = folders[i % folders.len()];
        let name = format!("Note-{i:03}.md");
        fs::write(
            root.join(folder).join(name),
            format!("# Note {i:03}\n- [ ] open {i}\n- [x] done {i}\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("x/Same.md"),
        "# Same\n- [ ] open x\n- [x] done x\n",
    )
    .unwrap();
    fs::write(
        root.join("y/same.md"),
        "# same\n- [ ] open y\n- [x] done y\n",
    )
    .unwrap();
}

