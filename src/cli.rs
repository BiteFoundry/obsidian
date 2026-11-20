//! Clap CLI definition & command executor for `obsidian` (`obs`).

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::{
    config::{AppConfig, DEFAULT_SERVE_PORT},
    daemon,
    mcp::McpServer,
    models::{TaskFilter, VaultSummary},
    serve,
    storage::Storage,
    tui,
    updater,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, move_note, move_note_to_trash,
            rewrite_wikilinks_for_move, safe_vault_path, toggle_task_at_line_verified,
        },
        init_starter_vault, merged_vaults, native_obsidian_config_path, resolve_active_vault,
        vault_from_dir,
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "obsidian",
    version,
    about = "Obsidian-native terminal workspace, interactive knowledge graph & AI agent toolkit (CLI · TUI · HTTP · MCP)"
)]
pub struct Cli {
    /// Emit machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Target vault name, ID, or filesystem path (overrides active vault & OBSIDIAN_VAULT).
    #[arg(long, global = true)]
    pub vault: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage auto-discovered & manually registered local Obsidian vaults.
    Vaults {
        #[command(subcommand)]
        action: VaultsCmd,
    },
    /// Scaffold a starter local Obsidian vault with Welcome note and Daily Notes folder.
    Init {
        /// Directory path for the new vault.
        dir: PathBuf,
    },
    /// Incrementally scan the active vault and rebuild SQLite WAL + FTS5 + graph index.
    Reindex,
    /// Full-text search notes via SQLite FTS5 (+ Korean/CJK <3 char LIKE fallback).
    Search {
        /// Search query string.
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read, list, create, append, edit, move, or safely trash notes.
    Notes {
        #[command(subcommand)]
        action: NotesCmd,
    },
    /// Open or create today's Daily Note (optionally appending a line/task).
    Daily {
        /// Optional date override (e.g. 2026-11-20).
        #[arg(long)]
        date: Option<String>,
        /// Optional text to append to the daily note.
        #[arg(long)]
        append: Option<String>,
    },
    /// List incoming linked mentions (backlinks) to a note.
    Backlinks {
        /// Note relative path, title, or alias.
        note: String,
    },
    /// List outgoing wikilinks and embeds from a note.
    Links {
        /// Note relative path, title, or alias.
        note: String,
    },
    /// Print local (around a note) or global vault knowledge graph nodes and edges.
    Graph {
        /// Optional center note (omitting outputs global vault graph).
        note: Option<String>,
        /// Hop depth for local BFS neighborhood (1..5).
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// Find orphan notes with zero backlinks and zero outgoing links.
    Orphans,
    /// List all tags across the vault, or list notes matching a specific tag.
    Tags {
        /// Optional tag filter (e.g. architecture or #architecture).
        tag: Option<String>,
    },
    /// List Markdown checklist tasks across the vault or inside a single note.
    Tasks {
        /// Show only open (`- [ ]`) tasks.
        #[arg(long)]
        open: bool,
        /// Show only completed (`- [x]`) tasks.
        #[arg(long)]
        done: bool,
        /// Filter tasks to a specific note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Surgically toggle a Markdown task checkbox (`- [ ]` <-> `- [x]`) at a 1-based line number.
    ToggleTask {
        /// Note relative path, title, or alias.
        note: String,
        /// 1-based line number of the task checkbox.
        line: usize,
        /// Optional expected task text at `line`; refuses to toggle if external edits shifted the line.
        #[arg(long)]
        expected_text: Option<String>,
        /// Optional expected current checked state (`true`/`false`); refuses to toggle on mismatch.
        #[arg(long)]
        expected_checked: Option<bool>,
    },
    /// Control the lightweight background vault watcher & IPC daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Start the local HTTP JSON API server (default: 127.0.0.1:8792).
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
    },
    /// Start the stdio JSON-RPC 2.0 MCP server (18 typed tools).
    Mcp,
    /// Inspect environment, auto-discovered Obsidian vaults, SQLite index, and terminal capabilities.
    Doctor,
    /// Check for or apply a verified GitHub Release binary update.
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum VaultsCmd {
    /// List all auto-discovered and registered Obsidian vaults.
    List,
    /// Register an existing local directory as an Obsidian vault.
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Set the default active vault by name or ID.
    Use { name: String },
    /// Remove a manually registered vault from config.toml (does not delete files on disk).
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
pub enum NotesCmd {
    /// List notes in the active vault.
    List {
        #[arg(long)]
        folder: Option<String>,
    },
    /// Read a note's Markdown content and metadata.
    Read { note: String },
    /// Create a new note atomically.
    Create {
        title: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Append text/task/callout to an existing note.
    Append { note: String, text: String },
    /// Open a note in `$EDITOR` and reindex on exit.
    Edit { note: String },
    /// Move/rename a note within the vault.
    Move { src: String, dst: String },
    /// Move a note safely to `.trash/` (requires `--yes`).
    Delete {
        note: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    Start,
    Stop,
    Status,
    #[command(hide = true)]
    Run,
}

pub async fn run(cli: Cli) -> Result<()> {
    updater::cleanup_stale_old_binary();
    let mut cfg = AppConfig::load();

    match cli.command {
        None => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            tui::run_tui(cfg, vault, storage).await
        }

        Some(Commands::Vaults { action }) => match action {
            VaultsCmd::List => {
                let storage = Storage::open_default()?;
                let active_id = resolve_active_vault(&cfg, cli.vault.as_deref())
                    .ok()
                    .map(|v| v.id);
                let vaults: Vec<VaultSummary> = merged_vaults(&cfg)
                    .into_iter()
                    .map(|v| {
                        let exists = v.path.is_dir();
                        if exists {
                            let _ = storage.sync_vault(&v);
                        }
                        let active = active_id.as_deref() == Some(v.id.as_str());
                        let note_count = storage.note_count(&v.id).unwrap_or(0);
                        VaultSummary {
                            id: v.id,
                            name: v.name,
                            exists,
                            path: v.path,
                            active,
                            discovered: v.discovered,
                            open_in_obsidian: v.open_in_obsidian,
                            note_count,
                        }
                    })
                    .collect();
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&vaults)?);
                } else if vaults.is_empty() {
                    println!("No vaults found. Run `obsidian init <dir>` or `obsidian vaults add <path>`.");
                } else {
                    for v in vaults {
                        let mark = if v.active { "*" } else { " " };
                        let src = if v.discovered { "auto" } else { "reg " };
                        println!(
                            "{mark} {:<22} [{src}] {:>4} notes  {}",
                            v.name,
                            v.note_count,
                            v.path.display()
                        );
                    }
                }
                Ok(())
            }
            VaultsCmd::Add { path, name } => {
                if !path.is_dir() {
                    bail!("Directory does not exist: {}", path.display());
                }
                let profile = vault_from_dir(&path, name.as_deref());
                cfg.upsert_vault(profile.clone(), true);
                cfg.save()?;
                let storage = Storage::open_default()?;
                let stats = storage.sync_vault(&profile)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "vault": profile,
                            "stats": stats,
                        }))?
                    );
                } else {
                    println!(
                        "Added vault '{}' ({}) — indexed {} notes.",
                        profile.name,
                        profile.path.display(),
                        stats.total_notes
                    );
                }
                Ok(())
            }
            VaultsCmd::Use { name } => {
                let target = resolve_active_vault(&cfg, Some(&name))?;
                cfg.active_vault = target.name.clone();
                cfg.save()?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&target)?);
                } else {
                    println!("Active vault set to '{}' ({})", target.name, target.path.display());
                }
                Ok(())
            }
            VaultsCmd::Remove { name } => {
                let removed = cfg.remove_vault(&name);
                cfg.save()?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&json!({ "removed": removed, "name": name }))?);
                } else if removed {
                    println!("Removed vault '{name}' from registry.");
                } else {
                    println!("Vault '{name}' was not in manual registry.");
                }
                Ok(())
            }
        },

        Some(Commands::Init { dir }) => {
            let profile = init_starter_vault(&dir)?;
            cfg.upsert_vault(profile.clone(), true);
            cfg.save()?;
            let storage = Storage::open_default()?;
            let stats = storage.sync_vault(&profile)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "vault": profile,
                        "stats": stats,
                    }))?
                );
            } else {
                println!(
                    "Initialized starter Obsidian vault '{}' at {} ({} notes).",
                    profile.name,
                    profile.path.display(),
                    stats.total_notes
                );
            }
            Ok(())
        }

        Some(Commands::Reindex) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let stats = storage.reindex_vault(&vault)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!(
                    "Reindexed '{}' in {}ms: {} notes ({} updated, {} removed), {} links, {} tags, {} tasks.",
                    stats.vault_name,
                    stats.elapsed_ms,
                    stats.total_notes,
                    stats.updated,
                    stats.removed,
                    stats.total_links,
                    stats.total_tags,
                    stats.total_tasks,
                );
            }
            Ok(())
        }

        Some(Commands::Search { query, limit }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let hits = storage.search_notes(&vault.id, &query, limit)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("No notes matching '{query}'.");
            } else {
                for h in hits {
                    println!("{} — {}", h.rel_path, h.snippet);
                }
            }
            Ok(())
        }

        Some(Commands::Notes { action }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;

            match action {
                NotesCmd::List { folder } => {
                    let notes = storage.list_notes(&vault.id, folder.as_deref())?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&notes)?);
                    } else {
                        for n in notes {
                            let bm = if n.is_bookmarked { "★ " } else { "  " };
                            println!(
                                "{bm}{:<42} {:>4}w  ←{} →{}",
                                n.rel_path, n.word_count, n.backlink_count, n.outgoing_count
                            );
                        }
                    }
                    Ok(())
                }
                NotesCmd::Read { note } => {
                    let detail = storage.load_note_detail(&vault, &note)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&detail)?);
                    } else {
                        print!("{}", detail.raw_markdown);
                    }
                    Ok(())
                }
                NotesCmd::Create {
                    title,
                    folder,
                    content,
                    tags,
                } => {
                    let tag_vec: Vec<String> = tags
                        .unwrap_or_default()
                        .split([',', ' '])
                        .map(|s| s.trim().trim_start_matches('#').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let (rel_path, _) = create_note(
                        &vault.path,
                        &title,
                        folder.as_deref(),
                        content.as_deref(),
                        &tag_vec,
                    )?;
                    storage.sync_vault(&vault)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "created": true, "rel_path": rel_path }))?);
                    } else {
                        println!("Created {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Append { note, text } => {
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let abs = safe_vault_path(&vault.path, &rel_path)?;
                    append_to_note(&abs, &text)?;
                    storage.sync_vault(&vault)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "appended": true, "rel_path": rel_path }))?);
                    } else {
                        println!("Appended to {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Edit { note } => {
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let abs = safe_vault_path(&vault.path, &rel_path)?;
                    tui::launch_external_editor(&abs)?;
                    let stats = storage.sync_vault(&vault)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "edited": true, "rel_path": rel_path, "stats": stats }))?);
                    } else {
                        println!("Edited and reindexed {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Move { src, dst } => {
                    let src_rel = storage.resolve_note_path(&vault.id, &src)?;
                    let was_bookmarked = storage
                        .get_note_summary(&vault.id, &src_rel)?
                        .map(|s| s.is_bookmarked)
                        .unwrap_or(false);
                    let (all_paths_before, aliases_before) =
                        storage.all_paths_and_aliases(&vault.id)?;
                    let new_rel = move_note(&vault.path, &src_rel, &dst)?;
                    let updated_links_in_notes = rewrite_wikilinks_for_move(
                        &vault.path,
                        &src_rel,
                        &new_rel,
                        &all_paths_before,
                        &aliases_before,
                    )?;
                    storage.sync_vault(&vault)?;
                    if was_bookmarked {
                        let _ = storage.set_bookmark(&vault.id, &new_rel, true);
                    }
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "moved": true,
                                "from": src_rel,
                                "to": new_rel,
                                "updated_links_in_notes": updated_links_in_notes,
                                "bookmark_preserved": was_bookmarked,
                            }))?
                        );
                    } else {
                        println!(
                            "Moved {src_rel} -> {new_rel} (updated links in {updated_links_in_notes} note(s))"
                        );
                    }
                    Ok(())
                }
                NotesCmd::Delete { note, yes } => {
                    if !yes {
                        bail!("Refusing to trash note without --yes");
                    }
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let trashed = move_note_to_trash(&vault.path, &rel_path)?;
                    storage.sync_vault(&vault)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "deleted": true, "rel_path": rel_path, "trash_path": trashed }))?);
                    } else {
                        println!("Moved {rel_path} -> {}", trashed.display());
                    }
                    Ok(())
                }
            }
        }

        Some(Commands::Daily { date, append }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let (rel_path, abs, created) = ensure_daily_note(&vault, date.as_deref())?;
            if let Some(txt) = append.as_deref().filter(|s| !s.trim().is_empty()) {
                append_to_note(&abs, txt)?;
            }
            storage.sync_vault(&vault)?;
            let detail = storage.load_note_detail(&vault, &rel_path)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "rel_path": rel_path,
                        "created": created,
                        "note": detail,
                    }))?
                );
            } else {
                println!("{}", detail.raw_markdown);
            }
            Ok(())
        }

        Some(Commands::Backlinks { note }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let rel_path = storage.resolve_note_path(&vault.id, &note)?;
            let bl = storage.get_backlinks(&vault.id, &rel_path)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&bl)?);
            } else if bl.is_empty() {
                println!("No backlinks to {rel_path}.");
            } else {
                for b in bl {
                    println!("← {}  —  {}", b.source_path, b.context_snippet);
                }
            }
            Ok(())
        }

        Some(Commands::Links { note }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let rel_path = storage.resolve_note_path(&vault.id, &note)?;
            let ol = storage.get_outgoing_links(&vault.id, &rel_path)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&ol)?);
            } else if ol.is_empty() {
                println!("No outgoing links from {rel_path}.");
            } else {
                for l in ol {
                    let status = if l.resolved { "→" } else { "◌ (unresolved)" };
                    println!("{status} {} ({})", l.target_raw, l.target_resolved_path.unwrap_or_else(|| "missing".into()));
                }
            }
            Ok(())
        }

        Some(Commands::Graph { note, depth }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let center = match note.as_deref() {
                Some(n) => Some(storage.resolve_note_path(&vault.id, n)?),
                None => None,
            };
            let graph = storage.get_graph(&vault.id, center.as_deref(), depth)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&graph)?);
            } else {
                println!(
                    "Graph ({} nodes, {} edges):",
                    graph.nodes.len(),
                    graph.edges.len()
                );
                for n in &graph.nodes {
                    println!("  [d={}] {} (in:{} out:{})", n.depth, n.title, n.in_degree, n.out_degree);
                }
            }
            Ok(())
        }

        Some(Commands::Orphans) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let orphans = storage.find_orphans(&vault.id)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&orphans)?);
            } else if orphans.is_empty() {
                println!("No orphan notes found.");
            } else {
                for o in orphans {
                    println!("{}", o.rel_path);
                }
            }
            Ok(())
        }

        Some(Commands::Tags { tag }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            if let Some(t) = tag {
                let notes = storage.find_by_tag(&vault.id, &t)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&notes)?);
                } else {
                    for n in notes {
                        println!("{}", n.rel_path);
                    }
                }
            } else {
                let tags = storage.list_tags(&vault.id)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&tags)?);
                } else {
                    for tc in tags {
                        println!("#{:<28} ({})", tc.tag, tc.count);
                    }
                }
            }
            Ok(())
        }

        Some(Commands::Tasks { open, done, note }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let filter = if open {
                TaskFilter::Open
            } else if done {
                TaskFilter::Done
            } else {
                TaskFilter::All
            };
            let note_rel = match note.as_deref() {
                Some(n) => Some(storage.resolve_note_path(&vault.id, n)?),
                None => None,
            };
            let tasks = storage.list_tasks(&vault.id, filter, note_rel.as_deref())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else {
                for t in tasks {
                    let box_c = if t.checked { "[x]" } else { "[ ]" };
                    println!("{box_c} {}:{}  {}", t.note_path, t.line_no, t.text);
                }
            }
            Ok(())
        }

        Some(Commands::ToggleTask {
            note,
            line,
            expected_text,
            expected_checked,
        }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;
            let rel_path = storage.resolve_note_path(&vault.id, &note)?;
            let abs = safe_vault_path(&vault.path, &rel_path)?;
            let checked = toggle_task_at_line_verified(
                &abs,
                line,
                expected_text.as_deref(),
                expected_checked,
            )?;
            storage.sync_vault(&vault)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "rel_path": rel_path,
                        "line": line,
                        "checked": checked,
                    }))?
                );
            } else {
                println!(
                    "{} {}:{}",
                    if checked { "[x]" } else { "[ ]" },
                    rel_path,
                    line
                );
            }
            Ok(())
        }

        Some(Commands::Daemon { action }) => match action {
            DaemonCmd::Start => {
                let st = daemon::start_daemon(cli.vault.as_deref()).await?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&st)?);
                } else {
                    println!(
                        "obsidian daemon running (pid={}, vault={})",
                        st.pid.unwrap_or(0),
                        st.vault_name.as_deref().unwrap_or("default")
                    );
                }
                Ok(())
            }
            DaemonCmd::Stop => {
                let stopped = daemon::stop_daemon().await?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&json!({ "stopped": stopped }))?);
                } else if stopped {
                    println!("obsidian daemon stopped.");
                } else {
                    println!("obsidian daemon was not running.");
                }
                Ok(())
            }
            DaemonCmd::Status => {
                let st = daemon::daemon_status().await?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&st)?);
                } else if st.running {
                    println!(
                        "Running (pid={}, vault={}, notes={}, uptime={}s)",
                        st.pid.unwrap_or(0),
                        st.vault_name.as_deref().unwrap_or("—"),
                        st.total_notes,
                        st.uptime_secs.unwrap_or(0)
                    );
                } else {
                    println!("obsidian daemon is stopped.");
                }
                Ok(())
            }
            DaemonCmd::Run => daemon::run_daemon_foreground(cfg, cli.vault).await,
        },

        Some(Commands::Serve { bind, port, token }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            serve::run_http_server(cfg, vault, storage, &bind, port, token).await
        }

        Some(Commands::Mcp) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let server = McpServer::new(cfg, vault, storage);
            server.run_stdio()
        }

        Some(Commands::Doctor) => {
            let native_cfg = native_obsidian_config_path();
            let vaults = merged_vaults(&cfg);
            let active = resolve_active_vault(&cfg, cli.vault.as_deref()).ok();
            let caps = crate::img_preview::detect_graphics_caps(cfg.image_protocol);
            let report = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "config_file": crate::config::config_file_path(),
                "db_path": crate::config::db_path(),
                "native_obsidian_json": native_cfg,
                "native_obsidian_json_exists": native_cfg.as_ref().is_some_and(|p| p.is_file()),
                "discovered_and_registered_vaults": vaults.len(),
                "active_vault": active.as_ref().map(|v| json!({ "name": v.name, "path": v.path })),
                "image_protocol": caps.protocol.label(),
                "default_serve_port": DEFAULT_SERVE_PORT,
            });
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("@bitefoundry/obsidian v{}", env!("CARGO_PKG_VERSION"));
                println!("  Config:        {}", crate::config::config_file_path().display());
                println!("  SQLite Index:  {}", crate::config::db_path().display());
                println!(
                    "  obsidian.json: {}",
                    native_cfg
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "not found".into())
                );
                println!("  Vaults found:  {}", vaults.len());
                if let Some(v) = active {
                    println!("  Active vault:  {} ({})", v.name, v.path.display());
                }
                println!("  Image proto:   {}", caps.protocol.label());
            }
            Ok(())
        }

        Some(Commands::Update { check, force }) => {
            let res = updater::check_or_apply_update(&cfg.github_repo, check, force).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if res.updated {
                println!("Updated obsidian {} -> {}", res.current_version, res.latest_version);
            } else if res.update_available {
                println!(
                    "Update available: {} -> {} ({})",
                    res.current_version, res.latest_version, res.release_url
                );
            } else {
                println!("Already up to date (v{}).", res.current_version);
            }
            Ok(())
        }
    }
}
