//! Shared domain models for Obsidian vaults, notes, Markdown AST, knowledge graph,
//! FTS5 search hits, background watcher events, and CLI/HTTP/MCP JSON responses.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const POLL_RETRY_SUMMARY: &str = "Watcher idle — retrying on next interval";

/// Semantic Obsidian callout type (`> [!tip]`, `> [!warning]`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalloutKind {
    #[default]
    Note,
    Tip,
    Info,
    Warning,
    Danger,
    Example,
    Quote,
    Success,
    Question,
}

impl CalloutKind {
    pub fn parse_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tip" | "hint" | "important" => Self::Tip,
            "info" | "todo" | "abstract" | "summary" | "tldr" => Self::Info,
            "warning" | "caution" | "attention" => Self::Warning,
            "danger" | "error" | "failure" | "fail" | "missing" | "bug" => Self::Danger,
            "example" => Self::Example,
            "quote" | "cite" => Self::Quote,
            "success" | "check" | "done" => Self::Success,
            "question" | "help" | "faq" => Self::Question,
            _ => Self::Note,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Note => "NOTE",
            Self::Tip => "TIP",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Danger => "DANGER",
            Self::Example => "EXAMPLE",
            Self::Quote => "QUOTE",
            Self::Success => "SUCCESS",
            Self::Question => "QUESTION",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Note => "◆",
            Self::Tip => "✦",
            Self::Info => "ℹ",
            Self::Warning => "▲",
            Self::Danger => "✖",
            Self::Example => "▸",
            Self::Quote => "❝",
            Self::Success => "✓",
            Self::Question => "?",
        }
    }
}

/// Inline formatted span inside a Markdown paragraph, heading, list item, or callout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InlineSpan {
    Text {
        text: String,
    },
    Bold {
        text: String,
    },
    Italic {
        text: String,
    },
    Code {
        text: String,
    },
    Strikethrough {
        text: String,
    },
    Highlight {
        text: String,
    },
    Wikilink {
        target: String,
        heading: Option<String>,
        alias: Option<String>,
        resolved_path: Option<String>,
    },
    ExternalLink {
        label: String,
        url: String,
    },
    Tag {
        tag: String,
    },
}

impl InlineSpan {
    pub fn plain_text(&self) -> String {
        match self {
            Self::Text { text }
            | Self::Bold { text }
            | Self::Italic { text }
            | Self::Code { text }
            | Self::Strikethrough { text }
            | Self::Highlight { text } => text.clone(),
            Self::Wikilink {
                target,
                heading,
                alias,
                ..
            } => {
                if let Some(a) = alias {
                    a.clone()
                } else if let Some(h) = heading {
                    format!("{target}#{h}")
                } else {
                    target.clone()
                }
            }
            Self::ExternalLink { label, .. } => label.clone(),
            Self::Tag { tag } => format!("#{tag}"),
        }
    }
}

/// Parsed Obsidian-flavored Markdown block for rich TUI canvas rendering & interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoteBlock {
    Heading {
        level: u8,
        text: String,
        spans: Vec<InlineSpan>,
        line_no: usize,
    },
    Paragraph {
        spans: Vec<InlineSpan>,
        line_no: usize,
    },
    Callout {
        callout_kind: CalloutKind,
        title: String,
        collapsed: bool,
        body_lines: Vec<Vec<InlineSpan>>,
        line_no: usize,
    },
    Task {
        checked: bool,
        indent: usize,
        text: String,
        spans: Vec<InlineSpan>,
        line_no: usize,
    },
    BulletItem {
        indent: usize,
        spans: Vec<InlineSpan>,
        line_no: usize,
    },
    NumberedItem {
        number: String,
        indent: usize,
        spans: Vec<InlineSpan>,
        line_no: usize,
    },
    CodeBlock {
        language: String,
        code: String,
        line_no: usize,
    },
    Blockquote {
        lines: Vec<Vec<InlineSpan>>,
        line_no: usize,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        line_no: usize,
    },
    ImageEmbed {
        raw_target: String,
        alt: String,
        width_hint: Option<u32>,
        resolved_path: Option<PathBuf>,
        line_no: usize,
    },
    NoteEmbed {
        target: String,
        heading: Option<String>,
        resolved_path: Option<String>,
        line_no: usize,
    },
    Divider {
        line_no: usize,
    },
}

