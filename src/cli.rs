//! Clap CLI definition & command executor for `bitefoundry-obsidian`.

use std::{ffi::OsString, path::PathBuf};

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

use crate::{
    config::{AppConfig, DEFAULT_SERVE_PORT},
    daemon,
    mcp::McpServer,
    models::{CodedError, LinkStatus, NoteContent, NoteOmittedCounts, TaskFilter, VaultSummary},
    serve,
    storage::Storage,
    tui, updater,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, move_note_to_trash, safe_vault_path,
            toggle_task_at_line_verified,
        },
        init_starter_vault, merged_vaults, native_obsidian_config_path, resolve_active_vault,
        resolve_registered_vault, vault_from_dir,
    },
};

#[derive(Debug, Parser)]
#[command(
    name = "bitefoundry-obsidian",
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
    /// Full-text search notes via SQLite FTS5 (Korean/CJK queries also match inside longer words).
    Search {
        /// Search query string.
        #[arg(allow_hyphen_values = true)]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read, list, create, append, replace, edit, move, or safely trash notes.
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
        /// With --json, embed the full note detail instead of the compact note object.
        #[arg(long)]
        full: bool,
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
        /// Hop depth for local BFS neighborhood (1..5; other values are clamped and the result reports the depth used).
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
        #[arg(long, conflicts_with = "done")]
        open: bool,
        /// Show only completed (`- [x]`) tasks.
        #[arg(long)]
        done: bool,
        /// Filter tasks to a specific note.
        #[arg(long)]
        note: Option<String>,
        /// Page size, 1..500. With --json the default is 50; text output lists everything unless set.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=500))]
        limit: Option<u64>,
        /// Continue after the next_cursor value of the previous page.
        #[arg(long)]
        cursor: Option<String>,
        /// Return every item in one response.
        #[arg(long, conflicts_with_all = ["limit", "cursor"])]
        all: bool,
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
    ///
    /// Requests need `Authorization: Bearer <token>`. Without --token a random token is generated
    /// for the run and printed once to stderr.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        /// Bearer token required on /api/v1/* (required for a non-loopback --bind).
        #[arg(long)]
        token: Option<String>,
        /// Serve a loopback bind without any token instead of generating one.
        #[arg(long)]
        allow_keyless: bool,
        /// Exact browser origin allowed to call the API cross-origin (repeatable), e.g. https://app.example.com.
        #[arg(
            long = "allow-origin",
            visible_alias = "cors-origin",
            value_name = "ORIGIN"
        )]
        allow_origin: Vec<String>,
        /// Extra Host header name to accept besides loopback and the --bind address (repeatable).
        #[arg(long = "allow-host", value_name = "HOST")]
        allow_host: Vec<String>,
    },
    /// Start the stdio JSON-RPC 2.0 MCP server (20 typed tools).
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
        /// Page size, 1..500. With --json the default is 50; text output lists everything unless set.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=500))]
        limit: Option<u64>,
        /// Continue after the next_cursor value of the previous page.
        #[arg(long)]
        cursor: Option<String>,
        /// Return every item in one response.
        #[arg(long, conflicts_with_all = ["limit", "cursor"])]
        all: bool,
    },
    /// Read raw Markdown, or add --json for a compact object (--full for all metadata).
    Read {
        note: String,
        /// With --json, return the compact note object. This is the default; kept for compatibility.
        #[arg(long)]
        compact: bool,
        /// With --json, return summary, parsed, backlinks, outgoing_links, and images.
        #[arg(long, conflicts_with = "compact")]
        full: bool,
    },
    /// Create a new note atomically.
    Create {
        #[arg(allow_hyphen_values = true)]
        title: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Append text/task/callout to an existing note.
    Append {
        note: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Replace a literal string when it occurs exactly once in the Markdown body.
    Replace {
        /// Note relative path, title, or alias.
        note: String,
        /// Exact text to find, including spacing and line endings.
        #[arg(long)]
        find: String,
        /// Replacement text. Pass an empty value to delete the match.
        #[arg(long)]
        replace: String,
    },
    /// Open a note in `$VISUAL` or `$EDITOR` and reindex after a successful exit.
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

impl Cli {
    /// Parses `std::env::args_os()`, allowing leading-hyphen free-text values on supported
    /// options without swallowing known flags or `--help`.
    pub fn parse_args() -> Self {
        Self::parse_from_args(std::env::args_os())
    }

    /// Parses CLI arguments with hyphen-value normalization and exits on clap error.
    pub fn parse_from_args<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        match Self::try_parse_from_args(itr) {
            Ok(cli) => cli,
            Err(err) => err.exit(),
        }
    }

    /// Parses CLI arguments with hyphen-value normalization, returning clap errors directly.
    pub fn try_parse_from_args<I, T>(itr: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let cli = <Self as Parser>::try_parse_from(normalize_cli_args(itr))?;
        let detail_flag = match &cli.command {
            Some(Commands::Notes {
                action: NotesCmd::Read { compact, full, .. },
            }) => *compact || *full,
            Some(Commands::Daily { full, .. }) => *full,
            _ => false,
        };
        if detail_flag && !cli.json {
            return Err(<Self as clap::CommandFactory>::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--compact and --full require --json",
            ));
        }
        Ok(cli)
    }
}

