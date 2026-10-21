//! The WM's SIGCHLD handling must reap only the children it tracks.
//!
//! A handler that reaped every exited child raced `Command::spawn`, which waits
//! on a child whose `exec` failed and panics if that wait fails. On a machine
//! without `Xwayland`, that crashed the WM at startup about once in 200 starts.
//! These tests install the real handler in their own process, so they run in a
//! plain `cargo test`.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

use shoestring_wm::child_reaper;

/// Install the handler once per test process. The self-pipe's read end is
/// kept open (and never drained: a full pipe only drops wakeups).
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| std::mem::forget(child_reaper::install().expect("install reaper")));
}

/// Sweep until `pid` is reaped, returning its status.
fn reap(pid: i32) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((_, status)) = child_reaper::sweep().into_iter().find(|(p, _)| *p == pid) {
            return status;
        }
        assert!(Instant::now() < deadline, "child {pid} was never reaped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A failed spawn on std's fork path (forced here by `pre_exec`, as smithay
/// does for Xwayland) returns an error instead of panicking.
#[test]
fn failed_spawn_is_an_error_not_a_panic() {
    install();
    for _ in 0..2000 {
        let mut cmd = Command::new("shoestring-test-no-such-binary");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: the hook does nothing.
        unsafe {
            cmd.pre_exec(|| Ok(()));
        }
        assert!(cmd.spawn().is_err());
    }
}

/// Waiting on an untracked child still works under the handler.
#[test]
fn untracked_children_keep_their_status() {
    install();
    for _ in 0..50 {
        let status = Command::new("sh")
            .args(["-c", "exit 3"])
            .status()
            .expect("run sh");
        assert_eq!(status.code(), Some(3));
    }
}

/// A tracked child is reaped by `sweep`, with its exit status. `kill`
/// signals a tracked child and fails once that child is reaped. One test, so
/// no other test's `sweep` takes these statuses.
#[test]
fn tracked_children_are_reaped_and_killable() {
    install();
    let child = Command::new("sh")
        .args(["-c", "exit 7"])
        .spawn()
        .expect("spawn sh");
    let pid = child_reaper::track(child);
    assert_eq!(reap(pid).code(), Some(7));

    let child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child_reaper::track(child);
    child_reaper::kill(pid).expect("kill tracked child");
    assert_eq!(reap(pid).signal(), Some(libc::SIGKILL));
    let err = child_reaper::kill(pid).expect_err("kill after reap");
    assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
}
