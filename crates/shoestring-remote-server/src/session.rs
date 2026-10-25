//! One connected remote client: bridge the WM's capture stream to the network
//! and replay the client's input back into the WM.
//!
//! Two directions run concurrently:
//!
//! - **frames** (a spawned thread): read `ServerMessage` frames off the WM's
//!   capture-stream connection and forward them to the network client.
//! - **input** (this thread): read `ClientMessage`s from the network and inject
//!   them into the WM (`inject_input`).
//!
//! Either side ending tears down the other: each holds a shutdown handle for
//! *both* sockets, so when one loop exits it shuts both down, unblocking the
//! peer's blocking read. The WM dropping the capture subscription (e.g. the
//! remote gate flips off → capture gate off → subscribers torn down) shows up
//! as the frames thread reading a `Bye`/EOF, which then ends the session.

use std::io::{BufReader, BufWriter, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use shoestring_ipc::{RawInput, WheelSteps};
use shoestring_remote::transport::Stream;
use shoestring_remote::{
    incompatibility, read_framed, write_framed, ClientMessage, Error, PeerInfo, ServerMessage,
};

use crate::holds::{Held, Holds};
use crate::viewers::Viewers;
use crate::wm;

/// How long a new connection has to send its first message (`Hello` or
/// `Graft`) before the session gives up on it, so a client that connects and
/// says nothing doesn't hold a session thread forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Shutdown handles + a one-shot flag so the first loop to finish can unblock
/// the other by closing both sockets.
struct Teardown {
    net: Stream,
    wm_capture: UnixStream,
    done: AtomicBool,
}

/// The live sessions, so closing the gate can end those that don't end on
/// their own.
///
/// A healthy session ends by itself once the WM closes its capture stream,
/// passing the WM's `Bye` on to its viewer. One whose viewer has stopped
/// reading can't: its frames thread is blocked writing to the viewer, so it
/// never reads the end of the capture stream, and its input loop is blocked
/// reading a viewer with nothing to say. Shutting both sockets down unblocks
/// the two.
#[derive(Default)]
pub struct Sessions {
    live: Mutex<Vec<Weak<Teardown>>>,
}

impl Sessions {
    fn add(&self, teardown: &Arc<Teardown>) {
        let mut live = self.live.lock().unwrap();
        live.retain(|t| t.strong_count() > 0);
        live.push(Arc::downgrade(teardown));
    }

    fn any_live(&self) -> bool {
        self.live
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.strong_count() > 0)
    }

    /// Give the live sessions `grace` to end on their own, then end the rest;
    /// returns how many had to be ended.
    pub fn end_all(&self, grace: Duration) -> usize {
        let deadline = Instant::now() + grace;
        while self.any_live() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let live = std::mem::take(&mut *self.live.lock().unwrap());
        let mut ended = 0;
        for teardown in live.iter().filter_map(Weak::upgrade) {
            teardown.stop();
            ended += 1;
        }
        ended
    }
}

/// The network writer, shared between the frames thread and the input loop.
/// Both write whole `ServerMessage` frames; the mutex makes each write+flush
/// atomic so a clipboard reply can never interleave mid-frame with a capture
/// frame on the wire.
type SharedNet = Arc<Mutex<BufWriter<Stream>>>;

impl Teardown {
    fn stop(&self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            let _ = self.net.shutdown(Shutdown::Both);
            let _ = self.wm_capture.shutdown(Shutdown::Both);
        }
    }
}

