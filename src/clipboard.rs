//! WM-side clipboard broker for the cross-machine remote-desktop feature.
//!
//! The remote stack copies/pastes between machines by reading one WM's
//! selection and writing it into another's, *gated by remote sharing* — an
//! explicit `Super+Shift+C/V` action, never a silent always-on stream. This
//! module is the native plumbing both directions ride:
//!
//! - [`ShoestringWm::handle_get_clipboard`] answers [`Request::GetClipboard`]:
//!   pick the best text mime the current owner offers, read the bytes *out of
//!   band* through a pipe, and reply with [`Response::Clipboard`]. The reply is
//!   **deferred** — the owning client writes asynchronously, so we hold the IPC
//!   connection open and finalize from a calloop pipe source (mirroring
//!   `remote_screenshot.rs`).
//! - [`ShoestringWm::handle_set_clipboard`] answers [`Request::SetClipboard`]:
//!   become the compositor-side selection owner under the given mime (plus the
//!   standard text aliases), caching the bytes to serve to anything that pastes.
//!
//! The two [`SelectionHandler`] hooks ([`note_new_selection`] /
//! [`serve_clipboard_selection`]) keep our view of the live selection coherent:
//! we track the owner's offered mimes (so `GetClipboard` knows what to ask for)
//! and serve our own cache when a client pastes a selection we set.
//!
//! This is deliberately **separate** from the opt-in `wlr-data-control` global:
//! that global lets any bound client observe every copy (a privacy surface, so
//! default-off), whereas this broker only ever moves the selection on an
//! explicit, remote-sharing-gated request.
//!
//! [`Request::GetClipboard`]: shoestring_ipc::Request::GetClipboard
//! [`Request::SetClipboard`]: shoestring_ipc::Request::SetClipboard
//! [`Response::Clipboard`]: shoestring_ipc::Response::Clipboard
//! [`SelectionHandler`]: smithay::wayland::selection::SelectionHandler
//! [`note_new_selection`]: ShoestringWm::note_new_selection
//! [`serve_clipboard_selection`]: ShoestringWm::serve_clipboard_selection

use std::{
    cell::RefCell,
    collections::HashMap,
    io::{Read, Write},
    os::fd::{FromRawFd, OwnedFd},
    rc::Rc,
};

use shoestring_ipc::Response;
use smithay::reexports::calloop::{
    generic::Generic, Interest, Mode, PostAction, RegistrationToken,
};
use smithay::wayland::selection::{
    data_device::{request_data_device_client_selection, set_data_device_selection},
    primary_selection::{request_primary_client_selection, set_primary_selection},
    SelectionTarget,
};

use crate::ipc::{write_response, Client, ClientId};
use crate::state::ShoestringWm;

/// Canonical UTF-8 text mimes, in descending preference order. We *request*
/// the first of these the owner offers (so `GetClipboard` reads text the way
/// vim/alacritty/firefox each expect it), and *offer* all of them when we set
/// a text selection so any paster finds the atom it asks for.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Live state for the clipboard broker, owned by [`ShoestringWm`].
#[derive(Default)]
pub struct ClipboardBroker {
    /// Bytes we serve when a client pastes a selection *we* set (mime → bytes).
    /// Populated only by [`ShoestringWm::handle_set_clipboard`]; cleared when a
    /// client takes the selection back (see [`ShoestringWm::note_new_selection`]).
    clipboard_cache: HashMap<String, Vec<u8>>,
    /// Same, for the primary selection.
    primary_cache: HashMap<String, Vec<u8>>,
    /// Mime types the *current* clipboard owner offers (client- or
    /// compositor-set). Drives mime selection in `handle_get_clipboard`.
    clipboard_mimes: Vec<String>,
    /// Same, for the primary selection.
    primary_mimes: Vec<String>,
    /// In-flight `GetClipboard` reads awaiting their pipe draining, keyed by id.
    pending_reads: HashMap<u64, ClipboardRead>,
    next_read_id: u64,
}

/// One deferred `GetClipboard` read: the owner is writing the selection bytes
/// into a pipe we registered with calloop; on EOF we reply and drop the conn.
struct ClipboardRead {
    client: Rc<RefCell<Client>>,
    client_id: ClientId,
    mime: String,
    buf: Vec<u8>,
    token: Option<RegistrationToken>,
}

