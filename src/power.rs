//! System power actions (shut down / reboot / suspend).
//!
//! These are deliberately *system* concerns the WM only brokers: it owns
//! no power policy, it just shells out to whatever session/init manager is
//! present. Each action walks a fallback chain so systemd, elogind, and
//! bare-init/FreeBSD all work without a build-time choice — the same
//! systemd-is-optional posture the rest of the WM keeps. The first
//! candidate whose program is found on `$PATH` and spawns wins; later ones
//! are only reached when the earlier program is absent.
//!
//! All three actions route through [`ShoestringWm::confirm_power`], which
//! reuses the `shoestring-confirm` dialog (see [`crate::confirm`]) — the
//! IPC requests are ungated because that human confirmation *is* the
//! authorization (mirrors `Request::Quit`).

use crate::state::ShoestringWm;

/// A system power transition the WM can broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    PowerOff,
    Reboot,
    Suspend,
}

impl PowerAction {
    /// Prompt shown in the confirm dialog.
    fn prompt(self) -> &'static str {
        match self {
            PowerAction::PowerOff => "Shut down?",
            PowerAction::Reboot => "Reboot?",
            PowerAction::Suspend => "Sleep (suspend)?",
        }
    }

    /// Ordered `(program, args)` candidates. The WM spawns the first whose
    /// `program` resolves on `$PATH`; the rest are fallbacks for systems
    /// lacking the earlier tool.
    fn candidates(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self {
            // `shutdown -h now` powers off on Linux; FreeBSD wants `-p now`
            // (`-h` only halts there), so pick the last-resort args per OS.
            PowerAction::PowerOff => {
                #[cfg(target_os = "freebsd")]
                {
                    &[
                        ("systemctl", &["poweroff"]),
                        ("loginctl", &["poweroff"]),
                        ("shutdown", &["-p", "now"]),
                    ]
                }
                #[cfg(not(target_os = "freebsd"))]
                {
                    &[
                        ("systemctl", &["poweroff"]),
                        ("loginctl", &["poweroff"]),
                        ("shutdown", &["-h", "now"]),
                    ]
                }
            }
            // `shutdown -r now` reboots on both Linux and FreeBSD.
            PowerAction::Reboot => &[
                ("systemctl", &["reboot"]),
                ("loginctl", &["reboot"]),
                ("shutdown", &["-r", "now"]),
            ],
            // No portable bare-init suspend; `zzz` covers FreeBSD, and on
            // non-systemd Linux elogind still provides `loginctl suspend`.
            PowerAction::Suspend => {
                #[cfg(target_os = "freebsd")]
                {
                    &[
                        ("systemctl", &["suspend"]),
                        ("loginctl", &["suspend"]),
                        ("zzz", &[]),
                    ]
                }
                #[cfg(not(target_os = "freebsd"))]
                {
                    &[("systemctl", &["suspend"]), ("loginctl", &["suspend"])]
                }
            }
        }
    }
}

/// Resolve `program` against `$PATH`, returning true if an executable file
/// of that name exists on it. Used to skip fallback candidates whose tool
/// isn't installed *before* spawning, so the chain actually falls through
/// (a bare `spawn` only fails on absence, but we also want to log which
/// tool we picked). An absolute/relative path with a `/` is taken as-is.
fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

impl ShoestringWm {
    /// Pop the confirm dialog for `action`; on the user's *Yes*, run it.
    /// Reused by every power `Request`. The dialog is the authorization, so
    /// the requests themselves stay ungated (see [`crate::power`]).
    pub fn confirm_power(&mut self, action: PowerAction) {
        self.confirm_action(action.prompt(), move |state| state.run_power_action(action));
    }

    /// Execute `action` by spawning the first available candidate in its
    /// fallback chain. Fire-and-forget: `systemctl`/`loginctl`/`shutdown`
    /// all return promptly after handing the request to the init/session
    /// manager, so we don't wait (and couldn't reliably — `SA_NOCLDWAIT`
    /// auto-reaps; see [`crate::confirm`]). A wholly unresolved chain is a
    /// logged warning, never a panic — matches the graceful-degradation
    /// posture for systemd-optional systems.
    pub fn run_power_action(&mut self, action: PowerAction) {
        for (program, args) in action.candidates() {
            if !on_path(program) {
                continue;
            }
            let mut cmd = std::process::Command::new(program);
            cmd.args(*args);
            match cmd.spawn() {
                Ok(child) => {
                    tracing::info!(pid = child.id(), program, ?args, ?action, "power action");
                    return;
                }
                // Found on PATH but failed to launch — try the next tool.
                Err(e) => tracing::warn!(program, ?args, error = %e, "power command spawn failed"),
            }
        }
        tracing::warn!(
            ?action,
            "no usable power tool found (tried systemctl/loginctl/shutdown)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_action_prefers_systemctl_then_loginctl() {
        for action in [
            PowerAction::PowerOff,
            PowerAction::Reboot,
            PowerAction::Suspend,
        ] {
            let c = action.candidates();
            assert_eq!(c[0].0, "systemctl", "{action:?} should try systemctl first");
            assert_eq!(
                c[1].0, "loginctl",
                "{action:?} should fall back to loginctl"
            );
        }
    }

    #[test]
    fn poweroff_and_reboot_have_a_shutdown_fallback() {
        // Bare-init / FreeBSD path: shutdown(8) must close out the chain.
        assert_eq!(
            PowerAction::PowerOff.candidates().last().unwrap().0,
            "shutdown"
        );
        assert_eq!(
            PowerAction::Reboot.candidates().last().unwrap().0,
            "shutdown"
        );
    }

    #[test]
    fn on_path_resolves_real_binary_and_rejects_bogus() {
        // `sh` is on PATH on every supported platform; the random name is not.
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-binary-xyzzy"));
        // A path with a slash is checked as a literal file.
        assert!(on_path("/bin/sh") || on_path("/usr/bin/sh"));
        assert!(!on_path("/nonexistent/path/to/nothing"));
    }
}
