//! Typed YAML reads and source-range edits for Markdown frontmatter.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tree_sitter::Node;

pub(crate) fn opening_delimiter(line: &str) -> bool {
    line.trim_start_matches('\u{feff}').trim_end() == "---"
}

pub(crate) fn closing_delimiter(line: &str) -> bool {
    matches!(line.trim_end(), "---" | "...")
}

pub(crate) fn read_properties(yaml: &str) -> Result<BTreeMap<String, Value>> {
    let properties =
        serde_yaml_ng::from_str::<Option<BTreeMap<String, serde_yaml_ng::Value>>>(yaml)
            .context("Invalid YAML frontmatter mapping")?
            .unwrap_or_default();
    properties
        .into_iter()
        .map(|(key, value)| Ok((key, serde_json::to_value(value)?)))
        .collect()
}

/// Replace top-level property values via YAML source offsets. Untouched source,
/// including the Markdown after the closing delimiter, is copied byte for byte.
pub fn upsert_frontmatter_in_markdown(
    raw: &str,
    updates: &BTreeMap<String, Value>,
) -> Result<String> {
    if updates.is_empty() {
        return Ok(raw.to_owned());
    }

    let opening_end = line_end(raw, 0);
    let closing_start = frontmatter_closing_start(raw, opening_end);
    let eol = if raw[..opening_end].ends_with("\r\n")
        || (closing_start.is_none() && raw.contains("\r\n"))
    {
        "\r\n"
    } else {
        "\n"
    };
    let mut yaml = closing_start
        .map(|end| raw[opening_end..end].to_owned())
        .unwrap_or_default();
    let before = read_properties(&yaml)?;
    if updates
        .iter()
        .all(|(key, value)| before.get(key) == (!value.is_null()).then_some(value))
    {
        return Ok(raw.to_owned());
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_yaml::LANGUAGE.into())?;
    for (key, value) in updates {
        let exists = before.contains_key(key);
        if exists || !value.is_null() {
            edit_property(&mut parser, &mut yaml, key, value, exists, eol)?;
        }
    }

    let after = read_properties(&yaml).context("Frontmatter edit would produce invalid YAML")?;
    for (key, wanted) in updates {
        let expected = (!wanted.is_null()).then_some(wanted);
        if after.get(key) != expected {
            bail!("Frontmatter edit did not round-trip property {key:?}");
        }
    }
    if let Some(end) = closing_start {
        return Ok(format!("{}{yaml}{}", &raw[..opening_end], &raw[end..]));
    }
    if yaml.is_empty() {
        return Ok(raw.to_owned());
    }
    let (bom, body) = raw
        .strip_prefix('\u{feff}')
        .map_or(("", raw), |body| ("\u{feff}", body));
    Ok(format!("{bom}---{eol}{yaml}---{eol}{body}"))
}

pub(crate) fn markdown_body_offset(raw: &str) -> usize {
    let bom_end = raw.len() - raw.trim_start_matches('\u{feff}').len();
    frontmatter_closing_start(raw, line_end(raw, 0)).map_or(bom_end, |start| line_end(raw, start))
}

fn frontmatter_closing_start(raw: &str, opening_end: usize) -> Option<usize> {
    if !opening_delimiter(&raw[..opening_end]) {
        return None;
    }
    let mut offset = opening_end;
    raw[opening_end..].split_inclusive('\n').find_map(|line| {
        let start = offset;
        offset += line.len();
        closing_delimiter(line).then_some(start)
    })
}

fn edit_property(
    parser: &mut tree_sitter::Parser,
    yaml: &mut String,
    key: &str,
    value: &Value,
    exists: bool,
    eol: &str,
) -> Result<()> {
    let tree = parser
        .parse(yaml.as_bytes(), None)
        .context("Cannot locate YAML properties")?;
    let root = tree.root_node();
    if root.has_error() {
        bail!("Cannot locate property ranges in YAML frontmatter");
    }
    let mapping = child(root, "document")
        .and_then(|document| child(document, "block_node").or_else(|| child(document, "flow_node")))
        .and_then(|node| child(node, "block_mapping").or_else(|| child(node, "flow_mapping")));
    let mut pair = None;
    if let Some(mapping) = mapping {
        let mut cursor = mapping.walk();
        for entry in mapping.named_children(&mut cursor) {
            let key_node = entry.child_by_field_name("key").or_else(|| {
                (mapping.kind() == "flow_mapping" && entry.kind() == "flow_node").then_some(entry)
            });
            if key_node.is_some_and(|node| {
                serde_yaml_ng::from_str::<String>(&yaml[node.byte_range()])
                    .is_ok_and(|candidate| candidate == key)
            }) && pair.replace(entry).is_some()
            {
                bail!("Duplicate YAML property {key:?}");
            }
        }
    }

    if pair.is_some() != exists {
        bail!("Cannot locate a unique source range for YAML property {key:?}");
    }
    let flow = mapping.filter(|node| node.kind() == "flow_mapping");
    match (pair, value.is_null()) {
        (Some(pair), true) => remove_property(yaml, pair, flow.is_some()),
        (Some(pair), false) => replace_value(yaml, pair, value, flow.is_some(), eol)?,
        (None, true) => {}
        (None, false) => {
            let entry = format!("{}: {}", render(&Value::String(key.into())), render(value));
            if let Some(flow) = flow {
                let close = child(flow, "}").context("Missing closing brace for YAML mapping")?;
                let mut last = close.prev_sibling();
                while last.is_some_and(|node| node.kind() == "comment") {
                    last = last.and_then(|node| node.prev_sibling());
                }
                let sep = if last.is_some_and(|node| matches!(node.kind(), "{" | ",")) {
                    ""
                } else {
                    ", "
                };
                yaml.insert_str(close.start_byte(), &format!("{sep}{entry}"));
            } else {
                let indent = mapping.map_or("", |node| {
                    &yaml[line_start(yaml, node.start_byte())..node.start_byte()]
                });
                yaml.push_str(&format!("{indent}{entry}{eol}"));
            }
        }
    }
    Ok(())
}

