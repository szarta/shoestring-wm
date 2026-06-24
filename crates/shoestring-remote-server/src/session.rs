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
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use shoestring_ipc::RawInput;
use shoestring_remote::{read_framed, write_framed, ClientMessage, ServerMessage};

use crate::viewers::Viewers;
use crate::wm;

/// Shutdown handles + a one-shot flag so the first loop to finish can unblock
/// the other by closing both sockets.
struct Teardown {
    net: TcpStream,
    wm_capture: UnixStream,
    done: AtomicBool,
}

impl Teardown {
    fn stop(&self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            let _ = self.net.shutdown(Shutdown::Both);
            let _ = self.wm_capture.shutdown(Shutdown::Both);
        }
    }
}

/// Handle one accepted client connection start to finish. Blocks until the
/// session ends; bumps the shared viewer count for its lifetime.
pub fn serve(net: TcpStream, output: Option<String>, viewers: Arc<Viewers>) -> Result<()> {
    let peer = net
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    net.set_nodelay(true).ok();

    // The client's first message must be Hello — its output size/scale. We
    // serve the WM's actual output size (echoed back via the WM's Ready), so
    // for now Hello is advisory; exact-size negotiation (resize the virtual
    // output to match) is a follow-up.
    let mut net_in = BufReader::new(net.try_clone().context("clone net for read")?);
    match read_framed::<_, ClientMessage>(&mut net_in) {
        Ok(ClientMessage::Hello {
            width,
            height,
            scale,
        }) => {
            tracing::info!(%peer, width, height, scale, "client hello");
        }
        Ok(other) => bail!("expected Hello first, got {other:?}"),
        Err(e) => bail!("reading Hello: {e}"),
    }

    // Open the WM capture stream now that the client is committed.
    let (wm_capture, _leftover) =
        wm::open_capture_stream(output).context("opening WM capture stream for client")?;

    let teardown = Arc::new(Teardown {
        net: net.try_clone().context("clone net for teardown")?,
        wm_capture: wm_capture
            .try_clone()
            .context("clone wm capture for teardown")?,
        done: AtomicBool::new(false),
    });

    // This client now counts as a viewer (lights the WM's "being viewed" chip).
    viewers.add();
    tracing::info!(%peer, "session started");

    // Frames thread: WM capture stream → network.
    let frames = {
        let teardown = Arc::clone(&teardown);
        let net_out = net.try_clone().context("clone net for frames")?;
        std::thread::Builder::new()
            .name("frames".into())
            .spawn(move || forward_frames(wm_capture, net_out, &teardown))
            .context("spawn frames thread")?
    };

    // Input loop on this thread: network → WM injection.
    let input_result = forward_input(net_in, &teardown);
    teardown.stop();
    let _ = frames.join();

    viewers.remove();
    tracing::info!(%peer, "session ended");
    input_result
}

/// Read `ServerMessage` frames from the WM and write them to the client until
/// either side closes. A `Bye` from the WM is forwarded, then the loop ends.
fn forward_frames(wm_capture: UnixStream, net_out: TcpStream, teardown: &Teardown) {
    let mut wm_in = BufReader::new(wm_capture);
    let mut net_out = BufWriter::new(net_out);
    // Read ends the loop on EOF / shutdown / decode error: the capture stream is
    // done (gate off, WM exit, or client gone).
    while let Ok(msg) = read_framed::<_, ServerMessage>(&mut wm_in) {
        let is_bye = matches!(msg, ServerMessage::Bye);
        // Flush each frame so it isn't stuck in the BufWriter waiting on the
        // next one — an idle desktop may send nothing more for a while.
        if write_framed(&mut net_out, &msg).is_err() || net_out.flush().is_err() {
            break;
        }
        if is_bye {
            break;
        }
    }
    teardown.stop();
}

/// Read `ClientMessage`s and inject them into the WM until Bye/EOF/error.
fn forward_input(mut net_in: BufReader<TcpStream>, teardown: &Teardown) -> Result<()> {
    // Read ends the loop on EOF / shutdown / malformed.
    while let Ok(msg) = read_framed::<_, ClientMessage>(&mut net_in) {
        if matches!(msg, ClientMessage::Bye) {
            break;
        }
        // Hello (re-sent) and anything non-input maps to None and is skipped.
        let Some(event) = to_raw_input(&msg) else {
            continue;
        };
        if let Err(e) = wm::inject(event) {
            // The gate likely flipped off mid-session; stop cleanly.
            tracing::debug!(error = %e, "inject failed; ending session");
            break;
        }
    }
    teardown.stop();
    Ok(())
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
        ClientMessage::Hello { .. } | ClientMessage::Bye => None,
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
