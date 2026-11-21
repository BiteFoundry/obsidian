//! Center Canvas Mode B: Interactive Knowledge Graph View (`G` toggle).
//!
//! Renders a deterministic, rock-solid Force-Directed + Spiral Braille Canvas
//! (`ratatui::widgets::canvas::Canvas` with `Marker::Braille`) showing:
//! - Focal node (`◉ Note Title`) pinned at center & highlighted in iconic Obsidian purple `#a78bfa`
//! - 1-hop neighbors (`●`) arranged evenly on an inner ellipse
//! - Outer/secondary nodes (`○`) and unresolved ghost links (`◌`) distributed on a golden-angle Fermat spiral
//! - Liang-Barsky viewport-clipped Braille edges that stay crisp at every zoom level (`0.25x`..=`6.0x`)
//! - Smooth cursor-centered mouse wheel zoom, drag pan, elastic 1-hop node dragging, and clickable zoom HUD pills!

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    f64::consts::{FRAC_PI_2, PI, TAU},
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
use unicode_width::UnicodeWidthStr;

use crate::{
    models::{GraphData, GraphNode},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        truncate_width,
    },
};

pub const GRAPH_ZOOM_MIN: f64 = 0.25;
pub const GRAPH_ZOOM_MAX: f64 = 6.0;
pub const GRAPH_WHEEL_FACTOR: f64 = 1.15;
pub const GRAPH_LOW_ZOOM_THRESHOLD: f64 = 0.70;
pub const GRAPH_HIGH_ZOOM_THRESHOLD: f64 = 1.50;

const MAX_RING1_NODES: usize = 10;
const SETTLE_FRAME_LIMIT: u16 = 45;

/// Clips a 2D line segment `(x1, y1) -> (x2, y2)` to the unit viewport `[0.0, 1.0] x [0.0, 1.0]`
/// using the Liang-Barsky parametric clipping algorithm.
///
/// Returns `None` if the segment lies entirely outside the unit rectangle or has non-finite coordinates.
pub fn clip_line_unit_rect(x1: f64, y1: f64, x2: f64, y2: f64) -> Option<(f64, f64, f64, f64)> {
    if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
        return None;
    }

    let dx = x2 - x1;
    let dy = y2 - y1;
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;

    let checks = [
        (-dx, x1),      // x >= 0.0
        (dx, 1.0 - x1), // x <= 1.0
        (-dy, y1),      // y >= 0.0
        (dy, 1.0 - y1), // y <= 1.0
    ];

    for (p, q) in checks {
        if p.abs() < 1e-12 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }

    if t0 > t1 {
        return None;
    }

    Some((
        (x1 + t0 * dx).clamp(0.0, 1.0),
        (y1 + t0 * dy).clamp(0.0, 1.0),
        (x1 + t1 * dx).clamp(0.0, 1.0),
        (y1 + t1 * dy).clamp(0.0, 1.0),
    ))
}

/// Deterministic sort order for graph nodes: focal first, then `(depth, Reverse(degree), id)`.
fn sorted_graph_nodes(graph: &GraphData) -> Vec<&GraphNode> {
    let center_id = graph.center.as_deref();
    let mut sorted: Vec<&GraphNode> = graph.nodes.iter().collect();
    sorted.sort_by(|a, b| {
        let is_center_a = center_id == Some(a.id.as_str());
        let is_center_b = center_id == Some(b.id.as_str());
        let deg_a = a.in_degree + a.out_degree;
        let deg_b = b.in_degree + b.out_degree;
        (!is_center_a)
            .cmp(&!is_center_b)
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| deg_b.cmp(&deg_a))
            .then_with(|| a.id.cmp(&b.id))
    });
    sorted
}

/// Returns the deterministic focal/center node ID (`graph.center` in Local mode, or highest-degree hub in Global mode).
fn focal_node_id(graph: &GraphData) -> Option<&str> {
    if let Some(center) = graph.center.as_deref() {
        if graph.nodes.iter().any(|n| n.id == center) {
            return Some(center);
        }
    }
    sorted_graph_nodes(graph).first().map(|n| n.id.as_str())
}

/// Lightweight live spring-physics & camera state for Interactive Knowledge Graph (`G`).
/// Rests at `energy == 0.0` (0% CPU, zero jitter) unless a node is actively dragged.
#[derive(Debug, Clone)]
pub struct GraphSim {
    pub positions: HashMap<String, (f64, f64)>,
    pub rest_positions: HashMap<String, (f64, f64)>,
    pub velocities: HashMap<String, (f64, f64)>,
    pub zoom: f64,
    pub pan_x: f64,
    pub pan_y: f64,
    pub dragging_node: Option<usize>,
    pub drag_started_selected: bool,
    pub drag_moved: bool,
    pub drag_anchor_id: Option<String>,
    pub focal_hub_id: Option<String>,
    pub panning: bool,
    pub last_mouse: Option<(u16, u16)>,
    pub hovered_node: Option<usize>,
    pub energy: f64,
    pub settle_frames: u16,
    pub canvas_area: Cell<Rect>,
    pub pane_area: Cell<Rect>,
    /// Tracks `Some(graph.center)` after the first sync so we distinguish initial sync
    /// (`None`) from Global scope (`Some(None)`) and Local scope (`Some(Some(id))`).
    last_scope_key: Option<Option<String>>,
}

impl Default for GraphSim {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            rest_positions: HashMap::new(),
            velocities: HashMap::new(),
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            dragging_node: None,
            drag_started_selected: false,
            drag_moved: false,
            drag_anchor_id: None,
            focal_hub_id: None,
            panning: false,
            last_mouse: None,
            hovered_node: None,
            energy: 0.0,
            settle_frames: 0,
            canvas_area: Cell::new(Rect::default()),
            pane_area: Cell::new(Rect::default()),
            last_scope_key: None,
        }
    }
}

impl GraphSim {
    /// Only runs physics when a node is actively being dragged (`drag_moved == true`)
    /// or its direct 1-hop neighbors are gently settling immediately after a drag.
    /// Simple click/select never wakes physics.
    pub fn is_active(&self) -> bool {
        (self.dragging_node.is_some() && self.drag_moved)
            || (self.energy > 0.0012 && self.settle_frames < SETTLE_FRAME_LIMIT)
    }

    pub fn max_pan(&self) -> f64 {
        (self.zoom * 1.25).max(1.2)
    }

