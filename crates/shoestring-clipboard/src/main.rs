//! `shoestring-clipboard` — observe and set the clipboard from out of focus.
//!
//! A tiny `wlr-data-control` client (one wayland fd, no async — mirrors the rest
//! of the shoestring suite) with two modes that compose over an ordinary unix
//! pipe to bridge the clipboard across machines:
//!
//! ```sh
//! # local copy → remote box
//! shoestring-clipboard watch | ssh dev-106 shoestring-clipboard set
//! # remote copy → local box (the other direction)
//! ssh dev-106 shoestring-clipboard watch | shoestring-clipboard set
//! ```
//!
//! - **watch** prints every new selection as one [`wire`] frame on stdout.
//! - **set** reads frames from stdin and takes ownership of the selection,
//!   serving the bytes to whoever pastes.
//!
//! `--primary` targets the primary (middle-click) selection instead of the
//! clipboard. It also works locally: `watch` feeds cliphist/copyq, `set` is a
//! `wl-copy`. Needs the WM's `zwlr_data_control_manager_v1` global (task 181).
//! `$SHOESTRING_CLIPBOARD_LOG` redirects the log to a file.

mod wire;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use wayland_client::backend::ObjectId;
use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// Text mime types we recognize, richest first. `watch` captures the first the
/// source advertises; `set` offers all of them (back-filling legacy aliases) so
/// the widest range of paste targets resolves.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

fn main() -> Result<()> {
    let cli = match parse_cli()? {
        CliAction::Run(c) => c,
        CliAction::Help => {
            print!("{}", help_text());
            return Ok(());
        }
        CliAction::Version => {
            println!("shoestring-clipboard {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };
    init_tracing();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        mode = ?cli.mode,
        primary = cli.primary,
        "shoestring-clipboard starting"
    );
    match cli.mode {
        Mode::Watch => run_watch(cli.primary),
        Mode::Set => run_set(cli.primary),
    }
}

// ---- CLI -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Mode {
    Watch,
    Set,
}

struct Cli {
    mode: Mode,
    /// Target the primary (middle-click) selection instead of the clipboard.
    primary: bool,
}

enum CliAction {
    Run(Cli),
    Help,
    Version,
}

fn parse_cli() -> Result<CliAction> {
    let mut mode: Option<Mode> = None;
    let mut primary = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--primary" | "-p" => primary = true,
            "watch" if mode.is_none() => mode = Some(Mode::Watch),
            "set" if mode.is_none() => mode = Some(Mode::Set),
            other => anyhow::bail!("unexpected argument: {other} (try --help)"),
        }
    }
    let mode = mode.context("expected a subcommand: `watch` or `set` (try --help)")?;
    Ok(CliAction::Run(Cli { mode, primary }))
}

fn help_text() -> String {
    format!(
        "shoestring-clipboard {}\n\
         Observe and set the shoestring-wm clipboard from out of focus.\n\n\
         USAGE:\n\
         \x20 shoestring-clipboard watch [--primary]   Print each new selection as a frame on stdout\n\
         \x20 shoestring-clipboard set   [--primary]   Take ownership of the selection from stdin frames\n\n\
         Bridge the clipboard across machines by piping the two over ssh:\n\
         \x20 shoestring-clipboard watch | ssh host shoestring-clipboard set\n\n\
         OPTIONS:\n\
         \x20 -p, --primary   Target the primary (middle-click) selection instead of the clipboard.\n\
         \x20 -h, --help      Print this help.\n\
         \x20 -V, --version   Print the version.\n\n\
         ENV:\n\
         \x20 SHOESTRING_CLIPBOARD_LOG   Redirect the log to this file.\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = match std::env::var_os("SHOESTRING_CLIPBOARD_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_CLIPBOARD_LOG path");
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_filter(env)
                .boxed()
        }
        // Default to stderr: stdout is the data channel in `watch` mode.
        None => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(env)
            .boxed(),
    };
    tracing_subscriber::registry().with(fmt_layer).init();
}

