//! Core TUI application state (`AppState`), presentation state (`UiState`), and action dispatcher.

mod actions_media;
mod actions_nav;
mod actions_notes;
mod actions_system;
mod dispatch;
mod editor;
mod media;
mod mouse;
mod navigation;
mod state;
mod sync;

#[cfg(test)]
mod tests;

pub use media::inline_preview_cache_key;
pub use state::{AppState, CanvasMode, Focus, OpenIntent, PendingPixelGraphic, UiState};
