#![allow(irrefutable_let_patterns)]

mod backend;
mod binds;
mod grabs;
mod handlers;
mod input;
mod layout;
mod state;
mod workspace;

use anyhow::Result;
use clap::Parser;
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing_subscriber::EnvFilter;

use crate::state::ShoestringWm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum BackendKind {
    /// Nested winit window for dev work inside an existing X11/Wayland session.
    Winit,
    /// Native DRM/KMS + libinput + libseat backend for running from a TTY.
    Tty,
}

#[derive(Debug, Parser)]
#[command(name = "shoestring-wm", version, about)]
struct Cli {
    /// Path to a TOML config file. Defaults to $XDG_CONFIG_HOME/shoestring-wm/config.toml.
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Which backend to launch. Auto-detected from the environment if omitted:
    /// `tty` when WAYLAND_DISPLAY and DISPLAY are both unset, `winit` otherwise.
    #[arg(short, long, value_enum)]
    backend: Option<BackendKind>,

    /// Optional client command to spawn once the compositor is up.
    /// Defaults to `weston-terminal` if available.
    #[arg(short = 'C', long = "command")]
    command: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (config, config_path) =
        shoestring_config::load_or_default(cli.config.as_deref())?;
    match &config_path {
        Some(p) => tracing::info!(path = %p.display(), "loaded config"),
        None => tracing::info!("no config file found; using built-in defaults"),
    }

    let mut event_loop: EventLoop<'static, ShoestringWm> = EventLoop::try_new()?;
    let display: Display<ShoestringWm> = Display::new()?;

    let mut state = ShoestringWm::new(&mut event_loop, display, config, config_path);

    let backend = cli.backend.unwrap_or_else(|| {
        let in_session = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some();
        if in_session { BackendKind::Winit } else { BackendKind::Tty }
    });
    tracing::info!(?backend, "starting backend");

    match backend {
        BackendKind::Winit => {
            #[cfg(feature = "winit")]
            {
                backend::winit::init_winit(&mut event_loop, &mut state)?;
            }
            #[cfg(not(feature = "winit"))]
            anyhow::bail!("winit backend not compiled in");
        }
        BackendKind::Tty => {
            #[cfg(feature = "tty")]
            {
                backend::udev::init_udev(&mut event_loop, &mut state)?;
            }
            #[cfg(not(feature = "tty"))]
            anyhow::bail!("tty backend not compiled in");
        }
    }

    tracing::info!(
        socket = ?state.socket_name,
        version = env!("CARGO_PKG_VERSION"),
        "shoestring-wm ready",
    );

    // Point child processes at our socket.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);

    if let Some(cmd) = cli.command.as_deref().or(Some("weston-terminal")) {
        spawn_client(cmd);
    }

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

fn spawn_client(command: &str) {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else { return };
    let args: Vec<&str> = parts.collect();
    match std::process::Command::new(program).args(&args).spawn() {
        Ok(child) => tracing::info!(pid = child.id(), %command, "spawned client"),
        Err(e) => tracing::warn!(%command, error = %e, "failed to spawn client"),
    }
}