/// Handle one accepted client connection start to finish. Blocks until the
/// session ends; bumps the shared viewer count for its lifetime. `peer` only
/// labels the logs; the listener has already decided the peer may connect.
pub fn serve(
    net: Stream,
    peer: String,
    output: Option<String>,
    viewers: Arc<Viewers>,
    holds: Arc<Holds>,
    sessions: Arc<Sessions>,
) -> Result<()> {
    // The client's first message discriminates the mode:
    // - `Hello` → **serve mode**: stream a whole output. Hello carries the
    //   client's output size/scale (advisory today; the WM serves its own size).
    // - `Graft { selector }` → **graft mode**: stream the single window matching
    //   `selector`, resolved against the WM's window list.
    let mut net_in = BufReader::new(net.try_clone().context("clone net for read")?);
    net.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("set handshake timeout")?;
    let first = read_framed::<_, ClientMessage>(&mut net_in);
    // Once the session is up, reads block for as long as the viewer is idle.
    net.set_read_timeout(None)
        .context("clear handshake timeout")?;
    let first = match first {
        Ok(msg) => msg,
        Err(shoestring_remote::Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            bail!("no Hello or Graft within {HANDSHAKE_TIMEOUT:?}")
        }
        Err(e) => bail!("reading first message: {e}"),
    };
    // Refuse a client on another protocol version with a reason it can show,
    // before opening anything on the WM.
    let client = match &first {
        ClientMessage::Hello { peer, .. } | ClientMessage::Graft { peer, .. } => peer.clone(),
        other => bail!("expected Hello or Graft first, got {other:?}"),
    };
    if let Some(reason) = incompatibility(&PeerInfo::ours(), &client) {
        tracing::warn!(%peer, %reason, "refusing client");
        let mut out = net.try_clone().context("clone net for refusal")?;
        let _ = write_framed(&mut out, &ServerMessage::Refused { reason }).and_then(|()| {
            out.flush()?;
            Ok(())
        });
        return Ok(());
    }
    tracing::info!(
        %peer,
        client = %client.software,
        protocol = client.protocol,
        legacy = client.is_legacy(),
        "client version"
    );
    let (wm_capture, graft_id) = match first {
        ClientMessage::Hello {
            width,
            height,
            scale,
            ..
        } => {
            tracing::info!(%peer, width, height, scale, "client hello (serve mode)");
            let (cap, _leftover) =
                wm::open_capture_stream(output).context("opening WM capture stream for client")?;
            (cap, None)
        }
        ClientMessage::Graft { selector, .. } => {
            tracing::info!(%peer, %selector, "client graft request");
            let id = wm::resolve_window(&selector).context("resolving graft selector")?;
            let cap = wm::open_window_capture_stream(&id)
                .context("opening WM window-capture stream for client")?;
            (cap, Some(id))
        }
        _ => unreachable!("checked above"),
    };

    let teardown = Arc::new(Teardown {
        net: net.try_clone().context("clone net for teardown")?,
        wm_capture: wm_capture
            .try_clone()
            .context("clone wm capture for teardown")?,
        done: AtomicBool::new(false),
    });
    sessions.add(&teardown);

    // This client now counts as a viewer (lights the WM's "being viewed" chip)
    // until the guard drops, which an early error below can't skip.
    let viewer = viewers.join();
    tracing::info!(%peer, "session started");

    // One shared writer for both directions (capture frames + clipboard replies).
    let net_out: SharedNet = Arc::new(Mutex::new(BufWriter::new(
        net.try_clone().context("clone net for writer")?,
    )));

    // Frames thread: WM capture stream → network.
    let frames = {
        let teardown = Arc::clone(&teardown);
        let net_out = Arc::clone(&net_out);
        std::thread::Builder::new()
            .name("frames".into())
            .spawn(move || forward_frames(wm_capture, net_out, &teardown))
            .context("spawn frames thread")?
    };

    // Input loop on this thread: network → WM injection + clipboard brokering.
    let input_result = forward_input(net_in, net_out, &teardown, graft_id, &holds);
    teardown.stop();
    let _ = frames.join();

    // Only now: the input loop has released what this viewer held, so anyone
    // waiting for the count to fall sees a seat with nothing left pressed.
    drop(viewer);
    tracing::info!(%peer, "session ended");
    input_result
}

/// Read `ServerMessage` frames from the WM and write them to the client until
/// either side closes. A `Bye` from the WM is forwarded, then the loop ends.
fn forward_frames(wm_capture: UnixStream, net_out: SharedNet, teardown: &Teardown) {
    let mut wm_in = BufReader::new(wm_capture);
    // Read ends the loop on EOF / shutdown / decode error: the capture stream is
    // done (gate off, WM exit, or client gone). A message this build doesn't
    // know (a newer WM) is skipped.
    loop {
        let mut msg = match read_framed::<_, ServerMessage>(&mut wm_in) {
            Ok(msg) => msg,
            Err(Error::UnknownMessage { tag, .. }) => {
                tracing::debug!(tag, "skipping a WM capture message this build doesn't know");
                continue;
            }
            Err(_) => break,
        };
        // The WM builds Ready; the client is talking to us, so it gets our
        // version information.
        if let ServerMessage::Ready { peer, .. } = &mut msg {
            *peer = PeerInfo::ours();
        }
        let is_bye = matches!(msg, ServerMessage::Bye);
        // Flush each frame so it isn't stuck in the BufWriter waiting on the
        // next one — an idle desktop may send nothing more for a while. The lock
        // is held for the whole write+flush so clipboard replies can't interleave.
        let ok = {
            let mut w = net_out.lock().unwrap();
            write_framed(&mut *w, &msg).is_ok() && w.flush().is_ok()
        };
        if !ok || is_bye {
            break;
        }
    }
    teardown.stop();
}

