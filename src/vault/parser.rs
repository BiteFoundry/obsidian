//! Obsidian-flavored Markdown & YAML Frontmatter parser.
//!
//! Powered by `bitefoundry_html::pulldown_cmark` (`into_offset_iter()`) with an Obsidian
//! extension layer for:
//! - YAML Frontmatter (`--- ... ---`) via `vault/frontmatter.rs` (round-trip preserving)
//! - Obsidian `%%comments%%` masking with invariant 1-based `line_no`
//! - Wikilinks & Embeds: `[[Note]]`, `[[Note|Alias]]`, `[[Note#Heading]]`, `![[image.png|400]]`
//! - Inline highlights (`==highlight==`) and tags (`#tag`, `#nested/tag`) outside code/URLs/escapes
//! - Obsidian Callouts (`> [!note]`, `> [!tip]-`, nested callouts & blockquotes)
//! - GFM Tables with styled `Vec<InlineSpan>` cells and column alignments
//! - Math (`$...$`, `$$...$$`), Footnotes (`[^1]`, `[^1]: ...`), inline HTML (`<br>`, `<u>`,
//!   `<mark>`, `<sub>`, `<sup>`, `<a>`) and embedded HTML blocks (`NoteBlock::HtmlEmbed`)
//! - Exact 1-based `line_no` for headings, tasks, callouts, tables, and embeds.

use std::collections::{BTreeMap, BTreeSet};

use bitefoundry_html::pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use serde_json::Value;

pub use super::frontmatter::upsert_frontmatter_in_markdown;
use super::frontmatter::{closing_delimiter, opening_delimiter, read_properties};

use crate::models::{
    CalloutKind, HeadingEntry, InlineSpan, MarkdownLinkRef, NoteBlock, ParsedNote, TableAlignment,
    TaskEntry, WikilinkRef,
};

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg"];

pub fn is_image_target(target: &str) -> bool {
    let base = target
        .split(['#', '?'])
        .next()
        .unwrap_or(target)
        .trim()
        .trim_matches(['<', '>']);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    if matches!(lower.as_str(), "html" | "htm" | "xhtml") {
        return false;
    }
    IMAGE_EXTENSIONS.contains(&lower.as_str())
}

