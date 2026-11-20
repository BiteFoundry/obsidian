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
    let lines: Vec<&str> = raw.lines().collect();
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

        // Fenced code block (``` or ~~~)
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence_char = trimmed.chars().next().unwrap_or('`');
            let fence_len = trimmed.chars().take_while(|&c| c == fence_char).count();
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
            plain_parts.push(headers.join(" "));
            i += 2;
            let mut rows = Vec::new();
            while i < body_lines.len() {
                let r_trim = body_lines[i].trim();
                if r_trim.is_empty() || !r_trim.contains('|') {
                    break;
                }
                let ln = body_start_idx + i + 1;
                let _ = parse_inline_spans(
                    r_trim,
                    ln,
                    &mut wikilinks,
                    &mut md_links,
                    &mut tag_set,
                );
                let row = split_table_row(r_trim);
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
                || nt.starts_with("```")
                || nt.starts_with("~~~")
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
    let mut map = BTreeMap::new();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (map, 0);
    }

    let mut end_idx = None;
    for (idx, line) in lines.iter().enumerate().skip(1) {
        let t = line.trim();
        if t == "---" || t == "..." {
            end_idx = Some(idx);
            break;
        }
    }

    let Some(end) = end_idx else {
        return (map, 0);
    };

    let mut current_list_key: Option<String> = None;
    let mut current_list: Vec<Value> = Vec::new();

    let flush_list = |map: &mut BTreeMap<String, Value>,
                      key: &mut Option<String>,
                      list: &mut Vec<Value>| {
        if let Some(k) = key.take() {
            map.insert(k, Value::Array(std::mem::take(list)));
        }
    };

    for &raw_line in &lines[1..end] {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Continuation YAML list item (`  - item`)
        if let Some(item_raw) = trimmed.strip_prefix("- ").or_else(|| {
            if trimmed == "-" {
                Some("")
            } else {
                None
            }
        }) {
            if current_list_key.is_some() {
                current_list.push(parse_yaml_scalar(item_raw.trim()));
                continue;
            }
        }

        // Key-value pair (`key: value` or `key:`)
        if let Some((k_raw, v_raw)) = raw_line.split_once(':') {
            let key = k_raw.trim().trim_matches(['"', '\'']).to_string();
            if !key.is_empty() {
                flush_list(&mut map, &mut current_list_key, &mut current_list);
                let val_str = v_raw.trim();
                if val_str.is_empty() {
                    current_list_key = Some(key);
                } else if val_str.starts_with('[') && val_str.ends_with(']') {
                    let inner = &val_str[1..val_str.len() - 1];
                    let items: Vec<Value> = inner
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(parse_yaml_scalar)
                        .collect();
                    map.insert(key, Value::Array(items));
                } else {
                    map.insert(key, parse_yaml_scalar(val_str));
                }
            }
        }
    }

    flush_list(&mut map, &mut current_list_key, &mut current_list);
    (map, end + 1)
}

fn parse_yaml_scalar(raw: &str) -> Value {
    let trimmed = raw.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        return Value::String(trimmed[1..trimmed.len() - 1].to_string());
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" | "~" => Value::Null,
        _ => {
            if let Ok(i) = trimmed.parse::<i64>() {
                Value::Number(i.into())
            } else if let Ok(f) = trimmed.parse::<f64>() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or_else(|| Value::String(trimmed.to_string()))
            } else {
                Value::String(trimmed.to_string())
            }
        }
    }
}

/// Cleanly inserts or updates YAML frontmatter keys while preserving existing keys and note body.
pub fn upsert_frontmatter_in_markdown(raw: &str, updates: &BTreeMap<String, Value>) -> String {
    let line_ending = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = raw.lines().collect();
    let (mut fm, body_start_idx) = parse_frontmatter(&lines);

    for (k, v) in updates {
        if v.is_null() {
            fm.remove(k);
        } else {
            fm.insert(k.clone(), v.clone());
        }
    }

    let body = if body_start_idx <= lines.len() {
        lines[body_start_idx..].join(line_ending)
    } else {
        String::new()
    };

    if fm.is_empty() {
        return body;
    }

    let mut out = String::new();
    out.push_str("---");
    out.push_str(line_ending);
    for (k, v) in &fm {
        match v {
            Value::Array(items) => {
                if items.is_empty() {
                    out.push_str(&format!("{k}: []{line_ending}"));
                } else {
                    let rendered: Vec<String> = items.iter().map(format_yaml_scalar).collect();
                    out.push_str(&format!("{k}: [{}]{line_ending}", rendered.join(", ")));
                }
            }
            other => {
                out.push_str(&format!("{k}: {}{line_ending}", format_yaml_scalar(other)));
            }
        }
    }
    out.push_str("---");
    out.push_str(line_ending);
    let body_trimmed = body.trim_start_matches(['\r', '\n']);
    if !body_trimmed.is_empty() {
        out.push_str(line_ending);
        out.push_str(body_trimmed);
        if !out.ends_with('\n') {
            out.push_str(line_ending);
        }
    }
    out
}

