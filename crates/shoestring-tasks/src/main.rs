//! shoestring-tasks — a console (TUI) mission-control for shoestring-wm.
//!
//! Lists every window grouped by workspace (including off-active and minimized
//! windows) and drives the WM's per-window IPC: focus, close, force-kill,
//! rename, move-to-workspace, minimize/maximize, sticky/always-on-top, raise/
//! lower, and per-window screenshot. Built on `shoestring-ipc` (the wire crate,
//! shared with `shoestring-ctl`); no Smithay dependency. The view stays live by
//! subscribing to the WM event stream and re-fetching on change.
//!
//! Usage:
//!   shoestring-tasks [-s <socket>]
//!
//! The socket auto-resolves from `$SHOESTRING_WM_SOCKET`, falling back to
//! `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`. Press `?` in the UI
//! for the full key list.

mod app;
mod ipc;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event};
use shoestring_ipc::client_socket_path;

use crate::app::App;

fn main() -> Result<()> {
    // Minimal hand-rolled arg parse (no clap — this binary takes one option).
    let mut socket: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" | "--socket" => {
                socket = Some(PathBuf::from(args.next().context("-s needs a path")?));
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("shoestring-tasks {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    let socket = socket
        .or_else(client_socket_path)
        .context("could not resolve socket path: set $SHOESTRING_WM_SOCKET or pass --socket")?;

    let mut app = App::new(socket.clone());
    app.refresh();

    // Subscribe to WM events on a background thread; each nudge means "re-fetch".
    let (tx, rx) = mpsc::channel::<()>();
    ipc::spawn_event_thread(socket, tx);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &rx);
    ratatui::restore();
    result
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App,
    rx: &mpsc::Receiver<()>,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Block up to ~200ms for a keystroke; in between, an event-stream
        // nudge or the timeout drives a refresh so the view stays current.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                // crossterm reports both press and release on some terminals;
                // act on press (and the default Kind on others).
                use ratatui::crossterm::event::KeyEventKind;
                if key.kind != KeyEventKind::Release {
                    let needs_refresh = app.on_key(key);
                    if app.should_quit {
                        return Ok(());
                    }
                    if needs_refresh {
                        app.refresh();
                    }
                }
            }
        }

        // Drain all pending event-stream nudges; refresh once if any arrived.
        let mut changed = false;
        while rx.try_recv().is_ok() {
            changed = true;
        }
        if changed {
            app.refresh();
        }
    }
}

fn print_usage() {
    println!(
        "shoestring-tasks — console window/workspace manager for shoestring-wm\n\n\
         USAGE:\n    shoestring-tasks [-s <socket>]\n\n\
         OPTIONS:\n    \
         -s, --socket <PATH>   Override the IPC socket path\n    \
         -h, --help            Show this help\n    \
         -V, --version         Show version\n\n\
         Press ? inside the UI for key bindings."
    );
}
