//! Oneshot mute control. Sets (or toggles) the *default* sink/source mute and
//! exits. The WM spawns this for the bar/keybind/IPC mute controls; the
//! long-running monitor then observes the change and reports it back.
//!
//! Two paths, in order:
//!  1. `wpctl set-mute @DEFAULT_AUDIO_{SINK,SOURCE}@ <1|0|toggle>` — the
//!     authoritative shared mute that pavucontrol / media keys / WirePlumber all
//!     use. We don't *hard*-depend on it: it's the preferred control path
//!     because it's what every other controller agrees on.
//!  2. Native libpipewire `Node.set_param(Props, mute)` fallback when `wpctl`
//!     is absent (a pure-PipeWire session with no WirePlumber). On such a setup
//!     the node soft-mute *is* the mute.
//!
//! Monitoring is always native (see [`crate::pw`]); only control prefers the
//! CLI, because the shared mute is WirePlumber-arbitrated on the systems that
//! ship it (Fedora / Debian / Arch / FreeBSD).

use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::pw::{Kind, Pw, Snapshot};

/// Requested mute change. `Toggle` reads the live mute first (native path) so we
/// never act on a stale value; `wpctl` has its own `toggle` verb.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    On,
    Off,
    Toggle,
}

/// How long to wait for the default device + its current mute to be discovered
/// on the native fallback path.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to keep pumping after a native `set_param` so it reaches the daemon.
const FLUSH: Duration = Duration::from_millis(300);

pub fn oneshot(kind: Kind, action: Action) -> Result<()> {
    match try_wpctl(kind, action) {
        Ok(()) => {
            tracing::info!(?kind, ?action, "set mute via wpctl");
            Ok(())
        }
        Err(e) => {
            tracing::debug!(error = %e, "wpctl unavailable; native PipeWire fallback");
            native_set(kind, action)
        }
    }
}

/// `wpctl set-mute @DEFAULT_AUDIO_{SINK,SOURCE}@ <1|0|toggle>`. Returns `Err`
/// (so the caller falls back) when the binary is missing or it exits non-zero.
fn try_wpctl(kind: Kind, action: Action) -> Result<()> {
    let target = match kind {
        Kind::Sink => "@DEFAULT_AUDIO_SINK@",
        Kind::Source => "@DEFAULT_AUDIO_SOURCE@",
    };
    let verb = match action {
        Action::On => "1",
        Action::Off => "0",
        Action::Toggle => "toggle",
    };
    let status = Command::new("wpctl")
        .args(["set-mute", target, verb])
        .status()
        .map_err(|e| anyhow!("spawn wpctl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("wpctl exited with {status}"))
    }
}

/// Native libpipewire fallback: discover the default node and set its soft-mute.
fn native_set(kind: Kind, action: Action) -> Result<()> {
    let on_change: Rc<dyn Fn(Snapshot)> = Rc::new(|_| {});
    let pw = Pw::connect(on_change)?;

    pw.pump_until(DISCOVER_TIMEOUT, |pw| pw.default_mute_known(kind));

    let target = match action {
        Action::On => true,
        Action::Off => false,
        Action::Toggle => !pw.default_mute(kind).ok_or_else(|| {
            anyhow!("no default {kind:?} to toggle (is PipeWire running with a default device?)")
        })?,
    };

    pw.set_default_mute(kind, target)?;
    tracing::info!(?kind, target, "set default mute (native)");

    pw.pump_until(FLUSH, |_| false);
    Ok(())
}