// ---- shared helpers ------------------------------------------------------

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn is_wouldblock(e: &wayland_client::backend::WaylandError) -> bool {
    matches!(e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
}

/// A CLOEXEC pipe; returns `(read, write)`.
fn make_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Pick the richest text mime the source advertises (case-insensitive), or the
/// first `text/*` if none of the well-known names match. `None` = no text on
/// offer (we skip non-text selections in v1).
fn pick_text_mime(mimes: &[String]) -> Option<String> {
    for pref in TEXT_MIMES {
        if let Some(m) = mimes.iter().find(|m| m.eq_ignore_ascii_case(pref)) {
            return Some(m.clone());
        }
    }
    mimes
        .iter()
        .find(|m| m.to_ascii_lowercase().starts_with("text/"))
        .cloned()
}

/// The mimes `set` should advertise for an incoming entry: the captured mime
/// plus every standard text alias (so any paste target resolves), all backed by
/// the same bytes.
fn offer_mimes(mime: &str) -> Vec<String> {
    let mut v = vec![mime.to_string()];
    let is_text = mime.to_ascii_lowercase().starts_with("text/")
        || TEXT_MIMES.iter().any(|t| t.eq_ignore_ascii_case(mime));
    if is_text {
        for extra in TEXT_MIMES {
            if !v.iter().any(|m| m.eq_ignore_ascii_case(extra)) {
                v.push((*extra).to_string());
            }
        }
    }
    v
}

// ---- watch ---------------------------------------------------------------

#[derive(Default)]
struct WatchState {
    primary: bool,
    /// Mimes advertised per live offer object, accumulated from `offer` events
    /// until the matching `selection` event names that offer as current.
    offers: HashMap<ObjectId, Vec<String>>,
    /// The offer + chosen mime to read once the event burst settles.
    pending: Option<(ZwlrDataControlOfferV1, String)>,
    /// Last bytes we emitted; identical re-offers are squelched (also breaks the
    /// echo when a `set` on the far end takes the selection back).
    last_emitted: Option<Vec<u8>>,
}

impl WatchState {
    fn on_selection(&mut self, id: Option<ZwlrDataControlOfferV1>) {
        match id {
            Some(offer) => {
                let mimes = self.offers.remove(&offer.id()).unwrap_or_default();
                // Any other lingering offers are stale now.
                self.offers.clear();
                match pick_text_mime(&mimes) {
                    Some(mime) => self.pending = Some((offer, mime)),
                    None => {
                        tracing::debug!(?mimes, "selection has no text mime; skipping");
                        offer.destroy();
                    }
                }
            }
            // Selection cleared: nothing to emit (we don't push empties).
            None => {
                self.offers.clear();
            }
        }
    }
}

fn run_watch(primary: bool) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland display")?;
    let (globals, mut queue) = registry_queue_init::<WatchState>(&conn)?;
    let qh = queue.handle();
    let min = if primary { 2 } else { 1 };
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, min..=2, ())
        .context("compositor has no zwlr_data_control_manager_v1")?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .context("compositor has no wl_seat")?;
    let _device = manager.get_data_device(&seat, &qh, ());

    let mut state = WatchState {
        primary,
        ..Default::default()
    };
    let stdout = std::io::stdout();
    tracing::info!("watching selection; emitting frames on stdout");
    loop {
        queue.blocking_dispatch(&mut state)?;
        if let Some((offer, mime)) = state.pending.take() {
            match read_offer(&conn, &mut queue, &offer, &mime) {
                Ok(data) => {
                    offer.destroy();
                    if state.last_emitted.as_deref() == Some(data.as_slice()) {
                        tracing::debug!("selection unchanged; not re-emitting");
                        continue;
                    }
                    let mut frame = Vec::new();
                    wire::encode_frame(&mut frame, &mime, &data);
                    let mut lock = stdout.lock();
                    if lock.write_all(&frame).and_then(|_| lock.flush()).is_err() {
                        tracing::info!("stdout closed; exiting");
                        break;
                    }
                    tracing::info!(%mime, bytes = data.len(), "emitted selection");
                    state.last_emitted = Some(data);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read selection; skipping");
                    offer.destroy();
                }
            }
        }
    }
    Ok(())
}