fn child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|node| node.kind() == kind);
    result
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, offset: usize) -> usize {
    text[offset..]
        .find('\n')
        .map_or(text.len(), |i| offset + i + 1)
}

// YAML nodes can include a trailing comment or the newline after a block scalar.
fn content_end(text: &str, node: Node<'_>) -> usize {
    if matches!(
        node.kind(),
        "block_node"
            | "block_mapping"
            | "block_mapping_pair"
            | "block_sequence"
            | "block_sequence_item"
            | "flow_node"
            | "flow_pair"
    ) {
        let mut cursor = node.walk();
        if let Some(last) = node
            .children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .last()
        {
            return content_end(text, last);
        }
    }
    node.start_byte()
        + text[node.byte_range()]
            .trim_end_matches([' ', '\t', '\r', '\n'])
            .len()
}

fn trailing_comment(node: Node<'_>, row: usize) -> Option<Node<'_>> {
    if node.start_position().row > row || node.end_position().row < row {
        return None;
    }
    if node.kind() == "comment" && node.start_position().row == row {
        return Some(node);
    }
    let mut cursor = node.walk();
    let result = node
        .children(&mut cursor)
        .find_map(|child| trailing_comment(child, row));
    result
}

fn remove_property(yaml: &mut String, pair: Node<'_>, flow: bool) {
    let start = pair.start_byte();
    let end = content_end(yaml, pair);
    if flow {
        let adjacent = |mut candidate: Option<Node<'_>>, forward: bool| {
            while let Some(node) = candidate {
                if node.kind() != "comment" {
                    return (node.kind() == ",").then_some(node.byte_range());
                }
                candidate = if forward {
                    node.next_sibling()
                } else {
                    node.prev_sibling()
                };
            }
            None
        };
        if let Some(comma) = adjacent(pair.next_sibling(), true) {
            yaml.replace_range(comma, "");
            yaml.replace_range(start..end, "");
        } else {
            yaml.replace_range(start..end, "");
            if let Some(comma) = adjacent(pair.prev_sibling(), false) {
                yaml.replace_range(comma, "");
            }
        }
    } else {
        if let Some(comment) = pair
            .parent()
            .and_then(|mapping| trailing_comment(mapping, pair.start_position().row))
        {
            let after_comment = line_end(yaml, comment.end_byte());
            if after_comment <= line_end(yaml, end) {
                yaml.replace_range(after_comment..line_end(yaml, end), "");
            }
            yaml.replace_range(start..comment.start_byte(), "");
        } else {
            yaml.replace_range(line_start(yaml, start)..line_end(yaml, end), "");
        }
    }
}

fn replace_value(
    yaml: &mut String,
    pair: Node<'_>,
    value: &Value,
    flow: bool,
    eol: &str,
) -> Result<()> {
    let colon = child(pair, ":");
    let rendered = render(value);
    let Some(old) = pair.child_by_field_name("value") else {
        if let Some(colon) = colon {
            yaml.insert_str(colon.end_byte(), &format!(" {rendered}"));
        } else if flow {
            yaml.insert_str(content_end(yaml, pair), &format!(": {rendered}"));
        } else {
            let start = pair.start_byte();
            let indent = &yaml[line_start(yaml, start)..start];
            let entry = format!("{indent}: {rendered}{eol}");
            yaml.insert_str(line_end(yaml, content_end(yaml, pair)), &entry);
        }
        return Ok(());
    };

    let end = content_end(yaml, old);
    let start = old.start_byte();
    let anchored = child(old, "anchor");
    let rendered = match anchored {
        Some(anchor) => format!("{} {rendered}", &yaml[anchor.byte_range()]),
        None => rendered,
    };
    if flow {
        yaml.replace_range(start..end, &rendered);
        return Ok(());
    }
    let colon = colon.context("Missing value indicator for YAML property")?;
    let row = colon.start_position().row;
    if old.start_position().row != row {
        yaml.replace_range(line_start(yaml, start)..line_end(yaml, end), "");
        yaml.insert_str(colon.end_byte(), &format!(" {rendered}"));
    } else if let Some(comment) =
        trailing_comment(pair, row).filter(|c| (start..end).contains(&c.start_byte()))
    {
        let first_line_end = line_end(yaml, comment.end_byte());
        let spacing = yaml[start..comment.start_byte()]
            .trim_end_matches([' ', '\t'])
            .len()
            + start;
        let suffix = &yaml[spacing..first_line_end];
        yaml.replace_range(start..line_end(yaml, end), &format!("{rendered}{suffix}"));
    } else {
        yaml.replace_range(start..end, &rendered);
    }
    Ok(())
}