impl ShoestringWm {
    /// Answer [`Request::GetClipboard`]. Returns `true` when the reply was sent
    /// inline (empty selection or a cached compositor selection) and the IPC
    /// dispatch may finish the turn, or `false` when the read is deferred (the
    /// connection is held open and finalized from the pipe source).
    ///
    /// [`Request::GetClipboard`]: shoestring_ipc::Request::GetClipboard
    pub(crate) fn handle_get_clipboard(
        &mut self,
        client_id: ClientId,
        client: &Rc<RefCell<Client>>,
        primary: bool,
    ) -> bool {
        let mimes = if primary {
            &self.clipboard.primary_mimes
        } else {
            &self.clipboard.clipboard_mimes
        };
        let Some(mime) = pick_text_mime(mimes) else {
            // No text the WM can read (empty selection, or owner offers only
            // non-text mimes we don't bridge): reply empty.
            let _ = write_response(
                client,
                &Response::Clipboard {
                    mime: None,
                    data: Vec::new(),
                },
            );
            return true;
        };

        // If we own the selection (compositor-set), serve the cache directly —
        // there is no owning client to pipe from.
        let cache = if primary {
            &self.clipboard.primary_cache
        } else {
            &self.clipboard.clipboard_cache
        };
        if let Some(data) = cache.get(&mime) {
            let _ = write_response(
                client,
                &Response::Clipboard {
                    mime: Some(mime),
                    data: data.clone(),
                },
            );
            return true;
        }

        // Client-owned selection: hand the owner the write end of a pipe and
        // read the bytes asynchronously so we never block the compositor.
        let (read_file, write_fd) = match make_pipe() {
            Ok(p) => p,
            Err(e) => {
                let _ = write_response(
                    client,
                    &Response::Error {
                        message: format!("clipboard: pipe failed: {e}"),
                    },
                );
                return true;
            }
        };

        // The two request fns return same-named-but-distinct error types, so
        // normalize to a string at each call site.
        let req = if primary {
            request_primary_client_selection(&self.seat, mime.clone(), write_fd)
                .map_err(|e| e.to_string())
        } else {
            request_data_device_client_selection(&self.seat, mime.clone(), write_fd)
                .map_err(|e| e.to_string())
        };
        if let Err(e) = req {
            // NoSelection / InvalidMimetype / ServerSideSelection: the owner
            // vanished or can't serve this mime. Treat as empty rather than
            // an error — the caller just gets nothing to ship.
            tracing::debug!(error = %e, mime, primary, "clipboard read request rejected");
            let _ = write_response(
                client,
                &Response::Clipboard {
                    mime: None,
                    data: Vec::new(),
                },
            );
            return true;
        }

        // Defer: register the read end and finalize on EOF. The dispatch arm
        // has already marked the connection spent.
        let read_id = self.clipboard.next_read_id;
        self.clipboard.next_read_id = self.clipboard.next_read_id.wrapping_add(1);
        self.clipboard.pending_reads.insert(
            read_id,
            ClipboardRead {
                client: Rc::clone(client),
                client_id,
                mime,
                buf: Vec::new(),
                token: None,
            },
        );

        let source = Generic::new(read_file, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, move |_, file, state| {
                // SAFETY: the calloop source owns the File; we never drop the fd
                // out from under it.
                let file = unsafe { file.get_mut() };
                let mut buf = [0u8; 4096];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => {
                            state.finalize_clipboard_read(read_id);
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => {
                            if let Some(p) = state.clipboard.pending_reads.get_mut(&read_id) {
                                p.buf.extend_from_slice(&buf[..n]);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "clipboard read pipe failed");
                            state.finalize_clipboard_read(read_id);
                            return Ok(PostAction::Remove);
                        }
                    }
                }
            });

        match token {
            Ok(t) => {
                if let Some(p) = self.clipboard.pending_reads.get_mut(&read_id) {
                    p.token = Some(t);
                }
                false
            }
            Err(e) => {
                // Couldn't register the source: finalize inline with whatever
                // we have (nothing) so the client isn't left hanging.
                tracing::warn!(error = %e, "insert clipboard read source failed");
                self.finalize_clipboard_read(read_id);
                true
            }
        }
    }

    /// Send the deferred `GetClipboard` reply once the owner's pipe has drained
    /// (or failed). Removes the calloop source and drops the IPC connection.
    fn finalize_clipboard_read(&mut self, read_id: u64) {
        let Some(mut pending) = self.clipboard.pending_reads.remove(&read_id) else {
            return;
        };
        if let Some(t) = pending.token.take() {
            self.loop_handle.remove(t);
        }
        let _ = write_response(
            &pending.client,
            &Response::Clipboard {
                mime: Some(pending.mime),
                data: std::mem::take(&mut pending.buf),
            },
        );
        self.drop_ipc_client(pending.client_id);
    }

    /// Answer [`Request::SetClipboard`]: become the compositor-side owner of the
    /// selection, caching `data` under `mime` (plus the standard text aliases
    /// when `mime` is textual) so any paster finds the bytes.
    ///
    /// [`Request::SetClipboard`]: shoestring_ipc::Request::SetClipboard
    pub(crate) fn handle_set_clipboard(&mut self, primary: bool, mime: String, data: Vec<u8>) {
        let mimes = offered_mimes(&mime);
        let mut cache = HashMap::with_capacity(mimes.len());
        for m in &mimes {
            cache.insert(m.clone(), data.clone());
        }
        let dh = self.display_handle.clone();
        if primary {
            self.clipboard.primary_cache = cache;
            self.clipboard.primary_mimes = mimes.clone();
            set_primary_selection(&dh, &self.seat, mimes, ());
        } else {
            self.clipboard.clipboard_cache = cache;
            self.clipboard.clipboard_mimes = mimes.clone();
            set_data_device_selection(&dh, &self.seat, mimes, ());
        }
    }

    /// [`SelectionHandler::new_selection`] hook: a client took the selection.
    /// Track its offered mimes and drop any compositor cache for that target —
    /// the bytes now live in the client, not us.
    ///
    /// [`SelectionHandler::new_selection`]: smithay::wayland::selection::SelectionHandler::new_selection
    pub(crate) fn note_new_selection(&mut self, ty: SelectionTarget, mimes: Vec<String>) {
        match ty {
            SelectionTarget::Clipboard => {
                self.clipboard.clipboard_mimes = mimes;
                self.clipboard.clipboard_cache.clear();
            }
            SelectionTarget::Primary => {
                self.clipboard.primary_mimes = mimes;
                self.clipboard.primary_cache.clear();
            }
        }
    }

    /// [`SelectionHandler::send_selection`] hook: a client is pasting a
    /// selection *we* set — write the cached bytes for `mime` into `fd`.
    ///
    /// The write is synchronous; clipboard payloads here are text (small), so
    /// blocking on the pipe is bounded. An unknown mime drops `fd`, giving the
    /// paster EOF.
    ///
    /// [`SelectionHandler::send_selection`]: smithay::wayland::selection::SelectionHandler::send_selection
    pub(crate) fn serve_clipboard_selection(
        &mut self,
        ty: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) {
        let cache = match ty {
            SelectionTarget::Clipboard => &self.clipboard.clipboard_cache,
            SelectionTarget::Primary => &self.clipboard.primary_cache,
        };
        if let Some(data) = cache.get(&mime).cloned() {
            let mut file = std::fs::File::from(fd);
            if let Err(e) = file.write_all(&data) {
                tracing::debug!(error = %e, mime, "clipboard serve write failed");
            }
        }
    }
}

