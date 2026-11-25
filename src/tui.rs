//! Interactive Terminal UI runtime: raw mode, Windows `CONIN$` mouse capture,
//! bracketed paste (Unix), Kitty/iTerm2/Sixel pixel overlay with ghost-erase tracking,
//! background vault watcher events, and seamless `$EDITOR` handoff with automatic re-indexing.

use std::{
    borrow::Cow,
    io::{self, Stdout, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::Result;
#[cfg(not(windows))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, EventStream,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use tokio::sync::mpsc;

use crate::{
    config::{AppConfig, ImageProtocolPreference, VaultProfile},
    img_preview::{self, ActivePixelGraphic, TerminalGraphicsCaps, TerminalImageProtocol},
    models::CoreEvent,
    storage::Storage,
    sync,
    ui::{
        self,
        app::{AppState, UiState},
        event,
        overlays::Overlay,
    },
};

static KITTY_USED_GLOBAL: AtomicBool = AtomicBool::new(false);

/// VT DECSET sequences for X10 + button-motion + SGR mouse reporting.
/// `crossterm 0.28` skips ANSI mouse codes on Windows, so writing these explicitly
/// ensures ConPTY / Windows Terminal / xterm.js report mouse wheel events cleanly.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const VT_ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const VT_DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn write_vt_mouse_enable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_ENABLE_MOUSE.as_bytes())?;
    w.flush()
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn write_vt_mouse_disable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_DISABLE_MOUSE.as_bytes())?;
    w.flush()
}

#[cfg(windows)]
mod win_console {
    use std::ptr::null_mut;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;

    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    pub const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

    /// Raw-mode + ConPTY mouse capture console input mode (`0x0098`):
    /// enables `ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS`,
    /// explicitly clearing `ENABLE_QUICK_EDIT_MODE`, `ENABLE_VIRTUAL_TERMINAL_INPUT`,
    /// `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT`, and `ENABLE_PROCESSED_INPUT`.
    pub const TUI_MOUSE_CAPTURE_MODE: u32 =
        ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
    }

    fn with_conin_handle<T>(f: impl FnOnce(isize) -> Option<T>) -> Option<T> {
        const CONIN_WIDE: [u16; 7] = [
            b'C' as u16,
            b'O' as u16,
            b'N' as u16,
            b'I' as u16,
            b'N' as u16,
            b'$' as u16,
            0,
        ];
        let conin = unsafe {
            CreateFileW(
                CONIN_WIDE.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if conin != 0 && conin != INVALID_HANDLE_VALUE {
            let res = f(conin);
            unsafe {
                CloseHandle(conin);
            }
            return res;
        }
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if std_in != 0 && std_in != INVALID_HANDLE_VALUE {
            f(std_in)
        } else {
            None
        }
    }

    pub fn snapshot_console_mode() -> Option<u32> {
        with_conin_handle(|h| {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                Some(mode)
            } else {
                None
            }
        })
    }

    pub fn enforce_windows_mouse_capture() {
        let _ = with_conin_handle(|h| {
            unsafe {
                SetConsoleMode(h, TUI_MOUSE_CAPTURE_MODE);
            }
            Some(())
        });
    }

    pub fn restore_console_mode(orig_mode: Option<u32>) {
        if let Some(mode) = orig_mode {
            let _ = with_conin_handle(|h| {
                unsafe {
                    SetConsoleMode(h, mode | ENABLE_EXTENDED_FLAGS);
                }
                Some(())
            });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PixelReconcilePlan {
    erase_and_redraw: bool,
    delete_kitty: bool,
    erase_rects: Vec<Rect>,
    emit_desired: bool,
}

#[derive(Debug, Default)]
struct PixelLayerTracker {
    active: Vec<ActivePixelGraphic>,
    last_term_size: (u16, u16),
}

impl PixelLayerTracker {
    fn plan_transition(
        &self,
        desired: &[ActivePixelGraphic],
        term_size: (u16, u16),
        kitty_used: bool,
    ) -> PixelReconcilePlan {
        let term_resized = self.last_term_size != term_size;
        if self.active == desired && !term_resized {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: false,
            };
        }

        let had_active = !self.active.is_empty();
        let delete_kitty = had_active
            && (kitty_used
                || self
                    .active
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty));
        let erase_rects = self
            .active
            .iter()
            .map(|prev| {
                Rect::new(
                    prev.rect.x,
                    prev.rect.y,
                    prev.rect.width.max(prev.cols),
                    prev.rect.height.max(prev.rows),
                )
            })
            .collect();

        PixelReconcilePlan {
            erase_and_redraw: had_active,
            delete_kitty,
            erase_rects,
            emit_desired: !desired.is_empty(),
        }
    }

    fn commit(&mut self, desired: Vec<ActivePixelGraphic>, term_size: (u16, u16)) {
        self.active = desired;
        self.last_term_size = term_size;
    }

    fn clear(&mut self) {
        self.active.clear();
        self.last_term_size = (0, 0);
    }
}

/// Overlay Z-order guard: only allows modal pixel graphics when `ImagePreview` is the active overlay.
fn pixel_graphics_allowed_for_overlays(overlay: Option<&Overlay>, cache_key: &str) -> bool {
    match overlay {
        Some(Overlay::ImagePreview(_)) => !cache_key.starts_with("inline:"),
        None => cache_key.starts_with("inline:"),
        Some(_) => false,
    }
}

fn parse_pixel_key(key: &str) -> (&str, Option<(u16, u16, u16, u16)>) {
    if let Some((base, crop_str)) = key.rsplit_once("#crop:") {
        let mut parts = crop_str.split(',');
        if let (Some(sx), Some(sy), Some(vw), Some(vh), None) = (
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next(),
        ) {
            return (base, Some((sx, sy, vw, vh)));
        }
    }
    (key, None)
}

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
    pixel_tracker: PixelLayerTracker,
    #[cfg(windows)]
    orig_conin_mode: Option<u32>,
}