fn is_hyphen_text_option(cmd_path: &[&str], long_name: &str) -> bool {
    matches!(
        (cmd_path, long_name),
        (["daily"], "append")
            | (["notes", "create"], "content")
            | (["notes", "replace"], "find" | "replace")
            | (["toggle-task"], "expected-text")
    )
}

fn hyphen_positional_slots(cmd_path: &[&str]) -> usize {
    match cmd_path {
        ["notes", "read" | "replace" | "edit" | "delete"] => 1,
        ["notes", "append" | "move"] => 2,
        ["toggle-task"] => 1,
        _ => 0,
    }
}

fn looks_like_flag_typo(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix("--") {
        let name = rest.split('=').next().unwrap_or(rest);
        return !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    if let Some(rest) = token.strip_prefix('-') {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphabetic());
    }
    false
}

fn is_known_flag(cmd: &clap::Command, token: &str) -> bool {
    if token == "--" {
        return true;
    }
    if let Some(rest) = token.strip_prefix("--") {
        let name = rest.split('=').next().unwrap_or(rest);
        if name.is_empty() {
            return false;
        }
        return cmd.get_arguments().any(|arg| {
            arg.get_long() == Some(name)
                || arg
                    .get_all_aliases()
                    .is_some_and(|aliases| aliases.contains(&name))
        });
    }
    if let Some(rest) = token.strip_prefix('-') {
        if rest.is_empty() {
            return false;
        }
        return rest.chars().all(|ch| {
            cmd.get_arguments().any(|arg| {
                arg.get_short() == Some(ch)
                    || arg
                        .get_all_short_aliases()
                        .is_some_and(|aliases| aliases.contains(&ch))
            })
        });
    }
    false
}

fn has_potential_hyphen_option_literal(raw: &[OsString]) -> bool {
    let mut args = raw.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(
            token,
            "--append" | "--content" | "--find" | "--replace" | "--expected-text"
        ) && args
            .peek()
            .and_then(|next| next.to_str())
            .is_some_and(|next| next != "--" && next.len() > 1 && next.starts_with('-'))
        {
            return true;
        }
    }
    false
}

fn has_potential_hyphen_note_positional(raw: &[OsString]) -> bool {
    let mut saw_target_cmd = false;
    for arg in raw.iter().skip(1) {
        let Some(token) = arg.to_str() else {
            continue;
        };
        if token == "--" {
            return false;
        }
        if matches!(token, "notes" | "toggle-task") {
            saw_target_cmd = true;
            continue;
        }
        if saw_target_cmd
            && token.len() > 1
            && token.starts_with('-')
            && !looks_like_flag_typo(token)
        {
            return true;
        }
    }
    false
}

