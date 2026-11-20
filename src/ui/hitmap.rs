//! Rendered screen region -> clickable target mapping (`HitMap`).

use ratatui::layout::{Position, Rect};

use crate::models::SidebarTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    VaultSwitcher,
    ToggleGraph,
    OpenDaily,
    ToggleInspector,
    Help,
    Settings,
    QuickSwitcher,
    ToggleSidebar,
    ToggleBookmark,
    ToggleWidth,
    QuickAppend,
    NewNote,
    EditExternal,
    LinkExtractor,
    ToggleHrefs,
    ImagePreview,
    ReindexNow,
    GraphToggleScope,
    ConfirmOk,
    ConfirmCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    SidebarTab(SidebarTab),
    SidebarRow(usize),
    CanvasBlock(usize),
    TaskCheckbox { line_no: usize },
    WikilinkBlock(usize),
    ImageChip(usize),
    GraphCanvas,
    GraphNode(usize),
    InspectorBacklink(usize),
    InspectorOutgoing(usize),
    Button(ButtonId),
    OverlayBody,
    OverlayItem { index: usize },
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let pos = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(pos))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }
}