fn format_yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.is_empty()
                || s.contains(':')
                || s.contains('#')
                || s.contains('[')
                || s.contains(']')
                || s.contains(',')
                || s.starts_with(' ')
                || s.ends_with(' ')
            {
                format!("\"{}\"", s.replace('"', "\\\""))
            } else {
                s.clone()
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
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

fn is_table_separator(trimmed: &str) -> bool {
    if !trimmed.contains('-') || !trimmed.contains('|') {
        return false;
    }
    trimmed
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn split_table_row(trimmed: &str) -> Vec<String> {
    let s = trimmed.trim().trim_start_matches('|').trim_end_matches('|');
    s.split('|').map(|c| c.trim().to_string()).collect()
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
        let (target_part, alias_part) = match inner.split_once('|') {
            Some((t, a)) => (t.trim(), Some(a.trim())),
            None => (inner.trim(), None),
        };
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

    if let Some(rest) = trimmed.strip_prefix("![") {
        if let Some((alt, url_part)) = rest.split_once("](") {
            if let Some(url) = url_part.strip_suffix(')') {
                let target = url.trim().trim_matches(['<', '>']);
                if !target.is_empty() && !alt.contains(']') && is_image_target(target) {
                    md_links.push(MarkdownLinkRef {
                        label: alt.to_string(),
                        url: target.to_string(),
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

        // Inline code: `...`
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                flush_text(&mut text_buf, &mut spans);
                spans.push(InlineSpan::Code {
                    text: rest[1..1 + end].to_string(),
                });
                i += 1 + end + 1;
                continue;
            }
        }

        // Embed or Wikilink: `![[...]]` or `[[...]]`
        let is_embed = rest.starts_with("![[");
        if is_embed || rest.starts_with("[[") {
            let prefix_len = if is_embed { 3 } else { 2 };
            if let Some(close) = rest[prefix_len..].find("]]") {
                let inner = &rest[prefix_len..prefix_len + close];
                if !inner.contains('\n') && !inner.is_empty() {
                    let (target_part, alias) = match inner.split_once('|') {
                        Some((t, a)) => (t.trim(), Some(a.trim().to_string())),
                        None => (inner.trim(), None),
                    };
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
                    }
                }
            }
        }

        // Standard Markdown link / inline image: `![alt](url)` or `[label](url)`
        let md_is_img = rest.starts_with("![");
        if md_is_img || rest.starts_with('[') {
            let start_off = if md_is_img { 2 } else { 1 };
            if let Some(mid) = rest[start_off..].find("](") {
                let label = &rest[start_off..start_off + mid];
                let after_mid = &rest[start_off + mid + 2..];
                if let Some(close) = after_mid.find(')') {
                    let url = after_mid[..close].trim();
                    if !url.is_empty() && !label.contains('\n') {
                        flush_text(&mut text_buf, &mut spans);
                        md_links.push(MarkdownLinkRef {
                            label: label.to_string(),
                            url: url.to_string(),
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
                        i += start_off + mid + 2 + close + 1;
                        continue;
                    }
                }
            }
        }

        // Bold: `**...**`
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                if end > 0 {
                    flush_text(&mut text_buf, &mut spans);
                    spans.push(InlineSpan::Bold {
                        text: rest[2..2 + end].to_string(),
                    });
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Highlight: `==...==`
        if rest.starts_with("==") {
            if let Some(end) = rest[2..].find("==") {
                if end > 0 {
                    flush_text(&mut text_buf, &mut spans);
                    spans.push(InlineSpan::Highlight {
                        text: rest[2..2 + end].to_string(),
                    });
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Strikethrough: `~~...~~`
        if rest.starts_with("~~") {
            if let Some(end) = rest[2..].find("~~") {
                if end > 0 {
                    flush_text(&mut text_buf, &mut spans);
                    spans.push(InlineSpan::Strikethrough {
                        text: rest[2..2 + end].to_string(),
                    });
                    i += 2 + end + 2;
                    continue;
                }
            }
        }

        // Italic: `*...*` (avoiding `**`)
        if rest.starts_with('*') && !rest.starts_with("**") {
            if let Some(end) = rest[1..].find('*') {
                if end > 0 && !rest[1..].starts_with(' ') {
                    flush_text(&mut text_buf, &mut spans);
                    spans.push(InlineSpan::Italic {
                        text: rest[1..1 + end].to_string(),
                    });
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
        let updated = upsert_frontmatter_in_markdown(orig, &updates);
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
