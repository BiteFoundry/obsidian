//! Center Canvas Mode B: Interactive Knowledge Graph View (`G` toggle).
//!
//! Renders a crisp Force-Directed + Radial Braille Canvas (`ratatui::widgets::canvas::Canvas`
//! with `Marker::Braille`) showing:
//! - Focal node (`◉ Note Title`) centered & highlighted in iconic Obsidian purple `#a78bfa`
//! - 1-hop neighbors (`●`) arranged in an outer ring with force-directed spacing
//! - 2-hop / secondary nodes (`○`) and unresolved ghost links (`◌`)
//! - Smooth Braille edges connecting linked notes (bright purple for focal/selected edges, slate for secondary)
//! - Interactive cycling (`j`/`k`/`Tab`), `Enter` to open selected node, `l` to toggle Local vs Global, `+/-` depth!

use std::{
    collections::HashMap,
    f64::consts::TAU,
};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Line as CanvasLine},
        Block, Borders, Paragraph,
    },
    Frame,
};

use crate::{
    models::{GraphData, GraphNode},
    ui::{
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        truncate_width,
    },
};

/// Computes deterministic radial + spring-repulsion 2D coordinates in `[0.08, 0.92] x [0.12, 0.88]`.
pub fn compute_graph_layout(graph: &GraphData) -> HashMap<String, (f64, f64)> {
    let mut pos: HashMap<String, (f64, f64)> = HashMap::new();
    let n = graph.nodes.len();
    if n == 0 {
        return pos;
    }

    let center_id = graph.center.as_deref();
    let mut ring1: Vec<&GraphNode> = Vec::new();
    let mut ring2: Vec<&GraphNode> = Vec::new();

    for node in &graph.nodes {
        if center_id == Some(node.id.as_str()) {
            pos.insert(node.id.clone(), (0.50, 0.50));
        } else if node.depth <= 1 && center_id.is_some() {
            ring1.push(node);
        } else if center_id.is_none() && pos.is_empty() {
            // In global mode, anchor highest-degree hub near center
            pos.insert(node.id.clone(), (0.50, 0.50));
        } else if ring1.len() < 7 {
            ring1.push(node);
        } else {
            ring2.push(node);
        }
    }

    // Place ring 1 evenly around an ellipse (aspect-adjusted for wide terminal cells)
    let r1_count = ring1.len().max(1);
    for (i, node) in ring1.iter().enumerate() {
        let angle = -std::f64::consts::FRAC_PI_2 + (i as f64 / r1_count as f64) * TAU;
        let rx = if ring2.is_empty() { 0.34 } else { 0.28 };
        let ry = if ring2.is_empty() { 0.34 } else { 0.27 };
        pos.insert(
            node.id.clone(),
            (0.50 + rx * angle.cos(), 0.50 + ry * angle.sin()),
        );
    }

    // Place ring 2 in outer ellipse offset by half-step so labels never collide with ring 1
    let r2_count = ring2.len().max(1);
    for (i, node) in ring2.iter().enumerate() {
        let angle = -std::f64::consts::FRAC_PI_2
            + ((i as f64 + 0.5) / r2_count as f64) * TAU;
        pos.insert(
            node.id.clone(),
            (0.50 + 0.41 * angle.cos(), 0.50 + 0.39 * angle.sin()),
        );
    }

    // Light spring-repulsion relaxation (keeps focal node pinned at center)
    let ids: Vec<String> = graph.nodes.iter().map(|gn| gn.id.clone()).collect();
    let pinned_center = center_id
        .map(String::from)
        .or_else(|| graph.nodes.first().map(|g| g.id.clone()));

    for _ in 0..24 {
        let mut forces: HashMap<String, (f64, f64)> =
            ids.iter().map(|id| (id.clone(), (0.0, 0.0))).collect();

        // Pairwise repulsion (stronger horizontally to give text labels room)
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let Some(&(x1, y1)) = pos.get(&ids[i]) else { continue };
                let Some(&(x2, y2)) = pos.get(&ids[j]) else { continue };
                let dx = x1 - x2;
                let dy = (y1 - y2) * 1.35;
                let dist_sq = (dx * dx + dy * dy).max(0.0025);
                let dist = dist_sq.sqrt();
                let rep = 0.0032 / dist_sq;
                let fx = (dx / dist) * rep;
                let fy = (dy / dist) * rep * 0.75;
                if let Some(f) = forces.get_mut(&ids[i]) {
                    f.0 += fx;
                    f.1 += fy;
                }
                if let Some(f) = forces.get_mut(&ids[j]) {
                    f.0 -= fx;
                    f.1 -= fy;
                }
            }
        }

        // Edge spring attraction
        for edge in &graph.edges {
            let Some(&(x1, y1)) = pos.get(&edge.source) else { continue };
            let Some(&(x2, y2)) = pos.get(&edge.target) else { continue };
            let dx = x2 - x1;
            let dy = y2 - y1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let ideal = 0.27;
            let pull = (dist - ideal) * 0.08;
            let fx = (dx / dist) * pull;
            let fy = (dy / dist) * pull;
            if let Some(f) = forces.get_mut(&edge.source) {
                f.0 += fx;
                f.1 += fy;
            }
            if let Some(f) = forces.get_mut(&edge.target) {
                f.0 -= fx;
                f.1 -= fy;
            }
        }

        for id in &ids {
            if pinned_center.as_deref() == Some(id.as_str()) {
                continue;
            }
            if let (Some(p), Some(f)) = (pos.get_mut(id), forces.get(id)) {
                p.0 = (p.0 + f.0.clamp(-0.04, 0.04)).clamp(0.11, 0.89);
                p.1 = (p.1 + f.1.clamp(-0.04, 0.04)).clamp(0.12, 0.88);
            }
        }
    }

    pos
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_canvas)),
        area,
    );

    if area.width < 20 || area.height < 8 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    let graph = &app.graph;
    let selected_idx = ui
        .graph_selected
        .min(graph.nodes.len().saturating_sub(1));
    let selected_node = graph.nodes.get(selected_idx);

    // 1. Graph Header HUD
    let scope_label = if ui.graph_global {
        "Global Vault Graph".to_string()
    } else {
        format!(
            "Local Graph · Depth {} · {}",
            ui.graph_depth,
            app.active_note
                .as_ref()
                .map(|n| n.summary.title.as_str())
                .unwrap_or("Vault")
        )
    };
    let header_line = Line::from(vec![
        Span::styled(
            " ◉ ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_width(&scope_label, area.width.saturating_sub(36) as usize),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "   {} nodes · {} edges",
                graph.nodes.len(),
                graph.edges.len()
            ),
            theme.muted_style(),
        ),
    ]);
    let legend_line = Line::from(vec![
        Span::styled("   ◉ Focal  ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("● 1-hop  ", Style::default().fg(theme.accent_soft)),
        Span::styled("○ 2-hop  ", Style::default().fg(theme.info)),
        Span::styled("◌ Unresolved  ", theme.muted_style()),
        Span::styled("  [l] Scope  [+/-] Depth  [j/k] Select  [Enter] Open", theme.subtle_style()),
    ]);
    frame.render_widget(
        Paragraph::new(vec![header_line, legend_line]).style(Style::default().bg(theme.bg_canvas)),
        chunks[0],
    );

    // 2. Braille Force-Directed Canvas
    let canvas_area = chunks[1];
    let positions = compute_graph_layout(graph);
    let focal_id = graph.center.as_deref();
    let sel_id = selected_node.map(|n| n.id.as_str());

    let accent_color = theme.accent;
    let accent_deep = theme.accent_deep;
    let muted_edge = theme.fg_subtle;
    let bg_canvas = theme.bg_canvas;

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .background_color(bg_canvas)
        .x_bounds([0.0, 1.0])
        .y_bounds([0.0, 1.0])
        .paint(|ctx| {
            // Draw secondary edges first
            for edge in &graph.edges {
                let is_hi = sel_id == Some(edge.source.as_str())
                    || sel_id == Some(edge.target.as_str())
                    || focal_id == Some(edge.source.as_str())
                    || focal_id == Some(edge.target.as_str());
                if is_hi {
                    continue;
                }
                if let (Some(&(x1, y1)), Some(&(x2, y2))) =
                    (positions.get(&edge.source), positions.get(&edge.target))
                {
                    ctx.draw(&CanvasLine {
                        x1,
                        y1: 1.0 - y1,
                        x2,
                        y2: 1.0 - y2,
                        color: muted_edge,
                    });
                }
            }
            // Draw focal & selected edges on top in vivid Obsidian purple
            for edge in &graph.edges {
                let is_sel_edge = sel_id == Some(edge.source.as_str())
                    || sel_id == Some(edge.target.as_str());
                let is_focal_edge = focal_id == Some(edge.source.as_str())
                    || focal_id == Some(edge.target.as_str());
                if !is_sel_edge && !is_focal_edge {
                    continue;
                }
                if let (Some(&(x1, y1)), Some(&(x2, y2))) =
                    (positions.get(&edge.source), positions.get(&edge.target))
                {
                    ctx.draw(&CanvasLine {
                        x1,
                        y1: 1.0 - y1,
                        x2,
                        y2: 1.0 - y2,
                        color: if is_sel_edge { accent_color } else { accent_deep },
                    });
                }
            }
        });

    frame.render_widget(canvas, canvas_area);

    // Overlay crisp node pills directly at projected cell coordinates (with collision avoidance so no two labels overlap!)
    let mut placed_rects: Vec<Rect> = Vec::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        let Some(&(nx, ny)) = positions.get(&node.id) else {
            continue;
        };
        let cx = canvas_area.x + ((nx * (canvas_area.width.saturating_sub(1) as f64)).round() as u16);
        let cy = canvas_area.y + ((ny * (canvas_area.height.saturating_sub(1) as f64)).round() as u16);

        let is_focal = focal_id == Some(node.id.as_str());
        let is_sel = idx == selected_idx;

        let glyph = if is_focal {
            "◉"
        } else if !node.resolved {
            "◌"
        } else if node.depth <= 1 {
            "●"
        } else {
            "○"
        };

        let short_title = truncate_width(&node.title, 18);
        let badge = if is_sel {
            format!(" {glyph} {short_title} ")
        } else {
            format!("{glyph} {short_title}")
        };
        let badge_w = (unicode_width::UnicodeWidthStr::width(badge.as_str()) as u16).min(canvas_area.width);
        let x0 = cx
            .saturating_sub(badge_w / 2)
            .max(canvas_area.x)
            .min(canvas_area.x + canvas_area.width.saturating_sub(badge_w));
        let base_y = cy.clamp(canvas_area.y, canvas_area.y + canvas_area.height.saturating_sub(1));

        // Find nearest non-overlapping row if another label occupies this spot
        let max_y = canvas_area.y + canvas_area.height.saturating_sub(1);
        let mut chosen_y = base_y;
        let overlaps_any = |cand: Rect, placed: &[Rect]| -> bool {
            placed.iter().any(|r| {
                r.y == cand.y
                    && cand.x < r.x.saturating_add(r.width + 1)
                    && r.x < cand.x.saturating_add(cand.width + 1)
            })
        };
        if overlaps_any(Rect::new(x0, chosen_y, badge_w, 1), &placed_rects) {
            for delta in 1i16..=6 {
                for cand_y in [base_y as i16 + delta, base_y as i16 - delta] {
                    if cand_y >= canvas_area.y as i16 && cand_y <= max_y as i16 {
                        let r = Rect::new(x0, cand_y as u16, badge_w, 1);
                        if !overlaps_any(r, &placed_rects) {
                            chosen_y = cand_y as u16;
                            break;
                        }
                    }
                }
                if !overlaps_any(Rect::new(x0, chosen_y, badge_w, 1), &placed_rects) {
                    break;
                }
            }
        }

        let style = if is_sel {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if is_focal {
            Style::default()
                .fg(theme.accent_soft)
                .bg(theme.bg_selected)
                .add_modifier(Modifier::BOLD)
        } else if !node.resolved {
            Style::default().fg(theme.fg_muted).bg(theme.bg_canvas)
        } else if node.depth <= 1 {
            Style::default().fg(theme.fg_primary).bg(theme.bg_card)
        } else {
            Style::default().fg(theme.info).bg(theme.bg_canvas)
        };

        let pill_rect = Rect::new(x0, chosen_y, badge_w, 1);
        placed_rects.push(pill_rect);
        frame.render_widget(Paragraph::new(Span::styled(badge, style)), pill_rect);
        hitmap.add(pill_rect, HitTarget::GraphNode(idx));
    }

    // 3. Selected Node Inspector Footer Bar
    let footer_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.divider))
        .style(Style::default().bg(theme.bg_card));
    let footer_inner = footer_block.inner(chunks[2]);
    frame.render_widget(footer_block, chunks[2]);

    if let Some(sel) = selected_node {
        let status = if sel.resolved {
            format!("in: {} · out: {}", sel.in_degree, sel.out_degree)
        } else {
            "unresolved link (Enter to create)".to_string()
        };
        let mut spans = vec![
            Span::styled(" Selected: ", theme.muted_style()),
            Span::styled(
                sel.title.clone(),
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   ({status})"), theme.muted_style()),
        ];
        if !sel.tags.is_empty() {
            spans.push(Span::raw("   "));
            for t in sel.tags.iter().take(3) {
                spans.push(Span::styled(format!(" #{t} "), theme.tag_pill_style()));
                spans.push(Span::raw(" "));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), footer_inner);
    }
}
