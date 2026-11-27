//! Clap CLI argument and subcommand definitions.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{config::DEFAULT_SERVE_PORT, shell_alias::AliasCommand};

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
    /// Manage an opt-in short shell alias (`ob` -> `bitefoundry-obsidian`).
    Alias {
        #[command(subcommand)]
        action: AliasCommand,
    },
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