/// Drain a selection offer into bytes: hand the compositor the write end of a
/// pipe, flush the request, then block-read our end until the source closes it.
fn read_offer(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<WatchState>,
    offer: &ZwlrDataControlOfferV1,
    mime: &str,
) -> Result<Vec<u8>> {
    let (rx, tx) = make_pipe().context("pipe for selection transfer")?;
    offer.receive(mime.to_string(), tx.as_fd());
    // Push the receive request to the compositor, then drop our write end so we
    // observe EOF once the source has written everything.
    queue.flush()?;
    conn.flush()?;
    drop(tx);
    let mut file = std::fs::File::from(rx);
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            // A new offer is introduced; its mimes arrive as `offer` events.
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id } if !state.primary => state.on_selection(id),
            Event::PrimarySelection { id } if state.primary => state.on_selection(id),
            _ => {}
        }
    }

    event_created_child!(WatchState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WatchState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ---- set -----------------------------------------------------------------

struct SourceData {
    data: Vec<u8>,
    mimes: Vec<String>,
}

#[derive(Default)]
struct SetState {
    primary: bool,
    /// Bytes + advertised mimes per source we still own.
    serving: HashMap<ObjectId, SourceData>,
    /// The content currently set; an incoming frame equal to it is skipped so a
    /// bidirectional `watch | set` pair settles instead of echoing forever.
    last_set: Option<Vec<u8>>,
    /// stdin still feeding us frames.
    stdin_open: bool,
    /// Time to leave the loop.
    exit: bool,
}

fn run_set(primary: bool) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland display")?;
    let (globals, mut queue) = registry_queue_init::<SetState>(&conn)?;
    let qh = queue.handle();
    let min = if primary { 2 } else { 1 };
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, min..=2, ())
        .context("compositor has no zwlr_data_control_manager_v1 (--primary needs version 2)")?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .context("compositor has no wl_seat")?;
    let device = manager.get_data_device(&seat, &qh, ());

    let stdin_fd = libc::STDIN_FILENO;
    set_nonblocking(stdin_fd).context("set stdin non-blocking")?;
    let mut state = SetState {
        primary,
        stdin_open: true,
        ..Default::default()
    };
    let mut inbuf: Vec<u8> = Vec::new();
    tracing::info!("serving selection from stdin frames");

    while !state.exit {
        conn.flush()?;
        let read_guard = conn.prepare_read();
        let wl_fd = read_guard.as_ref().map(|g| g.connection_fd().as_raw_fd());

        let mut pfds: [libc::pollfd; 2] = unsafe { std::mem::zeroed() };
        let mut nfds = 0usize;
        let mut wl_idx = None;
        if let Some(fd) = wl_fd {
            pfds[nfds] = pollfd(fd, libc::POLLIN);
            wl_idx = Some(nfds);
            nfds += 1;
        }
        let mut stdin_idx = None;
        if state.stdin_open {
            pfds[nfds] = pollfd(stdin_fd, libc::POLLIN);
            stdin_idx = Some(nfds);
            nfds += 1;
        }

        let n = unsafe { libc::poll(pfds.as_mut_ptr(), nfds as libc::nfds_t, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if let Some(g) = read_guard {
                drop(g);
            }
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }

        // Wayland: dispatch device/source events (send, cancelled).
        if let (Some(idx), Some(guard)) = (wl_idx, read_guard) {
            if pfds[idx].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    if !is_wouldblock(&e) {
                        tracing::warn!(error = ?e, "wayland read failed");
                    }
                }
            } else {
                drop(guard);
            }
        }

        // stdin: buffer available bytes, then take ownership for each full frame.
        if let Some(idx) = stdin_idx {
            if pfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                match read_available(stdin_fd, &mut inbuf)? {
                    ReadOutcome::Eof => {
                        state.stdin_open = false;
                        tracing::info!("stdin closed");
                        // Nothing left to own → done.
                        if state.serving.is_empty() {
                            state.exit = true;
                        }
                    }
                    ReadOutcome::More => {
                        while let Some((mime, data)) = wire::decode_frame(&mut inbuf)? {
                            apply_set(&manager, &device, &qh, &mut state, mime, data);
                        }
                        conn.flush()?;
                    }
                }
            }
        }

        queue.dispatch_pending(&mut state)?;
    }
    Ok(())
}