impl NoteBlock {
    pub fn line_no(&self) -> usize {
        match self {
            Self::Heading { line_no, .. }
            | Self::Paragraph { line_no, .. }
            | Self::Callout { line_no, .. }
            | Self::Task { line_no, .. }
            | Self::BulletItem { line_no, .. }
            | Self::NumberedItem { line_no, .. }
            | Self::CodeBlock { line_no, .. }
            | Self::Blockquote { line_no, .. }
            | Self::Table { line_no, .. }
            | Self::ImageEmbed { line_no, .. }
            | Self::NoteEmbed { line_no, .. }
            | Self::Divider { line_no } => *line_no,
        }
    }

    pub fn first_wikilink(&self) -> Option<(String, Option<String>)> {
        let spans: Vec<&InlineSpan> = match self {
            Self::Heading { spans, .. }
            | Self::Paragraph { spans, .. }
            | Self::Task { spans, .. }
            | Self::BulletItem { spans, .. }
            | Self::NumberedItem { spans, .. } => spans.iter().collect(),
            Self::Callout { body_lines, .. } => body_lines.iter().flatten().collect(),
            Self::Blockquote { lines, .. } => lines.iter().flatten().collect(),
            Self::NoteEmbed {
                target,
                resolved_path,
                ..
            } => return Some((target.clone(), resolved_path.clone())),
            _ => Vec::new(),
        };
        for span in spans {
            if let InlineSpan::Wikilink {
                target,
                resolved_path,
                ..
            } = span
            {
                return Some((target.clone(), resolved_path.clone()));
            }
        }
        None
    }
}

/// Raw wikilink or embed extracted from a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikilinkRef {
    pub target_raw: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub is_embed: bool,
    pub line_no: usize,
}

/// Standard Markdown external/local link `[label](url)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownLinkRef {
    pub label: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_image: bool,
    pub line_no: usize,
}

/// Heading in a note outline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadingEntry {
    pub note_path: String,
    pub level: u8,
    pub text: String,
    pub line_no: usize,
}

/// Checklist task inside a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEntry {
    pub note_path: String,
    pub note_title: String,
    pub line_no: usize,
    pub text: String,
    pub checked: bool,
}

/// Complete parsed representation of a single `.md` file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParsedNote {
    pub frontmatter: BTreeMap<String, serde_json::Value>,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub wikilinks: Vec<WikilinkRef>,
    pub md_links: Vec<MarkdownLinkRef>,
    pub headings: Vec<HeadingEntry>,
    pub tasks: Vec<TaskEntry>,
    pub blocks: Vec<NoteBlock>,
    pub body_markdown: String,
    pub body_plain: String,
    pub word_count: usize,
    pub body_start_line: usize,
}

/// Summary row of a note stored in SQLite.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NoteSummary {
    pub vault_id: String,
    pub rel_path: String,
    pub title: String,
    pub folder: String,
    pub frontmatter: BTreeMap<String, serde_json::Value>,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub word_count: usize,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub is_bookmarked: bool,
    pub backlink_count: usize,
    pub outgoing_count: usize,
}

/// Full loaded note for TUI Canvas, CLI read, HTTP API, and MCP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoteDetail {
    pub summary: NoteSummary,
    pub raw_markdown: String,
    pub parsed: ParsedNote,
    pub backlinks: Vec<BacklinkEntry>,
    pub outgoing_links: Vec<OutgoingLinkEntry>,
    pub images: Vec<NoteImageRef>,
}