fn normalize_cli_args<I, T>(itr: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw: Vec<OsString> = itr.into_iter().map(Into::into).collect();
    if !has_potential_hyphen_option_literal(&raw) && !has_potential_hyphen_note_positional(&raw) {
        return raw;
    }

    let mut root = <Cli as CommandFactory>::command();
    root.build();

    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(raw[0].clone());

    let mut cmd: &clap::Command = &root;
    let mut cmd_path: Vec<&str> = Vec::new();
    let mut deferred_positionals: Vec<OsString> = Vec::new();
    let mut has_hyphen_positional = false;
    let mut i = 1usize;

    while i < raw.len() {
        let slots = hyphen_positional_slots(&cmd_path);
        let Some(token) = raw[i].to_str() else {
            if slots > 0 {
                deferred_positionals.push(raw[i].clone());
            } else {
                out.push(raw[i].clone());
            }
            i += 1;
            continue;
        };

        if token == "--" {
            if has_hyphen_positional {
                out.push(raw[i].clone());
                out.append(&mut deferred_positionals);
                out.extend_from_slice(&raw[i + 1..]);
            } else {
                out.append(&mut deferred_positionals);
                out.extend_from_slice(&raw[i..]);
            }
            return out;
        }

        if let Some(long_rest) = token.strip_prefix("--") {
            let (name, attached) = match long_rest.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long_rest, None),
            };
            let matched_arg = cmd.get_arguments().find(|arg| {
                arg.get_long() == Some(name)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&name))
            });
            if let Some(arg_def) = matched_arg {
                if attached.is_none() && arg_def.get_action().takes_values() {
                    if let Some(next_os) = raw.get(i + 1) {
                        let next_str = next_os.to_str();
                        let next_starts_with_dash =
                            next_str.is_some_and(|s| s.len() > 1 && s.starts_with('-'));
                        if is_hyphen_text_option(&cmd_path, name) {
                            if next_starts_with_dash && !is_known_flag(cmd, next_str.unwrap()) {
                                let mut combined = raw[i].clone();
                                combined.push("=");
                                combined.push(next_os);
                                out.push(combined);
                                i += 2;
                                continue;
                            }
                            if !next_starts_with_dash {
                                out.push(raw[i].clone());
                                out.push(next_os.clone());
                                i += 2;
                                continue;
                            }
                            out.push(raw[i].clone());
                            i += 1;
                            continue;
                        } else if !next_starts_with_dash {
                            out.push(raw[i].clone());
                            out.push(next_os.clone());
                            i += 2;
                            continue;
                        }
                    }
                }
                out.push(raw[i].clone());
                i += 1;
                continue;
            }
            if slots > 0 && deferred_positionals.len() < slots && !looks_like_flag_typo(token) {
                deferred_positionals.push(raw[i].clone());
                has_hyphen_positional = true;
                i += 1;
                continue;
            }
            out.push(raw[i].clone());
            i += 1;
            continue;
        }

        if token.len() > 1 && token.starts_with('-') {
            if !is_known_flag(cmd, token)
                && slots > 0
                && deferred_positionals.len() < slots
                && !looks_like_flag_typo(token)
            {
                deferred_positionals.push(raw[i].clone());
                has_hyphen_positional = true;
                i += 1;
                continue;
            }
            out.push(raw[i].clone());
            i += 1;
            continue;
        }

        if slots == 0 {
            if let Some(sub) = cmd.get_subcommands().find(|sc| {
                sc.get_name() == token || sc.get_all_aliases().any(|alias| alias == token)
            }) {
                cmd_path.push(sub.get_name());
                cmd = sub;
            }
            out.push(raw[i].clone());
        } else {
            deferred_positionals.push(raw[i].clone());
        }
        i += 1;
    }

    if has_hyphen_positional {
        out.push(OsString::from("--"));
    }
    out.extend(deferred_positionals);
    out
}

fn cli_compact_note_json(
    content: &NoteContent,
    counts: NoteOmittedCounts,
    vault_spec: Option<&str>,
) -> serde_json::Value {
    let hint = match vault_spec {
        Some(spec) => format!(
            "bitefoundry-obsidian --vault {:?} notes read {:?} --json --full",
            spec, content.rel_path
        ),
        None => format!(
            "bitefoundry-obsidian notes read {:?} --json --full",
            content.rel_path
        ),
    };
    json!({
        "rel_path": content.rel_path,
        "title": content.title,
        "raw_markdown": content.raw_markdown,
        "mtime_ms": content.mtime_ms,
        "omitted": {
            "backlinks": counts.backlinks,
            "outgoing_links": counts.outgoing_links,
            "images": serde_json::Value::Null,
            "fields": ["summary", "parsed"],
        },
        "hint": hint,
    })
}