/// Read `ClientMessage`s and inject them into the WM until Bye/EOF/error,
/// then release whatever this viewer still held that no other viewer does.
fn forward_input(
    mut net_in: BufReader<Stream>,
    net_out: SharedNet,
    teardown: &Teardown,
    graft_id: Option<String>,
    holds: &Holds,
) -> Result<()> {
    let mut mine = holds.session();
    // Read ends the loop on EOF / shutdown / malformed. A message this build
    // doesn't know (a newer client) is skipped.
    loop {
        let msg = match read_framed::<_, ClientMessage>(&mut net_in) {
            Ok(msg) => msg,
            Err(Error::UnknownMessage { tag, .. }) => {
                tracing::debug!(tag, "skipping a client message this build doesn't know");
                continue;
            }
            Err(_) => break,
        };
        match msg {
            ClientMessage::Bye => break,
            // Viewer Pull: read our WM's selection and ship it back to this one
            // client (never a broadcast — the requester is the only recipient).
            ClientMessage::GetClipboard { primary } => {
                let reply = match wm::get_clipboard(primary) {
                    Ok((mime, data)) => ServerMessage::Clipboard {
                        primary,
                        mime: mime.unwrap_or_default(),
                        data,
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, "get_clipboard failed; replying empty");
                        ServerMessage::Clipboard {
                            primary,
                            mime: String::new(),
                            data: Vec::new(),
                        }
                    }
                };
                let mut w = net_out.lock().unwrap();
                if write_framed(&mut *w, &reply).is_err() || w.flush().is_err() {
                    break;
                }
            }
            // Viewer Push: install the buffer the client read on its machine.
            ClientMessage::SetClipboard {
                primary,
                mime,
                data,
            } => {
                if let Err(e) = wm::set_clipboard(primary, mime, data) {
                    tracing::debug!(error = %e, "set_clipboard failed");
                }
            }
            // Hello/Graft (re-sent) and anything non-input maps to None and is
            // skipped.
            other => {
                let Some(event) = to_raw_input(&other) else {
                    continue;
                };
                // Graft mode targets the grafted window; serve mode drives the
                // global seat.
                let inject = || match &graft_id {
                    Some(id) => wm::inject_to_window(id, event),
                    None => wm::inject(event, true),
                };
                // Key and button transitions pass only when they change what
                // the viewers hold as a whole, and are delivered in the order
                // they were counted (see holds.rs).
                let delivered = match held_transition(event) {
                    Some((held, pressed)) => {
                        holds.transition_then(&mut mine, held, pressed, inject)
                    }
                    None => Some(inject()),
                };
                let Some(result) = delivered else {
                    continue;
                };
                if let Err(e) = result {
                    // The gate likely flipped off mid-session; stop cleanly.
                    tracing::debug!(error = %e, "inject failed; ending session");
                    break;
                }
            }
        }
    }
    teardown.stop();
    holds.end_then(&mut mine, |released| {
        release_held(released, graft_id.is_none())
    });
    Ok(())
}

/// The key or button a raw event presses or releases, if any.
fn held_transition(event: RawInput) -> Option<(Held, bool)> {
    match event {
        RawInput::Key { keycode, pressed } => Some((Held::Key(keycode), pressed)),
        RawInput::Button { button, pressed } => Some((Held::Button(button), pressed)),
        RawInput::Motion { .. } | RawInput::Axis { .. } => None,
    }
}

/// Release what an ended session held and nobody else does, so a viewer that
/// dropped mid-chord or mid-drag leaves nothing pressed. Serve-mode keys and
/// buttons go back through the bindings path, which also stops a keybind
/// repeat the key was driving and ends a Super+drag the button started. If the gate has closed these fail, harmlessly: the WM released
/// everything injection held when it did.
fn release_held(released: Vec<Held>, serve: bool) {
    for held in released {
        let event = match held {
            Held::Key(keycode) => RawInput::Key {
                keycode,
                pressed: false,
            },
            Held::Button(button) => RawInput::Button {
                button,
                pressed: false,
            },
        };
        tracing::debug!(?held, "releasing what a closed session held");
        if let Err(e) = wm::inject(event, serve) {
            tracing::debug!(error = %e, "release after session end failed");
            return;
        }
    }
}