/// Compact read result shared by CLI and MCP.
#[derive(Debug, Clone, Serialize)]
pub struct NoteContent {
    pub rel_path: String,
    pub title: String,
    pub raw_markdown: String,
}

/// Result of moving a note through the CLI or MCP.
#[derive(Debug, Serialize)]
pub struct NoteMove {
    pub moved: bool,
    pub from: String,
    pub to: String,
    pub updated_links_in_notes: usize,
    pub bookmark_preserved: bool,
}

/// Result of a literal Markdown body replacement.
#[derive(Debug, Serialize)]
pub struct NoteTextReplacement {
    pub replaced: bool,
    pub matches: usize,
    pub rel_path: String,
}

/// Resolved local image embedded in a note (`![[img.png]]` or `![alt](img.png)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteImageRef {
    pub raw_target: String,
    pub display_name: String,
    pub abs_path: PathBuf,
    pub mime_type: String,
    pub line_no: usize,
}

/// Backlink (incoming mention) from another note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklinkEntry {
    pub source_path: String,
    pub source_title: String,
    pub context_snippet: String,
    pub heading: Option<String>,
    pub is_embed: bool,
}

/// Outgoing wikilink from the active note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingLinkEntry {
    pub target_raw: String,
    pub target_resolved_path: Option<String>,
    pub target_title: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub is_embed: bool,
    pub resolved: bool,
}

/// Tag aggregate count across a vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

/// Knowledge Graph node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub title: String,
    pub folder: String,
    pub resolved: bool,
    pub depth: usize,
    pub in_degree: usize,
    pub out_degree: usize,
    pub tags: Vec<String>,
}

/// Knowledge Graph directed edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub is_embed: bool,
}

/// Local or global knowledge graph payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphData {
    pub center: Option<String>,
    pub depth: usize,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Full-text search hit returned by SQLite FTS5 or short-query Korean LIKE fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub rel_path: String,
    pub title: String,
    pub folder: String,
    pub snippet: String,
    pub tags: Vec<String>,
    pub word_count: usize,
    pub mtime_ms: i64,
}

/// Vault incremental index scan report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub vault_id: String,
    pub vault_name: String,
    pub scanned: usize,
    pub updated: usize,
    pub removed: usize,
    pub total_notes: usize,
    pub total_links: usize,
    pub total_tags: usize,
    pub total_tasks: usize,
    pub elapsed_ms: u64,
}

/// Vault summary returned by CLI/HTTP/MCP `vaults`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultSummary {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub active: bool,
    pub discovered: bool,
    pub open_in_obsidian: bool,
    pub exists: bool,
    pub note_count: usize,
}

/// Filter for listing checklist tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskFilter {
    #[default]
    All,
    Open,
    Done,
}

/// Left sidebar active tab (`1 Files`, `2 Tags`, `3 Bookmarks`, `4 Outline`, `5 Tasks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarTab {
    #[default]
    Files,
    Tags,
    Bookmarks,
    Outline,
    Tasks,
}

impl SidebarTab {
    pub fn all() -> &'static [SidebarTab] {
        &[
            Self::Files,
            Self::Tags,
            Self::Bookmarks,
            Self::Outline,
            Self::Tasks,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Tags => "Tags",
            Self::Bookmarks => "Marks",
            Self::Outline => "Outl",
            Self::Tasks => "Tasks",
        }
    }

    pub fn key_num(self) -> char {
        match self {
            Self::Files => '1',
            Self::Tags => '2',
            Self::Bookmarks => '3',
            Self::Outline => '4',
            Self::Tasks => '5',
        }
    }
}

/// Clipboard inline PNG staged before saving into vault attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImageUpload {
    pub temp_id: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

/// Result of `bitefoundry-obsidian update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

/// Events pushed from the background vault watcher/daemon to the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    VaultIndexed {
        stats: IndexStats,
    },
    NoteChanged {
        rel_path: String,
    },
    SyncStatus {
        busy: bool,
        summary: String,
    },
    Toast {
        level: String,
        message: String,
    },
}