    /// Synchronizes layout with updated `GraphData`.
    ///
    /// - When `graph.center` changes (switching center note or toggling Local/Global scope),
    ///   recomputes a fresh deterministic layout and resets camera (`zoom = 1.0`, `pan = 0.0`).
    /// - When `graph.center` is unchanged (e.g. live inline editing or minor graph refresh),
    ///   preserves `zoom`, `pan_x`, `pan_y`, and all existing node positions, placing only
    ///   newly added nodes cleanly near their connected neighbor.
    pub fn sync_from_graph(&mut self, graph: &GraphData) {
        let scope_key = Some(graph.center.clone());
        let center_changed = self.last_scope_key != scope_key;

        if graph.nodes.is_empty() {
            self.last_scope_key = scope_key;
            self.positions.clear();
            self.rest_positions.clear();
            self.velocities.clear();
            self.focal_hub_id = None;
            self.dragging_node = None;
            self.drag_anchor_id = None;
            self.drag_moved = false;
            self.panning = false;
            self.hovered_node = None;
            self.energy = 0.0;
            self.settle_frames = 0;
            if center_changed {
                self.zoom = 1.0;
                self.pan_x = 0.0;
                self.pan_y = 0.0;
            }
            return;
        }

        if center_changed || self.positions.is_empty() {
            self.last_scope_key = scope_key;
            let layout = compute_graph_layout(graph);
            self.focal_hub_id = focal_node_id(graph).map(String::from);
            self.positions = layout.clone();
            self.rest_positions = layout;
            self.velocities.clear();
            if center_changed {
                self.zoom = 1.0;
                self.pan_x = 0.0;
                self.pan_y = 0.0;
            }
            self.dragging_node = None;
            self.drag_anchor_id = None;
            self.drag_moved = false;
            self.panning = false;
            self.hovered_node = None;
            self.energy = 0.0;
            self.settle_frames = 0;
            return;
        }

        // Same center & already positioned: preserve camera and existing node positions!
        let current_ids: HashSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
        self.positions
            .retain(|id, _| current_ids.contains(id.as_str()));
        self.rest_positions
            .retain(|id, _| current_ids.contains(id.as_str()));
        self.velocities
            .retain(|id, _| current_ids.contains(id.as_str()));
        if self
            .focal_hub_id
            .as_deref()
            .is_none_or(|id| !current_ids.contains(id))
        {
            self.focal_hub_id = focal_node_id(graph).map(String::from);
        }

        let mut new_nodes: Vec<&GraphNode> = graph
            .nodes
            .iter()
            .filter(|n| !self.positions.contains_key(&n.id))
            .collect();
        if !new_nodes.is_empty() {
            new_nodes.sort_by(|a, b| a.id.cmp(&b.id));
            let golden_angle = PI * (3.0 - 5.0_f64.sqrt());
            let mut fallback_layout: Option<HashMap<String, (f64, f64)>> = None;

            for (slot_idx, node) in new_nodes.iter().enumerate() {
                let mut neighbor_ids: Vec<&str> = graph
                    .edges
                    .iter()
                    .filter_map(|e| {
                        if e.source == node.id {
                            Some(e.target.as_str())
                        } else if e.target == node.id {
                            Some(e.source.as_str())
                        } else {
                            None
                        }
                    })
                    .filter(|nid| self.positions.contains_key(*nid))
                    .collect();
                neighbor_ids.sort_unstable();
                neighbor_ids.dedup();

                let pos = if let Some(&anchor_nid) = neighbor_ids.first() {
                    let (ax, ay) = self.positions[anchor_nid];
                    let base_angle = if (ax - 0.5).abs() < 1e-4 && (ay - 0.5).abs() < 1e-4 {
                        -FRAC_PI_2
                    } else {
                        (ay - 0.5).atan2(ax - 0.5)
                    };
                    let angle = base_angle + (slot_idx as f64 + 0.5) * golden_angle;
                    let dist = 0.16;
                    (
                        (ax + dist * angle.cos()).clamp(0.08, 0.92),
                        (ay + dist * 0.92 * angle.sin()).clamp(0.08, 0.92),
                    )
                } else {
                    let layout = fallback_layout.get_or_insert_with(|| compute_graph_layout(graph));
                    layout.get(&node.id).copied().unwrap_or((0.5, 0.5))
                };
                self.positions.insert(node.id.clone(), pos);
                self.rest_positions.insert(node.id.clone(), pos);
            }
        }

        if self.dragging_node.is_some_and(|i| i >= graph.nodes.len()) {
            self.dragging_node = None;
            self.drag_moved = false;
        }
        if self.hovered_node.is_some_and(|i| i >= graph.nodes.len()) {
            self.hovered_node = None;
        }
        if self
            .drag_anchor_id
            .as_deref()
            .is_some_and(|id| !current_ids.contains(id))
        {
            self.drag_anchor_id = None;
        }
    }

    pub fn reset_view(&mut self, graph: &GraphData) {
        self.last_scope_key = Some(graph.center.clone());
        let layout = compute_graph_layout(graph);
        self.focal_hub_id = focal_node_id(graph).map(String::from);
        self.positions = layout.clone();
        self.rest_positions = layout;
        self.velocities.clear();
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.dragging_node = None;
        self.drag_anchor_id = None;
        self.drag_moved = false;
        self.panning = false;
        self.energy = 0.0;
        self.settle_frames = 0;
    }

    pub fn world_to_view(&self, wx: f64, wy: f64) -> (f64, f64) {
        (
            0.5 + (wx - 0.5) * self.zoom + self.pan_x,
            0.5 + (wy - 0.5) * self.zoom + self.pan_y,
        )
    }

    /// Exact unclamped inverse of `world_to_view` for a screen cell `(col, row)`.
    pub fn screen_to_world_raw(&self, col: u16, row: u16) -> Option<(f64, f64)> {
        let area = self.canvas_area.get();
        if area.width < 2 || area.height < 2 {
            return None;
        }
        let vx = (col as f64 - area.x as f64) / (area.width.saturating_sub(1) as f64);
        let vy = (row as f64 - area.y as f64) / (area.height.saturating_sub(1) as f64);
        let wx = 0.5 + (vx - 0.5 - self.pan_x) / self.zoom;
        let wy = 0.5 + (vy - 0.5 - self.pan_y) / self.zoom;
        Some((wx, wy))
    }

    /// Screen cell `(col, row)` mapped into clamped world coordinates for dragging a node.
    pub fn screen_to_world(&self, col: u16, row: u16) -> Option<(f64, f64)> {
        let (wx, wy) = self.screen_to_world_raw(col, row)?;
        Some((wx.clamp(0.04, 0.96), wy.clamp(0.06, 0.94)))
    }

    pub fn move_dragged_node(&mut self, graph: &GraphData, col: u16, row: u16) {
        let Some(idx) = self.dragging_node else {
            return;
        };
        let Some(node) = graph.nodes.get(idx) else {
            return;
        };
        if let Some((wx, wy)) = self.screen_to_world(col, row) {
            self.positions.insert(node.id.clone(), (wx, wy));
            self.rest_positions.insert(node.id.clone(), (wx, wy));
            self.velocities.insert(node.id.clone(), (0.0, 0.0));
            self.drag_anchor_id = Some(node.id.clone());
            self.drag_moved = true;
            self.energy = 0.02;
            self.settle_frames = 0;
        }
    }

    pub fn pan_by_screen_delta(&mut self, dcol: i32, drow: i32) {
        let area = self.canvas_area.get();
        if area.width < 2 || area.height < 2 {
            return;
        }
        let max_pan = self.max_pan();
        self.pan_x = (self.pan_x + (dcol as f64) / (area.width as f64)).clamp(-max_pan, max_pan);
        self.pan_y = (self.pan_y + (drow as f64) / (area.height as f64)).clamp(-max_pan, max_pan);
    }