fn render(value: &Value) -> String {
    match value {
        Value::String(s)
            if s.starts_with(|ch: char| ch.is_alphabetic() || ch == '_')
                && s.trim() == s
                && s.chars()
                    .all(|ch| ch.is_alphanumeric() || "_-. /".contains(ch))
                && serde_yaml_ng::from_str::<serde_yaml_ng::Value>(s)
                    .is_ok_and(|value| value.as_str() == Some(s)) =>
        {
            s.clone()
        }
        Value::Array(items) => {
            let items = items.iter().map(render).collect::<Vec<_>>().join(", ");
            format!("[{items}]")
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(raw: &str, changes: Value) -> Result<String> {
        upsert_frontmatter_in_markdown(raw, &serde_json::from_value(changes)?)
    }

    #[test]
    fn patch_round_trips_quoted_strings_and_nested_yaml() {
        let before = "---\r\n# review\r\nid: \"00127\"\nliteral: 'true'\r\nstatus: draft  # next step\r\ntracking:\r\n  reviewer: owner\r\n  ticket: QA\r\n---\r\n\r\n\r\n# Review\nBody.";
        let after = edit(before, json!({"status": "ready"})).unwrap();
        assert_eq!(after, before.replace("status: draft", "status: ready"));
        let parsed = super::super::parser::parse_markdown("Review.md", "Review", &after);
        assert_eq!(parsed.frontmatter["id"], "00127");
        assert_eq!(parsed.frontmatter["literal"], "true");
        assert_eq!(
            parsed.frontmatter["tracking"],
            json!({"reviewer":"owner", "ticket":"QA"})
        );
    }

    #[test]
    fn edits_block_values_and_appends_typed_properties() {
        let header = "---\r\ntracking:  # owner\n  reviewer: owner\r\n  ticket: QA\n# leave this\r\nmessage: |+ # keep this too\r\n  first\r\n  ---\n  last\r\n\r\nnullish:\n...\r\n\r\n\r\nTail";
        let updated = edit(
            header,
            json!({
                "tracking": null,
                "message": "ready",
                "nullish": null,
                "new: list": ["true", "00127", "emoji 🚀", 7, false],
            }),
        )
        .unwrap();
        assert_eq!(updated, "---\r\n# owner\n# leave this\r\nmessage: ready # keep this too\r\n\r\n\"new: list\": [\"true\", \"00127\", \"emoji 🚀\", 7, false]\r\n...\r\n\r\n\r\nTail");
        let parsed = super::super::parser::parse_markdown("Review.md", "Review", &updated);
        assert_eq!(
            parsed.frontmatter["new: list"],
            json!(["true", "00127", "emoji 🚀", 7, false])
        );
        assert_eq!(
            edit(header, json!({"tracking":[1, 2]})).unwrap(),
            header.replace(
                "tracking:  # owner\n  reviewer: owner\r\n  ticket: QA\n",
                "tracking: [1, 2]  # owner\n"
            )
        );
    }

    #[test]
    fn adds_a_header_without_reconstructing_body() {
        let body = "\r\n\nA note\r\nsecond line";
        assert_eq!(edit(body, json!({"missing": null})).unwrap(), body);
        let after = edit(body, json!({"status": "ready"})).unwrap();
        assert_eq!(after, format!("---\r\nstatus: ready\r\n---\r\n{body}"));
        assert_eq!(
            edit("---\nstatus: null\n---", json!({"status":null})).unwrap(),
            "---\n---"
        );
    }

    #[test]
    fn preserves_flow_neighbors_and_rejects_invalid_edits() {
        let body = "---\n{tracking: {id: '00127'}, status: draft, old: null}\n---\nBody.";
        let after = edit(body, json!({"status": "true", "old": null, "new": false})).unwrap();
        assert_eq!(
            after,
            "---\n{tracking: {id: '00127'}, status: \"true\",  new: false}\n---\nBody."
        );
        assert!(edit(
            "---\nstatus: [broken\n---\nBody.",
            json!({"status":"ready"})
        )
        .is_err());
        assert!(edit(
            "---\nanchor: &reuse draft\nreference: *reuse\n---\nBody.",
            json!({"anchor":null})
        )
        .is_err());
        let anchored = "---\nanchor: &reuse draft\nreference: *reuse\n---";
        assert_eq!(
            edit(anchored, json!({"anchor": "ready"})).unwrap(),
            anchored.replace("draft", "ready")
        );
    }
}