/// Pick the most-preferred text mime present in `available`, or `None` if the
/// owner offers no text we read.
fn pick_text_mime(available: &[String]) -> Option<String> {
    TEXT_MIMES
        .iter()
        .find(|m| available.iter().any(|a| a == *m))
        .map(|m| m.to_string())
}

/// The mimes to offer when setting a selection. Text mimes fan out to the full
/// alias set (so every paster finds its atom); anything else is offered as-is.
fn offered_mimes(mime: &str) -> Vec<String> {
    if mime.starts_with("text/") || TEXT_MIMES.contains(&mime) {
        TEXT_MIMES.iter().map(|m| m.to_string()).collect()
    } else {
        vec![mime.to_string()]
    }
}

/// Create a pipe with a non-blocking read end (for the calloop poll loop) and a
/// blocking write end (handed to the selection owner, which writes then closes).
fn make_pipe() -> std::io::Result<(std::fs::File, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid 2-element array for pipe2 to fill.
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both fds are freshly-owned and valid.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_nonblocking(&read_fd)?;
    Ok((std::fs::File::from(read_fd), write_fd))
}

/// Set `O_NONBLOCK` on `fd`.
fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is a valid open fd owned by `fd` for the call's duration.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_utf8_text_plain() {
        let avail = vec![
            "UTF8_STRING".to_string(),
            "text/plain".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            pick_text_mime(&avail).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn falls_back_through_atoms() {
        let avail = vec!["STRING".to_string(), "UTF8_STRING".to_string()];
        assert_eq!(pick_text_mime(&avail).as_deref(), Some("UTF8_STRING"));
    }

    #[test]
    fn no_text_mime_is_none() {
        let avail = vec![
            "image/png".to_string(),
            "x-special/gnome-copied-files".to_string(),
        ];
        assert_eq!(pick_text_mime(&avail), None);
    }

    #[test]
    fn text_mime_fans_out_to_aliases() {
        let offered = offered_mimes("text/plain;charset=utf-8");
        assert_eq!(
            offered,
            TEXT_MIMES.iter().map(|m| m.to_string()).collect::<Vec<_>>()
        );
        // A bare text/* subtype still fans out.
        assert_eq!(offered_mimes("text/html").len(), TEXT_MIMES.len());
    }

    #[test]
    fn non_text_mime_offered_verbatim() {
        assert_eq!(offered_mimes("image/png"), vec!["image/png".to_string()]);
    }

    #[test]
    fn make_pipe_roundtrips_with_nonblocking_read() {
        use std::io::{Read, Write};
        let (mut read, write) = make_pipe().expect("pipe");
        // Empty read end is non-blocking → WouldBlock, not a hang.
        let mut buf = [0u8; 8];
        let err = read.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        // Write then close the write end; read drains then sees EOF.
        let mut wf = std::fs::File::from(write);
        wf.write_all(b"hello").unwrap();
        drop(wf);
        let mut got = Vec::new();
        // Spin past WouldBlock until data/EOF (single-threaded test pipe).
        loop {
            match read.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("read: {e}"),
            }
        }
        assert_eq!(got, b"hello");
    }
}
