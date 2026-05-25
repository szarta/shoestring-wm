//! shoestring-lock — screen locker for shoestring-wm.
//!
//! Drives `ext-session-lock-v1`: locks the session, creates one black
//! surface per output, reads a password through `wl_keyboard`, verifies
//! it via PAM, and unlocks on success.
//!
//! The MVP grabs the seat keyboard the moment the compositor focuses
//! one of our lock surfaces and shows three lines centered on each
//! output:
//!
//! ```text
//!     <hostname> · locked
//!     Password: ********
//!     (status / error)
//! ```
//!
//! No idle trigger, no multi-output text positioning, no fade — just
//! enough to keep an unattended workstation usable on a single-user
//! desktop. Spawned by shoestring-wm via the `Action::Lock` /
//! `shoestring-ctl lock` paths.

use std::{
    fs,
    io::Write,
    os::fd::AsFd,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use xkbcommon::xkb;

const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
];

#[derive(Debug, Parser)]
#[command(name = "shoestring-lock", version, about)]
struct Cli {
    /// PAM service name; defaults to `system-auth` then `login` depending
    /// on what's installed. Override to use a custom stack.
    #[arg(long)]
    pam_service: Option<String>,

    /// Optional path to a TTF font for the prompt. Falls back to
    /// $SHOESTRING_LOCK_FONT, then a small built-in candidate list.
    #[arg(long)]
    font: Option<PathBuf>,

    /// Pixel size for the prompt text.
    #[arg(long, default_value_t = 28.0)]
    font_size: f32,
}

struct OutputEntry {
    output: WlOutput,
    /// Stable registry name; kept for future logging.
    #[allow(dead_code)]
    name: u32,
    surface: Option<WlSurface>,
    /// Held to keep the lock surface alive — destroying it would
    /// unmap the only thing standing between the user and the
    /// unlocked desktop.
    #[allow(dead_code)]
    lock_surface: Option<ExtSessionLockSurfaceV1>,
    /// Last configure size from the compositor (logical pixels).
    size: Option<(u32, u32)>,
    /// True once we've drawn a buffer matching the latest configure.
    #[allow(dead_code)]
    drawn: bool,
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    /// Held so the manager stays bound for the lifetime of the locker.
    #[allow(dead_code)]
    lock_manager: ExtSessionLockManagerV1,
    lock: Option<ExtSessionLockV1>,
    /// `true` after the compositor sent `locked` on the lock object.
    locked: bool,
    /// `true` after we've sent `unlock_and_destroy`; the main loop will
    /// exit on the next iteration so the destroy actually flushes.
    finished: bool,

    outputs: Vec<OutputEntry>,

    /// Kept alive so the seat proxy stays bound; we read events through
    /// the registered Dispatch impl, not by polling this field.
    #[allow(dead_code)]
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    xkb_ctx: xkb::Context,
    xkb_keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,

    font: Font,
    font_size: f32,
    /// What the user has typed so far. Held in memory until verify;
    /// zeroed on each verify attempt.
    password: String,
    /// Single-line message under the prompt (errors, "verifying…").
    status: String,
    hostname: String,
    pam_service: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SHOESTRING_LOCK_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shoestring-lock: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let font = load_font(cli.font.as_deref())?;
    let pam_service = cli.pam_service.unwrap_or_else(pick_pam_service);
    let hostname = read_hostname();