    /// Zooms smoothly around the center of the current viewport, preserving whatever world point
    /// is centered on screen and clamping zoom to `[0.25, 6.0]`.
    pub fn zoom_by(&mut self, factor: f64) {
        let old_zoom = self.zoom;
        let new_zoom = (old_zoom * factor).clamp(GRAPH_ZOOM_MIN, GRAPH_ZOOM_MAX);
        if (new_zoom - old_zoom).abs() < 1e-12 {
            return;
        }
        let ratio = new_zoom / old_zoom;
        self.zoom = new_zoom;
        let max_pan = self.max_pan();
        self.pan_x = (self.pan_x * ratio).clamp(-max_pan, max_pan);
        self.pan_y = (self.pan_y * ratio).clamp(-max_pan, max_pan);
    }

    /// Cursor-centered zoom: keeps the world coordinate directly under screen cell `(col, row)`
    /// stationary on screen across zoom-in and zoom-out, clamping zoom to `[0.25, 6.0]`
    /// and pan dynamically to `[-max_pan, max_pan]` where `max_pan = (zoom * 1.25).max(1.2)`.
    pub fn zoom_at(&mut self, col: u16, row: u16, factor: f64) {
        let area = self.canvas_area.get();
        if area.width < 2 || area.height < 2 {
            self.zoom_by(factor);
            return;
        }

        let clamped_col = col.clamp(area.x, area.x + area.width.saturating_sub(1));
        let clamped_row = row.clamp(area.y, area.y + area.height.saturating_sub(1));
        let vx = (clamped_col - area.x) as f64 / (area.width.saturating_sub(1) as f64);
        let vy = (clamped_row - area.y) as f64 / (area.height.saturating_sub(1) as f64);

        let old_zoom = self.zoom;
        let new_zoom = (old_zoom * factor).clamp(GRAPH_ZOOM_MIN, GRAPH_ZOOM_MAX);
        if (new_zoom - old_zoom).abs() < 1e-12 {
            return;
        }

        // World point currently beneath (vx, vy)
        let wx = 0.5 + (vx - 0.5 - self.pan_x) / old_zoom;
        let wy = 0.5 + (vy - 0.5 - self.pan_y) / old_zoom;

        // Solve for new pan so world_to_view(wx, wy) == (vx, vy)
        self.zoom = new_zoom;
        let max_pan = self.max_pan();
        self.pan_x = (vx - 0.5 - (wx - 0.5) * new_zoom).clamp(-max_pan, max_pan);
        self.pan_y = (vy - 0.5 - (wy - 0.5) * new_zoom).clamp(-max_pan, max_pan);
    }

    /// Heavily damped elastic local adjustment triggered ONLY by actual node dragging.
    ///
    /// - Keeps non-neighbor nodes and the focal/center node strictly stationary (zero global collapse/drift).
    /// - Direct 1-hop neighbors of the dragged node gently flex with an elastic tether to their rest positions.
    pub fn step_physics(&mut self, graph: &GraphData) {
        if graph.nodes.is_empty() || !self.is_active() {
            self.energy = 0.0;
            return;
        }
        if self.positions.is_empty() {
            let layout = compute_graph_layout(graph);
            self.focal_hub_id = focal_node_id(graph).map(String::from);
            self.positions = layout.clone();
            self.rest_positions = layout;
        }

        let active_drag_id = if self.drag_moved {
            self.dragging_node
                .and_then(|idx| graph.nodes.get(idx))
                .map(|gn| gn.id.clone())
        } else {
            None
        };
        let Some(anchor_id) = active_drag_id
            .as_deref()
            .or(self.drag_anchor_id.as_deref())
            .map(str::to_string)
        else {
            self.velocities.clear();
            self.energy = 0.0;
            return;
        };

        if active_drag_id.is_none() {
            self.settle_frames = self.settle_frames.saturating_add(1);
            if self.settle_frames >= SETTLE_FRAME_LIMIT {
                self.velocities.clear();
                self.energy = 0.0;
                self.drag_anchor_id = None;
                return;
            }
        } else {
            self.settle_frames = 0;
        }

        let Some(&(ax, ay)) = self.positions.get(&anchor_id) else {
            self.energy = 0.0;
            self.drag_anchor_id = None;
            return;
        };

        let pinned_focal = graph.center.as_deref().or(self.focal_hub_id.as_deref());

        // Direct 1-hop neighbors of the dragged node (excluding the pinned focal node)
        let mut neighbors: Vec<&str> = graph
            .edges
            .iter()
            .filter_map(|e| {
                if e.source == anchor_id {
                    Some(e.target.as_str())
                } else if e.target == anchor_id {
                    Some(e.source.as_str())
                } else {
                    None
                }
            })
            .filter(|&nid| nid != anchor_id && pinned_focal != Some(nid))
            .collect();
        neighbors.sort_unstable();
        neighbors.dedup();

        if neighbors.is_empty() {
            self.velocities.clear();
            self.energy = 0.0;
            if active_drag_id.is_none() {
                self.drag_anchor_id = None;
            }
            return;
        }

        let all_positions: Vec<(&str, (f64, f64))> = {
            let mut v: Vec<(&str, (f64, f64))> = self
                .positions
                .iter()
                .map(|(k, &p)| (k.as_str(), p))
                .collect();
            v.sort_by(|a, b| a.0.cmp(b.0));
            v
        };

        type NodeKinematics = (String, (f64, f64), (f64, f64));
        let mut max_speed = 0.0_f64;
        let mut next_updates: Vec<NodeKinematics> = Vec::with_capacity(neighbors.len());

        for nid in neighbors {
            let Some(&(px, py)) = self.positions.get(nid) else {
                continue;
            };
            let (rx, ry) = self.rest_positions.get(nid).copied().unwrap_or((px, py));

            // 1. Elastic tether toward rest position (prevents collapse or runaway drift)
            let mut fx = (rx - px) * 0.14;
            let mut fy = (ry - py) * 0.14;

            // 2. Gentle spring toward the dragged node along the shared edge
            let dx = ax - px;
            let dy = ay - py;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let ideal = 0.24;
            let pull = ((dist - ideal) * 0.055).clamp(-0.014, 0.014);
            fx += (dx / dist) * pull;
            fy += (dy / dist) * pull;

            // 3. Soft short-range repulsion so neighbors don't land directly on top of another node
            for &(other_id, (ox, oy)) in &all_positions {
                if other_id == nid {
                    continue;
                }
                let rdx = px - ox;
                let rdy = (py - oy) * 1.25;
                let dist_sq = rdx * rdx + rdy * rdy;
                if dist_sq >= 0.0196 {
                    continue;
                }
                let rdist = dist_sq.max(0.0025).sqrt();
                let rep = (0.00025 / dist_sq.max(0.004)).min(0.008);
                fx += (rdx / rdist) * rep;
                fy += (rdy / rdist) * rep * 0.80;
            }

            let (vx0, vy0) = self.velocities.get(nid).copied().unwrap_or((0.0, 0.0));
            let vx = ((vx0 + fx) * 0.58).clamp(-0.014, 0.014);
            let vy = ((vy0 + fy) * 0.58).clamp(-0.014, 0.014);
            let speed = (vx * vx + vy * vy).sqrt();
            if speed > max_speed {
                max_speed = speed;
            }

            let nx = (px + vx).clamp(0.05, 0.95);
            let ny = (py + vy).clamp(0.07, 0.93);
            next_updates.push((nid.to_string(), (nx, ny), (vx, vy)));
        }

        for (id, pos, vel) in next_updates {
            self.positions.insert(id.clone(), pos);
            self.velocities.insert(id, vel);
        }

        if active_drag_id.is_some() {
            self.energy = max_speed.max(0.006);
        } else if max_speed <= 0.0012 {
            self.velocities.clear();
            self.energy = 0.0;
            self.drag_anchor_id = None;
        } else {
            self.energy = max_speed;
        }
    }
}