/// Translate a client input message into the WM's raw-injection event. Returns
/// `None` for non-input messages (`Hello`, `Bye`) which the caller handles.
fn to_raw_input(msg: &ClientMessage) -> Option<RawInput> {
    match *msg {
        ClientMessage::Key { keycode, pressed } => Some(RawInput::Key { keycode, pressed }),
        ClientMessage::PointerMotion { x, y } => Some(RawInput::Motion { x, y }),
        ClientMessage::PointerButton { button, pressed } => {
            Some(RawInput::Button { button, pressed })
        }
        ClientMessage::PointerAxis {
            horizontal,
            vertical,
            wheel,
        } => Some(RawInput::Axis {
            horizontal,
            vertical,
            wheel: wheel.map(|(horizontal, vertical)| WheelSteps {
                horizontal,
                vertical,
            }),
        }),
        ClientMessage::Hello { .. }
        | ClientMessage::Graft { .. }
        | ClientMessage::Bye
        | ClientMessage::GetClipboard { .. }
        | ClientMessage::SetClipboard { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn teardown() -> (Arc<Teardown>, UnixStream, UnixStream) {
        let (net, viewer) = UnixStream::pair().unwrap();
        let (wm_capture, wm) = UnixStream::pair().unwrap();
        let teardown = Arc::new(Teardown {
            net: Stream::Unix(net),
            wm_capture,
            done: AtomicBool::new(false),
        });
        (teardown, viewer, wm)
    }

    #[test]
    fn end_all_unblocks_a_write_to_a_viewer_that_stopped_reading() {
        let sessions = Sessions::default();
        let (live, _viewer, _wm) = teardown();
        sessions.add(&live);
        let (gone, _, _) = teardown();
        sessions.add(&gone);
        drop(gone);

        // Fill the socket to a viewer that never reads; the write blocks.
        let mut out = live.net.try_clone().unwrap();
        let writer = std::thread::spawn(move || {
            let chunk = [0u8; 64 * 1024];
            loop {
                if let Err(e) = out.write_all(&chunk) {
                    return e.kind();
                }
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!writer.is_finished(), "the write should be blocked");

        assert_eq!(
            sessions.end_all(Duration::from_millis(50)),
            1,
            "only the live session counts"
        );
        assert_eq!(writer.join().unwrap(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            sessions.end_all(Duration::ZERO),
            0,
            "end_all forgets what it ended"
        );
    }

    #[test]
    fn input_messages_map_to_raw_events() {
        assert_eq!(
            to_raw_input(&ClientMessage::Key {
                keycode: 30,
                pressed: true
            }),
            Some(RawInput::Key {
                keycode: 30,
                pressed: true
            })
        );
        assert_eq!(
            to_raw_input(&ClientMessage::PointerMotion { x: 4.0, y: 8.0 }),
            Some(RawInput::Motion { x: 4.0, y: 8.0 })
        );
        assert_eq!(
            to_raw_input(&ClientMessage::PointerButton {
                button: 0x110,
                pressed: false
            }),
            Some(RawInput::Button {
                button: 0x110,
                pressed: false
            })
        );
        assert_eq!(
            to_raw_input(&ClientMessage::PointerAxis {
                horizontal: 1.0,
                vertical: -2.0,
                wheel: None,
            }),
            Some(RawInput::Axis {
                horizontal: 1.0,
                vertical: -2.0,
                wheel: None,
            })
        );
        assert_eq!(
            to_raw_input(&ClientMessage::PointerAxis {
                horizontal: 0.0,
                vertical: 15.0,
                wheel: Some((0.0, 120.0)),
            }),
            Some(RawInput::Axis {
                horizontal: 0.0,
                vertical: 15.0,
                wheel: Some(WheelSteps {
                    horizontal: 0.0,
                    vertical: 120.0
                }),
            })
        );
    }

    #[test]
    fn non_input_messages_map_to_none() {
        assert_eq!(
            to_raw_input(&ClientMessage::Hello {
                width: 1,
                height: 2,
                scale: 1.0,
                peer: PeerInfo::ours(),
            }),
            None
        );
        assert_eq!(to_raw_input(&ClientMessage::Bye), None);
    }
}