    let conn = Connection::connect_to_env().context("connect WAYLAND_DISPLAY")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .context("wl_compositor missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let lock_manager: ExtSessionLockManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("ext_session_lock_manager_v1 missing — compositor doesn't support locking")?;
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok();

    // Outputs: bind whatever the registry currently knows about. New
    // outputs after lock are out of scope for the MVP.
    let outputs: Vec<OutputEntry> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| {
                let output =
                    globals
                        .registry()
                        .bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, ());
                OutputEntry {
                    output,
                    name: g.name,
                    surface: None,
                    lock_surface: None,
                    size: None,
                    drawn: false,
                }
            })
            .collect()
    });

    // Take the lock; compositor will respond with `locked` or `finished`.
    let lock = lock_manager.lock(&qh, ());

    let mut state = State {
        compositor,
        shm,
        lock_manager,
        lock: Some(lock),
        locked: false,
        finished: false,
        outputs,
        seat,
        keyboard: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_keymap: None,
        xkb_state: None,
        font,
        font_size: cli.font_size,
        password: String::new(),
        status: String::new(),
        hostname,
        pam_service,
    };

    // Create one lock surface per output up front. The compositor will
    // send a configure for each which we ack and draw into.
    for i in 0..state.outputs.len() {
        create_lock_surface(&mut state, &qh, i);
    }

    while !state.finished {
        queue
            .blocking_dispatch(&mut state)
            .context("wayland dispatch")?;
    }
    // Final roundtrip so the unlock_and_destroy actually reaches the
    // compositor before we exit and the connection is torn down.
    let _ = conn.roundtrip();
    Ok(())
}

fn create_lock_surface(state: &mut State, qh: &QueueHandle<State>, idx: usize) {
    let Some(lock) = state.lock.as_ref() else {
        return;
    };
    let surface = state.compositor.create_surface(qh, ());
    let entry = &mut state.outputs[idx];
    let lock_surface =
        lock.get_lock_surface(&surface, &entry.output, qh, OutputSurfaceData { idx });
    entry.surface = Some(surface);
    entry.lock_surface = Some(lock_surface);
}

// ---- Wayland event handlers ---------------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: <WlShm as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtSessionLockManagerV1,
        _: <ExtSessionLockManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: <ExtSessionLockV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => {
                tracing::info!("compositor confirmed lock");
                state.locked = true;
            }
            ext_session_lock_v1::Event::Finished => {
                // Compositor refused the lock or yanked it (e.g. another
                // locker beat us / privileges). Quit without trying to
                // unlock — calling unlock_and_destroy now would be a
                // protocol error.
                tracing::warn!("session lock finished by compositor — exiting");
                state.lock = None;
                state.finished = true;
            }
            _ => {}
        }
    }
}

/// Per-lock-surface user data so we can map configure events back to
/// the originating output entry.
struct OutputSurfaceData {
    idx: usize,
}

impl Dispatch<ExtSessionLockSurfaceV1, OutputSurfaceData> for State {
    fn event(
        state: &mut Self,
        lock_surface: &ExtSessionLockSurfaceV1,
        event: <ExtSessionLockSurfaceV1 as wayland_client::Proxy>::Event,
        data: &OutputSurfaceData,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            lock_surface.ack_configure(serial);
            if data.idx >= state.outputs.len() {
                return;
            }
            state.outputs[data.idx].size = Some((width, height));
            state.outputs[data.idx].drawn = false;
            if let Err(e) = redraw_output(state, qh, data.idx) {
                tracing::warn!(error = %e, "redraw failed");
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(format, WEnum::Value(KeymapFormat::XkbV1)) {
                    tracing::warn!("non-xkbv1 keymap format; ignoring");
                    return;
                }
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.xkb_ctx,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(km)) => {
                        state.xkb_state = Some(xkb::State::new(&km));
                        state.xkb_keymap = Some(km);
                    }
                    Ok(None) => tracing::warn!("keymap parse returned None"),
                    Err(e) => tracing::warn!(error = %e, "keymap parse failed"),
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xs) = state.xkb_state.as_mut() {
                    xs.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                    return;
                }
                // wayland keycode is evdev (offset 8 from xkb keycode).
                let xkb_code = key + 8;
                let mut dirty = false;
                let mut submit = false;
                if let Some(xs) = state.xkb_state.as_ref() {
                    let sym = xs.key_get_one_sym(xkb_code.into());
                    let utf8 = xs.key_get_utf8(xkb_code.into());
                    match sym {
                        xkb::Keysym::Return | xkb::Keysym::KP_Enter => submit = true,
                        xkb::Keysym::BackSpace => {
                            if state.password.pop().is_some() {
                                dirty = true;
                            }
                        }
                        xkb::Keysym::Escape => {
                            if !state.password.is_empty() {
                                state.password.clear();
                                dirty = true;
                            }
                        }
                        _ => {
                            // Skip control chars (BS/Tab/etc already
                            // handled above) and modifier-only events.
                            if !utf8.is_empty() && !utf8.chars().all(|c| c.is_control()) {
                                state.password.push_str(&utf8);
                                dirty = true;
                            }
                        }
                    }
                }
                if submit {
                    verify_password(state);
                    dirty = true;
                }
                if dirty {
                    redraw_all(state, qh);
                }
            }
            _ => {}
        }
    }
}

