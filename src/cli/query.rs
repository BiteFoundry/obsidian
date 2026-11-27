//! Search, links, graph, orphans, tags, and task CLI handlers.

use anyhow::Result;
use serde_json::json;

use crate::{
    config::AppConfig,
    models::{LinkStatus, TaskFilter},
    safe_text::sanitize_terminal_inline,
    storage::Storage,
    vault::{
        fs::{safe_vault_path, toggle_task_at_line_verified},
        resolve_active_vault,
    },
};

pub(super) fn handle_search(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    query: String,
    limit: usize,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    let page = storage.search_notes_page(&vault.id, &query, limit)?;
    if as_json {
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
        println!("No notes matching '{}'.", sanitize_terminal_inline(&query));
    } else {
        for h in &page.hits {
            println!(
                "{} — {}",
                sanitize_terminal_inline(&h.rel_path),
                sanitize_terminal_inline(&h.snippet)
            );
        }
        if page.has_more {
            eprintln!("More results available. Raise --limit (max 200) or narrow the query.");
        }
    }
    Ok(())
}

pub(super) fn handle_backlinks(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    note: String,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
    let bl = storage.get_backlinks(&vault.id, &rel_path)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&bl)?);
    } else if bl.is_empty() {
        println!("No backlinks to {}.", sanitize_terminal_inline(&rel_path));
    } else {
        for b in bl {
            println!(
                "← {}  —  {}",
                sanitize_terminal_inline(&b.source_path),
                sanitize_terminal_inline(&b.context_snippet)
            );
        }
    }
    Ok(())
}

pub(super) fn handle_links(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    note: String,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    let rel_path = storage.resolve_note_path(&vault.id, &note)?;
    let ol = storage.get_outgoing_links(&vault, &rel_path)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&ol)?);
    } else if ol.is_empty() {
        println!(
            "No outgoing links from {}.",
            sanitize_terminal_inline(&rel_path)
        );
    } else {
        for l in ol {
            match l.status {
                LinkStatus::Resolved | LinkStatus::Attachment => {
                    let resolved = l.target_resolved_path.unwrap_or_default();
                    println!(
                        "→ {} ({})",
                        sanitize_terminal_inline(&l.target_raw),
                        sanitize_terminal_inline(&resolved)
                    );
                }
                LinkStatus::Ambiguous => {
                    let joined = l.candidates.join(", ");
                    println!(
                        "◌ (unresolved) {} (ambiguous: {})",
                        sanitize_terminal_inline(&l.target_raw),
                        sanitize_terminal_inline(&joined)
                    );
                }
                LinkStatus::Missing => {
                    println!(
                        "◌ (unresolved) {} (missing)",
                        sanitize_terminal_inline(&l.target_raw)
                    );
                }
            }
        }
    }
    Ok(())
}

pub(super) fn handle_graph(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    note: Option<String>,
    depth: usize,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    let center = match note.as_deref() {
        Some(n) => Some(storage.resolve_note_path(&vault.id, n)?),
        None => None,
    };
    let graph = storage.get_graph(&vault, center.as_deref(), depth)?;
    if as_json {
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
                n.depth,
                sanitize_terminal_inline(&n.title),
                n.in_degree,
                n.out_degree
            );
        }
    }
    Ok(())
}

pub(super) fn handle_orphans(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    let orphans = storage.find_orphans(&vault.id)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&orphans)?);
    } else if orphans.is_empty() {
        println!("No orphan notes found.");
    } else {
        for o in orphans {
            println!("{}", sanitize_terminal_inline(&o.rel_path));
        }
    }
    Ok(())
}

pub(super) fn handle_tags(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    tag: Option<String>,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
    let storage = Storage::open_default()?;
    storage.sync_vault(&vault)?;
    if let Some(t) = tag {
        let notes = storage.find_by_tag(&vault.id, &t)?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&notes)?);
        } else {
            for n in notes {
                println!("{}", sanitize_terminal_inline(&n.rel_path));
            }
        }
    } else {
        let tags = storage.list_tags(&vault.id)?;
        if as_json {
            println!("{}", serde_json::to_string_pretty(&tags)?);
        } else {
            for tc in tags {
                println!("#{:<28} ({})", sanitize_terminal_inline(&tc.tag), tc.count);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_tasks(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    open: bool,
    done: bool,
    note: Option<String>,
    limit: Option<u64>,
    cursor: Option<String>,
    all: bool,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
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
    } else if as_json {
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
    if as_json {
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
            println!(
                "{box_c} {}:{}  {}",
                sanitize_terminal_inline(&t.note_path),
                t.line_no,
                sanitize_terminal_inline(&t.text)
            );
        }
        if let Some(next_cursor) = page.next_cursor.as_deref().filter(|_| page.has_more) {
            eprintln!(
                "More results available. Continue with --cursor {:?}",
                sanitize_terminal_inline(next_cursor)
            );
        }
    }
    Ok(())
}

pub(super) fn handle_toggle_task(
    cfg: &AppConfig,
    vault_spec: Option<&str>,
    as_json: bool,
    note: String,
    line: usize,
    expected_text: Option<String>,
    expected_checked: Option<bool>,
) -> Result<()> {
    let vault = resolve_active_vault(cfg, vault_spec)?;
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
    storage.sync_paths(&vault, std::slice::from_ref(&rel_path), &[])?;
    if as_json {
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
            sanitize_terminal_inline(&rel_path),
            line
        );
    }
    Ok(())
}
