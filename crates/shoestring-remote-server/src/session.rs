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
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use shoestring_ipc::RawInput;
use shoestring_remote::transport::Stream;
use shoestring_remote::{read_framed, write_framed, ClientMessage, ServerMessage};

use crate::holds::{Held, Holds};
use crate::viewers::Viewers;
use crate::wm;

/// How long a new connection has to send its first message (`Hello` or
/// `Graft`) before the session gives up on it, so a client that connects and
/// says nothing doesn't hold a session thread forever.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Shutdown handles + a one-shot flag so the first loop to finish can unblock
/// the other by closing both sockets.
struct Teardown {
    net: Stream,
    wm_capture: UnixStream,
    done: AtomicBool,
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
    let (wm_capture, graft_id) = match first {
        Ok(ClientMessage::Hello {
            width,
            height,
            scale,
        }) => {
            tracing::info!(%peer, width, height, scale, "client hello (serve mode)");
            let (cap, _leftover) =
                wm::open_capture_stream(output).context("opening WM capture stream for client")?;
            (cap, None)
        }
        Ok(ClientMessage::Graft { selector }) => {
            tracing::info!(%peer, %selector, "client graft request");
            let id = wm::resolve_window(&selector).context("resolving graft selector")?;
            let cap = wm::open_window_capture_stream(&id)
                .context("opening WM window-capture stream for client")?;
            (cap, Some(id))
        }
        Ok(other) => bail!("expected Hello or Graft first, got {other:?}"),
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

    let teardown = Arc::new(Teardown {
        net: net.try_clone().context("clone net for teardown")?,
        wm_capture: wm_capture
            .try_clone()
            .context("clone wm capture for teardown")?,
        done: AtomicBool::new(false),
    });

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
    // done (gate off, WM exit, or client gone).
    while let Ok(msg) = read_framed::<_, ServerMessage>(&mut wm_in) {
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
    // Read ends the loop on EOF / shutdown / malformed.
    while let Ok(msg) = read_framed::<_, ClientMessage>(&mut net_in) {
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
                // Key and button transitions pass only when they change what
                // the viewers hold as a whole (see holds.rs).
                if let Some((held, pressed)) = held_transition(event) {
                    if !holds.transition(&mut mine, held, pressed) {
                        continue;
                    }
                }
                // Graft mode targets the grafted window; serve mode drives the
                // global seat.
                let result = match &graft_id {
                    Some(id) => wm::inject_to_window(id, event),
                    None => wm::inject(event, true),
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
    release_held(holds.end(&mut mine), graft_id.is_none());
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
/// dropped mid-chord or mid-drag leaves nothing pressed. Serve-mode keys go
/// back through the bindings path, which also stops a keybind repeat the key
/// was driving. If the gate has closed these fail, harmlessly: the WM released
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
        } => Some(RawInput::Axis {
            horizontal,
            vertical,
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
                vertical: -2.0
            }),
            Some(RawInput::Axis {
                horizontal: 1.0,
                vertical: -2.0
            })
        );
    }

    #[test]
    fn non_input_messages_map_to_none() {
        assert_eq!(
            to_raw_input(&ClientMessage::Hello {
                width: 1,
                height: 2,
                scale: 1.0
            }),
            None
        );
        assert_eq!(to_raw_input(&ClientMessage::Bye), None);
    }
}