// ---- PAM ----------------------------------------------------------------

/// Minimal ConversationHandler: any echo-off prompt (PAM's password
/// prompt) replies with our cached password. Echo-on (e.g. username,
/// 2FA prompts) is left unimplemented in the MVP — Linux-PAM stacks
/// almost never echo-on for a desktop login.
struct PasswordConv {
    password: std::ffi::CString,
}

impl pam_client2::ConversationHandler for PasswordConv {
    fn prompt_echo_on(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Err(pam_client2::ErrorCode::CONV_ERR)
    }
    fn prompt_echo_off(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Ok(self.password.clone())
    }
    fn text_info(&mut self, _msg: &std::ffi::CStr) {}
    fn error_msg(&mut self, msg: &std::ffi::CStr) {
        tracing::debug!(?msg, "pam error_msg");
    }
}

fn verify_password(state: &mut State) {
    let user = match std::env::var("USER") {
        Ok(u) => u,
        Err(_) => {
            state.status = "no $USER set".into();
            return;
        }
    };
    let password_plain = std::mem::take(&mut state.password);
    state.status = "verifying…".into();
    tracing::debug!(user, "attempting PAM auth");

    let password = match std::ffi::CString::new(password_plain) {
        Ok(c) => c,
        Err(_) => {
            state.status = "password contains NUL".into();
            return;
        }
    };
    let conv = PasswordConv { password };
    let mut ctx = match pam_client2::Context::new(&state.pam_service, Some(&user), conv) {
        Ok(c) => c,
        Err(e) => {
            state.status = format!("pam init failed: {e}");
            tracing::warn!(service = %state.pam_service, error = %e, "pam init failed");
            return;
        }
    };
    match ctx.authenticate(pam_client2::Flag::NONE) {
        Ok(()) => match ctx.acct_mgmt(pam_client2::Flag::NONE) {
            Ok(()) => {
                tracing::info!("auth ok, unlocking");
                if let Some(lock) = state.lock.take() {
                    lock.unlock_and_destroy();
                }
                state.finished = true;
            }
            Err(e) => {
                tracing::info!(error = %e, "acct_mgmt rejected user");
                state.status = "account disabled".into();
            }
        },
        Err(e) => {
            tracing::info!(error = %e, "auth failed");
            state.status = "authentication failed".into();
        }
    }
}

fn pick_pam_service() -> String {
    for s in ["system-auth", "login", "passwd"] {
        if Path::new(&format!("/etc/pam.d/{s}")).exists() {
            return s.into();
        }
    }
    "login".into()
}

// ---- Drawing ------------------------------------------------------------

fn redraw_all(state: &mut State, qh: &QueueHandle<State>) {
    for i in 0..state.outputs.len() {
        if state.outputs[i].size.is_some() {
            if let Err(e) = redraw_output(state, qh, i) {
                tracing::warn!(idx = i, error = %e, "redraw failed");
            }
        }
    }
}