pub fn mime_type_for_path(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn obsidian_cmark_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InlineStyleFlags {
    bold: bool,
    italic: bool,
    strike: bool,
    highlight: bool,
    underline: bool,
}

impl InlineStyleFlags {
    fn to_span(self, text: String) -> InlineSpan {
        if text.is_empty() {
            return InlineSpan::Text { text };
        }
        let active_count = (self.bold as u8)
            + (self.italic as u8)
            + (self.strike as u8)
            + (self.highlight as u8)
            + (self.underline as u8);
        match active_count {
            0 => InlineSpan::Text { text },
            1 => {
                if self.bold {
                    InlineSpan::Bold { text }
                } else if self.italic {
                    InlineSpan::Italic { text }
                } else if self.strike {
                    InlineSpan::Strikethrough { text }
                } else if self.highlight {
                    InlineSpan::Highlight { text }
                } else {
                    InlineSpan::Underline { text }
                }
            }
            _ => InlineSpan::Styled {
                text,
                bold: self.bold,
                italic: self.italic,
                strike: self.strike,
                highlight: self.highlight,
                underline: self.underline,
            },
        }
    }
}

#[derive(Debug, Clone)]
enum PendingEmbed {
    Wikilink {
        target: String,
        heading: Option<String>,
        alias: Option<String>,
        line_no: usize,
    },
    MarkdownImage {
        alt: String,
        url: String,
        line_no: usize,
    },
}

#[derive(Debug, Clone)]
enum BlockContainer {
    Root(Vec<NoteBlock>),
    BlockQuote {
        gfm_kind: Option<BlockQuoteKind>,
        blocks: Vec<NoteBlock>,
        line_no: usize,
    },
}

impl BlockContainer {
    fn push_block(&mut self, block: NoteBlock) {
        match self {
            Self::Root(blocks) | Self::BlockQuote { blocks, .. } => blocks.push(block),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ListKind {
    Unordered,
    Ordered(u64),
}

#[derive(Debug, Clone)]
struct ActiveListItem {
    line_no: usize,
    indent: usize,
    ordered_number: Option<String>,
    task_checked: Option<bool>,
    emitted: bool,
}

#[derive(Debug, Clone)]
struct ActiveTable {
    line_no: usize,
    alignments: Vec<TableAlignment>,
    headers: Vec<Vec<InlineSpan>>,
    rows: Vec<Vec<Vec<InlineSpan>>>,
    in_head: bool,
    cur_row: Vec<Vec<InlineSpan>>,
    in_cell: bool,
}

#[derive(Debug, Clone)]
struct ActiveLink {
    url: String,
    title: Option<String>,
    line_no: usize,
    label_buf: String,
}

#[derive(Debug, Clone)]
struct ActiveImage {
    url: String,
    title: Option<String>,
    line_no: usize,
    alt_buf: String,
}

#[derive(Debug, Clone)]
struct ActiveFootnoteDef {
    label: String,
    line_no: usize,
    spans: Vec<InlineSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineHtmlTagKind {
    Style,
    Sub,
    Sup,
    Link,
}

/// Parses an Obsidian Markdown document into structured metadata, graph references, tasks, outline, and UI blocks.
pub fn parse_markdown(rel_path: &str, note_title: &str, raw: &str) -> ParsedNote {
    let raw_no_bom = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let lines: Vec<&str> = raw_no_bom.lines().collect();
    let (frontmatter, body_start_idx) = parse_frontmatter(&lines);

    let mut tag_set: BTreeSet<String> = BTreeSet::new();
    let mut aliases: Vec<String> = Vec::new();

    if let Some(v) = frontmatter.get("tags").or_else(|| frontmatter.get("tag")) {
        for t in extract_string_list(v) {
            if let Some(clean) = normalize_tag(&t) {
                tag_set.insert(clean);
            }
        }
    }
    if let Some(v) = frontmatter
        .get("aliases")
        .or_else(|| frontmatter.get("alias"))
    {
        for a in extract_string_list(v) {
            let trimmed = a.trim();
            if !trimmed.is_empty() && !aliases.iter().any(|x| x.eq_ignore_ascii_case(trimmed)) {
                aliases.push(trimmed.to_string());
            }
        }
    }

    let body_lines = if body_start_idx <= lines.len() {
        &lines[body_start_idx..]
    } else {
        &[]
    };
    let body_markdown = body_lines.join("\n");
    let preprocessed = preprocess_obsidian_body(&body_markdown);
    let line_starts = compute_line_starts(&preprocessed);

    let mut wikilinks = Vec::new();
    let mut md_links = Vec::new();
    let mut headings = Vec::new();
    let mut tasks = Vec::new();
    let mut plain_parts: Vec<String> = Vec::new();

    let blocks = parse_body_events(
        rel_path,
        note_title,
        &preprocessed,
        body_lines,
        &line_starts,
        body_start_idx,
        &mut wikilinks,
        &mut md_links,
        &mut tag_set,
        &mut headings,
        &mut tasks,
        &mut plain_parts,
    );

    let body_plain = plain_parts.join("\n");
    let word_count = body_plain.split_whitespace().count();

    ParsedNote {
        frontmatter,
        aliases,
        tags: tag_set.into_iter().collect(),
        wikilinks,
        md_links,
        headings,
        tasks,
        blocks,
        body_markdown,
        body_plain,
        word_count,
        body_start_line: body_start_idx + 1,
    }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_index_at(line_starts: &[usize], byte_offset: usize) -> usize {
    line_starts
        .partition_point(|&start| start <= byte_offset)
        .saturating_sub(1)
}

/// Pre-processes Obsidian-specific syntax outside code fences and inline code while preserving
/// the exact `\n` count on every line so all 1-based `line_no` offsets stay 100% invariant:
/// 1. Strips non-newline characters inside paired `%%...%%` comments.
/// 2. Escapes unescaped `|` inside `[[...]]` / `![[...]]` on table-like lines so `pulldown-cmark`
///    never splits `| **Bold** [[Note|Alias]] |` across columns.
fn preprocess_obsidian_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut at_line_start = true;
    let mut in_fence: Option<(char, usize)> = None;

    while i < len {
        if at_line_start {
            let line_end = body[i..].find('\n').map_or(len, |rel| i + rel);
            let line = &body[i..line_end];
            let trimmed = line.trim();

            if let Some((fc, flen)) = in_fence {
                let close_len = trimmed.chars().take_while(|&c| c == fc).count();
                if close_len >= flen && trimmed[close_len..].trim().is_empty() {
                    in_fence = None;
                }
                out.push_str(line);
                if line_end < len {
                    out.push('\n');
                    i = line_end + 1;
                } else {
                    i = len;
                }
                continue;
            }

            if let Some((fc, flen)) = is_code_fence_opener(trimmed) {
                in_fence = Some((fc, flen));
                out.push_str(line);
                if line_end < len {
                    out.push('\n');
                    i = line_end + 1;
                } else {
                    i = len;
                }
                continue;
            }
            at_line_start = false;
        }

        let b = bytes[i];
        if b == b'\n' {
            out.push('\n');
            i += 1;
            at_line_start = true;
            continue;
        }

        // Preserve backslash escapes verbatim
        if b == b'\\' && i + 1 < len {
            let next_ch = body[i + 1..].chars().next().unwrap();
            out.push('\\');
            out.push(next_ch);
            i += 1 + next_ch.len_utf8();
            if next_ch == '\n' {
                at_line_start = true;
            }
            continue;
        }

        // Preserve inline code spans verbatim (never mask %% or rewrite [[|]] inside code)
        if b == b'`' {
            if let Some(end) = find_inline_code_end(body, i) {
                out.push_str(&body[i..end]);
                i = end;
                continue;
            }
            let ticks = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            out.push_str(&body[i..i + ticks]);
            i += ticks;
            continue;
        }

        // Mask Obsidian `%%...%%` comments while keeping exact `\n` count
        if b == b'%' && i + 1 < len && bytes[i + 1] == b'%' {
            if let Some(close_rel) = find_closing_obsidian_comment(&body[i + 2..]) {
                let comment_inner = &body[i + 2..i + 2 + close_rel];
                for ch in comment_inner.chars() {
                    if ch == '\n' {
                        out.push('\n');
                    }
                }
                i += 2 + close_rel + 2;
                if i > 0 && bytes[i - 1] == b'\n' {
                    at_line_start = true;
                }
                continue;
            }
        }

        // Protect unescaped `|` inside `[[...]]` / `![[...]]` on table-like lines
        if b == b'[' && i + 1 < len && bytes[i + 1] == b'[' {
            let line_start = body[..i].rfind('\n').map_or(0, |p| p + 1);
            let line_end = body[i..].find('\n').map_or(len, |rel| i + rel);
            if let Some(rel_close) = body[i + 2..line_end].find("]]") {
                let inner = &body[i + 2..i + 2 + rel_close];
                if !inner.contains("[[") && inner.contains('|') {
                    let before_on_line = &body[line_start..i];
                    let after_on_line = &body[i + 2 + rel_close + 2..line_end];
                    let on_table_line = before_on_line.contains('|') || after_on_line.contains('|');
                    if on_table_line {
                        out.push_str("[[");
                        let ib = inner.as_bytes();
                        let mut k = 0usize;
                        while k < ib.len() {
                            if ib[k] == b'\\' && k + 1 < ib.len() {
                                let ch = inner[k + 1..].chars().next().unwrap();
                                out.push('\\');
                                out.push(ch);
                                k += 1 + ch.len_utf8();
                            } else if ib[k] == b'|' {
                                out.push_str("\\|");
                                k += 1;
                            } else {
                                let ch = inner[k..].chars().next().unwrap();
                                out.push(ch);
                                k += ch.len_utf8();
                            }
                        }
                        out.push_str("]]");
                        i += 2 + rel_close + 2;
                        continue;
                    }
                }
            }
        }

        let ch = body[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn find_closing_obsidian_comment(after_open: &str) -> Option<usize> {
    let bytes = after_open.as_bytes();
    let mut k = 0usize;
    while k + 1 < bytes.len() {
        if bytes[k] == b'\\' && k + 1 < bytes.len() {
            k += 2;
            continue;
        }
        if bytes[k] == b'%' && bytes[k + 1] == b'%' {
            return Some(k);
        }
        k += 1;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn parse_body_events(
    rel_path: &str,
    note_title: &str,
    src: &str,
    body_lines: &[&str],
    line_starts: &[usize],
    body_start_idx: usize,
    wikilinks: &mut Vec<WikilinkRef>,
    md_links: &mut Vec<MarkdownLinkRef>,
    tag_set: &mut BTreeSet<String>,
    headings: &mut Vec<HeadingEntry>,
    tasks: &mut Vec<TaskEntry>,
    plain_parts: &mut Vec<String>,
) -> Vec<NoteBlock> {
    let opts = obsidian_cmark_options();
    let line_no_at = |offset: usize| -> usize {
        let idx = line_index_at(line_starts, offset);
        body_start_idx + idx + 1
    };
    let indent_at = |offset: usize| -> usize {
        let idx = line_index_at(line_starts, offset);
        body_lines
            .get(idx)
            .map(|l| l.chars().take_while(|&c| c == ' ' || c == '\t').count())
            .unwrap_or(0)
    };

    let mut containers: Vec<BlockContainer> = vec![BlockContainer::Root(Vec::new())];
    let mut style_stack: Vec<InlineStyleFlags> = vec![InlineStyleFlags::default()];
    let mut inline_html_stack: Vec<InlineHtmlTagKind> = Vec::new();
    let mut sub_depth = 0usize;
    let mut sup_depth = 0usize;

    let mut cur_spans: Vec<InlineSpan> = Vec::new();
    let mut pending_text = String::new();
    let mut pending_text_line = body_start_idx + 1;

    let mut active_heading: Option<(u8, usize)> = None;
    let mut active_para: Option<(usize, usize, bool)> = None; // (start_offset, line_no, has_complex_html)
    let mut active_code: Option<(String, bool, usize, String)> = None; // (language, is_indented, line_no, buf)
    let mut active_html: Option<(usize, String)> = None;
    let mut active_table: Option<ActiveTable> = None;
    let mut active_link: Option<ActiveLink> = None;
    let mut active_image: Option<ActiveImage> = None;
    let mut active_footnote: Option<ActiveFootnoteDef> = None;

    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut item_stack: Vec<ActiveListItem> = Vec::new();

    // Tracks standalone embed candidacy for the current line of an active paragraph
    // (`![[...]]` or `![alt](img)`), including tight lines separated only by `SoftBreak`.
    let mut para_line_start_idx: usize = 0;
    let mut para_line_embed: Option<PendingEmbed> = None;
    let mut para_line_has_other: bool = false;
    let mut para_line_no: usize = body_start_idx + 1;

    let current_flags = |stack: &[InlineStyleFlags]| -> InlineStyleFlags {
        stack.last().copied().unwrap_or_default()
    };

    let push_styled_text = |spans: &mut Vec<InlineSpan>,
                            text: String,
                            flags: InlineStyleFlags,
                            sub_d: usize,
                            sup_d: usize| {
        if text.is_empty() {
            return;
        }
        if sub_d > 0 {
            spans.push(InlineSpan::Subscript { text });
        } else if sup_d > 0 {
            spans.push(InlineSpan::Superscript { text });
        } else {
            spans.push(flags.to_span(text));
        }
    };

    let flush_pending_text = |pending_text: &mut String,
                              pending_text_line: usize,
                              cur_spans: &mut Vec<InlineSpan>,
                              style_stack: &[InlineStyleFlags],
                              sub_depth: usize,
                              sup_depth: usize,
                              active_link: &mut Option<ActiveLink>,
                              active_image: &mut Option<ActiveImage>,
                              active_para: Option<(usize, usize, bool)>,
                              para_line_start_idx: usize,
                              para_line_embed: &mut Option<PendingEmbed>,
                              para_line_has_other: &mut bool,
                              wikilinks: &mut Vec<WikilinkRef>,
                              md_links: &mut Vec<MarkdownLinkRef>,
                              tag_set: &mut BTreeSet<String>| {
        if pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(pending_text);
        if let Some(img) = active_image.as_mut() {
            img.alt_buf.push_str(&text);
            return;
        }
        if let Some(link) = active_link.as_mut() {
            link.label_buf.push_str(&text);
            return;
        }

        let flags = current_flags(style_stack);
        let prev_char = last_char_of_spans(cur_spans);
        let before_len = cur_spans.len();
        let mut extracted_embed: Option<PendingEmbed> = None;
        let scan_standalone = active_para.is_some()
            && before_len == para_line_start_idx
            && !*para_line_has_other
            && para_line_embed.is_none();

        scan_obsidian_extensions(
            &text,
            pending_text_line,
            flags,
            sub_depth,
            sup_depth,
            prev_char,
            cur_spans,
            wikilinks,
            md_links,
            tag_set,
            scan_standalone.then_some(&mut extracted_embed),
        );

        if active_para.is_some() {
            let added = &cur_spans[before_len..];
            if scan_standalone && extracted_embed.is_some() && added.len() == 1 {
                *para_line_embed = extracted_embed;
            } else if added.iter().any(|s| !s.plain_text().trim().is_empty()) {
                *para_line_has_other = true;
            }
        }
    };

    let flush_list_item_if_needed = |item_stack: &mut [ActiveListItem],
                                     cur_spans: &mut Vec<InlineSpan>,
                                     containers: &mut [BlockContainer],
                                     tasks: &mut Vec<TaskEntry>,
                                     plain_parts: &mut Vec<String>| {
        let Some(item) = item_stack.last_mut() else {
            return;
        };
        if item.emitted {
            return;
        }
        if cur_spans.is_empty() && item.task_checked.is_none() {
            return;
        }
        item.emitted = true;
        let mut spans = std::mem::take(cur_spans);
        apply_cross_span_highlights(&mut spans);
        let clean_text = spans_to_plain(&spans).trim().to_string();
        if !clean_text.is_empty() {
            plain_parts.push(clean_text.clone());
        }
        let block = if let Some(checked) = item.task_checked {
            tasks.push(TaskEntry {
                note_path: rel_path.to_string(),
                note_title: note_title.to_string(),
                line_no: item.line_no,
                text: clean_text.clone(),
                checked,
            });
            NoteBlock::Task {
                checked,
                indent: item.indent,
                text: clean_text,
                spans,
                line_no: item.line_no,
            }
        } else if let Some(number) = item.ordered_number.clone() {
            NoteBlock::NumberedItem {
                number,
                indent: item.indent,
                spans,
                line_no: item.line_no,
            }
        } else {
            NoteBlock::BulletItem {
                indent: item.indent,
                spans,
                line_no: item.line_no,
            }
        };
        if let Some(container) = containers.last_mut() {
            container.push_block(block);
        }
    };

    for (event, range) in Parser::new_ext(src, opts).into_offset_iter() {
        // Fast paths inside raw code or HTML blocks
        if let Some((_, _, _, ref mut code_buf)) = active_code {
            match event {
                Event::Text(t) | Event::Code(t) => {
                    code_buf.push_str(&t);
                    continue;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (language, is_indented, line_no, mut code) = active_code.take().unwrap();
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    // Obsidian allows top-level 4-space-indented task lines (`    - [ ] ...`).
                    if is_indented
                        && !code.trim().is_empty()
                        && code
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .all(|l| parse_single_task_line(l).is_some())
                    {
                        for (rel_idx, raw_line) in code.lines().enumerate() {
                            if raw_line.trim().is_empty() {
                                continue;
                            }
                            let task_ln = line_no + rel_idx;
                            let orig_idx = task_ln.saturating_sub(body_start_idx + 1);
                            let orig_line = body_lines.get(orig_idx).copied().unwrap_or(raw_line);
                            let indent = orig_line
                                .chars()
                                .take_while(|&c| c == ' ' || c == '\t')
                                .count();
                            let trimmed = &orig_line[indent..];
                            let after_bullet = trimmed
                                .strip_prefix("- ")
                                .or_else(|| trimmed.strip_prefix("* "))
                                .or_else(|| trimmed.strip_prefix("+ "))
                                .unwrap_or(trimmed);
                            let (checked, content) =
                                if let Some(r) = after_bullet.strip_prefix("[ ] ") {
                                    (false, r)
                                } else if after_bullet == "[ ]" {
                                    (false, "")
                                } else if let Some(r) = after_bullet
                                    .strip_prefix("[x] ")
                                    .or_else(|| after_bullet.strip_prefix("[X] "))
                                {
                                    (true, r)
                                } else {
                                    (true, "")
                                };
                            let spans = parse_inline_spans(
                                content,
                                task_ln,
                                wikilinks,
                                md_links,
                                tag_set,
                            );
                            let clean_text = spans_to_plain(&spans).trim().to_string();
                            if !clean_text.is_empty() {
                                plain_parts.push(clean_text.clone());
                            }
                            tasks.push(TaskEntry {
                                note_path: rel_path.to_string(),
                                note_title: note_title.to_string(),
                                line_no: task_ln,
                                text: clean_text.clone(),
                                checked,
                            });
                            if let Some(c) = containers.last_mut() {
                                c.push_block(NoteBlock::Task {
                                    checked,
                                    indent,
                                    text: clean_text,
                                    spans,
                                    line_no: task_ln,
                                });
                            }
                        }
                        continue;
                    }
                    if !code.is_empty() {
                        plain_parts.push(code.clone());
                    }
                    if let Some(c) = containers.last_mut() {
                        c.push_block(NoteBlock::CodeBlock {
                            language,
                            code,
                            line_no,
                        });
                    }
                    continue;
                }
                _ => continue,
            }
        }

        if let Some((_, ref mut html_buf)) = active_html {
            match event {
                Event::Html(t) | Event::Text(t) => {
                    html_buf.push_str(&t);
                    continue;
                }
                Event::End(TagEnd::HtmlBlock) => {
                    let (line_no, raw_html) = active_html.take().unwrap();
                    push_html_embed_block(raw_html, line_no, &mut containers, plain_parts);
                    continue;
                }
                _ => continue,
            }
        }

        // Coalesce unescaped `Event::Text` runs so Obsidian extensions like `[[Note|Alias]]`
        // and `==highlight==` are seen intact across pulldown-cmark's punctuation chunks.
        if let Event::Text(ref t) = event {
            let is_escaped_punct = src.as_bytes().get(range.start) == Some(&b'\\')
                && range.len() == 2
                && t.len() == 1
                && t.as_ref() != "|";
            if is_escaped_punct {
                flush_pending_text(
                    &mut pending_text,
                    pending_text_line,
                    &mut cur_spans,
                    &style_stack,
                    sub_depth,
                    sup_depth,
                    &mut active_link,
                    &mut active_image,
                    active_para,
                    para_line_start_idx,
                    &mut para_line_embed,
                    &mut para_line_has_other,
                    wikilinks,
                    md_links,
                    tag_set,
                );
                if let Some(img) = active_image.as_mut() {
                    img.alt_buf.push_str(t);
                } else if let Some(link) = active_link.as_mut() {
                    link.label_buf.push_str(t);
                } else {
                    push_styled_text(
                        &mut cur_spans,
                        t.to_string(),
                        current_flags(&style_stack),
                        sub_depth,
                        sup_depth,
                    );
                    if active_para.is_some() {
                        para_line_has_other = true;
                    }
                }
            } else {
                if pending_text.is_empty() {
                    pending_text_line = line_no_at(range.start);
                }
                pending_text.push_str(t);
            }
            continue;
        }

        // Any non-Text event flushes buffered text first.
        flush_pending_text(
            &mut pending_text,
            pending_text_line,
            &mut cur_spans,
            &style_stack,
            sub_depth,
            sup_depth,
            &mut active_link,
            &mut active_image,
            active_para,
            para_line_start_idx,
            &mut para_line_embed,
            &mut para_line_has_other,
            wikilinks,
            md_links,
            tag_set,
        );

        let ev_line = line_no_at(range.start);

        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    cur_spans.clear();
                    active_heading = Some((heading_level_u8(level), ev_line));
                }
                Tag::Paragraph => {
                    if active_footnote.is_some() {
                        if let Some(fn_def) = active_footnote.as_mut() {
                            if !fn_def.spans.is_empty() {
                                fn_def.spans.push(InlineSpan::LineBreak);
                            }
                        }
                    } else if item_stack.is_empty()
                        || item_stack.last().is_some_and(|i| i.emitted)
                    {
                        cur_spans.clear();
                        para_line_start_idx = 0;
                        para_line_embed = None;
                        para_line_has_other = false;
                        para_line_no = ev_line;
                        active_para = Some((range.start, ev_line, false));
                    } else {
                        // Paragraph inside a loose list item: keep accumulating into item spans
                        if !cur_spans.is_empty() {
                            cur_spans.push(InlineSpan::LineBreak);
                        }
                    }
                }
                Tag::BlockQuote(gfm_kind) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    containers.push(BlockContainer::BlockQuote {
                        gfm_kind,
                        blocks: Vec::new(),
                        line_no: ev_line,
                    });
                }
                Tag::CodeBlock(kind) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    let (language, is_indented) = match kind {
                        CodeBlockKind::Fenced(info) => (info.trim().to_string(), false),
                        CodeBlockKind::Indented => (String::new(), true),
                    };
                    active_code = Some((language, is_indented, ev_line, String::new()));
                }
                Tag::HtmlBlock => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    active_html = Some((ev_line, String::new()));
                }
                Tag::List(start_num) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    let kind = match start_num {
                        Some(n) => ListKind::Ordered(n),
                        None => ListKind::Unordered,
                    };
                    list_stack.push(kind);
                }
                Tag::Item => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    let ordered_number = match list_stack.last_mut() {
                        Some(ListKind::Ordered(n)) => {
                            let num = n.to_string();
                            *n += 1;
                            Some(num)
                        }
                        _ => None,
                    };
                    cur_spans.clear();
                    item_stack.push(ActiveListItem {
                        line_no: ev_line,
                        indent: indent_at(range.start),
                        ordered_number,
                        task_checked: None,
                        emitted: false,
                    });
                }
                Tag::Table(alignments) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    active_table = Some(ActiveTable {
                        line_no: ev_line,
                        alignments: alignments.into_iter().map(map_alignment).collect(),
                        headers: Vec::new(),
                        rows: Vec::new(),
                        in_head: false,
                        cur_row: Vec::new(),
                        in_cell: false,
                    });
                }
                Tag::TableHead => {
                    if let Some(t) = active_table.as_mut() {
                        t.in_head = true;
                        t.cur_row.clear();
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = active_table.as_mut() {
                        t.cur_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = active_table.as_mut() {
                        t.in_cell = true;
                        cur_spans.clear();
                    }
                }
                Tag::FootnoteDefinition(label) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    cur_spans.clear();
                    active_footnote = Some(ActiveFootnoteDef {
                        label: label.to_string(),
                        line_no: ev_line,
                        spans: Vec::new(),
                    });
                }
                Tag::Strong => {
                    let mut next = current_flags(&style_stack);
                    next.bold = true;
                    style_stack.push(next);
                }
                Tag::Emphasis => {
                    let mut next = current_flags(&style_stack);
                    next.italic = true;
                    style_stack.push(next);
                }
                Tag::Strikethrough => {
                    let mut next = current_flags(&style_stack);
                    next.strike = true;
                    style_stack.push(next);
                }
                Tag::Link {
                    dest_url, title, ..
                } => {
                    let title_opt = (!title.is_empty()).then(|| title.to_string());
                    active_link = Some(ActiveLink {
                        url: dest_url.to_string(),
                        title: title_opt,
                        line_no: ev_line,
                        label_buf: String::new(),
                    });
                }
                Tag::Image {
                    dest_url, title, ..
                } => {
                    let title_opt = (!title.is_empty()).then(|| title.to_string());
                    active_image = Some(ActiveImage {
                        url: dest_url.to_string(),
                        title: title_opt,
                        line_no: ev_line,
                        alt_buf: String::new(),
                    });
                }
                _ => {}
            },

            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    if let Some((level, line_no)) = active_heading.take() {
                        let mut spans = std::mem::take(&mut cur_spans);
                        apply_cross_span_highlights(&mut spans);
                        let clean_text = spans_to_plain(&spans);
                        headings.push(HeadingEntry {
                            note_path: rel_path.to_string(),
                            level,
                            text: clean_text.clone(),
                            line_no,
                        });
                        if !clean_text.is_empty() {
                            plain_parts.push(clean_text.clone());
                        }
                        if let Some(c) = containers.last_mut() {
                            c.push_block(NoteBlock::Heading {
                                level,
                                text: clean_text,
                                spans,
                                line_no,
                            });
                        }
                    }
                }
                TagEnd::Paragraph => {
                    if let Some(fn_def) = active_footnote.as_mut() {
                        let mut spans = std::mem::take(&mut cur_spans);
                        apply_cross_span_highlights(&mut spans);
                        fn_def.spans.extend(spans);
                    } else if let Some((start_off, line_no, has_complex_html)) = active_para.take()
                    {
                        let embed_cand = para_line_embed.take();
                        let line_has_other = para_line_has_other;
                        let line_start = para_line_start_idx;
                        para_line_start_idx = 0;
                        para_line_has_other = false;

                        let raw_slice = src.get(start_off..range.end).unwrap_or("").trim();
                        if has_complex_html || looks_like_standalone_html_paragraph(raw_slice) {
                            cur_spans.clear();
                            push_html_embed_block(
                                raw_slice.to_string(),
                                line_no,
                                &mut containers,
                                plain_parts,
                            );
                        } else if !line_has_other && embed_cand.is_some() {
                            let embed = embed_cand.unwrap();
                            let embed_block = build_standalone_embed_block(embed);
                            if let Some(eb) = embed_block {
                                // Emit any preceding paragraph lines before this standalone embed line.
                                let prefix_spans = trim_trailing_space_spans(&cur_spans[..line_start]);
                                if !prefix_spans.is_empty() {
                                    let mut p_spans = prefix_spans;
                                    apply_cross_span_highlights(&mut p_spans);
                                    let plain = spans_to_plain(&p_spans);
                                    if !plain.trim().is_empty() {
                                        plain_parts.push(plain);
                                    }
                                    if let Some(c) = containers.last_mut() {
                                        c.push_block(NoteBlock::Paragraph {
                                            spans: p_spans,
                                            line_no: para_line_no,
                                        });
                                    }
                                }
                                cur_spans.clear();
                                if let Some(c) = containers.last_mut() {
                                    c.push_block(eb);
                                }
                            } else {
                                let mut spans = std::mem::take(&mut cur_spans);
                                apply_cross_span_highlights(&mut spans);
                                let plain = spans_to_plain(&spans);
                                if !plain.trim().is_empty() {
                                    plain_parts.push(plain);
                                }
                                if !spans.is_empty() {
                                    if let Some(c) = containers.last_mut() {
                                        c.push_block(NoteBlock::Paragraph {
                                            spans,
                                            line_no: para_line_no,
                                        });
                                    }
                                }
                            }
                        } else {
                            let mut spans = std::mem::take(&mut cur_spans);
                            apply_cross_span_highlights(&mut spans);
                            let plain = spans_to_plain(&spans);
                            if !plain.trim().is_empty() {
                                plain_parts.push(plain);
                            }
                            if !spans.is_empty() {
                                if let Some(c) = containers.last_mut() {
                                    c.push_block(NoteBlock::Paragraph {
                                        spans,
                                        line_no: para_line_no,
                                    });
                                }
                            }
                        }
                    }
                }
                TagEnd::BlockQuote(_) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    if containers.len() > 1 {
                        if let Some(BlockContainer::BlockQuote {
                            gfm_kind,
                            blocks,
                            line_no,
                        }) = containers.pop()
                        {
                            let finalized =
                                finalize_blockquote(gfm_kind, blocks, line_no, plain_parts);
                            if let Some(parent) = containers.last_mut() {
                                parent.push_block(finalized);
                            }
                        }
                    }
                }
                TagEnd::List(_) => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    list_stack.pop();
                }
                TagEnd::Item => {
                    flush_list_item_if_needed(
                        &mut item_stack,
                        &mut cur_spans,
                        &mut containers,
                        tasks,
                        plain_parts,
                    );
                    item_stack.pop();
                }
                TagEnd::TableCell => {
                    if let Some(t) = active_table.as_mut() {
                        t.in_cell = false;
                        let mut cell_spans = std::mem::take(&mut cur_spans);
                        apply_cross_span_highlights(&mut cell_spans);
                        let cell_plain = spans_to_plain(&cell_spans);
                        if !cell_plain.trim().is_empty() {
                            plain_parts.push(cell_plain);
                        }
                        t.cur_row.push(cell_spans);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = active_table.as_mut() {
                        t.headers = std::mem::take(&mut t.cur_row);
                        t.in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = active_table.as_mut() {
                        let row = std::mem::take(&mut t.cur_row);
                        if t.in_head {
                            t.headers = row;
                        } else {
                            t.rows.push(row);
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = active_table.take() {
                        if let Some(c) = containers.last_mut() {
                            c.push_block(NoteBlock::Table {
                                headers: t.headers,
                                rows: t.rows,
                                alignments: t.alignments,
                                line_no: t.line_no,
                            });
                        }
                    }
                }
                TagEnd::FootnoteDefinition => {
                    if let Some(mut fn_def) = active_footnote.take() {
                        if !cur_spans.is_empty() {
                            let mut rem = std::mem::take(&mut cur_spans);
                            apply_cross_span_highlights(&mut rem);
                            fn_def.spans.extend(rem);
                        }
                        let plain = spans_to_plain(&fn_def.spans);
                        if !plain.trim().is_empty() {
                            plain_parts.push(plain);
                        }
                        if let Some(c) = containers.last_mut() {
                            c.push_block(NoteBlock::FootnoteDef {
                                label: fn_def.label,
                                spans: fn_def.spans,
                                line_no: fn_def.line_no,
                            });
                        }
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::Link => {
                    if let Some(link) = active_link.take() {
                        let label = if link.label_buf.is_empty() {
                            link.url.clone()
                        } else {
                            link.label_buf
                        };
                        md_links.push(MarkdownLinkRef {
                            label: label.clone(),
                            url: link.url.clone(),
                            title: link.title,
                            is_image: false,
                            line_no: link.line_no,
                        });
                        cur_spans.push(InlineSpan::ExternalLink {
                            label,
                            url: link.url,
                        });
                        if active_para.is_some() {
                            para_line_has_other = true;
                        }
                    }
                }
                TagEnd::Image => {
                    if let Some(img) = active_image.take() {
                        let alt = img.alt_buf;
                        md_links.push(MarkdownLinkRef {
                            label: alt.clone(),
                            url: img.url.clone(),
                            title: img.title,
                            is_image: true,
                            line_no: img.line_no,
                        });
                        let label = if alt.is_empty() {
                            img.url.clone()
                        } else {
                            alt.clone()
                        };
                        let is_only_item = active_para.is_some()
                            && cur_spans.len() == para_line_start_idx
                            && !para_line_has_other
                            && para_line_embed.is_none();
                        cur_spans.push(InlineSpan::ExternalLink {
                            label,
                            url: img.url.clone(),
                        });
                        if is_only_item {
                            para_line_embed = Some(PendingEmbed::MarkdownImage {
                                alt,
                                url: img.url,
                                line_no: img.line_no,
                            });
                        } else if active_para.is_some() {
                            para_line_has_other = true;
                        }
                    }
                }
                _ => {}
            },

            Event::Code(code) => {
                if let Some(img) = active_image.as_mut() {
                    img.alt_buf.push_str(&code);
                } else if let Some(link) = active_link.as_mut() {
                    link.label_buf.push_str(&code);
                } else {
                    cur_spans.push(InlineSpan::Code {
                        text: code.to_string(),
                    });
                    if active_para.is_some() {
                        para_line_has_other = true;
                    }
                }
            }

            Event::InlineMath(math) => {
                if let Some(link) = active_link.as_mut() {
                    link.label_buf.push_str(&math);
                } else {
                    cur_spans.push(InlineSpan::Math {
                        text: math.to_string(),
                        display: false,
                    });
                    if active_para.is_some() {
                        para_line_has_other = true;
                    }
                }
            }

            Event::DisplayMath(math) => {
                cur_spans.push(InlineSpan::Math {
                    text: math.to_string(),
                    display: true,
                });
                if active_para.is_some() {
                    para_line_has_other = true;
                }
            }

            Event::FootnoteReference(label) => {
                cur_spans.push(InlineSpan::FootnoteRef {
                    label: label.to_string(),
                });
                if active_para.is_some() {
                    para_line_has_other = true;
                }
            }

            Event::InlineHtml(raw_tag) => {
                handle_inline_html(
                    &raw_tag,
                    ev_line,
                    &mut style_stack,
                    &mut inline_html_stack,
                    &mut sub_depth,
                    &mut sup_depth,
                    &mut active_link,
                    &mut cur_spans,
                    md_links,
                    &mut active_para,
                    &mut para_line_has_other,
                );
            }

            Event::Html(raw_html) => {
                push_html_embed_block(raw_html.to_string(), ev_line, &mut containers, plain_parts);
            }

            Event::SoftBreak => {
                if let Some(img) = active_image.as_mut() {
                    img.alt_buf.push(' ');
                } else if let Some(link) = active_link.as_mut() {
                    link.label_buf.push(' ');
                } else if active_para.is_some()
                    && !para_line_has_other
                    && para_line_embed.is_some()
                {
                    // Previous line inside this tight paragraph was a standalone embed (`![[...]]` / `![alt](img)`):
                    // flush any preceding text, emit the embed block, and start a fresh paragraph line.
                    let embed = para_line_embed.take().unwrap();
                    if let Some(eb) = build_standalone_embed_block(embed) {
                        let prefix_spans = trim_trailing_space_spans(&cur_spans[..para_line_start_idx]);
                        if !prefix_spans.is_empty() {
                            let mut p_spans = prefix_spans;
                            apply_cross_span_highlights(&mut p_spans);
                            let plain = spans_to_plain(&p_spans);
                            if !plain.trim().is_empty() {
                                plain_parts.push(plain);
                            }
                            if let Some(c) = containers.last_mut() {
                                c.push_block(NoteBlock::Paragraph {
                                    spans: p_spans,
                                    line_no: para_line_no,
                                });
                            }
                        }
                        cur_spans.clear();
                        if let Some(c) = containers.last_mut() {
                            c.push_block(eb);
                        }
                        para_line_start_idx = 0;
                        para_line_has_other = false;
                        para_line_no = line_no_at(range.end);
                    } else {
                        push_styled_text(
                            &mut cur_spans,
                            " ".to_string(),
                            current_flags(&style_stack),
                            sub_depth,
                            sup_depth,
                        );
                        para_line_start_idx = cur_spans.len();
                        para_line_embed = None;
                        para_line_has_other = false;
                    }
                } else {
                    // Inside the first paragraph of a `BlockQuote(None)` starting with `[!`,
                    // preserve the line boundary after the callout header as `LineBreak`.
                    let is_callout_header_break = matches!(
                        containers.last(),
                        Some(BlockContainer::BlockQuote {
                            gfm_kind: None,
                            blocks,
                            ..
                        }) if blocks.is_empty()
                    ) && !cur_spans.contains(&InlineSpan::LineBreak)
                        && spans_to_plain(&cur_spans).trim_start().starts_with("[!");
                    if is_callout_header_break {
                        cur_spans.push(InlineSpan::LineBreak);
                    } else {
                        push_styled_text(
                            &mut cur_spans,
                            " ".to_string(),
                            current_flags(&style_stack),
                            sub_depth,
                            sup_depth,
                        );
                    }
                    if active_para.is_some() {
                        para_line_start_idx = cur_spans.len();
                        para_line_embed = None;
                        para_line_has_other = false;
                    }
                }
            }

            Event::HardBreak => {
                if let Some(img) = active_image.as_mut() {
                    img.alt_buf.push(' ');
                } else if let Some(link) = active_link.as_mut() {
                    link.label_buf.push(' ');
                } else {
                    cur_spans.push(InlineSpan::LineBreak);
                }
            }

            Event::Rule => {
                flush_list_item_if_needed(
                    &mut item_stack,
                    &mut cur_spans,
                    &mut containers,
                    tasks,
                    plain_parts,
                );
                if let Some(c) = containers.last_mut() {
                    c.push_block(NoteBlock::Divider { line_no: ev_line });
                }
            }

            Event::TaskListMarker(checked) => {
                if let Some(item) = item_stack.last_mut() {
                    item.task_checked = Some(checked);
                }
            }

            Event::Text(_) => unreachable!("handled above"),
        }
    }

    flush_list_item_if_needed(
        &mut item_stack,
        &mut cur_spans,
        &mut containers,
        tasks,
        plain_parts,
    );

    match containers.into_iter().next() {
        Some(BlockContainer::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

fn build_standalone_embed_block(embed: PendingEmbed) -> Option<NoteBlock> {
    match embed {
        PendingEmbed::Wikilink {
            target,
            heading,
            alias,
            line_no,
        } => {
            if is_image_target(&target) {
                let width_hint = alias.as_deref().and_then(|a| a.parse::<u32>().ok());
                let alt = alias
                    .as_deref()
                    .filter(|a| a.parse::<u32>().is_err())
                    .unwrap_or(&target)
                    .to_string();
                Some(NoteBlock::ImageEmbed {
                    raw_target: target,
                    alt,
                    width_hint,
                    resolved_path: None,
                    line_no,
                })
            } else {
                Some(NoteBlock::NoteEmbed {
                    target,
                    heading,
                    resolved_path: None,
                    line_no,
                })
            }
        }
        PendingEmbed::MarkdownImage { alt, url, line_no } => {
            if is_image_target(&url) {
                let display_alt = if alt.is_empty() { url.clone() } else { alt };
                Some(NoteBlock::ImageEmbed {
                    raw_target: url,
                    alt: display_alt,
                    width_hint: None,
                    resolved_path: None,
                    line_no,
                })
            } else {
                None
            }
        }
    }
}

fn trim_trailing_space_spans(spans: &[InlineSpan]) -> Vec<InlineSpan> {
    let mut out = spans.to_vec();
    while let Some(last) = out.last() {
        if matches!(last, InlineSpan::Text { text } if text.trim().is_empty()) {
            out.pop();
        } else {
            break;
        }
    }
    out
}

fn heading_level_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn map_alignment(a: Alignment) -> TableAlignment {
    match a {
        Alignment::Right => TableAlignment::Right,
        Alignment::Center => TableAlignment::Center,
        Alignment::None | Alignment::Left => TableAlignment::Left,
    }
}

fn map_gfm_blockquote_kind(kind: BlockQuoteKind) -> CalloutKind {
    match kind {
        BlockQuoteKind::Note => CalloutKind::Note,
        BlockQuoteKind::Tip | BlockQuoteKind::Important => CalloutKind::Tip,
        BlockQuoteKind::Warning | BlockQuoteKind::Caution => CalloutKind::Warning,
    }
}

fn finalize_blockquote(
    gfm_kind: Option<BlockQuoteKind>,
    mut blocks: Vec<NoteBlock>,
    line_no: usize,
    plain_parts: &mut Vec<String>,
) -> NoteBlock {
    if let Some(kind) = gfm_kind {
        let callout_kind = map_gfm_blockquote_kind(kind);
        let title = callout_kind.label().to_string();
        plain_parts.push(title.clone());
        return NoteBlock::Callout {
            callout_kind,
            title,
            collapsed: false,
            body_blocks: blocks,
            line_no,
        };
    }

    // Detect Obsidian `[!type][+-]? optional title` on the first line of the first paragraph.
    if let Some(NoteBlock::Paragraph {
        spans,
        line_no: para_ln,
    }) = blocks.first()
    {
        let split_pos = spans
            .iter()
            .position(|s| matches!(s, InlineSpan::LineBreak));
        let header_slice = match split_pos {
            Some(pos) => &spans[..pos],
            None => spans.as_slice(),
        };
        let header_plain = spans_to_plain(header_slice);
        if let Some((callout_kind, title, collapsed)) = parse_callout_header_text(&header_plain) {
            let remaining_spans: Vec<InlineSpan> = match split_pos {
                Some(pos) => spans[pos + 1..].to_vec(),
                None => Vec::new(),
            };
            let para_line_no = *para_ln;
            blocks.remove(0);
            if !remaining_spans.is_empty() && !spans_to_plain(&remaining_spans).trim().is_empty() {
                blocks.insert(
                    0,
                    NoteBlock::Paragraph {
                        spans: remaining_spans,
                        line_no: para_line_no + 1,
                    },
                );
            }
            if !title.is_empty() {
                plain_parts.push(title.clone());
            }
            return NoteBlock::Callout {
                callout_kind,
                title,
                collapsed,
                body_blocks: blocks,
                line_no,
            };
        }
    }

    NoteBlock::Blockquote { blocks, line_no }
}

fn parse_callout_header_text(line: &str) -> Option<(CalloutKind, String, bool)> {
    let rest = line.trim_start();
    let after_open = rest.strip_prefix("[!")?;
    let close_idx = after_open.find(']')?;
    let raw_type = after_open[..close_idx].trim();
    if raw_type.is_empty() {
        return None;
    }
    let mut remainder = &after_open[close_idx + 1..];
    let mut collapsed = false;
    if let Some(r) = remainder.strip_prefix('-') {
        collapsed = true;
        remainder = r;
    } else if let Some(r) = remainder.strip_prefix('+') {
        remainder = r;
    }
    let kind = CalloutKind::parse_str(raw_type);
    let title = remainder.trim();
    let final_title = if title.is_empty() {
        kind.label().to_string()
    } else {
        title.to_string()
    };
    Some((kind, final_title, collapsed))
}

fn sanitize_html_summary(raw_html: &str) -> Vec<String> {
    let doc = bitefoundry_html::DomDocument::parse(raw_html);
    let rendered = bitefoundry_html::render_dom(
        &doc,
        76,
        &bitefoundry_html::HtmlTheme::no_color(),
        None,
        None,
        false,
    );
    rendered
        .lines
        .into_iter()
        .map(|l| {
            l.spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
        .collect()
}

fn push_html_embed_block(
    raw_html: String,
    line_no: usize,
    containers: &mut [BlockContainer],
    plain_parts: &mut Vec<String>,
) {
    let trimmed = raw_html.trim();
    if trimmed.is_empty() {
        return;
    }
    // Ignore pure HTML comments (`<!-- ... -->`) so comments never render noisy cards.
    if trimmed.starts_with("<!--") && trimmed.ends_with("-->") && !trimmed[4..trimmed.len() - 3].contains("-->") {
        return;
    }
    let summary_lines = sanitize_html_summary(trimmed);
    for line in &summary_lines {
        plain_parts.push(line.clone());
    }
    if let Some(c) = containers.last_mut() {
        c.push_block(NoteBlock::HtmlEmbed {
            raw_html: trimmed.to_string(),
            summary_lines,
            line_no,
        });
    }
}

fn looks_like_standalone_html_paragraph(raw: &str) -> bool {
    let trimmed = raw.trim();
    if !trimmed.starts_with('<') || !trimmed.ends_with('>') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    const COMPLEX_PREFIXES: &[&str] = &[
        "<div",
        "<details",
        "<summary",
        "<svg",
        "<canvas",
        "<style",
        "<script",
        "<table",
        "<form",
        "<button",
        "<input",
        "<select",
        "<textarea",
        "<iframe",
        "<section",
        "<article",
        "<header",
        "<footer",
        "<nav",
    ];
    COMPLEX_PREFIXES.iter().any(|p| lower.starts_with(p))
}

#[allow(clippy::too_many_arguments)]
fn handle_inline_html(
    raw_tag: &str,
    line_no: usize,
    style_stack: &mut Vec<InlineStyleFlags>,
    inline_html_stack: &mut Vec<InlineHtmlTagKind>,
    sub_depth: &mut usize,
    sup_depth: &mut usize,
    active_link: &mut Option<ActiveLink>,
    cur_spans: &mut Vec<InlineSpan>,
    md_links: &mut Vec<MarkdownLinkRef>,
    active_para: &mut Option<(usize, usize, bool)>,
    para_has_other_content: &mut bool,
) {
    let trimmed = raw_tag.trim();
    if trimmed.is_empty() || (trimmed.starts_with("<!--") && trimmed.ends_with("-->")) {
        return;
    }

    let inner = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or("")
        .trim();
    if inner.is_empty() {
        return;
    }

    let is_closing = inner.starts_with('/');
    let tag_body = inner.trim_start_matches('/').trim_end_matches('/').trim();
    let tag_name = tag_body
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    match tag_name.as_str() {
        "br" => {
            cur_spans.push(InlineSpan::LineBreak);
        }
        "u" | "ins" => {
            if is_closing {
                pop_inline_html_style(style_stack, inline_html_stack);
            } else {
                let mut next = style_stack.last().copied().unwrap_or_default();
                next.underline = true;
                style_stack.push(next);
                inline_html_stack.push(InlineHtmlTagKind::Style);
            }
        }
        "mark" => {
            if is_closing {
                pop_inline_html_style(style_stack, inline_html_stack);
            } else {
                let mut next = style_stack.last().copied().unwrap_or_default();
                next.highlight = true;
                style_stack.push(next);
                inline_html_stack.push(InlineHtmlTagKind::Style);
            }
        }
        "b" | "strong" => {
            if is_closing {
                pop_inline_html_style(style_stack, inline_html_stack);
            } else {
                let mut next = style_stack.last().copied().unwrap_or_default();
                next.bold = true;
                style_stack.push(next);
                inline_html_stack.push(InlineHtmlTagKind::Style);
            }
        }
        "i" | "em" => {
            if is_closing {
                pop_inline_html_style(style_stack, inline_html_stack);
            } else {
                let mut next = style_stack.last().copied().unwrap_or_default();
                next.italic = true;
                style_stack.push(next);
                inline_html_stack.push(InlineHtmlTagKind::Style);
            }
        }
        "s" | "del" | "strike" => {
            if is_closing {
                pop_inline_html_style(style_stack, inline_html_stack);
            } else {
                let mut next = style_stack.last().copied().unwrap_or_default();
                next.strike = true;
                style_stack.push(next);
                inline_html_stack.push(InlineHtmlTagKind::Style);
            }
        }
        "sub" => {
            if is_closing {
                *sub_depth = sub_depth.saturating_sub(1);
                if matches!(inline_html_stack.last(), Some(InlineHtmlTagKind::Sub)) {
                    inline_html_stack.pop();
                }
            } else {
                *sub_depth += 1;
                inline_html_stack.push(InlineHtmlTagKind::Sub);
            }
        }
        "sup" => {
            if is_closing {
                *sup_depth = sup_depth.saturating_sub(1);
                if matches!(inline_html_stack.last(), Some(InlineHtmlTagKind::Sup)) {
                    inline_html_stack.pop();
                }
            } else {
                *sup_depth += 1;
                inline_html_stack.push(InlineHtmlTagKind::Sup);
            }
        }
        "a" => {
            if is_closing {
                if matches!(inline_html_stack.last(), Some(InlineHtmlTagKind::Link)) {
                    inline_html_stack.pop();
                }
                if let Some(link) = active_link.take() {
                    let label = if link.label_buf.is_empty() {
                        link.url.clone()
                    } else {
                        link.label_buf
                    };
                    md_links.push(MarkdownLinkRef {
                        label: label.clone(),
                        url: link.url.clone(),
                        title: link.title,
                        is_image: false,
                        line_no: link.line_no,
                    });
                    cur_spans.push(InlineSpan::ExternalLink {
                        label,
                        url: link.url,
                    });
                    if active_para.is_some() {
                        *para_has_other_content = true;
                    }
                }
            } else if let Some(href) = extract_html_attr(tag_body, "href") {
                *active_link = Some(ActiveLink {
                    url: href,
                    title: extract_html_attr(tag_body, "title"),
                    line_no,
                    label_buf: String::new(),
                });
                inline_html_stack.push(InlineHtmlTagKind::Link);
            }
        }
        "div" | "details" | "summary" | "svg" | "canvas" | "style" | "script" | "iframe"
        | "form" | "button" | "input" | "select" | "textarea" | "table" => {
            if let Some((_, _, has_complex)) = active_para.as_mut() {
                *has_complex = true;
            }
        }
        // Other benign inline tags (`<span>`, `<kbd>`, etc.) are silently stripped so raw tags never leak.
        _ => {}
    }
}

fn pop_inline_html_style(
    style_stack: &mut Vec<InlineStyleFlags>,
    inline_html_stack: &mut Vec<InlineHtmlTagKind>,
) {
    if matches!(inline_html_stack.last(), Some(InlineHtmlTagKind::Style)) {
        inline_html_stack.pop();
        if style_stack.len() > 1 {
            style_stack.pop();
        }
    }
}

fn extract_html_attr(tag_body: &str, attr_name: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let needle = format!("{attr_name}=");
    let pos = lower.find(&needle)?;
    let after = tag_body[pos + needle.len()..].trim_start();
    if let Some(rest) = after.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else if let Some(rest) = after.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some(rest[..end].to_string())
    } else {
        let val: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>')
            .collect();
        (!val.is_empty()).then_some(val)
    }
}

fn last_char_of_spans(spans: &[InlineSpan]) -> Option<char> {
    spans
        .last()
        .and_then(|s| s.plain_text().chars().next_back())
}

/// Obsidian extension scanner executed ONLY on unescaped `Event::Text` runs (outside code fences,
/// inline code, links, images, and HTML blocks):
/// - `![[embed]]` / `[[wikilink]]` (blocking `[[#Section]]` false-positive tags)
/// - `==highlight==` (combining with active `InlineStyleFlags`)
/// - `#tag` / `#nested/tag`
/// - Bare `https://` / `http://` URLs
#[allow(clippy::too_many_arguments)]
fn scan_obsidian_extensions(
    input: &str,
    line_no: usize,
    flags: InlineStyleFlags,
    sub_depth: usize,
    sup_depth: usize,
    initial_prev_char: Option<char>,
    out: &mut Vec<InlineSpan>,
    wikilinks: &mut Vec<WikilinkRef>,
    md_links: &mut Vec<MarkdownLinkRef>,
    tag_set: &mut BTreeSet<String>,
    mut standalone_embed_out: Option<&mut Option<PendingEmbed>>,
) {
    let len = input.len();
    let mut i = 0usize;
    let mut text_buf = String::new();

    let flush_text = |buf: &mut String, out: &mut Vec<InlineSpan>| {
        if !buf.is_empty() {
            let text = std::mem::take(buf);
            if sub_depth > 0 {
                out.push(InlineSpan::Subscript { text });
            } else if sup_depth > 0 {
                out.push(InlineSpan::Superscript { text });
            } else {
                out.push(flags.to_span(text));
            }
        }
    };

    while i < len {
        let rest = &input[i..];

        // Embed or Wikilink: `![[...]]` or `[[...]]`
        let is_embed = rest.starts_with("![[");
        if is_embed || rest.starts_with("[[") {
            let prefix_len = if is_embed { 3 } else { 2 };
            if let Some(close) = rest[prefix_len..].find("]]") {
                let inner = &rest[prefix_len..prefix_len + close];
                if !inner.contains('\n') && !inner.is_empty() && !inner.contains("[[") {
                    let (target_part, alias_opt) = split_wikilink_alias(inner);
                    let alias = alias_opt.map(String::from);
                    let (target, heading) = match target_part.split_once('#') {
                        Some((t, h)) => (t.trim(), Some(h.trim().to_string())),
                        None => (target_part, None),
                    };
                    if !target.is_empty() {
                        let is_sole_token = is_embed
                            && text_buf.trim().is_empty()
                            && input[i + prefix_len + close + 2..].trim().is_empty();
                        flush_text(&mut text_buf, out);
                        wikilinks.push(WikilinkRef {
                            target_raw: target.to_string(),
                            alias: alias.clone(),
                            heading: heading.clone(),
                            is_embed,
                            line_no,
                        });
                        if is_sole_token {
                            if let Some(slot) = standalone_embed_out.as_deref_mut() {
                                *slot = Some(PendingEmbed::Wikilink {
                                    target: target.to_string(),
                                    heading: heading.clone(),
                                    alias: alias.clone(),
                                    line_no,
                                });
                            }
                        }
                        out.push(InlineSpan::Wikilink {
                            target: target.to_string(),
                            heading,
                            alias,
                            resolved_path: None,
                        });
                        i += prefix_len + close + 2;
                        continue;
                    } else if let Some(h) = heading.filter(|h| !h.is_empty()) {
                        // Same-note heading/block reference like `[[#Section]]` must not leak `#Section` as a tag.
                        flush_text(&mut text_buf, out);
                        let display = alias.unwrap_or_else(|| format!("#{h}"));
                        out.push(flags.to_span(display));
                        i += prefix_len + close + 2;
                        continue;
                    }
                }
            }
        }

        // Obsidian Highlight: `==...==`
        if let Some(after) = rest.strip_prefix("==") {
            if let Some(end) = after.find("==") {
                if end > 0 {
                    let inner = &after[..end];
                    flush_text(&mut text_buf, out);
                    let mut hl_flags = flags;
                    hl_flags.highlight = true;
                    scan_obsidian_extensions(
                        inner,
                        line_no,
                        hl_flags,
                        sub_depth,
                        sup_depth,
                        Some(' '),
                        out,
                        wikilinks,
                        md_links,
                        tag_set,
                        None,
                    );
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Inline tag `#tag` or `#nested/tag` (must be at start or preceded by whitespace/opening punct)
        if let Some(after_hash) = rest.strip_prefix('#') {
            let prev_char = if i == 0 {
                initial_prev_char
            } else {
                input[..i].chars().next_back()
            };
            let valid_boundary = prev_char
                .map(|c| c.is_whitespace() || matches!(c, '(' | '[' | ',' | ';' | ':'))
                .unwrap_or(true);
            if valid_boundary {
                let tag_len = after_hash
                    .char_indices()
                    .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
                    .last()
                    .map(|(idx, c)| idx + c.len_utf8())
                    .unwrap_or(0);
                if tag_len > 0 {
                    let raw_tag = &after_hash[..tag_len];
                    if let Some(clean_tag) = normalize_tag(raw_tag) {
                        flush_text(&mut text_buf, out);
                        tag_set.insert(clean_tag.clone());
                        out.push(InlineSpan::Tag { tag: clean_tag });
                        i += 1 + tag_len;
                        continue;
                    }
                }
            }
        }

        // Bare URL (`https://...` or `http://...`)
        if rest.starts_with("https://") || rest.starts_with("http://") {
            let url_len = rest
                .char_indices()
                .take_while(|(_, c)| {
                    !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"' | '\'')
                })
                .last()
                .map(|(idx, c)| idx + c.len_utf8())
                .unwrap_or(0);
            if url_len > 8 {
                let url = rest[..url_len].trim_end_matches(['.', ',', ';', ':', '!']);
                if url.len() > 8 {
                    flush_text(&mut text_buf, out);
                    md_links.push(MarkdownLinkRef {
                        label: url.to_string(),
                        url: url.to_string(),
                        title: None,
                        is_image: false,
                        line_no,
                    });
                    out.push(InlineSpan::ExternalLink {
                        label: url.to_string(),
                        url: url.to_string(),
                    });
                    i += url.len();
                    continue;
                }
            }
        }

        let ch = rest.chars().next().unwrap();
        text_buf.push(ch);
        i += ch.len_utf8();
    }

    flush_text(&mut text_buf, out);
}

/// Pairs `==...==` highlight markers that span across inline Markdown formatting tags
/// (for example `==**bold highlight**==`).
fn apply_cross_span_highlights(spans: &mut Vec<InlineSpan>) {
    let has_marker = spans.iter().any(|s| match s {
        InlineSpan::Text { text }
        | InlineSpan::Bold { text }
        | InlineSpan::Italic { text }
        | InlineSpan::Underline { text }
        | InlineSpan::Strikethrough { text }
        | InlineSpan::Styled { text, .. } => text.contains("=="),
        _ => false,
    });
    if !has_marker {
        return;
    }

    // Flatten text-bearing spans around `"=="` tokens, then pair every opening `"=="`
    // with a matching closing `"=="`.
    enum Tok {
        Marker,
        Span(InlineSpan),
    }

    let mut tokens: Vec<Tok> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        let maybe_parts: Option<(InlineStyleFlags, String)> = match span {
            InlineSpan::Text { text } if text.contains("==") => {
                Some((InlineStyleFlags::default(), text))
            }
            InlineSpan::Bold { text } if text.contains("==") => Some((
                InlineStyleFlags {
                    bold: true,
                    ..Default::default()
                },
                text,
            )),
            InlineSpan::Italic { text } if text.contains("==") => Some((
                InlineStyleFlags {
                    italic: true,
                    ..Default::default()
                },
                text,
            )),
            InlineSpan::Underline { text } if text.contains("==") => Some((
                InlineStyleFlags {
                    underline: true,
                    ..Default::default()
                },
                text,
            )),
            InlineSpan::Strikethrough { text } if text.contains("==") => Some((
                InlineStyleFlags {
                    strike: true,
                    ..Default::default()
                },
                text,
            )),
            InlineSpan::Styled {
                text,
                bold,
                italic,
                strike,
                highlight,
                underline,
            } if text.contains("==") => Some((
                InlineStyleFlags {
                    bold,
                    italic,
                    strike,
                    highlight,
                    underline,
                },
                text,
            )),
            other => {
                tokens.push(Tok::Span(other));
                None
            }
        };
        if let Some((flags, text)) = maybe_parts {
            let mut first = true;
            for part in text.split("==") {
                if !first {
                    tokens.push(Tok::Marker);
                }
                first = false;
                if !part.is_empty() {
                    tokens.push(Tok::Span(flags.to_span(part.to_string())));
                }
            }
        }
    }

    let marker_count = tokens.iter().filter(|t| matches!(t, Tok::Marker)).count();
    let pairs = marker_count / 2;
    let mut markers_consumed = 0usize;
    let mut in_hl = false;

    for tok in tokens {
        match tok {
            Tok::Marker => {
                if markers_consumed < pairs * 2 {
                    in_hl = !in_hl;
                    markers_consumed += 1;
                } else {
                    spans.push(InlineSpan::Text {
                        text: "==".to_string(),
                    });
                }
            }
            Tok::Span(span) => {
                if in_hl {
                    spans.push(add_highlight_flag(span));
                } else {
                    spans.push(span);
                }
            }
        }
    }
}

fn add_highlight_flag(span: InlineSpan) -> InlineSpan {
    match span {
        InlineSpan::Text { text } => InlineSpan::Highlight { text },
        InlineSpan::Bold { text } => InlineSpan::Styled {
            text,
            bold: true,
            italic: false,
            strike: false,
            highlight: true,
            underline: false,
        },
        InlineSpan::Italic { text } => InlineSpan::Styled {
            text,
            bold: false,
            italic: true,
            strike: false,
            highlight: true,
            underline: false,
        },
        InlineSpan::Underline { text } => InlineSpan::Styled {
            text,
            bold: false,
            italic: false,
            strike: false,
            highlight: true,
            underline: true,
        },
        InlineSpan::Strikethrough { text } => InlineSpan::Styled {
            text,
            bold: false,
            italic: false,
            strike: true,
            highlight: true,
            underline: false,
        },
        InlineSpan::Styled {
            text,
            bold,
            italic,
            strike,
            underline,
            ..
        } => InlineSpan::Styled {
            text,
            bold,
            italic,
            strike,
            highlight: true,
            underline,
        },
        other => other,
    }
}

/// Parses inline Obsidian Markdown spans via `pulldown-cmark` + Obsidian extension layer.
pub fn parse_inline_spans(
    input: &str,
    line_no: usize,
    wikilinks: &mut Vec<WikilinkRef>,
    md_links: &mut Vec<MarkdownLinkRef>,
    tag_set: &mut BTreeSet<String>,
) -> Vec<InlineSpan> {
    let preprocessed = preprocess_obsidian_body(input);
    let line_starts = compute_line_starts(&preprocessed);
    let body_lines: Vec<&str> = input.lines().collect();
    let mut headings = Vec::new();
    let mut tasks = Vec::new();
    let mut plain_parts = Vec::new();
    let blocks = parse_body_events(
        "",
        "",
        &preprocessed,
        &body_lines,
        &line_starts,
        line_no.saturating_sub(1),
        wikilinks,
        md_links,
        tag_set,
        &mut headings,
        &mut tasks,
        &mut plain_parts,
    );
    let mut out = Vec::new();
    for block in blocks {
        match block {
            NoteBlock::Paragraph { spans, .. }
            | NoteBlock::Heading { spans, .. }
            | NoteBlock::Task { spans, .. }
            | NoteBlock::BulletItem { spans, .. }
            | NoteBlock::NumberedItem { spans, .. }
            | NoteBlock::FootnoteDef { spans, .. } => out.extend(spans),
            _ => {}
        }
    }
    out
}

/// Parses YAML frontmatter (`--- ... ---`) at the top of a Markdown file.
/// Returns `(frontmatter_map, first_body_line_index)`.
pub fn parse_frontmatter(lines: &[&str]) -> (BTreeMap<String, Value>, usize) {
    if !lines.first().is_some_and(|line| opening_delimiter(line)) {
        return (BTreeMap::new(), 0);
    }
    let Some(end) = lines
        .iter()
        .skip(1)
        .position(|line| closing_delimiter(line))
    else {
        return (BTreeMap::new(), 0);
    };
    let yaml = lines[1..=end].join("\n") + "\n";
    (read_properties(&yaml).unwrap_or_default(), end + 2)
}

fn extract_string_list(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

pub fn normalize_tag(raw: &str) -> Option<String> {
    let stripped = raw.trim().trim_start_matches('#').trim();
    if stripped.is_empty() {
        return None;
    }
    // Pure numeric tags like `#123` are not valid Obsidian tags.
    if stripped.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(stripped.to_ascii_lowercase())
}

/// Parses a single Markdown task line, returning `(checked, clean_plain_text, raw_content_trimmed)`.
pub fn parse_single_task_line(line: &str) -> Option<(bool, String, String)> {
    let indent = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let trimmed = &line[indent..];
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    let (checked, content) = if let Some(rest) = after_bullet.strip_prefix("[ ] ") {
        (false, rest)
    } else if after_bullet == "[ ]" {
        (false, "")
    } else if let Some(rest) = after_bullet
        .strip_prefix("[x] ")
        .or_else(|| after_bullet.strip_prefix("[X] "))
    {
        (true, rest)
    } else if after_bullet == "[x]" || after_bullet == "[X]" {
        (true, "")
    } else {
        return None;
    };

    let mut wikilinks = Vec::new();
    let mut md_links = Vec::new();
    let mut tags = BTreeSet::new();
    let spans = parse_inline_spans(content, 1, &mut wikilinks, &mut md_links, &mut tags);
    let clean_text = spans_to_plain(&spans);
    Some((checked, clean_text, content.trim().to_string()))
}

fn split_wikilink_alias(inner: &str) -> (&str, Option<&str>) {
    match inner.split_once('|') {
        Some((lhs, rhs)) => {
            let lhs = lhs.strip_suffix('\\').unwrap_or(lhs);
            (lhs.trim(), Some(rhs.trim()))
        }
        None => (inner.trim(), None),
    }
}

pub fn spans_to_plain(spans: &[InlineSpan]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.plain_text());
    }
    out
}

// ---------------------------------------------------------------------------
// Surgical byte-span helpers shared with `vault::fs` byte-preserving link rewriter
// ---------------------------------------------------------------------------

pub(crate) fn is_code_fence_opener(trimmed: &str) -> Option<(char, usize)> {
    if trimmed.starts_with("```") {
        let flen = trimmed.bytes().take_while(|&b| b == b'`').count();
        return (!trimmed[flen..].contains('`')).then_some(('`', flen));
    }
    if trimmed.starts_with("~~~") {
        let flen = trimmed.bytes().take_while(|&b| b == b'~').count();
        return Some(('~', flen));
    }
    None
}

pub(crate) fn find_inline_code_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let ticks = bytes[start..].iter().take_while(|&&b| b == b'`').count();
    if ticks == 0 {
        return None;
    }
    let mut k = start + ticks;
    while k < len {
        if bytes[k] == b'`' {
            let close_ticks = bytes[k..].iter().take_while(|&&b| b == b'`').count();
            k += close_ticks;
            if close_ticks == ticks {
                return Some(k);
            }
        } else {
            k += 1;
        }
    }
    None
}

pub(crate) fn find_matching_bracket(line: &str, open_bracket: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut j = open_bracket + 1;
    let mut depth = 1usize;
    while j < len {
        if bytes[j] == b'\\' && j + 1 < len {
            j += 2;
            continue;
        }
        if bytes[j] == b'`' {
            if let Some(end) = find_inline_code_end(line, j) {
                j = end;
            } else {
                j += bytes[j..].iter().take_while(|&&b| b == b'`').count();
            }
            continue;
        }
        if bytes[j] == b'[' {
            depth += 1;
            j += 1;
        } else if bytes[j] == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
            j += 1;
        } else {
            j += 1;
        }
    }
    None
}

pub(crate) struct MarkdownLinkDest {
    pub dest_start: usize,
    pub dest_end: usize,
    #[allow(dead_code)]
    pub title: Option<String>,
    pub angle_bracketed: bool,
    pub close_end: usize,
}

pub(crate) fn parse_inline_markdown_link_dest(
    line: &str,
    open_paren: usize,
) -> Option<MarkdownLinkDest> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut j = open_paren + 1;
    while j < len && matches!(bytes[j], b' ' | b'\t') {
        j += 1;
    }
    if j >= len {
        return None;
    }

    let (dest_start, dest_end, angle_bracketed) = if bytes[j] == b'<' {
        let start = j + 1;
        j += 1;
        while j < len {
            if bytes[j] == b'\\' && j + 1 < len {
                j += 2;
                continue;
            }
            if bytes[j] == b'>' {
                break;
            }
            if matches!(bytes[j], b'<' | b'\n' | b'\r') {
                return None;
            }
            j += 1;
        }
        if j >= len || bytes[j] != b'>' {
            return None;
        }
        let end = j;
        j += 1;
        (start, end, true)
    } else {
        let start = j;
        let mut paren_depth = 0usize;
        while j < len {
            if bytes[j] == b'\\' && j + 1 < len && bytes[j + 1].is_ascii_punctuation() {
                j += 2;
                continue;
            }
            if bytes[j] <= b' ' || bytes[j] == 0x7F {
                break;
            }
            if bytes[j] == b'(' {
                paren_depth += 1;
                j += 1;
            } else if bytes[j] == b')' {
                if paren_depth == 0 {
                    break;
                }
                paren_depth -= 1;
                j += 1;
            } else {
                j += 1;
            }
        }
        if paren_depth != 0 {
            return None;
        }
        (start, j, false)
    };

    let ws_start = j;
    while j < len && matches!(bytes[j], b' ' | b'\t') {
        j += 1;
    }
    if j < len && bytes[j] == b')' {
        return Some(MarkdownLinkDest {
            dest_start,
            dest_end,
            title: None,
            angle_bracketed,
            close_end: j + 1,
        });
    }

    if j > ws_start && j < len && matches!(bytes[j], b'"' | b'\'' | b'(') {
        let close_delim = match bytes[j] {
            b'"' => b'"',
            b'\'' => b'\'',
            b'(' => b')',
            _ => unreachable!(),
        };
        let is_paren_title = bytes[j] == b'(';
        let title_start = j + 1;
        j += 1;
        let mut closed = false;
        let mut title_end = title_start;
        while j < len {
            if bytes[j] == b'\\' && j + 1 < len {
                j += 2;
                continue;
            }
            if is_paren_title && bytes[j] == b'(' {
                return None;
            }
            if bytes[j] == close_delim {
                title_end = j;
                j += 1;
                closed = true;
                break;
            }
            j += 1;
        }
        if !closed {
            return None;
        }
        while j < len && matches!(bytes[j], b' ' | b'\t') {
            j += 1;
        }
        if j < len && bytes[j] == b')' {
            return Some(MarkdownLinkDest {
                dest_start,
                dest_end,
                title: Some(line[title_start..title_end].to_string()),
                angle_bracketed,
                close_end: j + 1,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_wikilinks_tags_callouts_tasks_headings() {
        let md = r#"---
status: active
tags: [architecture, project/q4]
aliases:
  - RFC-042
  - Q4 Architecture
created: 2026-11-01
---

# Q4 Architecture RFC

See [[Storage Engine|SQLite WAL]] and [[Projects/Graph Layout#Force Model]] for details.
Also tagged with #rust and #nested/tag.

```bash
# Not a heading or #not-a-tag
echo "[[NotALink]]"
```

Inline code `#ignore-tag` and `[[ignore-link]]` stay literal.

> [!tip]- Performance Note
> Incremental scans diff `mtime_ms` and `size_bytes`.

## Action Items

- [x] Implement SQLite WAL + FTS5
- [ ] Render interactive Knowledge Graph

![[diagram.png|400]]
"#;

        let parsed = parse_markdown("Projects/Q4 Architecture RFC.md", "Q4 Architecture RFC", md);
        assert_eq!(
            parsed.frontmatter.get("status").and_then(|v| v.as_str()),
            Some("active")
        );
        assert_eq!(parsed.aliases, vec!["RFC-042", "Q4 Architecture"]);
        assert!(parsed.tags.contains(&"architecture".to_string()));
        assert!(parsed.tags.contains(&"project/q4".to_string()));
        assert!(parsed.tags.contains(&"rust".to_string()));
        assert!(parsed.tags.contains(&"nested/tag".to_string()));
        assert!(!parsed.tags.contains(&"not-a-tag".to_string()));
        assert!(!parsed.tags.contains(&"ignore-tag".to_string()));

        assert_eq!(parsed.wikilinks.len(), 3);
        assert_eq!(parsed.wikilinks[0].target_raw, "Storage Engine");
        assert_eq!(parsed.wikilinks[0].alias.as_deref(), Some("SQLite WAL"));
        assert_eq!(parsed.wikilinks[1].target_raw, "Projects/Graph Layout");
        assert_eq!(parsed.wikilinks[1].heading.as_deref(), Some("Force Model"));
        assert!(parsed.wikilinks[2].is_embed);
        assert_eq!(parsed.wikilinks[2].target_raw, "diagram.png");

        assert_eq!(parsed.headings.len(), 2);
        assert_eq!(parsed.headings[0].level, 1);
        assert_eq!(parsed.headings[1].text, "Action Items");

        assert_eq!(parsed.tasks.len(), 2);
        assert!(parsed.tasks[0].checked);
        assert!(!parsed.tasks[1].checked);
        assert!(parsed.tasks[0].line_no > 15);
    }

    #[test]
    fn test_upsert_frontmatter_preserves_body() {
        let orig = "---\nstatus: draft\n---\n\n# Hello\n\nBody text.\n";
        let mut updates = BTreeMap::new();
        updates.insert("status".into(), Value::String("published".into()));
        updates.insert(
            "tags".into(),
            Value::Array(vec![
                Value::String("core".into()),
                Value::String("rfc".into()),
            ]),
        );
        let updated = upsert_frontmatter_in_markdown(orig, &updates).unwrap();
        assert!(updated.contains("status: published"));
        assert!(updated.contains("tags: [core, rfc]"));
        assert!(updated.contains("# Hello"));
        assert!(updated.contains("Body text."));
    }

    #[test]
    fn test_is_image_target_accepts_svg_and_rejects_html() {
        assert!(is_image_target("diagram.png"));
        assert!(is_image_target("assets/photo.JPEG?v=2#top"));
        assert!(is_image_target("anim.gif"));
        assert!(is_image_target("pic.webp"));
        assert!(is_image_target("scan.bmp"));
        assert!(is_image_target("vector.svg"));
        assert!(is_image_target("icons/logo.SVG?raw=1"));

        // Must never classify HTML/HTM/XHTML or Markdown as images
        assert!(!is_image_target("page.html"));
        assert!(!is_image_target("report.HTM"));
        assert!(!is_image_target("doc.xhtml"));
        assert!(!is_image_target("Note.md"));

        // Standalone embeds of .svg / .png produce ImageEmbed blocks; .html does not
        let parsed = parse_markdown(
            "Test.md",
            "Test",
            "![[diagram.svg]]\n\n![icon](logo.svg)\n\n![page](index.html)\n\n![[photo.png]]\n",
        );
        let img_embeds: Vec<&str> = parsed
            .blocks
            .iter()
            .filter_map(|b| match b {
                NoteBlock::ImageEmbed { raw_target, .. } => Some(raw_target.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(img_embeds, vec!["diagram.svg", "logo.svg", "photo.png"]);
    }
}
