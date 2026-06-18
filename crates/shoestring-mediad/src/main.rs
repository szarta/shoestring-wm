//! shoestring-mediad — the media-privacy helper for shoestring-wm.
//!
//! Modes:
//! - `monitor` (default): long-running. Watches PipeWire for the default
//!   sink/source mute (and, later, active cameras) and reports each change to
//!   the WM via `Request::ReportMedia`. Autostarted alongside the bar.
//! - `audio-mute <on|off|toggle>` / `mic-mute <on|off|toggle>`: oneshot. Sets
//!   the real default sink/source mute and exits. The WM spawns these for the
//!   bar/keybind/IPC mute controls.
//! - `status`: oneshot. Prints the current snapshot as JSON (debugging).
//!
//! This binary (with the screencast portal) is the only part of the project
//! that links libpipewire. The compositor never does — it just caches what we
//! report and delegates control to us.

mod control;
mod monitor;
mod pw;
mod wm;

use anyhow::{bail, Result};

use control::Action;
use pw::Kind;

fn main() -> Result<()> {
    init_tracing();
    // SAFETY: pipewire::init() is libpipewire's global init; run once up front.
    unsafe { pipewire_init() };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("monitor");
    match mode {
        "monitor" => monitor::run(),
        "audio-mute" => control::oneshot(Kind::Sink, parse_action(args.get(1))?),
        "mic-mute" => control::oneshot(Kind::Source, parse_action(args.get(1))?),
        "status" => print_status(),
        other => bail!("unknown mode {other:?} (use: monitor | audio-mute | mic-mute | status)"),
    }
}

/// `pipewire::init` is safe to call but documented as a global init; wrap it so
/// the `unsafe` in `main` reads as "global, once".
unsafe fn pipewire_init() {
    pipewire::init();
}

fn parse_action(arg: Option<&String>) -> Result<Action> {
    match arg.map(String::as_str) {
        Some("on") => Ok(Action::On),
        Some("off") => Ok(Action::Off),
        Some("toggle") | None => Ok(Action::Toggle),
        Some(other) => bail!("expected on|off|toggle, got {other:?}"),
    }
}

/// Connect, let discovery settle briefly, and print the snapshot as JSON.
fn print_status() -> Result<()> {
    use std::rc::Rc;
    let pw = pw::Pw::connect(Rc::new(|_| {}))?;
    pw.pump_until(std::time::Duration::from_secs(1), |_| false);
    let s = pw.snapshot();
    println!(
        r#"{{"audio_muted":{},"mic_muted":{},"camera_active":{}}}"#,
        s.audio_muted, s.mic_muted, s.camera_active
    );
    Ok(())
}

/// Logs to stderr (which autostart routes to the WM's log). Filter via
/// `SHOESTRING_MEDIAD_LOG` (env-filter directives) or `RUST_LOG`; default info.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("SHOESTRING_MEDIAD_LOG")
        .ok()
        .map(EnvFilter::new)
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
