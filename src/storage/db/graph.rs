//! Backlinks, outgoing link classification, BFS knowledge graph, and orphan queries.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{anyhow, Result};
use rusqlite::params;

use super::{notes::load_tags_map, search::{extract_context_snippet, first_line_preview}, Storage};
use crate::{
    config::VaultProfile,
    models::{
        BacklinkEntry, GraphData, GraphEdge, GraphNode, LinkStatus, NoteSummary, OutgoingLinkEntry,
    },
    vault::fs::{
        folder_from_rel_path, is_local_attachment_dest, normalize_rel_path,
        resolve_attachment_staged, title_from_rel_path, VaultLinkIndex,
    },
};

impl Storage {
    pub fn get_backlinks(
        &self,
        vault_id: &str,
        target_rel_path: &str,
    ) -> Result<Vec<BacklinkEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let target_title = title_from_rel_path(target_rel_path);
        let mut stmt = conn.prepare(
            "SELECT l.source_path, n.title, l.target_raw, l.heading, l.is_embed
             FROM links l
             JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.source_path
             WHERE l.vault_id = ?1 AND l.target_resolved_path = ?2 AND l.source_path != ?2
             ORDER BY LOWER(n.title) ASC, l.id ASC",
        )?;
        // Keep the metadata cursor open while fetching bodies so both reads share its snapshot.
        let mut body_stmt =
            conn.prepare("SELECT body_plain FROM notes WHERE vault_id = ?1 AND rel_path = ?2")?;
        let rows = stmt.query_map(params![vault_id, target_rel_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)? != 0,
            ))
        })?;

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            let (source_path, source_title, target_raw, heading, is_embed) = r?;
            if !seen.insert(source_path.clone()) {
                continue;
            }
            let body_plain: String =
                body_stmt.query_row(params![vault_id, source_path], |r| r.get(0))?;
            let snippet = extract_context_snippet(&body_plain, &target_raw)
                .or_else(|| extract_context_snippet(&body_plain, &target_title))
                .unwrap_or_else(|| first_line_preview(&body_plain, 96));
            out.push(BacklinkEntry {
                source_path,
                source_title,
                context_snippet: snippet,
                heading,
                is_embed,
            });
        }
        Ok(out)
    }

    /// Returns all outgoing wikilinks from `source_rel_path` (including attachments, ambiguous, and missing links).
    pub fn get_outgoing_links(
        &self,
        vault: &VaultProfile,
        source_rel_path: &str,
    ) -> Result<Vec<OutgoingLinkEntry>> {
        self.get_outgoing_links_inner(vault, source_rel_path, None)
    }

    pub(super) fn get_outgoing_links_inner(
        &self,
        vault: &VaultProfile,
        source_rel_path: &str,
        precomputed_index: Option<&VaultLinkIndex>,
    ) -> Result<Vec<OutgoingLinkEntry>> {
        struct OutgoingLinkRow {
            target_raw: String,
            resolved_path: Option<String>,
            resolved_title: Option<String>,
            alias: Option<String>,
            heading: Option<String>,
            is_embed: bool,
            link_kind: String,
        }

        let rows: Vec<OutgoingLinkRow> = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))?;
            let mut stmt = conn.prepare_cached(
                "SELECT l.target_raw, l.target_resolved_path, n.title, l.alias, l.heading, l.is_embed, l.link_kind
                 FROM links l
                 LEFT JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.target_resolved_path
                 WHERE l.vault_id = ?1 AND l.source_path = ?2
                 ORDER BY l.id ASC",
            )?;
            let mapped = stmt.query_map(params![vault.id, source_rel_path], |r| {
                Ok(OutgoingLinkRow {
                    target_raw: r.get(0)?,
                    resolved_path: r.get(1)?,
                    resolved_title: r.get(2)?,
                    alias: r.get(3)?,
                    heading: r.get(4)?,
                    is_embed: r.get::<_, i64>(5)? != 0,
                    link_kind: r.get(6)?,
                })
            })?;
            let mut collected = Vec::new();
            for r in mapped {
                collected.push(r?);
            }
            collected
        };

        let has_unresolved_wikilink = rows
            .iter()
            .any(|row| row.resolved_path.is_none() && row.link_kind == "wikilink");
        let owned_index = if has_unresolved_wikilink && precomputed_index.is_none() {
            let (all_paths, aliases_map) = self.all_paths_and_aliases(&vault.id)?;
            Some(VaultLinkIndex::build(&all_paths, &aliases_map))
        } else {
            None
        };
        let link_index = precomputed_index.or(owned_index.as_ref());

        let mut out = Vec::with_capacity(rows.len());
        for OutgoingLinkRow {
            target_raw,
            resolved_path,
            resolved_title,
            alias,
            heading,
            is_embed,
            link_kind,
        } in rows
        {
            let target_title = resolved_title.unwrap_or_else(|| target_raw.clone());
            if let Some(rp) = resolved_path {
                out.push(OutgoingLinkEntry {
                    target_raw,
                    target_resolved_path: Some(rp),
                    target_title,
                    alias,
                    heading,
                    is_embed,
                    resolved: true,
                    status: LinkStatus::Resolved,
                    candidates: Vec::new(),
                });
                continue;
            }

            if link_kind == "wikilink" && is_local_attachment_dest(&target_raw) {
                if let Some((abs, _)) =
                    resolve_attachment_staged(&vault.path, source_rel_path, &target_raw)
                {
                    if let Ok(rel) = abs.strip_prefix(&vault.path) {
                        out.push(OutgoingLinkEntry {
                            target_raw,
                            target_resolved_path: Some(normalize_rel_path(rel)),
                            target_title,
                            alias,
                            heading,
                            is_embed,
                            resolved: true,
                            status: LinkStatus::Attachment,
                            candidates: Vec::new(),
                        });
                        continue;
                    }
                }
            }

            if link_kind == "wikilink" {
                if let Some(idx) = link_index {
                    let mut cands = idx.wikilink_candidates(&target_raw, source_rel_path);
                    if cands.len() >= 2 {
                        cands.truncate(10);
                        out.push(OutgoingLinkEntry {
                            target_raw,
                            target_resolved_path: None,
                            target_title,
                            alias,
                            heading,
                            is_embed,
                            resolved: false,
                            status: LinkStatus::Ambiguous,
                            candidates: cands,
                        });
                        continue;
                    }
                }
            }

            out.push(OutgoingLinkEntry {
                target_raw,
                target_resolved_path: None,
                target_title,
                alias,
                heading,
                is_embed,
                resolved: false,
                status: LinkStatus::Missing,
                candidates: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Builds a Local (BFS around `center_rel_path` up to `depth`) or Global (`center_rel_path == None`) Knowledge Graph.
    pub fn get_graph(
        &self,
        vault: &VaultProfile,
        center_rel_path: Option<&str>,
        depth: usize,
    ) -> Result<GraphData> {
        let depth = depth.clamp(1, 5);
        let vault_id = vault.id.as_str();

        // Load notes metadata, aliases, tags, and links in a single SQLite lock acquisition,
        // then release the lock before any filesystem attachment checks or BFS computation.
        let mut all_paths: Vec<String> = Vec::new();
        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut note_meta: HashMap<String, (String, String)> = HashMap::new();
        let tags_map: HashMap<String, Vec<String>>;
        let raw_links: Vec<(String, String, Option<String>, bool, String)>;
        {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))?;
            tags_map = load_tags_map(&conn, vault_id)?;
            {
                let mut stmt = conn.prepare_cached(
                    "SELECT rel_path, title, folder, aliases_json FROM notes WHERE vault_id = ?1",
                )?;
                let rows = stmt.query_map(params![vault_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                for r in rows {
                    let (rp, title, folder, aj) = r?;
                    if aj != "[]" {
                        let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
                        if !aliases.is_empty() {
                            aliases_map.insert(rp.clone(), aliases);
                        }
                    }
                    all_paths.push(rp.clone());
                    note_meta.insert(rp, (title, folder));
                }
            }
            {
                let mut stmt = conn.prepare_cached(
                    "SELECT source_path, target_raw, target_resolved_path, is_embed, link_kind
                     FROM links WHERE vault_id = ?1
                     ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![vault_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)? != 0,
                        r.get::<_, String>(4)?,
                    ))
                })?;
                let mut collected = Vec::new();
                for r in rows {
                    collected.push(r?);
                }
                raw_links = collected;
            }
        }

        let mut link_index: Option<VaultLinkIndex> = None;
        let mut attachment_cache: HashMap<(String, String), bool> = HashMap::new();

        // Filter out image and existing local attachment embeds so graph shows note-to-note connections
        let mut all_edges: Vec<(String, String, bool, bool)> = Vec::with_capacity(raw_links.len());
        let mut unresolved_status: HashMap<String, LinkStatus> = HashMap::new();
        for (src, raw, resolved_opt, is_embed, link_kind) in raw_links {
            if crate::vault::parser::is_image_target(&raw) {
                continue;
            }
            let (target_id, is_resolved) = match resolved_opt {
                Some(rp) => (rp, true),
                None => {
                    if is_local_attachment_dest(&raw) {
                        let cache_key = (folder_from_rel_path(&src), raw.clone());
                        let exists = *attachment_cache.entry(cache_key).or_insert_with(|| {
                            resolve_attachment_staged(&vault.path, &src, &raw).is_some()
                        });
                        if exists {
                            continue;
                        }
                    }
                    let target_id = format!("unresolved:{raw}");
                    let is_ambiguous = if link_kind == "wikilink" {
                        let idx = link_index
                            .get_or_insert_with(|| VaultLinkIndex::build(&all_paths, &aliases_map));
                        idx.wikilink_candidates(&raw, &src).len() >= 2
                    } else {
                        false
                    };
                    let entry = unresolved_status
                        .entry(target_id.clone())
                        .or_insert(LinkStatus::Missing);
                    if is_ambiguous {
                        *entry = LinkStatus::Ambiguous;
                    }
                    (target_id, false)
                }
            };
            if src == target_id {
                continue;
            }
            all_edges.push((src, target_id, is_embed, is_resolved));
        }

        let mut in_deg: HashMap<String, usize> = HashMap::new();
        let mut out_deg: HashMap<String, usize> = HashMap::new();
        let mut counted: HashSet<(&str, &str)> = HashSet::new();
        for (src, dst, _, _) in &all_edges {
            if counted.insert((src.as_str(), dst.as_str())) {
                *out_deg.entry(src.clone()).or_default() += 1;
                *in_deg.entry(dst.clone()).or_default() += 1;
            }
        }

        // Build undirected adjacency for BFS neighborhood discovery
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        for (src, dst, _, _) in &all_edges {
            adj.entry(src.clone()).or_default().push(dst.clone());
            adj.entry(dst.clone()).or_default().push(src.clone());
        }

        let mut node_depths: HashMap<String, usize> = HashMap::new();
        if let Some(center) = center_rel_path {
            let max_depth = depth;
            let mut q = VecDeque::new();
            node_depths.insert(center.to_string(), 0);
            q.push_back((center.to_string(), 0usize));
            while let Some((cur, d)) = q.pop_front() {
                if d >= max_depth {
                    continue;
                }
                if let Some(neighbors) = adj.get(&cur) {
                    for nxt in neighbors {
                        if !node_depths.contains_key(nxt) {
                            node_depths.insert(nxt.clone(), d + 1);
                            q.push_back((nxt.clone(), d + 1));
                        }
                    }
                }
            }
        } else {
            // Global graph: all notes + referenced unresolved nodes
            for rp in note_meta.keys() {
                node_depths.insert(rp.clone(), 0);
            }
            for (_, dst, _, is_res) in &all_edges {
                if !is_res {
                    node_depths.entry(dst.clone()).or_insert(1);
                }
            }
        }

        let mut nodes: Vec<GraphNode> = Vec::new();
        for (id, d) in &node_depths {
            if let Some((title, folder)) = note_meta.get(id) {
                nodes.push(GraphNode {
                    id: id.clone(),
                    title: title.clone(),
                    folder: folder.clone(),
                    resolved: true,
                    status: LinkStatus::Resolved,
                    depth: *d,
                    in_degree: *in_deg.get(id).unwrap_or(&0),
                    out_degree: *out_deg.get(id).unwrap_or(&0),
                    tags: tags_map.get(id).cloned().unwrap_or_default(),
                });
            } else {
                let raw_name = id.strip_prefix("unresolved:").unwrap_or(id).to_string();
                let status = unresolved_status
                    .get(id)
                    .copied()
                    .unwrap_or(LinkStatus::Missing);
                nodes.push(GraphNode {
                    id: id.clone(),
                    title: raw_name,
                    folder: String::new(),
                    resolved: false,
                    status,
                    depth: *d,
                    in_degree: *in_deg.get(id).unwrap_or(&0),
                    out_degree: 0,
                    tags: Vec::new(),
                });
            }
        }

        nodes.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| (b.in_degree + b.out_degree).cmp(&(a.in_degree + a.out_degree)))
                .then_with(|| {
                    a.title
                        .to_ascii_lowercase()
                        .cmp(&b.title.to_ascii_lowercase())
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut edge_set = HashSet::new();
        let mut edges = Vec::new();
        for (src, dst, is_embed, _) in all_edges {
            if node_depths.contains_key(&src)
                && node_depths.contains_key(&dst)
                && edge_set.insert((src.clone(), dst.clone()))
            {
                edges.push(GraphEdge {
                    source: src,
                    target: dst,
                    is_embed,
                });
            }
        }

        Ok(GraphData {
            center: center_rel_path.map(String::from),
            depth,
            nodes,
            edges,
        })
    }

    /// Returns orphan notes (0 incoming resolved links and 0 outgoing links).
    pub fn find_orphans(&self, vault_id: &str) -> Result<Vec<NoteSummary>> {
        let all = self.list_notes(vault_id, None)?;
        Ok(all
            .into_iter()
            .filter(|n| n.backlink_count == 0 && n.outgoing_count == 0)
            .collect())
    }
}
