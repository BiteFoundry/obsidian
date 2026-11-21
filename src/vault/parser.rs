//! Obsidian-flavored Markdown & YAML Frontmatter parser.
//!
//! Natively extracts:
//! - YAML Frontmatter (`--- ... ---`): `tags`, `aliases`, `status`, `created`, `updated`, custom properties
//! - Wikilinks & Embeds: `[[Note]]`, `[[Note|Alias]]`, `[[Note#Heading]]`, `[[Folder/Note]]`, `![[image.png|400]]`
//! - Standard Markdown links & images: `[label](url)`, `![alt](path)`
//! - Inline & frontmatter tags: `#tag`, `#nested/tag` (ignoring code fences, inline code, and URL fragments)
//! - Obsidian Callouts: `> [!note]`, `> [!tip]-`, `> [!warning]+`, etc.
//! - Tasks / Checklists: `- [ ] ...` / `- [x] ...` with exact 1-based line numbers
//! - Headings Outline: `#`..`######` with exact 1-based line numbers
//! - Structured `NoteBlock` list for crisp TUI canvas rendering.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

pub use super::frontmatter::upsert_frontmatter_in_markdown;
use super::frontmatter::{closing_delimiter, opening_delimiter, read_properties};

use crate::models::{
    CalloutKind, HeadingEntry, InlineSpan, MarkdownLinkRef, NoteBlock, ParsedNote, TaskEntry,
    WikilinkRef,
};

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

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
    if matches!(lower.as_str(), "svg" | "html" | "htm" | "xhtml") {
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

    let mut wikilinks = Vec::new();
    let mut md_links = Vec::new();
    let mut headings = Vec::new();
    let mut tasks = Vec::new();
    let mut blocks = Vec::new();
    let mut plain_parts: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < body_lines.len() {
        let line_no = body_start_idx + i + 1;
        let line = body_lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block (``` or ~~~); backtick lines containing another backtick
        // (such as ```inline code```) are inline code spans, not multiline fences.
        if let Some((fence_char, fence_len)) = is_code_fence_opener(trimmed) {
            let language = trimmed[fence_len..].trim().to_string();
            let mut code_lines = Vec::new();
            i += 1;
            while i < body_lines.len() {
                let candidate = body_lines[i].trim();
                let closing_len = candidate.chars().take_while(|&c| c == fence_char).count();
                if closing_len >= fence_len && candidate[closing_len..].trim().is_empty() {
                    i += 1;
                    break;
                }
                code_lines.push(body_lines[i]);
                i += 1;
            }
            let code = code_lines.join("\n");
            if !code.is_empty() {
                plain_parts.push(code.clone());
            }
            blocks.push(NoteBlock::CodeBlock {
                language,
                code,
                line_no,
            });
            continue;
        }

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            blocks.push(NoteBlock::Divider { line_no });
            i += 1;
            continue;
        }

        // ATX Heading (`#` .. `######`)
        if let Some((level, heading_text)) = parse_heading_line(trimmed) {
            let spans = parse_inline_spans(
                heading_text,
                line_no,
                &mut wikilinks,
                &mut md_links,
                &mut tag_set,
            );
            let clean_text = spans_to_plain(&spans);
            headings.push(HeadingEntry {
                note_path: rel_path.to_string(),
                level,
                text: clean_text.clone(),
                line_no,
            });
            plain_parts.push(clean_text.clone());
            blocks.push(NoteBlock::Heading {
                level,
                text: clean_text,
                spans,
                line_no,
            });
            i += 1;
            continue;
        }

        // Standalone embed line: `![[target]]` or `![alt](path)`
        if let Some(block) = parse_standalone_embed(
            trimmed,
            line_no,
            &mut wikilinks,
            &mut md_links,
        ) {
            blocks.push(block);
            i += 1;
            continue;
        }

        // Obsidian Callout: `> [!type][+-]? optional title`
        if let Some((callout_kind, title, collapsed)) = parse_callout_header(trimmed) {
            let mut callout_body = Vec::new();
            i += 1;
            while i < body_lines.len() {
                let next_trim = body_lines[i].trim_start();
                if let Some(rest) = next_trim.strip_prefix('>') {
                    let inner = rest.strip_prefix(' ').unwrap_or(rest);
                    let ln = body_start_idx + i + 1;
                    let spans = parse_inline_spans(
                        inner,
                        ln,
                        &mut wikilinks,
                        &mut md_links,
                        &mut tag_set,
                    );
                    let plain = spans_to_plain(&spans);
                    if !plain.is_empty() {
                        plain_parts.push(plain);
                    }
                    callout_body.push(spans);
                    i += 1;
                } else {
                    break;
                }
            }
            if !title.is_empty() {
                plain_parts.push(title.clone());
            }
            blocks.push(NoteBlock::Callout {
                callout_kind,
                title,
                collapsed,
                body_lines: callout_body,
                line_no,
            });
            continue;
        }

        // Standard Blockquote
        if trimmed.starts_with('>') {
            let mut quote_lines = Vec::new();
            while i < body_lines.len() {
                let next_trim = body_lines[i].trim_start();
                if let Some(rest) = next_trim.strip_prefix('>') {
                    let inner = rest.strip_prefix(' ').unwrap_or(rest);
                    let ln = body_start_idx + i + 1;
                    let spans = parse_inline_spans(
                        inner,
                        ln,
                        &mut wikilinks,
                        &mut md_links,
                        &mut tag_set,
                    );
                    let plain = spans_to_plain(&spans);
                    if !plain.is_empty() {
                        plain_parts.push(plain);
                    }
                    quote_lines.push(spans);
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(NoteBlock::Blockquote {
                lines: quote_lines,
                line_no,
            });
            continue;
        }

        // Markdown Table (`| col1 | col2 |` followed by `| --- | --- |`)
        if trimmed.contains('|')
            && i + 1 < body_lines.len()
            && is_table_separator(body_lines[i + 1].trim())
        {
            let headers = split_table_row(trimmed);
            for cell in &headers {
                let _ = parse_inline_spans(
                    cell,
                    line_no,
                    &mut wikilinks,
                    &mut md_links,
                    &mut tag_set,
                );
            }
            plain_parts.push(headers.join(" "));
            i += 2;
            let mut rows = Vec::new();
            while i < body_lines.len() {
                let r_trim = body_lines[i].trim();
                if r_trim.is_empty() || !r_trim.contains('|') {
                    break;
                }
                let ln = body_start_idx + i + 1;
                let row = split_table_row(r_trim);
                for cell in &row {
                    let _ = parse_inline_spans(
                        cell,
                        ln,
                        &mut wikilinks,
                        &mut md_links,
                        &mut tag_set,
                    );
                }
                plain_parts.push(row.join(" "));
                rows.push(row);
                i += 1;
            }
            blocks.push(NoteBlock::Table {
                headers,
                rows,
                line_no,
            });
            continue;
        }

        // Task / Checklist item: `- [ ] text` or `- [x] text`
        if let Some((indent, checked, content)) = parse_task_line(line) {
            let spans = parse_inline_spans(
                content,
                line_no,
                &mut wikilinks,
                &mut md_links,
                &mut tag_set,
            );
            let clean_text = spans_to_plain(&spans);
            tasks.push(TaskEntry {
                note_path: rel_path.to_string(),
                note_title: note_title.to_string(),
                line_no,
                text: clean_text.clone(),
                checked,
            });
            plain_parts.push(clean_text.clone());
            blocks.push(NoteBlock::Task {
                checked,
                indent,
                text: clean_text,
                spans,
                line_no,
            });
            i += 1;
            continue;
        }

        // Bullet list item
        if let Some((indent, content)) = parse_bullet_line(line) {
            let spans = parse_inline_spans(
                content,
                line_no,
                &mut wikilinks,
                &mut md_links,
                &mut tag_set,
            );
            plain_parts.push(spans_to_plain(&spans));
            blocks.push(NoteBlock::BulletItem {
                indent,
                spans,
                line_no,
            });
            i += 1;
            continue;
        }

        // Numbered list item
        if let Some((indent, number, content)) = parse_numbered_line(line) {
            let spans = parse_inline_spans(
                content,
                line_no,
                &mut wikilinks,
                &mut md_links,
                &mut tag_set,
            );
            plain_parts.push(spans_to_plain(&spans));
            blocks.push(NoteBlock::NumberedItem {
                number,
                indent,
                spans,
                line_no,
            });
            i += 1;
            continue;
        }

        // Paragraph (single or contiguous lines until blank/structural line)
        let mut para_lines = vec![trimmed];
        i += 1;
        while i < body_lines.len() {
            let next = body_lines[i];
            let nt = next.trim();
            if nt.is_empty()
                || is_code_fence_opener(nt).is_some()
                || nt.starts_with('#')
                || nt.starts_with('>')
                || is_horizontal_rule(nt)
                || parse_task_line(next).is_some()
                || parse_bullet_line(next).is_some()
                || parse_numbered_line(next).is_some()
                || nt.starts_with("![[")
                || (nt.starts_with("![") && nt.ends_with(')'))
            {
                break;
            }
            if nt.contains('|')
                && i + 1 < body_lines.len()
                && is_table_separator(body_lines[i + 1].trim())
            {
                break;
            }
            para_lines.push(nt);
            i += 1;
        }
        let joined = para_lines.join(" ");
        let spans = parse_inline_spans(
            &joined,
            line_no,
            &mut wikilinks,
            &mut md_links,
            &mut tag_set,
        );
        plain_parts.push(spans_to_plain(&spans));
        blocks.push(NoteBlock::Paragraph { spans, line_no });
    }

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

/// Parses YAML frontmatter (`--- ... ---`) at the top of a Markdown file.
/// Returns `(frontmatter_map, first_body_line_index)`.
pub fn parse_frontmatter(lines: &[&str]) -> (BTreeMap<String, Value>, usize) {
    if !lines.first().is_some_and(|line| opening_delimiter(line)) {
        return (BTreeMap::new(), 0);
    }
    let Some(end) = lines.iter().skip(1).position(|line| closing_delimiter(line)) else {
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

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first = trimmed.chars().next().unwrap_or(' ');
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let count = trimmed.chars().filter(|&c| c == first).count();
    count >= 3 && trimmed.chars().all(|c| c == first || c.is_whitespace())
}

fn parse_heading_line(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((hashes as u8, rest.trim()))
}

fn parse_callout_header(trimmed: &str) -> Option<(CalloutKind, String, bool)> {
    let rest = trimmed.strip_prefix('>')?.trim_start();
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

/// Parses a single Markdown task line, returning `(checked, clean_plain_text, raw_content_trimmed)`.
pub fn parse_single_task_line(line: &str) -> Option<(bool, String, String)> {
    let (_, checked, content) = parse_task_line(line)?;
    let mut wikilinks = Vec::new();
    let mut md_links = Vec::new();
    let mut tags = BTreeSet::new();
    let spans = parse_inline_spans(content, 1, &mut wikilinks, &mut md_links, &mut tags);
    let clean_text = spans_to_plain(&spans);
    Some((checked, clean_text, content.trim().to_string()))
}

fn parse_task_line(line: &str) -> Option<(usize, bool, &str)> {
    let indent = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let trimmed = &line[indent..];
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    if let Some(rest) = after_bullet.strip_prefix("[ ] ") {
        Some((indent, false, rest))
    } else if after_bullet == "[ ]" {
        Some((indent, false, ""))
    } else if let Some(rest) = after_bullet
        .strip_prefix("[x] ")
        .or_else(|| after_bullet.strip_prefix("[X] "))
    {
        Some((indent, true, rest))
    } else if after_bullet == "[x]" || after_bullet == "[X]" {
        Some((indent, true, ""))
    } else {
        None
    }
}

fn parse_bullet_line(line: &str) -> Option<(usize, &str)> {
    let indent = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let trimmed = &line[indent..];
    let rest = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    Some((indent, rest))
}

fn parse_numbered_line(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let trimmed = &line[indent..];
    let digits_len = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    let after = &trimmed[digits_len..];
    let rest = after.strip_prefix(". ").or_else(|| after.strip_prefix(") "))?;
    Some((indent, trimmed[..digits_len].to_string(), rest))
}

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

fn split_wikilink_alias(inner: &str) -> (&str, Option<&str>) {
    match inner.split_once('|') {
        Some((lhs, rhs)) => {
            let lhs = lhs.strip_suffix('\\').unwrap_or(lhs);
            (lhs.trim(), Some(rhs.trim()))
        }
        None => (inner.trim(), None),
    }
}

fn wrap_formatted_spans(
    inner_spans: Vec<InlineSpan>,
    wrap: impl Fn(String) -> InlineSpan,
) -> Vec<InlineSpan> {
    inner_spans
        .into_iter()
        .map(|span| match span {
            InlineSpan::Text { text } => wrap(text),
            other => other,
        })
        .collect()
}

fn is_table_separator(trimmed: &str) -> bool {
    if !trimmed.contains('-') || !trimmed.contains('|') {
        return false;
    }
    trimmed
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn split_table_row(trimmed: &str) -> Vec<String> {
    let s = trimmed.trim();
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = if s.ends_with('|') && !s.ends_with("\\|") {
        &s[..s.len() - 1]
    } else {
        s
    };
    let bytes = s.as_bytes();
    let mut cells = Vec::new();
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'\\' && idx + 1 < bytes.len() {
            idx += 2;
            continue;
        }
        if bytes[idx] == b'`' {
            if let Some(end) = find_inline_code_end(s, idx) {
                idx = end;
            } else {
                idx += bytes[idx..].iter().take_while(|&&b| b == b'`').count();
            }
            continue;
        }
        if bytes[idx] == b'|' {
            cells.push(s[start..idx].trim().to_string());
            idx += 1;
            start = idx;
            continue;
        }
        idx += 1;
    }
    cells.push(s[start..].trim().to_string());
    cells
}

fn parse_standalone_embed(
    trimmed: &str,
    line_no: usize,
    wikilinks: &mut Vec<WikilinkRef>,
    md_links: &mut Vec<MarkdownLinkRef>,
) -> Option<NoteBlock> {
    if let Some(inner) = trimmed.strip_prefix("![[").and_then(|s| s.strip_suffix("]]")) {
        if inner.contains("[[") || inner.contains("]]") {
            return None;
        }
        let (target_part, alias_part) = split_wikilink_alias(inner);
        let (target, heading) = match target_part.split_once('#') {
            Some((t, h)) => (t.trim(), Some(h.trim().to_string())),
            None => (target_part, None),
        };
        if target.is_empty() {
            return None;
        }
        if is_image_target(target) {
            let width_hint = alias_part.and_then(|a| a.parse::<u32>().ok());
            let alt = alias_part
                .filter(|a| a.parse::<u32>().is_err())
                .unwrap_or(target)
                .to_string();
            wikilinks.push(WikilinkRef {
                target_raw: target.to_string(),
                alias: alias_part.map(String::from),
                heading: None,
                is_embed: true,
                line_no,
            });
            return Some(NoteBlock::ImageEmbed {
                raw_target: target.to_string(),
                alt,
                width_hint,
                resolved_path: None,
                line_no,
            });
        } else {
            wikilinks.push(WikilinkRef {
                target_raw: target.to_string(),
                alias: alias_part.map(String::from),
                heading: heading.clone(),
                is_embed: true,
                line_no,
            });
            return Some(NoteBlock::NoteEmbed {
                target: target.to_string(),
                heading,
                resolved_path: None,
                line_no,
            });
        }
    }

    if trimmed.starts_with("![") {
        if let Some(close_bracket) = find_matching_bracket(trimmed, 1) {
            if trimmed.as_bytes().get(close_bracket + 1) == Some(&b'(') {
                if let Some(dest) = parse_inline_markdown_link_dest(trimmed, close_bracket + 1) {
                    if dest.close_end == trimmed.len() {
                        let alt = &trimmed[2..close_bracket];
                        let target = trimmed[dest.dest_start..dest.dest_end].trim();
                        if !target.is_empty() && is_image_target(target) {
                            md_links.push(MarkdownLinkRef {
                                label: alt.to_string(),
                                url: target.to_string(),
                                title: dest.title,
                                is_image: true,
                                line_no,
                            });
                            return Some(NoteBlock::ImageEmbed {
                                raw_target: target.to_string(),
                                alt: if alt.is_empty() {
                                    target.to_string()
                                } else {
                                    alt.to_string()
                                },
                                width_hint: None,
                                resolved_path: None,
                                line_no,
                            });
                        }
                    }
                }
            }
        }
    }

    None
}

/// Parses inline Obsidian Markdown spans (`[[wikilinks]]`, `![[embeds]]`, `` `code` ``,
/// `**bold**`, `*italic*`, `==highlight==`, `~~strike~~`, `[label](url)`, `#tags`).
pub fn parse_inline_spans(
    input: &str,
    line_no: usize,
    wikilinks: &mut Vec<WikilinkRef>,
    md_links: &mut Vec<MarkdownLinkRef>,
    tag_set: &mut BTreeSet<String>,
) -> Vec<InlineSpan> {
    let mut spans: Vec<InlineSpan> = Vec::new();
    let mut text_buf = String::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    let flush_text = |buf: &mut String, spans: &mut Vec<InlineSpan>| {
        if !buf.is_empty() {
            spans.push(InlineSpan::Text {
                text: std::mem::take(buf),
            });
        }
    };

    while i < len {
        let rest = &input[i..];

        // Inline code: `...`, ``...``, ```...``` (matching exact backtick count)
        if rest.starts_with('`') {
            let ticks = rest.bytes().take_while(|&b| b == b'`').count();
            if let Some(end) = find_inline_code_end(input, i) {
                flush_text(&mut text_buf, &mut spans);
                spans.push(InlineSpan::Code {
                    text: input[i + ticks..end - ticks].to_string(),
                });
                i = end;
                continue;
            }
            text_buf.push_str(&rest[..ticks]);
            i += ticks;
            continue;
        }

        // Embed or Wikilink: `![[...]]` or `[[...]]`
        let is_embed = rest.starts_with("![[");
        if is_embed || rest.starts_with("[[") {
            let prefix_len = if is_embed { 3 } else { 2 };
            if let Some(close) = rest[prefix_len..].find("]]") {
                let inner = &rest[prefix_len..prefix_len + close];
                if !inner.contains('\n') && !inner.is_empty() {
                    let (target_part, alias_opt) = split_wikilink_alias(inner);
                    let alias = alias_opt.map(String::from);
                    let (target, heading) = match target_part.split_once('#') {
                        Some((t, h)) => (t.trim(), Some(h.trim().to_string())),
                        None => (target_part, None),
                    };
                    if !target.is_empty() {
                        flush_text(&mut text_buf, &mut spans);
                        wikilinks.push(WikilinkRef {
                            target_raw: target.to_string(),
                            alias: alias.clone(),
                            heading: heading.clone(),
                            is_embed,
                            line_no,
                        });
                        spans.push(InlineSpan::Wikilink {
                            target: target.to_string(),
                            heading,
                            alias,
                            resolved_path: None,
                        });
                        i += prefix_len + close + 2;
                        continue;
                    } else if let Some(h) = heading.filter(|h| !h.is_empty()) {
                        // Same-note heading/block reference like `[[#Section]]` must not leak `#Section` as a tag.
                        flush_text(&mut text_buf, &mut spans);
                        let display = alias.unwrap_or_else(|| format!("#{h}"));
                        spans.push(InlineSpan::Text { text: display });
                        i += prefix_len + close + 2;
                        continue;
                    }
                }
            }
        }

        // Standard Markdown link / inline image: `![alt](url)` or `[label](url)`
        let md_is_img = rest.starts_with("![");
        if md_is_img || rest.starts_with('[') {
            let open_bracket = if md_is_img { i + 1 } else { i };
            if let Some(close_bracket) = find_matching_bracket(input, open_bracket) {
                if bytes.get(close_bracket + 1) == Some(&b'(') {
                    if let Some(dest) = parse_inline_markdown_link_dest(input, close_bracket + 1) {
                        let label = &input[open_bracket + 1..close_bracket];
                        let url = input[dest.dest_start..dest.dest_end].trim();
                        if !url.is_empty() && !label.contains('\n') {
                            flush_text(&mut text_buf, &mut spans);
                            md_links.push(MarkdownLinkRef {
                                label: label.to_string(),
                                url: url.to_string(),
                                title: dest.title,
                                is_image: md_is_img,
                                line_no,
                            });
                            spans.push(InlineSpan::ExternalLink {
                                label: if label.is_empty() {
                                    url.to_string()
                                } else {
                                    label.to_string()
                                },
                                url: url.to_string(),
                            });
                            i = dest.close_end;
                            continue;
                        }
                    }
                }
            }
        }

        // Bold: `**...**`
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                if end > 0 {
                    let inner = &rest[2..2 + end];
                    let inner_spans =
                        parse_inline_spans(inner, line_no, wikilinks, md_links, tag_set);
                    flush_text(&mut text_buf, &mut spans);
                    spans.extend(wrap_formatted_spans(inner_spans, |text| InlineSpan::Bold {
                        text,
                    }));
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Highlight: `==...==`
        if rest.starts_with("==") {
            if let Some(end) = rest[2..].find("==") {
                if end > 0 {
                    let inner = &rest[2..2 + end];
                    let inner_spans =
                        parse_inline_spans(inner, line_no, wikilinks, md_links, tag_set);
                    flush_text(&mut text_buf, &mut spans);
                    spans.extend(wrap_formatted_spans(inner_spans, |text| {
                        InlineSpan::Highlight { text }
                    }));
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Strikethrough: `~~...~~`
        if rest.starts_with("~~") {
            if let Some(end) = rest[2..].find("~~") {
                if end > 0 {
                    let inner = &rest[2..2 + end];
                    let inner_spans =
                        parse_inline_spans(inner, line_no, wikilinks, md_links, tag_set);
                    flush_text(&mut text_buf, &mut spans);
                    spans.extend(wrap_formatted_spans(inner_spans, |text| {
                        InlineSpan::Strikethrough { text }
                    }));
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Italic: `*...*` (avoiding `**`)
        if rest.starts_with('*') && !rest.starts_with("**") {
            if let Some(end) = rest[1..].find('*') {
                if end > 0 && !rest[1..].starts_with(' ') {
                    let inner = &rest[1..1 + end];
                    let inner_spans =
                        parse_inline_spans(inner, line_no, wikilinks, md_links, tag_set);
                    flush_text(&mut text_buf, &mut spans);
                    spans.extend(wrap_formatted_spans(inner_spans, |text| {
                        InlineSpan::Italic { text }
                    }));
                    i += 1 + end + 1;
                    continue;
                }
            }
        }

        // Inline tag `#tag` or `#nested/tag` (must be at start or preceded by whitespace/opening punct)
        if rest.starts_with('#') {
            let prev_char = input[..i].chars().next_back();
            let valid_boundary = prev_char
                .map(|c| c.is_whitespace() || matches!(c, '(' | '[' | ',' | ';' | ':'))
                .unwrap_or(true);
            if valid_boundary {
                let tag_len = rest[1..]
                    .char_indices()
                    .take_while(|(_, c)| {
                        c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
                    })
                    .last()
                    .map(|(idx, c)| idx + c.len_utf8())
                    .unwrap_or(0);
                if tag_len > 0 {
                    let raw_tag = &rest[1..1 + tag_len];
                    if let Some(clean_tag) = normalize_tag(raw_tag) {
                        flush_text(&mut text_buf, &mut spans);
                        tag_set.insert(clean_tag.clone());
                        spans.push(InlineSpan::Tag { tag: clean_tag });
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
                    flush_text(&mut text_buf, &mut spans);
                    md_links.push(MarkdownLinkRef {
                        label: url.to_string(),
                        url: url.to_string(),
                        title: None,
                        is_image: false,
                        line_no,
                    });
                    spans.push(InlineSpan::ExternalLink {
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

    flush_text(&mut text_buf, &mut spans);
    spans
}

pub fn spans_to_plain(spans: &[InlineSpan]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.plain_text());
    }
    out
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
        assert_eq!(parsed.frontmatter.get("status").and_then(|v| v.as_str()), Some("active"));
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
            Value::Array(vec![Value::String("core".into()), Value::String("rfc".into())]),
        );
        let updated = upsert_frontmatter_in_markdown(orig, &updates).unwrap();
        assert!(updated.contains("status: published"));
        assert!(updated.contains("tags: [core, rfc]"));
        assert!(updated.contains("# Hello"));
        assert!(updated.contains("Body text."));
    }

    #[test]
    fn test_is_image_target_rejects_html_and_svg() {
        assert!(is_image_target("diagram.png"));
        assert!(is_image_target("assets/photo.JPEG?v=2#top"));
        assert!(is_image_target("anim.gif"));
        assert!(is_image_target("pic.webp"));
        assert!(is_image_target("scan.bmp"));

        // Must never classify SVG or HTML/HTM as raster images
        assert!(!is_image_target("vector.svg"));
        assert!(!is_image_target("icons/logo.SVG?raw=1"));
        assert!(!is_image_target("page.html"));
        assert!(!is_image_target("report.HTM"));
        assert!(!is_image_target("doc.xhtml"));
        assert!(!is_image_target("Note.md"));

        // Standalone embeds of .svg / .html must not produce ImageEmbed blocks
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
        assert_eq!(img_embeds, vec!["photo.png"]);
    }
}
