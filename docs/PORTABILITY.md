# Portability

shoestring-wm targets **Wayland + Rust + bash**. Linux is the daily-driver
platform; FreeBSD is the canonical second target. This document captures
the audit of Linux-isms in the codebase and the gaps remaining for a
clean FreeBSD build.

## Build matrix

| Feature set | Linux | FreeBSD | macOS / Windows |
|---|---|---|---|
| `--features winit` (dev backend, runs nested) | ✓ | should build; untested — see [task 61](#follow-ups) | n/a (no Wayland host) |
| `--features tty` (daily driver, DRM/KMS) | ✓ | ✗ by design — see [§ TTY backend](#tty-backend) | ✗ |

The `tty` feature pulls Smithay's libinput / udev / libseat / DRM /
GBM / EGL backends, all of which are Linux-only. This is intentional;
the feature gate keeps the winit build free of those deps so winit
should compile and run on any Wayland-capable Unix.

## Audit summary

### TTY backend

Cleanly feature-gated. `src/backend/mod.rs` and `src/main.rs` both wrap
the TTY entry point in `#[cfg(feature = "tty")]`; building with
`--no-default-features --features winit` does not pull DRM/udev/libseat
into the link.

### config_watcher.rs

Uses the `notify` crate (cross-platform: inotify on Linux, kqueue on
BSD/macOS, polling fallback). No direct inotify wrapping. ✓

### shoestring-lock PAM service probing

`pick_pam_service()` in `crates/shoestring-lock/src/main.rs:595` probes
`/etc/pam.d/{system-auth,login,passwd}` and falls back to `"login"`.
Order is Linux-friendly (`system-auth` is glibc-PAM convention) and
FreeBSD-friendly (the `login` fallback exists on every BSD PAM
install). `--pam-service` CLI flag overrides when distro defaults are
wrong. ✓

`pam-client2` is hand-written FFI (no bindgen); links against
`libpam.so` which is present on every PAM-using OS. ✓

### Path assumptions in our code

We do not read `/proc`, `/sys`, `/dev/dri`, or `/dev/input` directly —
Smithay's backends do that, and only on the Linux-only TTY path.

`$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock` is the default
IPC socket. Wayland compositors on BSD set `XDG_RUNTIME_DIR`, and
`$SHOESTRING_WM_SOCKET` is an unconditional override. ✓

Font path lists in `shoestring-bar`, `shoestring-confirm`, and
`shoestring-lock` include `/usr/share/fonts/` (Linux distros) and
`/usr/local/share/fonts/` (FreeBSD pkg layout). Each binary also honors
`$SHOESTRING_*_FONT` for explicit overrides. ✓

### systemd / dbus / loginctl

None. The WM does not call dbus, does not assume systemd, and does not
shell out to `loginctl`. Session locking goes through
`ext-session-lock-v1` (the compositor protocol), not a system service.

### Direct Linux syscalls

None. No `libc::SYS_*`, no `pidfd_open`, no direct epoll/inotify. The
confirm helper deliberately uses a pipe-EOF idiom instead of
`pidfd_open` for exactly this reason — see `src/confirm.rs`.

### Shell scripts

All `#!/usr/bin/env bash`. No bashisms beyond what's in `scripts/actions/`,
which are themselves trivial. bash is the documented dep.

### Action scripts (`scripts/actions/`)

These call **Linux-only userland**:

| Script | Tool | BSD status |
|---|---|---|
| `shoestring-volume-{up,down,mute}` | `wpctl` (PipeWire) | works on FreeBSD if PipeWire is installed (`pkg install pipewire`) |
| `shoestring-mic-mute` | `wpctl` | same |
| `shoestring-brightness-{up,down}` | `brightnessctl` | **Linux-only** (reads `/sys/class/backlight`); no BSD equivalent. FreeBSD uses `backlight(8)`. |
| `shoestring-logout` | `pkill -x` | POSIX short flag; accepted by both GNU pkill and FreeBSD pkill. |

`scripts/actions/README.md` already lists the tool dependency per
script. They are user-facing helpers, not load-bearing WM code — the
WM does not depend on any of them.

## Follow-ups

Concrete blockers worth tracking as their own todos:

- **task 61** — actually build + smoke-test the winit feature on
  FreeBSD (e.g. inside Sway on a FreeBSD VM); document the
  `pkg install` line in `docs/install.rst`.
- **task 63** — `shoestring-brightness-*`: add a FreeBSD branch using
  `backlight(8)` (or document the script as Linux-only and ship a
  per-OS alternate in `scripts/actions/freebsd/`).