fn redraw_output(state: &mut State, qh: &QueueHandle<State>, idx: usize) -> Result<()> {
    let entry = &state.outputs[idx];
    let Some((w, h)) = entry.size else {
        return Ok(());
    };
    let Some(surface) = entry.surface.as_ref().cloned() else {
        return Ok(());
    };
    if w == 0 || h == 0 {
        return Ok(());
    }

    let stride = w as i32 * 4;
    let size = (stride as usize) * h as usize;
    let mut tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    // Pre-fill so a peek before our mmap stamp still shows black.
    {
        let zero_row = vec![0u8; stride as usize];
        for _ in 0..h {
            tmp.write_all(&zero_row)?;
        }
    }
    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };
    fill_black(&mut mmap, w, h);

    let title = format!("{} · locked", state.hostname);
    let masked: String = "•".repeat(state.password.chars().count());
    let prompt = format!("Password: {masked}");
    let status = &state.status;
    let font_px = state.font_size;
    let lines = [title.as_str(), prompt.as_str(), status.as_str()];

    // Vertical layout: center the block of (up to 3) lines on the output.
    let line_metrics =
        state
            .font
            .horizontal_line_metrics(font_px)
            .unwrap_or(fontdue::LineMetrics {
                ascent: font_px * 0.8,
                descent: -font_px * 0.2,
                line_gap: font_px * 0.2,
                new_line_size: font_px * 1.2,
            });
    let line_h = (line_metrics.ascent - line_metrics.descent + line_metrics.line_gap).max(font_px);
    let total_h = line_h * lines.len() as f32;
    let top = ((h as f32 - total_h) / 2.0).round() as i32;

    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let text_w = measure_text(&state.font, font_px, line);
        let x = ((w as i32 - text_w) / 2).max(0);
        let baseline_y = top + (i as f32 * line_h + line_metrics.ascent).round() as i32;
        draw_text(
            &mut mmap,
            w,
            h,
            &state.font,
            font_px,
            x,
            baseline_y,
            line,
            0xFFE0E0E0,
        );
    }

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    drop(mmap);

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    state.outputs[idx].drawn = true;
    buffer.destroy();
    Ok(())
}

fn fill_black(mmap: &mut MmapMut, w: u32, h: u32) {
    let stride = w as usize * 4;
    let row = vec![0u8; stride];
    // Argb8888 with alpha=0xFF so the compositor doesn't blend the lock.
    let mut row_opaque = row.clone();
    for px in row_opaque.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
    for y in 0..h as usize {
        let off = y * stride;
        mmap[off..off + stride].copy_from_slice(&row_opaque);
    }
}

fn measure_text(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as i32
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    font: &Font,
    size_px: f32,
    x_start: i32,
    baseline_y: i32,
    text: &str,
    color: u32,
) {
    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + metrics.xmin as f32).round() as i32;
        let gy = baseline_y - (metrics.ymin + metrics.height as i32);
        blit_alpha(
            mmap,
            dst_w,
            dst_h,
            gx,
            gy,
            metrics.width as u32,
            metrics.height as u32,
            &bitmap,
            color,
        );
        pen_x += metrics.advance_width;
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_alpha(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    dst_x: i32,
    dst_y: i32,
    src_w: u32,
    src_h: u32,
    src: &[u8],
    color: u32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    let [fb, fg, fr, _fa] = color.to_le_bytes();
    let stride = dst_w as usize * 4;
    for sy in 0..src_h as i32 {
        let dy = dst_y + sy;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w as i32 {
            let dx = dst_x + sx;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let coverage = src[(sy as u32 * src_w + sx as u32) as usize];
            if coverage == 0 {
                continue;
            }
            let off = dy as usize * stride + dx as usize * 4;
            let a = coverage as i32;
            for (i, fg_chan) in [fb, fg, fr].iter().enumerate() {
                let bg = mmap[off + i] as i32;
                let out = bg + (a * (*fg_chan as i32 - bg)) / 255;
                mmap[off + i] = out.clamp(0, 255) as u8;
            }
            mmap[off + 3] = 0xFF;
        }
    }
}

fn load_font(cfg_path: Option<&Path>) -> Result<Font> {
    if let Some(p) = cfg_path {
        let bytes = fs::read(p).with_context(|| format!("--font: {}", p.display()))?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("font parse: {e}"));
    }
    if let Some(p) = std::env::var_os("SHOESTRING_LOCK_FONT") {
        let bytes = fs::read(&p).context("$SHOESTRING_LOCK_FONT unreadable")?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("font parse: {e}"));
    }
    for path in FONT_CANDIDATES {
        let p = PathBuf::from(path);
        if p.exists() {
            let bytes = fs::read(&p)?;
            return Font::from_bytes(bytes, FontSettings::default())
                .map_err(|e| anyhow!("font parse: {e}"));
        }
    }
    anyhow::bail!("no font found in candidates: {FONT_CANDIDATES:?}")
}

fn read_hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "host".into())
}