impl TerminalGuard {
    fn enter(
        image_protocol: ImageProtocolPreference,
    ) -> Result<(
        Self,
        Terminal<CrosstermBackend<Stdout>>,
        TerminalGraphicsCaps,
    )> {
        #[cfg(windows)]
        let orig_conin_mode = win_console::snapshot_console_mode();

        enable_raw_mode()?;
        // Probe graphics & cell pixel dimensions while raw mode is active, BEFORE entering
        // alternate screen / mouse capture so Windows VT probe never clobbers ConPTY mouse mode.
        let graphics_caps = img_preview::probe_terminal_graphics_caps(image_protocol);

        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        #[cfg(windows)]
        {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
            write_vt_mouse_enable(&mut stdout)?;
        }

        let keyboard_enhanced = if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )
            .is_ok()
        } else {
            false
        };

        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
                pixel_tracker: PixelLayerTracker::default(),
                #[cfg(windows)]
                orig_conin_mode,
            },
            terminal,
            graphics_caps,
        ))
    }

    fn resume_after_editor(&mut self) -> Result<Terminal<CrosstermBackend<Stdout>>> {
        self.pixel_tracker.clear();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        #[cfg(windows)]
        {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
            write_vt_mouse_enable(&mut stdout)?;
        }

        if self.keyboard_enhanced {
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            );
        }

        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;
        Ok(terminal)
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used || KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(self.orig_conin_mode);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool, orig_conin_mode: Option<u32>) {
    #[cfg(not(windows))]
    let _ = orig_conin_mode;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(orig_conin_mode);
        prev(info);
    }));
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
) -> Result<()> {
    let size = terminal.size()?;
    ui.term_width = size.width;
    ui.term_height = size.height;

    // Ensure HD modal image is rendered before draw (and auto-route HTML attachments to HtmlPreview)
    app.prepare_active_overlay(ui, size.width, size.height);

    let mut stdout = io::stdout();
    let _ = execute!(stdout, BeginSynchronizedUpdate);

    let draw_res: Result<()> = (|| {
        terminal.draw(|f| {
            let _ = ui::draw(f, app, ui);
        })?;

        let mut desired: Vec<ActivePixelGraphic> = Vec::new();
        let mut resolved_payloads: Vec<(ActivePixelGraphic, String)> = Vec::new();

        if let Some(pending) = ui.pending_pixel.take() {
            if pixel_graphics_allowed_for_overlays(ui.overlay.as_ref(), &pending.cache_key) {
                let (base_key, crop) = parse_pixel_key(&pending.cache_key);
                if let Some(rendered) = app.preview_cache.get(base_key) {
                    // Never allow pixel graphics to touch the bottom-most status row (`term_height - 1`).
                    let safe_max_h = ui
                        .term_height
                        .saturating_sub(1)
                        .saturating_sub(pending.rect.y);
                    let safe_max_w = ui.term_width.saturating_sub(pending.rect.x);
                    let fit_w = pending.rect.width.min(safe_max_w);
                    let fit_h = pending.rect.height.min(safe_max_h);

                    if fit_w >= 4 && fit_h >= 2 {
                        let resolved: Option<(Cow<'_, str>, u16, u16)> =
                            if let Some((sx, sy, vw, vh)) = crop {
                                let eff_vw = vw.min(safe_max_w);
                                let eff_vh = vh.min(safe_max_h);
                                rendered
                                    .viewport_pixel_payload(sx, sy, eff_vw, eff_vh)
                                    .map(|(p, dw, dh)| (Cow::Owned(p), dw, dh))
                                    .or_else(|| {
                                        if rendered.cols <= eff_vw && rendered.rows <= eff_vh {
                                            rendered.pixel_payload.as_deref().map(|p| {
                                                (Cow::Borrowed(p), rendered.cols, rendered.rows)
                                            })
                                        } else {
                                            None
                                        }
                                    })
                            } else if rendered.cols <= fit_w && rendered.rows <= fit_h {
                                rendered
                                    .pixel_payload
                                    .as_deref()
                                    .map(|p| (Cow::Borrowed(p), rendered.cols, rendered.rows))
                            } else {
                                rendered
                                    .viewport_pixel_payload(0, 0, fit_w, fit_h)
                                    .map(|(p, dw, dh)| (Cow::Owned(p), dw, dh))
                            };

                        if let Some((payload, draw_cols, draw_rows)) = resolved {
                            let graphic = ActivePixelGraphic {
                                cache_key: pending.cache_key,
                                rect: Rect::new(
                                    pending.rect.x,
                                    pending.rect.y,
                                    draw_cols,
                                    draw_rows,
                                ),
                                protocol: pending.protocol,
                                cols: draw_cols,
                                rows: draw_rows,
                                payload_len: payload.len(),
                            };
                            desired.push(graphic.clone());
                            resolved_payloads.push((graphic, payload.into_owned()));
                        }
                    }
                }
            }
        }

        let term_size = (ui.term_width, ui.term_height);
        let plan = guard
            .pixel_tracker
            .plan_transition(&desired, term_size, guard.kitty_used);

        if plan.erase_and_redraw {
            if plan.delete_kitty {
                let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
                let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            }
            for rect in &plan.erase_rects {
                let erase = img_preview::erase_pixel_rect_sequence(
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    ui.term_width,
                    ui.term_height,
                );
                if !erase.is_empty() {
                    let _ = stdout.write_all(erase.as_bytes());
                }
            }
            let _ = stdout.flush();
            terminal.clear()?;
            terminal.draw(|f| {
                let _ = ui::draw(f, app, ui);
            })?;
            ui.pending_pixel = None;
        }

        if plan.emit_desired {
            for (graphic, payload) in &resolved_payloads {
                let move_seq = format!("\x1b[{};{}H", graphic.rect.y + 1, graphic.rect.x + 1);
                let _ = stdout.write_all(move_seq.as_bytes());
                let _ = stdout.write_all(payload.as_bytes());
                if graphic.protocol == TerminalImageProtocol::Kitty {
                    guard.kitty_used = true;
                    KITTY_USED_GLOBAL.store(true, Ordering::Relaxed);
                }
            }
        } else if plan.delete_kitty {
            guard.kitty_used = false;
        }

        guard.pixel_tracker.commit(desired, term_size);
        if let Some((cx, cy)) = ui.cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }
        Ok(())
    })();

    let _ = execute!(stdout, EndSynchronizedUpdate);
    let _ = stdout.flush();
    draw_res
}

