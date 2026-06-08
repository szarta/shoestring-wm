#![allow(irrefutable_let_patterns)]

mod backend;
mod binds;
mod config_watcher;
mod confirm;
mod cursor;
mod drawing;
mod grabs;
mod handlers;
mod inject;
mod input;
mod ipc;
mod layout;
mod output_management;
mod picker;
mod remote_command;
mod remote_screenshot;
mod screencopy;
mod state;
mod window_ext;
mod window_rules;
mod workspace;
mod xwayland;

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

    /// Write the bundled default config to the user's config path (or to
    /// `--config PATH` if given) and exit. Refuses to overwrite an existing
    /// file unless `--force` is also passed.
    #[arg(long)]
    write_default_config: bool,

    /// Allow `--write-default-config` to overwrite an existing file.
    #[arg(long)]
    force: bool,

    /// Force the runtime automation gate ON at startup, overriding
    /// `general.automation_enabled` from the config. Off by default so
    /// remote-automation IPC methods (key/text/click injection, future
    /// remote screenshot + command exec) refuse to fire. The flag only
    /// forces ON; `set_automation` IPC can still flip the gate at
    /// runtime.
    #[arg(long)]
    enable_automation: bool,
}

/// Initialise tracing. Writes to stderr by default; if `SHOESTRING_WM_LOG`
/// is set, appends to that file instead (with ANSI escapes disabled so the
/// file stays grep-friendly). The file case is the practical way to debug
/// the TTY backend, where stderr scrolls past on the console.
fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match std::env::var_os("SHOESTRING_WM_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_WM_LOG path");
            tracing_subscriber::fmt()
                .with_env_filter(env)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(env).init();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing();
    install_sigchld_autoreap();

    if cli.write_default_config {
        return write_default_config(cli.config.as_deref(), cli.force);
    }

    let (config, config_path) = shoestring_config::load_or_default(cli.config.as_deref())?;
    match &config_path {
        Some(p) => tracing::info!(path = %p.display(), "loaded config"),
        None => tracing::info!("no config file found; using built-in defaults"),
    }

    let mut event_loop: EventLoop<'static, ShoestringWm> = EventLoop::try_new()?;
    let display: Display<ShoestringWm> = Display::new()?;

    let mut state = ShoestringWm::new(&mut event_loop, display, config, config_path);

    if cli.enable_automation && !state.automation_enabled {
        state.automation_enabled = true;
        tracing::info!("automation gate forced on by --enable-automation");
    }

    let backend = cli.backend.unwrap_or_else(|| {
        let in_session =
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        if in_session {
            BackendKind::Winit
        } else {
            BackendKind::Tty
        }
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

    // Point child processes at our socket.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);

    // Push WAYLAND_DISPLAY into the systemd user manager so D-Bus-activated
    // services (xdg-desktop-portal, notification daemons, etc.) see it. The
    // session wrapper already imported the static variables (XDG_CURRENT_DESKTOP,
    // XDG_SESSION_TYPE, PATH); this covers the dynamic one set just above.
    // DISPLAY is imported separately in xwayland.rs once XWayland is ready.
    import_systemd_env(&["WAYLAND_DISPLAY"]);

    // IPC socket goes up after WAYLAND_DISPLAY is exported so
    // default_socket_path() can resolve it.
    state.start_ipc();

    // XWayland goes up before autostart so X11 apps in the autostart
    // list (and any first client launched from a terminal that inherits
    // our env) find $DISPLAY when they reach for it.
    state.start_xwayland();

    // Config hot-reload watcher. No-op when launched without a config file.
    state.start_config_watcher();

    tracing::info!(
        socket = ?state.socket_name,
        version = env!("CARGO_PKG_VERSION"),
        "shoestring-wm ready",
    );

    for entry in &state.config.general.autostart {
        spawn_client(entry);
    }

    if let Some(cmd) = cli.command.as_deref() {
        spawn_client(cmd);
    }

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

fn write_default_config(path: Option<&std::path::Path>, force: bool) -> Result<()> {
    let target = path
        .map(std::path::PathBuf::from)
        .or_else(shoestring_config::default_config_path)
        .ok_or_else(|| {
            anyhow::anyhow!("no config path: pass --config or set $HOME/$XDG_CONFIG_HOME")
        })?;

    if target.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, shoestring_config::default_config_toml())?;
    println!("wrote default config to {}", target.display());
    Ok(())
}

/// Reap exited helper children (autostart, bar, menus, ...) so they never
/// become zombies. We spawn these with `Command::spawn` and drop the
/// `Child`; without reaping they sit as `<defunct>` in `ps` forever.
///
/// Implementation: a SIGCHLD handler that calls `waitpid(-1, WNOHANG)` in a
/// loop. This reaps any child that has already exited without blocking, and
/// crucially does NOT break `waitpid(specific_pid, 0)` calls in smithay's
/// XWayland supervisor — those calls still see the child while it is alive.
///
/// The previous SA_NOCLDWAIT approach caused POSIX-specified breakage:
/// with SA_NOCLDWAIT set, *any* waitpid() call returns ECHILD immediately,
/// which panicked inside smithay when it waited on the XWayland process.
fn install_sigchld_autoreap() {
    // SAFETY: the handler only calls waitpid and is async-signal-safe.
    // We install it once before children are spawned.
    unsafe extern "C" fn sigchld_handler(_: libc::c_int) {
        loop {
            let ret = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if ret <= 0 {
                break;
            }
        }
    }

    use std::mem::MaybeUninit;
    unsafe {
        let mut sa: libc::sigaction = MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = sigchld_handler as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut()) != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(error = %err, "install_sigchld_autoreap failed; expect <defunct> in ps");
        }
    }
}

fn import_systemd_env(vars: &[&str]) {
    match std::process::Command::new("systemctl")
        .arg("--user")
        .arg("import-environment")
        .args(vars)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(status = %s, "systemctl import-environment exited non-zero"),
        Err(e) => tracing::warn!(error = %e, "systemctl import-environment failed"),
    }
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