pub async fn run(cli: Cli) -> Result<()> {
    updater::cleanup_stale_old_binary();
    let cfg = AppConfig::load();

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
                    println!("No vaults found. Run `bitefoundry-obsidian init <dir>` or `bitefoundry-obsidian vaults add <path>`.");
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
                let mut persisted = AppConfig::load_persisted()?;
                let profile = vault_from_dir(&path, name.as_deref());
                let norm_new = crate::config::normalize_path_key(&profile.path);
                if let Some(conflict) = merged_vaults(&persisted).into_iter().find(|v| {
                    v.name.eq_ignore_ascii_case(&profile.name)
                        && crate::config::normalize_path_key(&v.path) != norm_new
                }) {
                    bail!(
                        "Vault name '{}' is already registered for '{}'. Use a different `--name` or remove the existing vault first.",
                        conflict.name,
                        conflict.path.display()
                    );
                }
                persisted.register_vault(profile.clone(), true)?;
                persisted.save()?;
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
                let mut persisted = AppConfig::load_persisted()?;
                let target = resolve_registered_vault(&persisted, &name)?;
                persisted.active_vault = target.name.clone();
                persisted.save()?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&target)?);
                } else {
                    println!(
                        "Active vault set to '{}' ({})",
                        target.name,
                        target.path.display()
                    );
                }
                Ok(())
            }
            VaultsCmd::Remove { name } => {
                let mut persisted = AppConfig::load_persisted()?;
                let removed = persisted.remove_vault(&name);
                if removed {
                    persisted.save()?;
                }
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "removed": removed, "name": name }))?
                    );
                } else if removed {
                    println!("Removed vault '{name}' from registry.");
                } else {
                    println!("Vault '{name}' was not in manual registry.");
                }
                Ok(())
            }
        },

        Some(Commands::Init { dir }) => {
            // Read the stored config first: a broken file must stop the command before the
            // starter vault is created.
            let mut persisted = AppConfig::load_persisted()?;
            let profile = init_starter_vault(&dir)?;
            persisted.upsert_vault(profile.clone(), true);
            persisted.save()?;
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
            let page = storage.search_notes_page(&vault.id, &query, limit)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "query": query,
                        "limit": page.limit,
                        "returned": page.hits.len(),
                        "has_more": page.has_more,
                        "next_cursor": serde_json::Value::Null,
                        "results": page.hits,
                    }))?
                );
            } else if page.hits.is_empty() {
                println!("No notes matching '{query}'.");
            } else {
                for h in &page.hits {
                    println!("{} — {}", h.rel_path, h.snippet);
                }
                if page.has_more {
                    eprintln!(
                        "More results available. Raise --limit (max 200) or narrow the query."
                    );
                }
            }
            Ok(())
        }

        Some(Commands::Notes { action }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            storage.sync_vault(&vault)?;

            match action {
                NotesCmd::List {
                    folder,
                    limit,
                    cursor,
                    all,
                } => {
                    let effective_limit = if all {
                        None
                    } else if let Some(lim) = limit {
                        Some(lim as usize)
                    } else if cli.json {
                        Some(50)
                    } else {
                        None
                    };
                    let page = storage.list_notes_page(
                        &vault.id,
                        folder.as_deref(),
                        cursor.as_deref(),
                        effective_limit,
                    )?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "notes": page.items,
                                "returned": page.items.len(),
                                "total": page.total,
                                "has_more": page.has_more,
                                "next_cursor": page.next_cursor,
                            }))?
                        );
                    } else {
                        for n in &page.items {
                            let bm = if n.is_bookmarked { "★ " } else { "  " };
                            println!(
                                "{bm}{:<42} {:>4}w  ←{} →{}",
                                n.rel_path, n.word_count, n.backlink_count, n.outgoing_count
                            );
                        }
                        if let Some(next_cursor) =
                            page.next_cursor.as_deref().filter(|_| page.has_more)
                        {
                            eprintln!(
                                "More results available. Continue with --cursor {next_cursor:?}"
                            );
                        }
                    }
                    Ok(())
                }
                NotesCmd::Read {
                    note,
                    compact: _,
                    full,
                } => {
                    if !cli.json {
                        print!("{}", storage.load_note_content(&vault, &note)?.raw_markdown);
                    } else if full {
                        let detail = storage.load_note_detail(&vault, &note)?;
                        println!("{}", serde_json::to_string_pretty(&detail)?);
                    } else {
                        let content = storage.load_note_content(&vault, &note)?;
                        let counts = storage.note_omitted_counts(&vault.id, &content.rel_path)?;
                        let payload = cli_compact_note_json(&content, counts, cli.vault.as_deref());
                        println!("{}", serde_json::to_string_pretty(&payload)?);
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
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &json!({ "created": true, "rel_path": rel_path })
                            )?
                        );
                    } else {
                        println!("Created {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Append { note, text } => {
                    if text.is_empty() {
                        return Err(CodedError::new(
                            "invalid_argument",
                            "Text to append must not be empty",
                        )
                        .into());
                    }
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let abs = safe_vault_path(&vault.path, &rel_path)?;
                    append_to_note(&abs, &text)?;
                    storage.sync_vault(&vault)?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &json!({ "appended": true, "rel_path": rel_path })
                            )?
                        );
                    } else {
                        println!("Appended to {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Replace {
                    note,
                    find,
                    replace,
                } => {
                    let result = storage.replace_note_text(&vault, &note, &find, &replace)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else if result.replaced {
                        println!("Replaced one match in {}", result.rel_path);
                    } else {
                        println!(
                            "Found {} matches in {}. A replacement requires one match.",
                            result.matches, result.rel_path
                        );
                    }
                    Ok(())
                }
                NotesCmd::Edit { note } => {
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let abs = safe_vault_path(&vault.path, &rel_path)?;
                    tui::launch_external_editor(&abs)?;
                    let stats = storage.sync_vault(&vault)?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &json!({ "edited": true, "rel_path": rel_path, "stats": stats })
                            )?
                        );
                    } else {
                        println!("Edited and reindexed {rel_path}");
                    }
                    Ok(())
                }
                NotesCmd::Move { src, dst } => {
                    let moved = storage.move_note_with_links(&vault, &src, &dst)?;
                    if cli.json {
                        let payload = serde_json::to_value(&moved)?;
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    } else {
                        println!(
                            "Moved {} -> {} (updated links in {} note(s))",
                            moved.from, moved.to, moved.updated_links_in_notes
                        );
                    }
                    Ok(())
                }
                NotesCmd::Delete { note, yes } => {
                    if !yes {
                        return Err(CodedError::new(
                            "confirmation_required",
                            "Refusing to trash note without --yes",
                        )
                        .into());
                    }
                    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
                    let trashed = move_note_to_trash(&vault.path, &rel_path)?;
                    storage.sync_vault(&vault)?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &json!({ "deleted": true, "rel_path": rel_path, "trash_path": trashed })
                            )?
                        );
                    } else {
                        println!("Moved {rel_path} -> {}", trashed.display());
                    }
                    Ok(())
                }
            }
        }

        Some(Commands::Daily { date, append, full }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let (rel_path, abs, created) = ensure_daily_note(&vault, date.as_deref())?;
            let mut appended = false;
            if let Some(txt) = append.as_deref().filter(|s| !s.trim().is_empty()) {
                append_to_note(&abs, txt)?;
                appended = true;
            }
            storage.sync_vault(&vault)?;
            if cli.json {
                if appended && !full {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "rel_path": rel_path,
                            "created": created,
                            "appended": true,
                        }))?
                    );
                } else {
                    let note_val = if full {
                        let detail = storage.load_note_detail(&vault, &rel_path)?;
                        serde_json::to_value(&detail)?
                    } else {
                        let content = storage.load_note_content(&vault, &rel_path)?;
                        let counts = storage.note_omitted_counts(&vault.id, &content.rel_path)?;
                        cli_compact_note_json(&content, counts, cli.vault.as_deref())
                    };
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "rel_path": rel_path,
                            "created": created,
                            "note": note_val,
                        }))?
                    );
                }
            } else {
                let content = storage.load_note_content(&vault, &rel_path)?;
                println!("{}", content.raw_markdown);
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
            let ol = storage.get_outgoing_links(&vault, &rel_path)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&ol)?);
            } else if ol.is_empty() {
                println!("No outgoing links from {rel_path}.");
            } else {
                for l in ol {
                    match l.status {
                        LinkStatus::Resolved | LinkStatus::Attachment => {
                            println!(
                                "→ {} ({})",
                                l.target_raw,
                                l.target_resolved_path.unwrap_or_default()
                            );
                        }
                        LinkStatus::Ambiguous => {
                            println!(
                                "◌ (unresolved) {} (ambiguous: {})",
                                l.target_raw,
                                l.candidates.join(", ")
                            );
                        }
                        LinkStatus::Missing => {
                            println!("◌ (unresolved) {} (missing)", l.target_raw);
                        }
                    }
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
            let graph = storage.get_graph(&vault, center.as_deref(), depth)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&graph)?);
            } else {
                println!(
                    "Graph ({} nodes, {} edges):",
                    graph.nodes.len(),
                    graph.edges.len()
                );
                for n in &graph.nodes {
                    println!(
                        "  [d={}] {} (in:{} out:{})",
                        n.depth, n.title, n.in_degree, n.out_degree
                    );
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

        Some(Commands::Tasks {
            open,
            done,
            note,
            limit,
            cursor,
            all,
        }) => {
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
            let effective_limit = if all {
                None
            } else if let Some(lim) = limit {
                Some(lim as usize)
            } else if cli.json {
                Some(50)
            } else {
                None
            };
            let page = storage.list_tasks_page(
                &vault.id,
                filter,
                note_rel.as_deref(),
                cursor.as_deref(),
                effective_limit,
            )?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "tasks": page.items,
                        "returned": page.items.len(),
                        "total": page.total,
                        "has_more": page.has_more,
                        "next_cursor": page.next_cursor,
                    }))?
                );
            } else {
                for t in &page.items {
                    let box_c = if t.checked { "[x]" } else { "[ ]" };
                    println!("{box_c} {}:{}  {}", t.note_path, t.line_no, t.text);
                }
                if let Some(next_cursor) = page.next_cursor.as_deref().filter(|_| page.has_more) {
                    eprintln!("More results available. Continue with --cursor {next_cursor:?}");
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
                        "bitefoundry-obsidian daemon running (pid={}, vault={})",
                        st.pid.unwrap_or(0),
                        st.vault_name.as_deref().unwrap_or("default")
                    );
                }
                Ok(())
            }
            DaemonCmd::Stop => {
                let stopped = daemon::stop_daemon().await?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "stopped": stopped }))?
                    );
                } else if stopped {
                    println!("bitefoundry-obsidian daemon stopped.");
                } else {
                    println!("bitefoundry-obsidian daemon was not running.");
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
                        st.total_notes.unwrap_or(0),
                        st.uptime_secs.unwrap_or(0)
                    );
                } else {
                    println!("bitefoundry-obsidian daemon is stopped.");
                }
                Ok(())
            }
            DaemonCmd::Run => daemon::run_daemon_foreground(cfg, cli.vault).await,
        },

        Some(Commands::Serve {
            bind,
            port,
            token,
            allow_keyless,
            allow_origin,
            allow_host,
        }) => {
            let vault = resolve_active_vault(&cfg, cli.vault.as_deref())?;
            let storage = Storage::open_default()?;
            let opts = serve::ServeOptions {
                token,
                allow_keyless,
                allow_origins: allow_origin,
                allow_hosts: allow_host,
            };
            serve::run_http_server_with_options(cfg, vault, storage, &bind, port, opts).await
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
                println!(
                    "  Config:        {}",
                    crate::config::config_file_path().display()
                );
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
                println!(
                    "Updated obsidian {} -> {}",
                    res.current_version, res.latest_version
                );
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

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("bitefoundry-obsidian");
        argv.extend_from_slice(args);
        Cli::try_parse_from_args(argv)
    }

    #[test]
    fn parses_hyphen_values_across_free_text_commands() {
        for sample in ["- 목록 항목", "- first\n- second", "---\nkey: val\n---"] {
            let daily = parse(&["daily", "--append", sample, "--json"]).unwrap();
            assert!(daily.json);
            assert!(matches!(
                daily.command,
                Some(Commands::Daily { date: None, append: Some(ref s), full: false }) if s == sample
            ));

            let daily_eq = parse(&["daily", &format!("--append={sample}")]).unwrap();
            assert!(matches!(
                daily_eq.command,
                Some(Commands::Daily { date: None, append: Some(ref s), full: false }) if s == sample
            ));

            let created_body =
                parse(&["notes", "create", "Test", "--content", sample, "--json"]).unwrap();
            assert!(created_body.json);
            assert!(matches!(
                created_body.command,
                Some(Commands::Notes {
                    action: NotesCmd::Create { ref title, content: Some(ref s), .. }
                }) if title == "Test" && s == sample
            ));

            let created_body_eq =
                parse(&["notes", "create", "Test", &format!("--content={sample}")]).unwrap();
            assert!(matches!(
                created_body_eq.command,
                Some(Commands::Notes {
                    action: NotesCmd::Create { ref title, content: Some(ref s), .. }
                }) if title == "Test" && s == sample
            ));

            let created_title = parse(&["notes", "create", sample, "--json"]).unwrap();
            assert!(created_title.json);
            assert!(matches!(
                created_title.command,
                Some(Commands::Notes {
                    action: NotesCmd::Create { ref title, content: None, .. }
                }) if title == sample
            ));

            let search = parse(&["search", sample, "--limit", "5", "--json"]).unwrap();
            assert!(search.json);
            assert!(matches!(
                search.command,
                Some(Commands::Search { ref query, limit: 5 }) if query == sample
            ));

            let appended = parse(&["notes", "append", "Note", sample, "--json"]).unwrap();
            assert!(appended.json);
            assert!(matches!(
                appended.command,
                Some(Commands::Notes {
                    action: NotesCmd::Append { ref note, ref text }
                }) if note == "Note" && text == sample
            ));

            let replaced = parse(&[
                "notes",
                "replace",
                "Note",
                "--find",
                sample,
                "--replace",
                sample,
                "--json",
            ])
            .unwrap();
            assert!(replaced.json);
            assert!(matches!(
                replaced.command,
                Some(Commands::Notes {
                    action: NotesCmd::Replace { ref note, ref find, ref replace }
                }) if note == "Note" && find == sample && replace == sample
            ));
        }

        // Global `--vault` whose value matches a subcommand name does not confuse normalization.
        let with_vault = parse(&["--vault", "daily", "daily", "--append", "- 목록 항목"]).unwrap();
        assert_eq!(with_vault.vault.as_deref(), Some("daily"));
        assert!(matches!(
            with_vault.command,
            Some(Commands::Daily { append: Some(ref s), .. }) if s == "- 목록 항목"
        ));

        // Ordinary invocations skip building a second clap command tree.
        let to_os = |args: &[&str]| -> Vec<OsString> { args.iter().map(OsString::from).collect() };
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "notes",
            "list",
            "--json",
        ])));
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "daily",
            "--append",
            "plain text",
        ])));
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "daily",
            "--append=- 목록 항목",
        ])));
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "daily",
            "--help",
        ])));
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "daily",
            "-h",
        ])));
        assert!(!has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "notes",
            "append",
            "Note",
            "--",
            "--append",
            "- item",
        ])));
        assert!(has_potential_hyphen_option_literal(&to_os(&[
            "bitefoundry-obsidian",
            "daily",
            "--append",
            "- 목록 항목",
        ])));
    }

    #[test]
    fn preserves_known_flags_missing_values_help_equals_and_positional_separator() {
        // Explicit `=` and `--` support flag-looking literal values.
        let daily_eq = parse(&["daily", "--append=--json"]).unwrap();
        assert!(!daily_eq.json);
        assert!(matches!(
            daily_eq.command,
            Some(Commands::Daily { append: Some(ref s), .. }) if s == "--json"
        ));

        let create_eq = parse(&["notes", "create", "Test", "--content=--help"]).unwrap();
        assert!(matches!(
            create_eq.command,
            Some(Commands::Notes {
                action: NotesCmd::Create { content: Some(ref s), .. }
            }) if s == "--help"
        ));

        let replace_eq = parse(&[
            "notes",
            "replace",
            "Note",
            "--find=--json",
            "--replace=--help",
        ])
        .unwrap();
        assert!(matches!(
            replace_eq.command,
            Some(Commands::Notes {
                action: NotesCmd::Replace { ref find, ref replace, .. }
            }) if find == "--json" && replace == "--help"
        ));

        let search_sep = parse(&["search", "--", "--json"]).unwrap();
        assert!(!search_sep.json);
        assert!(matches!(
            search_sep.command,
            Some(Commands::Search { ref query, .. }) if query == "--json"
        ));

        let create_sep = parse(&["notes", "create", "--", "--help"]).unwrap();
        assert!(matches!(
            create_sep.command,
            Some(Commands::Notes {
                action: NotesCmd::Create { ref title, .. }
            }) if title == "--help"
        ));

        let append_sep = parse(&["notes", "append", "Note", "--", "--json"]).unwrap();
        assert!(!append_sep.json);
        assert!(matches!(
            append_sep.command,
            Some(Commands::Notes {
                action: NotesCmd::Append { ref text, .. }
            }) if text == "--json"
        ));

        // Missing values followed by real flags must fail rather than swallow the flag.
        for missing_value_argv in [
            &["daily", "--append", "--json"][..],
            &["daily", "--append", "--date", "2026-11-20"][..],
            &["daily", "--append", "--vault", "Work"][..],
            &["daily", "--append", "--help"][..],
            &["daily", "--append", "-h"][..],
            &["daily", "--append", "--"][..],
            &["notes", "create", "Test", "--content", "--json"][..],
            &["notes", "create", "Test", "--content", "--tags", "a"][..],
            &["notes", "create", "Test", "--content", "--help"][..],
            &["notes", "create", "--json"][..],
            &["notes", "create", "--content", "- item"][..],
            &["search", "--json"][..],
            &["search", "--limit", "5"][..],
            &["notes", "append", "Note", "--json"][..],
            &["notes", "replace", "Note", "--find", "--replace", "new"][..],
            &[
                "notes",
                "replace",
                "Note",
                "--find",
                "old",
                "--replace",
                "--json",
            ][..],
            &[
                "notes",
                "replace",
                "Note",
                "--find",
                "old",
                "--replace",
                "--help",
            ][..],
            // Non-text options keep standard clap rejection for hyphen-prefixed values.
            &["daily", "--date", "-2026-11-20"][..],
            &["search", "foo", "--limit", "-1"][..],
        ] {
            let err = parse(missing_value_argv)
                .expect_err(&format!("expected parse error for {missing_value_argv:?}"));
            assert_ne!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "unexpected help for {missing_value_argv:?}"
            );
        }

        // Actual help invocations (standalone or after a completed hyphen value) print help.
        for help_argv in [
            &["daily", "--help"][..],
            &["daily", "-h"][..],
            &["daily", "--append", "- 목록 항목", "--help"][..],
            &["notes", "create", "--help"][..],
            &["notes", "create", "- 목록 항목", "--help"][..],
            &[
                "notes",
                "create",
                "Test",
                "--content",
                "- 목록 항목",
                "--help",
            ][..],
            &["search", "--help"][..],
            &["search", "-h"][..],
            &["search", "- 목록 항목", "--help"][..],
            &["notes", "append", "--help"][..],
            &["notes", "append", "Note", "--help"][..],
            &["notes", "append", "Note", "- 목록 항목", "--help"][..],
            &["notes", "replace", "--help"][..],
            &[
                "notes",
                "replace",
                "Note",
                "--find",
                "- 목록 항목",
                "--replace",
                "new",
                "--help",
            ][..],
        ] {
            let err =
                parse(help_argv).expect_err(&format!("expected DisplayHelp for {help_argv:?}"));
            assert_eq!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected DisplayHelp for {help_argv:?}, got {err}"
            );
        }
    }
}