/// Take ownership of the selection for one incoming `(mime, data)` entry,
/// unless it equals what we already serve (echo-squelch).
fn apply_set(
    manager: &ZwlrDataControlManagerV1,
    device: &ZwlrDataControlDeviceV1,
    qh: &QueueHandle<SetState>,
    state: &mut SetState,
    mime: String,
    data: Vec<u8>,
) {
    if state.last_set.as_deref() == Some(data.as_slice()) {
        tracing::debug!("incoming selection matches current; not re-taking");
        return;
    }
    let mimes = offer_mimes(&mime);
    let source = manager.create_data_source(qh, ());
    for m in &mimes {
        source.offer(m.clone());
    }
    if state.primary {
        device.set_primary_selection(Some(&source));
    } else {
        device.set_selection(Some(&source));
    }
    tracing::info!(%mime, bytes = data.len(), offered = mimes.len(), "took selection");
    state.serving.insert(
        source.id(),
        SourceData {
            data: data.clone(),
            mimes,
        },
    );
    state.last_set = Some(data);
}

enum ReadOutcome {
    More,
    Eof,
}

/// Drain everything currently readable on `fd` into `buf`. `More` = appended
/// some bytes (or hit `WouldBlock`); `Eof` = the writer closed.
fn read_available(fd: RawFd, buf: &mut Vec<u8>) -> Result<ReadOutcome> {
    let mut tmp = [0u8; 8192];
    let mut got_any = false;
    loop {
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n > 0 {
            buf.extend_from_slice(&tmp[..n as usize]);
            got_any = true;
            continue;
        }
        if n == 0 {
            return Ok(if got_any {
                ReadOutcome::More
            } else {
                ReadOutcome::Eof
            });
        }
        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::WouldBlock => return Ok(ReadOutcome::More),
            std::io::ErrorKind::Interrupted => continue,
            _ => return Err(err.into()),
        }
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for SetState {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event;
        match event {
            // A client pasted: write our bytes to the fd, then close it (drop).
            Event::Send { mime_type, fd } => {
                if let Some(sd) = state.serving.get(&source.id()) {
                    if sd.mimes.iter().any(|m| m.eq_ignore_ascii_case(&mime_type)) {
                        let mut file = std::fs::File::from(fd);
                        if let Err(e) = file.write_all(&sd.data) {
                            tracing::warn!(error = %e, "failed writing selection to paster");
                        }
                    }
                    // Non-matching mime: drop `fd` (closes it, empty paste).
                }
            }
            // Another client (or our own next frame) replaced us.
            Event::Cancelled => {
                source.destroy();
                state.serving.remove(&source.id());
                tracing::debug!("source cancelled");
                if !state.stdin_open && state.serving.is_empty() {
                    state.exit = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for SetState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        _: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `set` ignores incoming offers/selections; it only publishes.
    }

    // The device still creates offer objects for the current selection even
    // though we never read them; give them inert user data so dispatch doesn't
    // panic on the data_offer event.
    event_created_child!(SetState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for SetState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for SetState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for SetState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for SetState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_richest_text_mime() {
        let mimes = vec![
            "TEXT".to_string(),
            "text/plain".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            pick_text_mime(&mimes).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn falls_back_to_any_text_subtype() {
        let mimes = vec!["text/html".to_string(), "image/png".to_string()];
        assert_eq!(pick_text_mime(&mimes).as_deref(), Some("text/html"));
    }

    #[test]
    fn skips_when_no_text_offered() {
        let mimes = vec!["image/png".to_string(), "x-special/gnome".to_string()];
        assert_eq!(pick_text_mime(&mimes), None);
    }

    #[test]
    fn set_backfills_text_aliases_without_duplicates() {
        let offered = offer_mimes("text/plain;charset=utf-8");
        assert_eq!(offered[0], "text/plain;charset=utf-8");
        for t in TEXT_MIMES {
            assert!(offered.iter().any(|m| m == t), "missing alias {t}");
        }
        // No dupes of the captured mime.
        assert_eq!(
            offered
                .iter()
                .filter(|m| *m == "text/plain;charset=utf-8")
                .count(),
            1
        );
    }

    #[test]
    fn non_text_mime_is_offered_verbatim_only() {
        let offered = offer_mimes("image/png");
        assert_eq!(offered, vec!["image/png".to_string()]);
    }
}
