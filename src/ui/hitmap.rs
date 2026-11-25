//! Rendered screen region -> clickable target mapping (`HitMap`).

use ratatui::layout::{Position, Rect};

use crate::{models::SidebarTab, ui::app::Focus};

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
    EditNote,
    ToggleEditorMode,
    FinishInlineEdit,
    SaveInlineEdit,
    EditExternal,
    LinkExtractor,
    ToggleHrefs,
    ImagePreview,
    HtmlPreview,
    HtmlToggleRaw,
    HtmlServeBrowser,
    ReindexNow,
    GraphToggleScope,
    GraphZoomOut,
    GraphZoomIn,
    GraphResetView,
    ConfirmOk,
    ConfirmCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Pane(Focus),
    SidebarTab(SidebarTab),
    SidebarRow(usize),
    SidebarTaskCheckbox(usize),
    CanvasBlock(usize),
    NoteBodyEmpty,
    InlineEditorBody,
    TaskCheckbox { line_no: usize },
    WikilinkBlock(usize),
    ImageChip(usize),
    HtmlChip(usize),
    GraphCanvas,
    GraphNode(usize),
    InspectorBacklink(usize),
    InspectorOutgoing(usize),
    InspectorOutline(usize),
    Button(ButtonId),
    OverlayBackdrop,
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