/// Computes 100% deterministic, well-spaced 2D coordinates in `[0.08, 0.92] x [0.08, 0.92]`.
///
/// - Sorts nodes deterministically by `(is_not_center, depth, Reverse(degree), id)`
/// - Places the focal node (or top hub in Global mode) pinned at `(0.50, 0.50)`
/// - Places 1-hop neighbors evenly on an inner ellipse
/// - Distributes outer nodes across a uniform-area golden-angle Fermat spiral (no single-ring pileups!)
/// - Runs deterministic cooled short-range spring-repulsion relaxation with strict `[0.08, 0.92]` bounds
pub fn compute_graph_layout(graph: &GraphData) -> HashMap<String, (f64, f64)> {
    let mut pos: HashMap<String, (f64, f64)> = HashMap::new();
    let n = graph.nodes.len();
    if n == 0 {
        return pos;
    }

    let sorted = sorted_graph_nodes(graph);
    let focal = sorted[0];
    pos.insert(focal.id.clone(), (0.50, 0.50));
    if n == 1 {
        return pos;
    }

    let center_is_local = graph.center.is_some();
    let mut ring1: Vec<&GraphNode> = Vec::new();
    let mut outer: Vec<&GraphNode> = Vec::new();

    for &node in &sorted[1..] {
        if (center_is_local && node.depth <= 1 && ring1.len() < MAX_RING1_NODES)
            || (!center_is_local && ring1.len() < 8)
        {
            ring1.push(node);
        } else {
            outer.push(node);
        }
    }

    // Inner ellipse for 1-hop / top-degree neighbors
    if !ring1.is_empty() {
        let r1_count = ring1.len() as f64;
        let rx = if outer.is_empty() { 0.32 } else { 0.21 };
        let ry = if outer.is_empty() { 0.30 } else { 0.20 };
        for (i, node) in ring1.iter().enumerate() {
            let angle = -FRAC_PI_2 + (i as f64 / r1_count) * TAU;
            pos.insert(
                node.id.clone(),
                (
                    (0.50 + rx * angle.cos()).clamp(0.08, 0.92),
                    (0.50 + ry * angle.sin()).clamp(0.08, 0.92),
                ),
            );
        }
    }

    // Uniform-area golden-angle Fermat spiral for outer nodes across [r_min, r_max]
    if !outer.is_empty() {
        let golden_angle = PI * (3.0 - 5.0_f64.sqrt());
        let r_min = if ring1.is_empty() { 0.16_f64 } else { 0.27_f64 };
        let r_max = 0.42_f64;
        let r_min_sq = r_min * r_min;
        let r_span_sq = r_max * r_max - r_min_sq;
        let m = outer.len() as f64;
        for (i, node) in outer.iter().enumerate() {
            let frac = (i as f64 + 0.5) / m;
            let r = (r_min_sq + r_span_sq * frac).sqrt();
            let theta = -FRAC_PI_2 + (i as f64 + 1.0) * golden_angle;
            let x = (0.50 + r * theta.cos()).clamp(0.08, 0.92);
            let y = (0.50 + r * 0.96 * theta.sin()).clamp(0.08, 0.92);
            pos.insert(node.id.clone(), (x, y));
        }
    }

    // Deterministic canonical node & edge ordering for cooled short-range relaxation
    let ids: Vec<String> = sorted.iter().map(|gn| gn.id.clone()).collect();
    let pinned_id = ids[0].as_str();
    let mut canonical_edges: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .filter(|e| e.source != e.target)
        .map(|e| {
            if e.source <= e.target {
                (e.source.as_str(), e.target.as_str())
            } else {
                (e.target.as_str(), e.source.as_str())
            }
        })
        .collect();
    canonical_edges.sort_unstable();
    canonical_edges.dedup();

    let ideal = (0.58 / (n as f64).sqrt()).clamp(0.10, 0.26);
    let rep_cutoff = ideal * 1.55;
    let rep_cutoff_sq = rep_cutoff * rep_cutoff;
    let mut vel: HashMap<&str, (f64, f64)> =
        ids.iter().map(|id| (id.as_str(), (0.0, 0.0))).collect();

    const ITERS: usize = 30;
    for iter in 0..ITERS {
        let mut forces: HashMap<&str, (f64, f64)> =
            ids.iter().map(|id| (id.as_str(), (0.0, 0.0))).collect();

        // Short-range pairwise repulsion (keeps labels separated without slamming onto borders)
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let id_i = ids[i].as_str();
                let id_j = ids[j].as_str();
                let Some(&(x1, y1)) = pos.get(id_i) else {
                    continue;
                };
                let Some(&(x2, y2)) = pos.get(id_j) else {
                    continue;
                };
                let dx = x1 - x2;
                let dy = (y1 - y2) * 1.25;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq >= rep_cutoff_sq {
                    continue;
                }
                let dist = dist_sq.max(1e-5).sqrt();
                let overlap = 1.0 - dist / rep_cutoff;
                let rep = (overlap * overlap * ideal * 0.45).min(0.032);
                let (ux, uy) = if dist > 1e-4 {
                    (dx / dist, dy / dist)
                } else {
                    let fallback_ang = ((i * 31 + j * 17) as f64) * 0.618;
                    (fallback_ang.cos(), fallback_ang.sin())
                };
                let fx = ux * rep;
                let fy = uy * rep * 0.80;
                if let Some(f) = forces.get_mut(id_i) {
                    f.0 += fx;
                    f.1 += fy;
                }
                if let Some(f) = forces.get_mut(id_j) {
                    f.0 -= fx;
                    f.1 -= fy;
                }
            }
        }

        // Gentle edge spring attraction toward adaptive ideal distance
        for &(src, dst) in &canonical_edges {
            let Some(&(x1, y1)) = pos.get(src) else {
                continue;
            };
            let Some(&(x2, y2)) = pos.get(dst) else {
                continue;
            };
            let dx = x2 - x1;
            let dy = y2 - y1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let pull = ((dist - ideal) * 0.055).clamp(-0.018, 0.018);
            let fx = (dx / dist) * pull;
            let fy = (dy / dist) * pull;
            if let Some(f) = forces.get_mut(src) {
                f.0 += fx;
                f.1 += fy;
            }
            if let Some(f) = forces.get_mut(dst) {
                f.0 -= fx;
                f.1 -= fy;
            }
        }

        let cool = 1.0 - (iter as f64 / ITERS as f64) * 0.72;
        let max_step = 0.028 * cool;
        for id in &ids {
            let id_s = id.as_str();
            if id_s == pinned_id {
                continue;
            }
            let (fx, fy) = forces.get(id_s).copied().unwrap_or((0.0, 0.0));
            let (vx0, vy0) = vel.get(id_s).copied().unwrap_or((0.0, 0.0));
            let vx = ((vx0 + fx) * 0.65).clamp(-max_step, max_step);
            let vy = ((vy0 + fy) * 0.65).clamp(-max_step, max_step);
            vel.insert(id_s, (vx, vy));
            if let Some(p) = pos.get_mut(id_s) {
                p.0 = (p.0 + vx).clamp(0.08, 0.92);
                p.1 = (p.1 + vy).clamp(0.08, 0.92);
            }
        }
    }

    pos
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    ui.graph_sim.pane_area.set(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_canvas)),
        area,
    );

    if area.width < 20 || area.height < 8 {
        return;
    }

    hitmap.add(area, HitTarget::Pane(Focus::Canvas));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    let graph = &app.graph;
    let selected_idx = ui.graph_selected.min(graph.nodes.len().saturating_sub(1));
    let selected_node = graph.nodes.get(selected_idx);

    // 1. Graph Header HUD + Clickable Zoom/Reset/Scope Controls
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

    let zoom_out_pill = "[-]";
    let zoom_pct_text = format!(" {:>3.0}% ", ui.graph_sim.zoom * 100.0);
    let zoom_in_pill = "[+]";
    let reset_pill = "[Reset]";
    let scope_pill = if ui.graph_global {
        "[l Global ▾]"
    } else {
        "[l Local ▾]"
    };

    let zoom_out_w = zoom_out_pill.width() as u16;
    let zoom_pct_w = zoom_pct_text.width() as u16;
    let zoom_in_w = zoom_in_pill.width() as u16;
    let reset_w = reset_pill.width() as u16;
    let scope_pill_w = scope_pill.width() as u16;
    let controls_w = zoom_out_w + zoom_pct_w + zoom_in_w + 1 + reset_w + 1 + scope_pill_w + 1;
    let show_controls = chunks[0].width > controls_w + 10;

    let left_w = if show_controls {
        chunks[0].width.saturating_sub(controls_w)
    } else {
        chunks[0].width
    };
    let counts_str = if left_w >= 44 {
        format!(
            "   {} nodes · {} edges",
            graph.nodes.len(),
            graph.edges.len()
        )
    } else {
        format!("  {} nodes", graph.nodes.len())
    };
    let counts_w = counts_str.width();
    let title_budget = (left_w as usize).saturating_sub(3 + counts_w);

    let header_line = Line::from(vec![
        Span::styled(
            " ◉ ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_width(&scope_label, title_budget),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(counts_str, theme.muted_style()),
    ]);
    let legend_line = Line::from(vec![
        Span::styled(
            "   ◉ Focal  ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("● 1-hop  ", Style::default().fg(theme.accent_soft)),
        Span::styled("○ 2-hop  ", Style::default().fg(theme.info)),
        Span::styled("◌ Ghost  ", theme.muted_style()),
        Span::styled(
            "  [Wheel/+/-] Zoom  [0/r] Reset  [Drag] Move/Pan  [l] Scope  [[/]] Depth  [Enter] Open",
            theme.subtle_style(),
        ),
    ]);

    frame.render_widget(
        Paragraph::new(header_line).style(Style::default().bg(theme.bg_canvas)),
        Rect::new(chunks[0].x, chunks[0].y, left_w, 1),
    );
    frame.render_widget(
        Paragraph::new(legend_line).style(Style::default().bg(theme.bg_canvas)),
        Rect::new(chunks[0].x, chunks[0].y + 1, chunks[0].width, 1),
    );

    if show_controls {
        let py = chunks[0].y;
        let mut rx = chunks[0].x + chunks[0].width.saturating_sub(controls_w);
        let btn_style = Style::default()
            .fg(theme.accent_soft)
            .bg(theme.bg_card)
            .add_modifier(Modifier::BOLD);

        // [-] Zoom Out
        frame.render_widget(
            Paragraph::new(Span::styled(zoom_out_pill, btn_style)),
            Rect::new(rx, py, zoom_out_w, 1),
        );
        hitmap.add_line(
            rx,
            py,
            zoom_out_w,
            HitTarget::Button(ButtonId::GraphZoomOut),
        );
        rx += zoom_out_w;

        // {zoom%}
        let zoom_style = if (ui.graph_sim.zoom - 1.0).abs() > 0.03 {
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_canvas)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_canvas)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(zoom_pct_text, zoom_style)),
            Rect::new(rx, py, zoom_pct_w, 1),
        );
        rx += zoom_pct_w;

        // [+] Zoom In
        frame.render_widget(
            Paragraph::new(Span::styled(zoom_in_pill, btn_style)),
            Rect::new(rx, py, zoom_in_w, 1),
        );
        hitmap.add_line(rx, py, zoom_in_w, HitTarget::Button(ButtonId::GraphZoomIn));
        rx += zoom_in_w + 1;

        // [Reset]
        frame.render_widget(
            Paragraph::new(Span::styled(reset_pill, btn_style)),
            Rect::new(rx, py, reset_w, 1),
        );
        hitmap.add_line(rx, py, reset_w, HitTarget::Button(ButtonId::GraphResetView));
        rx += reset_w + 1;

        // [l Local/Global ▾]
        let scope_style = if ui.graph_global {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            btn_style
        };
        frame.render_widget(
            Paragraph::new(Span::styled(scope_pill, scope_style)),
            Rect::new(rx, py, scope_pill_w, 1),
        );
        hitmap.add_line(
            rx,
            py,
            scope_pill_w,
            HitTarget::Button(ButtonId::GraphToggleScope),
        );
    }

    // 2. Braille Force-Directed Canvas (viewport-clipped edges + cursor-centered pan/zoom camera)
    let canvas_area = chunks[1];
    ui.graph_sim.canvas_area.set(canvas_area);
    hitmap.add(canvas_area, HitTarget::GraphCanvas);

    let fallback_pos;
    let world_positions = if ui.graph_sim.positions.is_empty() {
        fallback_pos = compute_graph_layout(graph);
        &fallback_pos
    } else {
        &ui.graph_sim.positions
    };
    let focal_id = focal_node_id(graph);
    let active_hi_idx = ui.graph_sim.hovered_node.unwrap_or(selected_idx);
    let sel_id = graph.nodes.get(active_hi_idx).map(|n| n.id.as_str());

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
            // Draw secondary edges first (clipped strictly to [0.0, 1.0]^2)
            for edge in &graph.edges {
                let is_hi = sel_id == Some(edge.source.as_str())
                    || sel_id == Some(edge.target.as_str())
                    || focal_id == Some(edge.source.as_str())
                    || focal_id == Some(edge.target.as_str());
                if is_hi {
                    continue;
                }
                if let (Some(&(wx1, wy1)), Some(&(wx2, wy2))) = (
                    world_positions.get(&edge.source),
                    world_positions.get(&edge.target),
                ) {
                    let (vx1, vy1) = ui.graph_sim.world_to_view(wx1, wy1);
                    let (vx2, vy2) = ui.graph_sim.world_to_view(wx2, wy2);
                    if let Some((cx1, cy1, cx2, cy2)) = clip_line_unit_rect(vx1, vy1, vx2, vy2) {
                        ctx.draw(&CanvasLine {
                            x1: cx1,
                            y1: 1.0 - cy1,
                            x2: cx2,
                            y2: 1.0 - cy2,
                            color: muted_edge,
                        });
                    }
                }
            }
            // Draw focal & hovered/selected edges on top in vivid Obsidian purple
            for edge in &graph.edges {
                let is_sel_edge =
                    sel_id == Some(edge.source.as_str()) || sel_id == Some(edge.target.as_str());
                let is_focal_edge = focal_id == Some(edge.source.as_str())
                    || focal_id == Some(edge.target.as_str());
                if !is_sel_edge && !is_focal_edge {
                    continue;
                }
                if let (Some(&(wx1, wy1)), Some(&(wx2, wy2))) = (
                    world_positions.get(&edge.source),
                    world_positions.get(&edge.target),
                ) {
                    let (vx1, vy1) = ui.graph_sim.world_to_view(wx1, wy1);
                    let (vx2, vy2) = ui.graph_sim.world_to_view(wx2, wy2);
                    if let Some((cx1, cy1, cx2, cy2)) = clip_line_unit_rect(vx1, vy1, vx2, vy2) {
                        ctx.draw(&CanvasLine {
                            x1: cx1,
                            y1: 1.0 - cy1,
                            x2: cx2,
                            y2: 1.0 - cy2,
                            color: if is_sel_edge {
                                accent_color
                            } else {
                                accent_deep
                            },
                        });
                    }
                }
            }
        });

    frame.render_widget(canvas, canvas_area);

    // 3. Deterministic priority-ordered, zoom-adaptive node pills (zero hover-width jitter, clean culling)
    let zoom = ui.graph_sim.zoom;
    let low_zoom = zoom < GRAPH_LOW_ZOOM_THRESHOLD;

    // Identify top hubs deterministically for compact low-zoom rendering
    let top_hubs: HashSet<usize> = if low_zoom {
        let mut by_deg: Vec<(usize, usize, &str)> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (i, n.in_degree + n.out_degree, n.id.as_str()))
            .collect();
        by_deg.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(b.2)));
        by_deg.into_iter().take(3).map(|(i, _, _)| i).collect()
    } else {
        HashSet::new()
    };

    // Collect visible nodes (strictly culled to [0.0, 1.0] x [0.0, 1.0]) and sort by placement priority:
    // dragging -> focal -> selected -> hovered -> highest degree -> depth -> id
    let mut visible: Vec<(usize, &GraphNode, f64, f64)> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(idx, node)| {
            let &(wx, wy) = world_positions.get(&node.id)?;
            let (nx, ny) = ui.graph_sim.world_to_view(wx, wy);
            if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
                return None;
            }
            Some((idx, node, nx, ny))
        })
        .collect();

    visible.sort_by(|&(idx_a, a, _, _), &(idx_b, b, _, _)| {
        let drag_a = ui.graph_sim.dragging_node == Some(idx_a);
        let drag_b = ui.graph_sim.dragging_node == Some(idx_b);
        let focal_a = focal_id == Some(a.id.as_str());
        let focal_b = focal_id == Some(b.id.as_str());
        let sel_a = idx_a == selected_idx;
        let sel_b = idx_b == selected_idx;
        let hov_a = ui.graph_sim.hovered_node == Some(idx_a);
        let hov_b = ui.graph_sim.hovered_node == Some(idx_b);
        let deg_a = a.in_degree + a.out_degree;
        let deg_b = b.in_degree + b.out_degree;
        (!drag_a)
            .cmp(&!drag_b)
            .then_with(|| (!focal_a).cmp(&!focal_b))
            .then_with(|| (!sel_a).cmp(&!sel_b))
            .then_with(|| (!hov_a).cmp(&!hov_b))
            .then_with(|| deg_b.cmp(&deg_a))
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.id.cmp(&b.id))
    });

    let title_max_w: usize = if zoom >= GRAPH_HIGH_ZOOM_THRESHOLD {
        26
    } else if zoom >= 1.0 {
        18
    } else {
        14
    };

    struct PlacedNode {
        idx: usize,
        rect: Rect,
        badge: String,
        style: Style,
    }

    let mut placed_rects: Vec<Rect> = Vec::with_capacity(visible.len());
    let mut placed_nodes: Vec<PlacedNode> = Vec::with_capacity(visible.len());
    let max_y = canvas_area.y + canvas_area.height.saturating_sub(1);

    let overlaps_any = |cand: Rect, placed: &[Rect]| -> bool {
        placed.iter().any(|r| {
            r.y == cand.y
                && cand.x < r.x.saturating_add(r.width + 1)
                && r.x < cand.x.saturating_add(cand.width + 1)
        })
    };

    for (idx, node, nx, ny) in visible {
        let cx =
            canvas_area.x + ((nx * (canvas_area.width.saturating_sub(1) as f64)).round() as u16);
        let cy =
            canvas_area.y + ((ny * (canvas_area.height.saturating_sub(1) as f64)).round() as u16);

        let is_focal = focal_id == Some(node.id.as_str());
        let is_sel = idx == selected_idx;
        let is_hovered = ui.graph_sim.hovered_node == Some(idx);
        let is_dragging = ui.graph_sim.dragging_node == Some(idx);

        let glyph = if is_dragging {
            "✥"
        } else if is_focal {
            "◉"
        } else if !node.resolved {
            "◌"
        } else if node.depth <= 1 {
            "●"
        } else {
            "○"
        };

        let show_title =
            !low_zoom || is_focal || is_sel || is_hovered || is_dragging || top_hubs.contains(&idx);
        // Consistent width across hover/select/normal states (NEVER widens on hover!)
        let badge = if show_title {
            let short_title = truncate_width(&node.title, title_max_w);
            format!(" {glyph} {short_title} ")
        } else {
            format!(" {glyph} ")
        };

        let badge_w = (badge.width() as u16).min(canvas_area.width);
        let x0 = cx
            .saturating_sub(badge_w / 2)
            .max(canvas_area.x)
            .min(canvas_area.x + canvas_area.width.saturating_sub(badge_w));
        let base_y = cy.clamp(canvas_area.y, max_y);

        let mut chosen_y = base_y;
        if !is_dragging && overlaps_any(Rect::new(x0, chosen_y, badge_w, 1), &placed_rects) {
            'search: for delta in 1i16..=6 {
                for cand_y in [base_y as i16 + delta, base_y as i16 - delta] {
                    if cand_y >= canvas_area.y as i16 && cand_y <= max_y as i16 {
                        let r = Rect::new(x0, cand_y as u16, badge_w, 1);
                        if !overlaps_any(r, &placed_rects) {
                            chosen_y = cand_y as u16;
                            break 'search;
                        }
                    }
                }
            }
        }

        let style = if is_sel || is_dragging {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if is_hovered {
            Style::default()
                .fg(theme.fg_primary)
                .bg(theme.bg_selected)
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
        placed_nodes.push(PlacedNode {
            idx,
            rect: pill_rect,
            badge,
            style,
        });
    }

    // Draw & register HitMap in reverse priority order so highest-priority nodes (focal/selected/hovered)
    // always paint on top and win hit-testing.
    for item in placed_nodes.into_iter().rev() {
        frame.render_widget(
            Paragraph::new(Span::styled(item.badge, item.style)),
            item.rect,
        );
        hitmap.add(item.rect, HitTarget::GraphNode(item.idx));
    }

    // 4. Selected Node Inspector Footer Bar
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{GraphData, GraphEdge, GraphNode};

    fn make_node(id: &str, depth: usize, deg: usize) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            title: id.trim_end_matches(".md").to_string(),
            folder: String::new(),
            resolved: true,
            depth,
            in_degree: deg,
            out_degree: 0,
            tags: Vec::new(),
        }
    }

    fn make_edge(src: &str, dst: &str) -> GraphEdge {
        GraphEdge {
            source: src.to_string(),
            target: dst.to_string(),
            is_embed: false,
        }
    }

    #[test]
    fn test_clip_line_unit_rect_cases() {
        // 1. Fully inside
        let inside = clip_line_unit_rect(0.2, 0.3, 0.8, 0.7).unwrap();
        assert!((inside.0 - 0.2).abs() < 1e-9);
        assert!((inside.1 - 0.3).abs() < 1e-9);
        assert!((inside.2 - 0.8).abs() < 1e-9);
        assert!((inside.3 - 0.7).abs() < 1e-9);

        // 2. Partially outside (one endpoint out on right)
        let partial = clip_line_unit_rect(0.5, 0.5, 1.5, 0.5).unwrap();
        assert!((partial.0 - 0.5).abs() < 1e-9);
        assert!((partial.1 - 0.5).abs() < 1e-9);
        assert!((partial.2 - 1.0).abs() < 1e-9);
        assert!((partial.3 - 0.5).abs() < 1e-9);

        // 3. Crossing viewport diagonally from (-0.5, -0.5) to (1.5, 1.5)
        let cross = clip_line_unit_rect(-0.5, -0.5, 1.5, 1.5).unwrap();
        assert!((cross.0 - 0.0).abs() < 1e-9);
        assert!((cross.1 - 0.0).abs() < 1e-9);
        assert!((cross.2 - 1.0).abs() < 1e-9);
        assert!((cross.3 - 1.0).abs() < 1e-9);

        // 4. Crossing viewport off-diagonal: (-0.5, 0.25) -> (1.5, 0.75)
        // y(x) = 0.25 + 0.25 * (x + 0.5) => x=0 -> y=0.375, x=1 -> y=0.625
        let diag = clip_line_unit_rect(-0.5, 0.25, 1.5, 0.75).unwrap();
        assert!((diag.0 - 0.0).abs() < 1e-9);
        assert!((diag.1 - 0.375).abs() < 1e-9);
        assert!((diag.2 - 1.0).abs() < 1e-9);
        assert!((diag.3 - 0.625).abs() < 1e-9);

        // 5. Fully outside (parallel & non-parallel)
        assert!(clip_line_unit_rect(-2.0, 0.5, -0.1, 0.5).is_none());
        assert!(clip_line_unit_rect(0.2, 1.5, 0.8, 1.5).is_none());
        assert!(clip_line_unit_rect(1.2, 0.2, 1.8, 0.9).is_none());
        assert!(clip_line_unit_rect(-0.3, 0.8, 0.1, 1.2).is_none());
        assert!(clip_line_unit_rect(f64::NAN, 0.5, 0.5, 0.5).is_none());

        // Corner touch at (0.0, 1.0)
        let corner = clip_line_unit_rect(-0.2, 0.8, 0.2, 1.2).unwrap();
        assert!((corner.0 - 0.0).abs() < 1e-9 && (corner.1 - 1.0).abs() < 1e-9);
        assert!((corner.2 - 0.0).abs() < 1e-9 && (corner.3 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_zoom_at_preserves_world_point_and_clamps_range() {
        let mut sim = GraphSim::default();
        sim.canvas_area.set(Rect::new(10, 4, 81, 21));

        let cursor_col = 70; // vx = 60/80 = 0.75
        let cursor_row = 9; // vy = 5/20 = 0.25

        let (wx_before, wy_before) = sim.screen_to_world_raw(cursor_col, cursor_row).unwrap();

        // Zoom in multiple times
        for _ in 0..5 {
            sim.zoom_at(cursor_col, cursor_row, GRAPH_WHEEL_FACTOR);
            let (wx_after, wy_after) = sim.screen_to_world_raw(cursor_col, cursor_row).unwrap();
            assert!(
                (wx_after - wx_before).abs() < 1e-9 && (wy_after - wy_before).abs() < 1e-9,
                "world coordinate under cursor must stay invariant on zoom in: before=({wx_before},{wy_before}) after=({wx_after},{wy_after})"
            );
        }

        // Zoom back out multiple times
        for _ in 0..8 {
            sim.zoom_at(cursor_col, cursor_row, 1.0 / GRAPH_WHEEL_FACTOR);
            let (wx_after, wy_after) = sim.screen_to_world_raw(cursor_col, cursor_row).unwrap();
            assert!(
                (wx_after - wx_before).abs() < 1e-9 && (wy_after - wy_before).abs() < 1e-9,
                "world coordinate under cursor must stay invariant on zoom out: before=({wx_before},{wy_before}) after=({wx_after},{wy_after})"
            );
        }

        // Clamp upper bound to 6.0
        for _ in 0..40 {
            sim.zoom_at(cursor_col, cursor_row, 1.5);
        }
        assert!((sim.zoom - GRAPH_ZOOM_MAX).abs() < 1e-9);

        // Clamp lower bound to 0.25
        for _ in 0..40 {
            sim.zoom_at(cursor_col, cursor_row, 0.5);
        }
        assert!((sim.zoom - GRAPH_ZOOM_MIN).abs() < 1e-9);
    }

    #[test]
    fn test_sync_from_graph_preserves_camera_and_existing_positions_for_same_center() {
        let mut graph = GraphData {
            center: Some("Center.md".into()),
            depth: 2,
            nodes: vec![
                make_node("Center.md", 0, 3),
                make_node("Alpha.md", 1, 2),
                make_node("Beta.md", 1, 1),
            ],
            edges: vec![
                make_edge("Center.md", "Alpha.md"),
                make_edge("Center.md", "Beta.md"),
            ],
        };

        let mut sim = GraphSim::default();
        sim.sync_from_graph(&graph);

        // User zooms, pans, and drags Alpha
        sim.zoom = 2.15;
        sim.pan_x = 0.32;
        sim.pan_y = -0.18;
        sim.positions.insert("Alpha.md".into(), (0.22, 0.77));
        let beta_before = sim.positions["Beta.md"];

        // Live edit adds a new node Gamma linked to Alpha (same center!)
        graph.nodes.push(make_node("Gamma.md", 2, 1));
        graph.edges.push(make_edge("Alpha.md", "Gamma.md"));
        sim.sync_from_graph(&graph);

        assert!((sim.zoom - 2.15).abs() < 1e-9, "zoom must be preserved");
        assert!((sim.pan_x - 0.32).abs() < 1e-9, "pan_x must be preserved");
        assert!(
            (sim.pan_y - (-0.18)).abs() < 1e-9,
            "pan_y must be preserved"
        );
        assert_eq!(sim.positions["Alpha.md"], (0.22, 0.77));
        assert_eq!(sim.positions["Beta.md"], beta_before);
        assert!(sim.positions.contains_key("Gamma.md"));
        let (gx, gy) = sim.positions["Gamma.md"];
        assert!((0.08..=0.92).contains(&gx) && (0.08..=0.92).contains(&gy));
        let dist_to_alpha = ((gx - 0.22).powi(2) + (gy - 0.77).powi(2)).sqrt();
        assert!(
            dist_to_alpha < 0.25,
            "new node should spawn cleanly near its connected neighbor (dist={dist_to_alpha})"
        );

        // Switching center resets camera
        graph.center = Some("Alpha.md".into());
        sim.sync_from_graph(&graph);
        assert!((sim.zoom - 1.0).abs() < 1e-9);
        assert!((sim.pan_x - 0.0).abs() < 1e-9);
        assert!((sim.pan_y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_deterministic_layout_and_spiral_spread_for_large_graph() {
        let mut nodes = vec![make_node("Hub.md", 0, 12)];
        let mut edges = Vec::new();
        for i in 1..=26 {
            let depth = if i <= 6 { 1 } else { 2 };
            let id = format!("Note_{i:02}.md");
            nodes.push(make_node(&id, depth, if depth == 1 { 4 } else { 1 }));
            if depth == 1 {
                edges.push(make_edge("Hub.md", &id));
            } else {
                let parent = format!("Note_{:02}.md", (i % 6) + 1);
                edges.push(make_edge(&parent, &id));
            }
        }

        let graph_a = GraphData {
            center: Some("Hub.md".into()),
            depth: 2,
            nodes: nodes.clone(),
            edges: edges.clone(),
        };

        // Reversed input order must produce bit-for-bit identical layout!
        let mut rev_nodes = nodes;
        rev_nodes.reverse();
        let mut rev_edges = edges;
        rev_edges.reverse();
        let graph_b = GraphData {
            center: Some("Hub.md".into()),
            depth: 2,
            nodes: rev_nodes,
            edges: rev_edges,
        };

        let pos_a = compute_graph_layout(&graph_a);
        let pos_b = compute_graph_layout(&graph_b);
        assert_eq!(
            pos_a, pos_b,
            "layout must be 100% deterministic regardless of input order"
        );
        assert_eq!(
            pos_a["Hub.md"],
            (0.50, 0.50),
            "focal node must be pinned at center"
        );

        // All positions strictly within [0.08, 0.92]
        for (id, &(x, y)) in &pos_a {
            assert!(
                (0.08..=0.92).contains(&x) && (0.08..=0.92).contains(&y),
                "node {id} out of bounds: ({x}, {y})"
            );
        }

        // Outer nodes (>20 total) must spread across multiple distinct radii, not jam on a single circle!
        let outer_radii: Vec<f64> = (7..=26)
            .map(|i| {
                let (x, y) = pos_a[&format!("Note_{i:02}.md")];
                ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt()
            })
            .collect();
        let min_r = outer_radii.iter().copied().fold(f64::INFINITY, f64::min);
        let max_r = outer_radii.iter().copied().fold(0.0_f64, f64::max);
        assert!(
            max_r - min_r > 0.08,
            "outer nodes should spread across multiple radii (min_r={min_r}, max_r={max_r})"
        );

        // No two nodes should collapse on top of each other
        let pts: Vec<(f64, f64)> = pos_a.values().copied().collect();
        let mut min_pair_dist = f64::INFINITY;
        for i in 0..pts.len() {
            for j in (i + 1)..pts.len() {
                let d = ((pts[i].0 - pts[j].0).powi(2) + (pts[i].1 - pts[j].1).powi(2)).sqrt();
                if d < min_pair_dist {
                    min_pair_dist = d;
                }
            }
        }
        assert!(
            min_pair_dist > 0.04,
            "nodes must remain well-separated without collapsing (min_pair_dist={min_pair_dist})"
        );
    }

    #[test]
    fn test_click_without_drag_never_wakes_physics_or_moves_nodes() {
        let graph = GraphData {
            center: Some("Center.md".into()),
            depth: 2,
            nodes: vec![
                make_node("Center.md", 0, 2),
                make_node("A.md", 1, 1),
                make_node("B.md", 1, 1),
            ],
            edges: vec![
                make_edge("Center.md", "A.md"),
                make_edge("Center.md", "B.md"),
            ],
        };

        let mut sim = GraphSim::default();
        sim.sync_from_graph(&graph);
        let before = sim.positions.clone();
        assert_eq!(sim.energy, 0.0);
        assert!(!sim.is_active());

        // Simulate clicking node 1 without dragging
        sim.dragging_node = Some(1);
        sim.drag_moved = false;
        assert!(!sim.is_active());
        assert_eq!(sim.energy, 0.0);

        for _ in 0..10 {
            sim.step_physics(&graph);
        }
        assert_eq!(
            sim.positions, before,
            "clicking without drag must move 0 nodes"
        );
        assert_eq!(sim.energy, 0.0);
    }

    #[test]
    fn test_drag_moves_node_keeps_focal_pinned_and_unrelated_nodes_stationary() {
        let graph = GraphData {
            center: Some("Center.md".into()),
            depth: 2,
            nodes: vec![
                make_node("Center.md", 0, 2),
                make_node("A.md", 1, 2),
                make_node("A_Child.md", 2, 1),
                make_node("B.md", 1, 1),
            ],
            edges: vec![
                make_edge("Center.md", "A.md"),
                make_edge("A.md", "A_Child.md"),
                make_edge("Center.md", "B.md"),
            ],
        };

        let mut sim = GraphSim::default();
        sim.canvas_area.set(Rect::new(0, 0, 101, 41));
        sim.sync_from_graph(&graph);
        let center_before = sim.positions["Center.md"];
        let b_before = sim.positions["B.md"];

        // Drag A.md (index 1) to (col=80, row=30)
        sim.dragging_node = Some(1);
        sim.move_dragged_node(&graph, 80, 30);
        assert!(sim.drag_moved);
        assert!(sim.is_active());

        for _ in 0..15 {
            sim.step_physics(&graph);
        }

        // Focal node stays strictly pinned; unrelated node B stays strictly motionless!
        assert_eq!(sim.positions["Center.md"], center_before);
        assert_eq!(sim.positions["B.md"], b_before);

        // Release and settle to 0 energy
        sim.dragging_node = None;
        sim.drag_moved = false;
        for _ in 0..60 {
            sim.step_physics(&graph);
        }
        assert_eq!(sim.energy, 0.0);
        assert!(!sim.is_active());
    }
}