pub async fn run_tui(cfg: AppConfig, vault: VaultProfile, storage: Storage) -> Result<()> {
    // Initial synchronous scan so first frame is immediately populated
    let initial_stats = storage.sync_vault(&vault)?;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl =
        sync::spawn_vault_watcher(vault.clone(), storage.clone(), cfg.watch_poll_ms, events_tx);

    let (mut guard, mut terminal, graphics_caps) = TerminalGuard::enter(cfg.image_protocol)?;
    #[cfg(windows)]
    let orig_conin_mode = guard.orig_conin_mode;
    #[cfg(not(windows))]
    let orig_conin_mode = None;
    install_panic_hook(guard.keyboard_enhanced, orig_conin_mode);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    app.graphics_caps = graphics_caps;
    app.last_index_stats = Some(initial_stats);
    let mut ui = UiState::from_config(&cfg);
    if let Ok(sz) = terminal.size() {
        ui.term_width = sz.width;
        ui.term_height = sz.height;
    }
    app.reload_from_storage(&mut ui);
    if cfg.load_failed {
        ui.toasts
            .warning("config.toml could not be parsed: using defaults, settings will not be saved");
    }

    let mut reader = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut physics_tick = tokio::time::interval(Duration::from_millis(33));
    physics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut redraw = true;
    loop {
        if redraw {
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;
        }

        if ui.should_quit {
            break;
        }

        // Handle pending native localhost HTML serving & browser launch (`o` inside HtmlPreview)
        if let Some((title, html)) = app.pending_html_serve.take() {
            let _ = app.serve_html_and_open(&title, &html, &mut ui, true).await;
            redraw = true;
            continue;
        }

        // Handle external `$EDITOR` launch cleanly (restore terminal -> edit -> reindex -> re-enter)
        if let Some(edit_path) = app.pending_external_editor.take() {
            drop(reader);
            guard.restore();
            let edit_res = launch_external_editor(&edit_path);
            terminal = guard.resume_after_editor()?;
            reader = EventStream::new();
            match edit_res.and_then(|()| app.storage.sync_vault(&app.active_vault)) {
                Ok(_) => {
                    app.reload_from_storage(&mut ui);
                    ui.toasts.success("Reloaded note after external edit");
                }
                Err(e) => ui.toasts.error(format!("Editor failed: {e}")),
            }
            redraw = true;
            continue;
        }

        let graph_animating =
            ui.canvas_mode == ui::app::CanvasMode::Graph && ui.graph_sim.is_active();

        redraw = tokio::select! {
            _ = physics_tick.tick(), if graph_animating => {
                app.dispatch(ui::action::Action::GraphTick, &mut ui);
                true
            }
            _ = tick.tick() => {
                let toast_expired = ui.toasts.prune();
                app.tick_inline_autosave(&mut ui) || toast_expired
            }
            Some(core_ev) = events_rx.recv() => {
                app.handle_core_event(core_ev, &mut ui);
                true
            }
            maybe_ev = reader.next() => {
                match maybe_ev {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        let text_input = ui.is_text_input_active();
                        let effective_focus = if ui.overlay.is_some() {
                            ui::app::Focus::Overlay
                        } else {
                            ui.focus
                        };
                        if let Some(act) = event::key_to_action(
                            key,
                            effective_focus,
                            ui.canvas_mode,
                            &ui.keymap,
                            text_input,
                        ) {
                            app.dispatch(act, &mut ui);
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        if let Some(act) = event::mouse_to_action(mouse) {
                            app.dispatch(act, &mut ui);
                        }
                    }
                    Some(Ok(CrosstermEvent::Paste(pasted))) => {
                        if ui.is_text_input_active() {
                            app.dispatch(ui::action::Action::Paste(pasted), &mut ui);
                        }
                    }
                    Some(Ok(CrosstermEvent::Resize(w, h))) => {
                        ui.term_width = w;
                        ui.term_height = h;
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
                true
            }
        };
    }

    app.html_server.shutdown();
    Ok(())
}

pub fn launch_external_editor(path: &Path) -> Result<()> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|command| !command.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    ui::composer::run_editor_command(&editor, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NoteDetail, NoteImageRef, NoteSummary, ParsedNote};
    use crate::ui::overlays::image_preview::ImagePreviewOverlay;

    fn sample_graphic(
        key: &str,
        rect: Rect,
        protocol: TerminalImageProtocol,
        payload_len: usize,
    ) -> ActivePixelGraphic {
        ActivePixelGraphic {
            cache_key: key.to_string(),
            rect,
            protocol,
            cols: rect.width,
            rows: rect.height,
            payload_len,
        }
    }

    #[test]
    fn test_parse_pixel_key_extracts_base_and_crop() {
        assert_eq!(
            parse_pixel_key("modal:img.png:92:100:Sixel#crop:2,10,80,25"),
            ("modal:img.png:92:100:Sixel", Some((2, 10, 80, 25)))
        );
        assert_eq!(
            parse_pixel_key("modal:img.png:92:100:Sixel"),
            ("modal:img.png:92:100:Sixel", None)
        );
        assert_eq!(
            parse_pixel_key("modal:img.png:92:100:Sixel#crop:bad"),
            ("modal:img.png:92:100:Sixel#crop:bad", None)
        );
    }

    #[test]
    fn test_pixel_tracker_close_switch_unchanged_and_resize() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "modal:a.png:80:100:Sixel#crop:0,0,80,24",
            Rect::new(4, 3, 80, 24),
            TerminalImageProtocol::Sixel,
            4096,
        );
        let img_b = sample_graphic(
            "modal:b.png:80:100:Kitty#crop:0,0,70,20",
            Rect::new(6, 4, 70, 20),
            TerminalImageProtocol::Kitty,
            3072,
        );

        // Initial paint
        let p0 = tracker.plan_transition(std::slice::from_ref(&img_a), (120, 34), false);
        assert!(!p0.erase_and_redraw);
        assert!(p0.emit_desired);
        tracker.commit(vec![img_a.clone()], (120, 34));

        // Unchanged tick -> no erase, no re-emit
        let p1 = tracker.plan_transition(std::slice::from_ref(&img_a), (120, 34), false);
        assert!(!p1.erase_and_redraw);
        assert!(!p1.emit_desired);

        // Closing image preview -> erase_and_redraw with img_a rect
        let p_close = tracker.plan_transition(&[], (120, 34), false);
        assert!(p_close.erase_and_redraw);
        assert!(!p_close.delete_kitty);
        assert_eq!(p_close.erase_rects, vec![img_a.rect]);
        assert!(!p_close.emit_desired);

        // Switching image A -> image B -> erase A & emit B
        let p_switch = tracker.plan_transition(std::slice::from_ref(&img_b), (120, 34), false);
        assert!(p_switch.erase_and_redraw);
        assert_eq!(p_switch.erase_rects, vec![img_a.rect]);
        assert!(p_switch.emit_desired);
        tracker.commit(vec![img_b.clone()], (120, 34));

        // Resize with Kitty active -> delete_kitty + erase + re-emit
        let p_resize = tracker.plan_transition(std::slice::from_ref(&img_b), (130, 34), true);
        assert!(p_resize.erase_and_redraw);
        assert!(p_resize.delete_kitty);
        assert!(p_resize.emit_desired);
    }

    #[test]
    fn test_overlay_z_order_guard_suppresses_pixels_under_non_image_modals() {
        let detail = NoteDetail {
            summary: NoteSummary {
                rel_path: "Note.md".into(),
                title: "Note".into(),
                ..Default::default()
            },
            raw_markdown: String::new(),
            parsed: ParsedNote::default(),
            backlinks: Vec::new(),
            outgoing_links: Vec::new(),
            images: vec![NoteImageRef {
                raw_target: "photo.png".into(),
                display_name: "photo.png".into(),
                mime_type: "image/png".into(),
                abs_path: std::path::PathBuf::from("photo.png"),
                line_no: 1,
            }],
        };
        let img_ov =
            Overlay::ImagePreview(ImagePreviewOverlay::from_note(&detail, Some(0)).unwrap());
        let help_ov = Overlay::Help { scroll: 0 };

        assert!(pixel_graphics_allowed_for_overlays(
            Some(&img_ov),
            "modal:photo.png:80:100:Sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            Some(&img_ov),
            "inline:photo.png"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            Some(&help_ov),
            "modal:photo.png:80:100:Sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            None,
            "modal:photo.png:80:100:Sixel"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_mouse_capture_mode_bits() {
        // 0x0098 = ENABLE_WINDOW_INPUT (0x0008) | ENABLE_MOUSE_INPUT (0x0010) | ENABLE_EXTENDED_FLAGS (0x0080)
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE, 0x0098);
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0040, 0); // QuickEdit cleared
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0200, 0); // VTInput cleared
    }

    #[test]
    fn test_vt_mouse_enable_and_disable_ansi_sequences() {
        let mut enable_buf = Vec::new();
        write_vt_mouse_enable(&mut enable_buf).unwrap();
        assert_eq!(
            String::from_utf8(enable_buf).unwrap(),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );

        let mut disable_buf = Vec::new();
        write_vt_mouse_disable(&mut disable_buf).unwrap();
        assert_eq!(
            String::from_utf8(disable_buf).unwrap(),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );
    }
}
