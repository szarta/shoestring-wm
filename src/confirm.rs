//! Modal confirmation plumbing.
//!
//! [`ShoestringWm::confirm_action`] spawns `shoestring-confirm` (a
//! layer-shell helper) with the given prompt, then watches the child's
//! stdout pipe via calloop. When the helper exits we read its exit code:
//! `0` means the user pressed Enter and we run the wrapped action,
//! anything else means cancel/error and we do nothing.
//!
//! The helper's stdout is piped only so we have an fd that EOFs on exit
//! — `shoestring-confirm` itself doesn't print anything there. This
//! avoids pulling in `pidfd_open` while still giving us non-blocking
//! "child exited" notification on calloop's main thread.
//!
//! Only one confirm dialog is active at a time. A second
//! `confirm_action` call while one is pending is dropped — this keeps
//! key-mash from stacking helpers, and the user can resolve the live
//! one with Enter/Esc.

use std::{
    io::Read,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    process::{Child, ChildStdout, Command, Stdio},
};

use smithay::reexports::calloop::{
    generic::Generic, Interest, Mode, PostAction, RegistrationToken,
};

use crate::state::ShoestringWm;

type OnAccept = Box<dyn FnOnce(&mut ShoestringWm) + 'static>;

pub struct PendingConfirm {
    child: Child,
    stdout_token: Option<RegistrationToken>,
    /// Callback run if the helper exits with code 0 (user pressed Enter).
    on_accept: Option<OnAccept>,
}

struct StdoutFd(ChildStdout);

impl AsFd for StdoutFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: borrow tied to &self; `ChildStdout` owns the fd, and
        // calloop owns the `Generic<StdoutFd>` for the source's lifetime
        // so the fd stays valid.
        unsafe { BorrowedFd::borrow_raw(self.0.as_raw_fd()) }
    }
}

impl ShoestringWm {
    /// Pop a modal confirm dialog. On Enter the helper exits 0 and we
    /// invoke `on_accept`. On Esc / click / compositor close / spawn
    /// failure we do nothing — fail-closed so an accidental keypress
    /// can never tear down state.
    pub fn confirm_action<F>(&mut self, prompt: &str, on_accept: F)
    where
        F: FnOnce(&mut ShoestringWm) + 'static,
    {
        if self.pending_confirm.is_some() {
            tracing::debug!(prompt, "confirm_action: already pending, ignoring");
            return;
        }

        let mut cmd = Command::new("shoestring-confirm");
        cmd.arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(socket) = self.socket_name.to_str() {
            cmd.env("WAYLAND_DISPLAY", socket);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "spawn shoestring-confirm failed; cancelling action");
                return;
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                tracing::warn!("shoestring-confirm: piped stdout missing");
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        };

        let pid = child.id();
        self.pending_confirm = Some(PendingConfirm {
            child,
            stdout_token: None,
            on_accept: Some(Box::new(on_accept)),
        });

        let source = Generic::new(StdoutFd(stdout), Interest::READ, Mode::Level);
        let insert = self.loop_handle.insert_source(source, |_, fd, state| {
            // SAFETY: see remote_command::register_pipe — calloop owns
            // the Generic<StdoutFd>; we only borrow during dispatch.
            let fd = unsafe { fd.get_mut() };
            let mut buf = [0u8; 64];
            loop {
                match fd.0.read(&mut buf) {
                    Ok(0) => {
                        state.finalize_confirm();
                        return Ok(PostAction::Remove);
                    }
                    // Helper isn't supposed to print to stdout, but if
                    // it does, drain and ignore — only EOF + exit code
                    // matter.
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "confirm pipe read failed");
                        state.finalize_confirm();
                        return Ok(PostAction::Remove);
                    }
                }
            }
        });

        match insert {
            Ok(token) => {
                if let Some(p) = self.pending_confirm.as_mut() {
                    p.stdout_token = Some(token);
                }
                tracing::debug!(pid, prompt, "shoestring-confirm spawned");
            }
            Err(e) => {
                tracing::warn!(error = %e, "register confirm pipe source");
                // Roll back: kill + reap the orphaned helper.
                if let Some(mut pending) = self.pending_confirm.take() {
                    let _ = pending.child.kill();
                    let _ = pending.child.wait();
                }
            }
        }
    }

    /// Helper exited (or its pipe errored out). Reap and dispatch the
    /// `on_accept` callback if exit code is 0.
    fn finalize_confirm(&mut self) {
        let Some(mut pending) = self.pending_confirm.take() else {
            return;
        };
        if let Some(t) = pending.stdout_token.take() {
            self.loop_handle.remove(t);
        }
        let exit_code = match pending.child.wait() {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) => {
                tracing::warn!(error = %e, "wait shoestring-confirm");
                return;
            }
        };
        if exit_code == 0 {
            if let Some(cb) = pending.on_accept.take() {
                cb(self);
            }
        } else {
            tracing::debug!(exit_code, "confirm cancelled");
        }
    }
}
