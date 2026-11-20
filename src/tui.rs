//! Interactive Terminal UI runtime: raw mode, mouse capture, bracketed paste, Kitty/iTerm2/Sixel pixel overlay,
//! background vault watcher events, and seamless `$EDITOR` handoff with automatic re-indexing.

use std::{
    io::{self, Stdout, Write},
    path::Path,
    process::Command,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use crate::{
    config::{AppConfig, VaultProfile},
    img_preview::{kitty_delete_preview_sequence, probe_terminal_graphics_caps, TerminalImageProtocol},
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

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
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

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
            },
            terminal,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used {
            let _ = stdout.write_all(kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub async fn run_tui(
    cfg: AppConfig,
    vault: VaultProfile,
    storage: Storage,
) -> Result<()> {
    // Initial synchronous scan so first frame is immediately populated
    let initial_stats = storage.sync_vault(&vault)?;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl =
        sync::spawn_vault_watcher(vault.clone(), storage.clone(), cfg.watch_poll_ms, events_tx);

    let (mut guard, mut terminal) = TerminalGuard::enter()?;
    let graphics_caps = probe_terminal_graphics_caps(cfg.image_protocol);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    app.graphics_caps = graphics_caps;
    app.last_index_stats = Some(initial_stats);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);

    let mut reader = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut physics_tick = tokio::time::interval(Duration::from_millis(25));
    physics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Ensure HD modal image is rendered before draw
        if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlay {
            let size = terminal.size()?;
            app.ensure_modal_image_cached(ov, size.width, size.height);
        }

        // Draw frame inside synchronized update block
        let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
        terminal.draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })?;
        let _ = execute!(io::stdout(), EndSynchronizedUpdate);

        // Emit Kitty/iTerm2/Sixel pixel graphic payload after ratatui flush if active
        if let Some(px) = ui.pending_pixel.take() {
            if px.protocol == TerminalImageProtocol::Kitty {
                guard.kitty_used = true;
            }
            let mut stdout = io::stdout();
            let _ = execute!(stdout, MoveTo(px.col, px.row));
            let _ = stdout.write_all(px.payload.as_bytes());
            let _ = stdout.flush();
        } else if guard.kitty_used {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.flush();
            guard.kitty_used = false;
        }

        if ui.should_quit {
            break;
        }

        // Handle external `$EDITOR` launch cleanly (restore terminal -> edit -> reindex -> re-enter)
        if let Some(edit_path) = app.pending_external_editor.take() {
            drop(reader);
            guard.restore();
            let edit_res = launch_external_editor(&edit_path);
            let (new_guard, new_terminal) = TerminalGuard::enter()?;
            guard = new_guard;
            terminal = new_terminal;
            reader = EventStream::new();
            match edit_res {
                Ok(()) => {
                    let _ = app.storage.sync_vault(&app.active_vault);
                    app.reload_from_storage(&mut ui);
                    ui.toasts.success("Reloaded note after external edit");
                }
                Err(e) => ui.toasts.error(format!("Editor failed: {e}")),
            }
            continue;
        }

        let graph_animating =
            ui.canvas_mode == ui::app::CanvasMode::Graph && ui.graph_sim.is_active();

        tokio::select! {
            _ = physics_tick.tick(), if graph_animating => {
                app.dispatch(ui::action::Action::GraphTick, &mut ui);
            }
            _ = tick.tick() => {
                ui.toasts.prune();
            }
            Some(core_ev) = events_rx.recv() => {
                app.handle_core_event(core_ev, &mut ui);
            }
            maybe_ev = reader.next() => {
                match maybe_ev {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        let text_input = ui.is_text_input_active();
                        if let Some(act) = event::key_to_action(
                            key,
                            ui.focus,
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
                            for ch in pasted.chars() {
                                if ch != '\r' && ch != '\n' {
                                    app.dispatch(ui::action::Action::Input(ch), &mut ui);
                                }
                            }
                        }
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {}
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

pub fn launch_external_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let mut cmd = Command::new(prog);
    for arg in parts {
        cmd.arg(arg);
    }
    cmd.arg(path);
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("Editor exited with status {status}");
    }
    Ok(())
}
