//! Note CRUD and Daily Note CLI handlers.

use anyhow::Result;
use serde_json::json;

use super::NotesCmd;
use crate::{
    config::AppConfig,
    models::{CodedError, NoteContent, NoteOmittedCounts},
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::Storage,
    tui,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, move_note_to_trash, safe_vault_path,
        },
        resolve_active_vault,
    },
};

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

pub(super) fn handle_notes(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    action: NotesCmd,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
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
            } else if as_json {
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
            if as_json {
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
                        sanitize_terminal_inline(&n.rel_path),
                        n.word_count,
                        n.backlink_count,
                        n.outgoing_count
                    );
                }
                if let Some(next_cursor) =
                    page.next_cursor.as_deref().filter(|_| page.has_more)
                {
                    eprintln!(
                        "More results available. Continue with --cursor {:?}",
                        sanitize_terminal_inline(next_cursor)
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
            if !as_json {
                let raw = storage.load_note_content(&vault, &note)?.raw_markdown;
                print!("{}", sanitize_terminal_text(&raw));
            } else if full {
                let detail = storage.load_note_detail(&vault, &note)?;
                println!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                let content = storage.load_note_content(&vault, &note)?;
                let counts = storage.note_omitted_counts(&vault.id, &content.rel_path)?;
                let payload = cli_compact_note_json(&content, counts, vault_spec);
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
            storage.sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "created": true, "rel_path": rel_path })
                    )?
                );
            } else {
                println!("Created {}", sanitize_terminal_inline(&rel_path));
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
            storage.sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "appended": true, "rel_path": rel_path })
                    )?
                );
            } else {
                println!("Appended to {}", sanitize_terminal_inline(&rel_path));
            }
            Ok(())
        }
        NotesCmd::Replace {
            note,
            find,
            replace,
        } => {
            let result = storage.replace_note_text(&vault, &note, &find, &replace)?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.replaced {
                println!(
                    "Replaced one match in {}",
                    sanitize_terminal_inline(&result.rel_path)
                );
            } else {
                println!(
                    "Found {} matches in {}. A replacement requires one match.",
                    result.matches,
                    sanitize_terminal_inline(&result.rel_path)
                );
            }
            Ok(())
        }
        NotesCmd::Edit { note } => {
            let rel_path = storage.resolve_note_path(&vault.id, &note)?;
            let abs = safe_vault_path(&vault.path, &rel_path)?;
            tui::launch_external_editor(&abs)?;
            let stats = storage
                .sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?
                .stats;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "edited": true, "rel_path": rel_path, "stats": stats })
                    )?
                );
            } else {
                println!("Edited and reindexed {}", sanitize_terminal_inline(&rel_path));
            }
            Ok(())
        }
        NotesCmd::Move { src, dst } => {
            let moved = storage.move_note_with_links(&vault, &src, &dst)?;
            if as_json {
                let payload = serde_json::to_value(&moved)?;
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!(
                    "Moved {} -> {} (updated links in {} note(s))",
                    sanitize_terminal_inline(&moved.from),
                    sanitize_terminal_inline(&moved.to),
                    moved.updated_links_in_notes
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
            storage.sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "deleted": true, "rel_path": rel_path, "trash_path": trashed })
                    )?
                );
            } else {
                let trashed_str = trashed.display().to_string();
                println!(
                    "Moved {} -> {}",
                    sanitize_terminal_inline(&rel_path),
                    sanitize_terminal_inline(&trashed_str)
                );
            }
            Ok(())
        }
    }
}

pub(super) fn handle_daily(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    date: Option<String>,
    append: Option<String>,
    full: bool,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    let (rel_path, abs, created) = ensure_daily_note(&vault, date.as_deref())?;
    let mut appended = false;
    if let Some(txt) = append.as_deref().filter(|s| !s.trim().is_empty()) {
        append_to_note(&abs, txt)?;
        appended = true;
    }
    if as_json && appended && !full {
        storage.sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?;
    } else {
        storage.sync_vault(&vault)?;
    }
    if as_json {
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
                cli_compact_note_json(&content, counts, vault_spec)
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
        println!("{}", sanitize_terminal_text(&content.raw_markdown));
    }
    Ok(())
}
