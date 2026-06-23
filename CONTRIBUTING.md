# Contributing to shoestring-wm

Thanks for your interest in shoestring-wm. This guide covers how to build,
run, and test the project, and the conventions a change is expected to
follow. For the *why* behind the codebase — the goals, the structure, and
the decisions that shaped it — read [`docs/architecture.md`](docs/architecture.md)
first; it is the map this guide assumes.

## Project shape in one minute

shoestring-wm is a Wayland compositor built on [Smithay](https://github.com/Smithay/smithay),
plus a fleet of small companion binaries (a bar, launcher, locker,
notifier, screenshot tools, an IPC client, and a desktop portal) that live
in the same Cargo workspace. A single `cargo build --workspace` produces
everything.

A few principles are load-bearing — a change that erodes one of these is a
regression, not a feature (see the architecture doc for the full list):

- **Low-dependency.** New dependencies are resisted; the handful of
  exceptions are deliberate and documented. Justify any addition.
- **Spec-correct over bespoke; we don't fork Smithay.** Smithay is pre-1.0
  and pinned to a specific git revision. When a conformance test fails
  because of spec-correct Smithay-core behavior (anvil fails it identically),
  it is tracked as a known xfail, *not* patched around. Genuine bugs go
  upstream.
- **Invasive features are opt-in.** Screen capture, input automation, and
  idle tracking default off and are absent (not merely inert) until enabled.
- **systemd-optional and portable.** Linux is primary, but every OS
  touchpoint must degrade gracefully when its facility is absent, so
  non-systemd Linux and the BSDs stay first-class.

## Prerequisites

You need a stable Rust toolchain (via [rustup](https://rustup.rs/)) and the
system development libraries. On Debian/Ubuntu (24.04+, which is what CI
uses):

```sh
sudo apt-get install -y \
    build-essential pkg-config \
    libwayland-dev libxkbcommon-dev \
    libdrm-dev libgbm-dev libegl-dev \
    libudev-dev libinput-dev \
    libseat-dev libdisplay-info-dev \
    libpam0g-dev

# Only if you build the screencast portal or media monitor:
sudo apt-get install -y libpipewire-0.3-dev libclang-dev

# Only to run X11 apps under the compositor (spawned on demand):
sudo apt-get install -y xwayland
```

Other distributions and FreeBSD are covered in
[`docs/install.rst`](docs/install.rst). On FreeBSD, libraries live under
`/usr/local/lib`, which the base linker doesn't search — export
`RUSTFLAGS="-L/usr/local/lib"` before building.

## Building

```sh
# Everything: the WM and every helper/sibling. This is what you want.
cargo build --workspace            # debug
cargo build --workspace --release  # optimized

# Just the compositor:
cargo build -p shoestring-wm
```

> **`cargo install --path .` is not enough.** It installs only the WM
> binary and silently skips the workspace helpers (`shoestring-confirm`,
> `-bar`, `-mediad`, …). A missing `shoestring-confirm`, for instance, makes
> confirm-gated actions (log out, shut down, quit) no-op with only a log
> line. Build with `--workspace` and install the binaries you need.

### Feature flags

The compositor has two real backends, selected by Cargo features
(`default = ["winit", "tty"]`):

- `winit` — a nested window for development inside an existing
  X11/Wayland session.
- `tty` — the udev/DRM backend for running as a real session compositor.

CI also compiles each backend in isolation, plus a backend-less `gl`-only
build (the shape the WLCS harness consumes), so feature-gated code doesn't
silently rot:

```sh
cargo build -p shoestring-wm --no-default-features --features winit
cargo build -p shoestring-wm --no-default-features --features tty
cargo build -p shoestring-wm --no-default-features --features gl
```

`wlcs-shoestring` is intentionally **excluded** from the workspace (it
instantiates a second copy of Smithay); build it explicitly via its own
manifest — see the WLCS section below.

## Running it for development

The backend auto-detects: if `WAYLAND_DISPLAY` or `DISPLAY` is set, the WM
nests itself in a winit window; otherwise it takes over the TTY. For
day-to-day work, run it nested:

```sh
# Nested winit window inside your current session, spawning a terminal:
cargo run -p shoestring-wm -- --command alacritty

# Bootstrap a config to edit (~/.config/shoestring-wm/config.toml):
cargo run -p shoestring-wm -- --write-default-config
```

See [`docs/running.rst`](docs/running.rst) for keybindings, autostart, and
session setup.

> **Never point a nested test instance at your live session's runtime dir.**
> A nested WM that shares the outer `XDG_RUNTIME_DIR` will clash on (or
> clobber) the live Wayland and IPC sockets and can take your real session
> down with it. Run throwaway instances in their *own* `XDG_RUNTIME_DIR`
> (the integration-test harness below does exactly this). Likewise,
> `SHOESTRING_WM_LOG=/path/to/log` redirects tracing to a file so a nested
> run doesn't spam your terminal.

To run on real hardware, switch to a free VT (e.g. Ctrl+Alt+F3), log in,
and launch with `WAYLAND_DISPLAY`/`DISPLAY` unset; the udev backend needs
`seatd` (or logind) for session management.

## Testing

The local check sequence is the same one CI gates on. Run it before
sending a change:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

A [pre-commit](https://pre-commit.com/) config wires fmt + clippy + test
(and `ruff` for the Python tooling) into a git hook:

```sh
pre-commit install            # run the hooks on every commit
pre-commit run --all-files    # run them now, across the tree
```

### Unit and integration tests

`cargo test --workspace` runs the unit tests (including the property tests
over the layout/tiling math in `src/layout.rs`) and *compiles* the
protocol integration suites under `tests/`.

Those integration suites (output-management, pointer-constraints/gestures,
tablet, idle-inhibit, virtual-pointer, foreign-toplevel-management) connect
to a **live** compositor over `$WAYLAND_DISPLAY`, so they are marked
`#[ignore]` and skipped by a plain `cargo test`. Run them against a
throwaway headless WM with the provided harness, which stands up a winit
instance in its own isolated runtime dir:

```sh
# Needs an X server + EGL. In CI that's Xvfb + Mesa llvmpipe (no GPU):
xvfb-run -a -s "-screen 0 1280x800x24" scripts/headless-integration-tests.sh

# Locally you can also run it against any existing X $DISPLAY (a winit
# window will appear). It launches its own WM and never touches your
# session's sockets.
```

### WLCS conformance

[WLCS](https://github.com/canonical/wlcs) (the Wayland Conformance Test
Suite) is a **blocking** gate. It builds the cdylib plugin in
`crates/wlcs-shoestring` and runs the suite against a tracked xfail
skip-list:

```sh
# Build the plugin (it has its own lockfile; build via its manifest):
cargo build --release --manifest-path crates/wlcs-shoestring/Cargo.toml

# Run the suite, gated against the skip-list. `wlcs` must be on $PATH
# (build it from canonical/wlcs at the pinned ref — see .github/workflows/wlcs.yml):
tests/wlcs/run-wlcs.sh crates/wlcs-shoestring/target/release/libwlcs_shoestring.so
```

The runner is **serial by default** (`WLCS_JOBS=1`) for a deterministic
verdict; parallelism reshuffles timing enough to flip a handful of
timing-sensitive tests run-to-run. The skip-list (`tests/wlcs/skip-list.txt`)
records expected failures, each verified to fail identically on Smithay's
own `anvil` reference compositor — i.e. they are Smithay-core behavior, not
our bugs. A red run means either a real regression or a skip-list entry that
now passes and should be removed. **Do not add an entry to the skip-list to
silence a failure your change introduced** — fix the change, or, if you
believe it's a genuine Smithay bug, raise it upstream and link it.

## Driving a live WM over IPC

The running compositor exposes a unix-socket IPC (newline-delimited JSON),
which is the most direct way to verify a change against the real thing
without rebuilding test scaffolding. The reference client is
`shoestring-ctl`:

```sh
shoestring-ctl -p windows      # list mapped windows + focus flag (pretty)
shoestring-ctl -p outputs      # outputs, modes, scales
shoestring-ctl event-stream    # tail live events (one JSON object per line)
```

Read-only queries are always available. Input synthesis and capture
(`inject_key`/`type`/`click`, `move_mouse`, `screenshot`, `run_command`,
`dispatch_action`) sit behind a runtime **automation gate** that is off by
default, so a normal session can't be poked by anything that reaches the
socket. Flip it for the session when you need it:

```sh
shoestring-ctl automation on
shoestring-ctl screenshot      # writes a PNG, prints its path
shoestring-ctl automation off
```

The full request/response/event surface and the gate semantics are
documented in [`docs/ipc.rst`](docs/ipc.rst) — keep that file in sync when
you change the IPC surface.

## Documentation

The user guide is reStructuredText under `docs/`, built with Sphinx. The
build runs with `-W` (warnings are errors), so a malformed reference fails
CI:

```sh
pip install sphinx          # only Sphinx + the bundled alabaster theme
make -C docs html           # HTML user guide  -> docs/_build/html/
make -C docs man            # man pages         -> docs/_build/man/
```

When you change a surface that has docs, update them in the same change:
the IPC surface → `docs/ipc.rst`; config keys → `docs/configuration.rst`;
keybindings → `docs/bindings.rst`; architecture/decisions →
`docs/architecture.md`.

## Coding conventions

- **Write code that reads like the code around it** — match the
  surrounding naming, comment density, and idioms. `cargo fmt` settles
  formatting; clippy (with `-D warnings`) settles the rest.
- **Explain the non-obvious in comments.** The existing source leans on
  comments that capture *why* a thing is done a particular way (protocol
  quirks, ordering constraints, Smithay gotchas). Match that — a reviewer
  shouldn't have to reverse-engineer a subtlety you already understood.
- **Keep portability intact.** Guard OS-specific code behind `cfg` and make
  the absent-facility path a graceful no-op or warning. Don't assume
  systemd, logind, or `/proc`.
- **Respect the gates.** Anything that can observe or drive the session
  without the user's active participation must stay opt-in and default-off.
- **Keep docs in sync** with the surface you touched (see above).

## Submitting changes

This is a pre-1.0 project; internal APIs and protocols still churn. Before
investing in a large or invasive change, it's worth opening an issue to
discuss the approach.

1. Fork and branch from `main`.
2. Make the change, keeping commits focused and their messages explaining
   *why*, not just *what*.
3. Run the full local check sequence (fmt, clippy, test, release build) —
   and the relevant integration/WLCS suites if you touched input, output,
   or protocol code.
4. Open a pull request describing the change and how you verified it. CI
   runs fmt/clippy/test/release, the per-backend feature matrix, the
   headless integration suite, the WLCS gate, the FreeBSD build, and the
   docs build — the same checks listed above.

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
