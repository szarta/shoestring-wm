"""Developer task runner for shoestring-wm (https://www.pyinvoke.org/).

Codifies the build / verify / deploy loop and the nested-WM test harness
that are otherwise tribal knowledge. Run ``inv --list`` for the menu.

Quick reference::

    inv build                # debug workspace build
    inv build --release      # release workspace build
    inv test                 # cargo test --workspace
    inv check                # fmt --check + clippy -D warnings (the static gate)
    inv check test           # full pre-commit gate (tasks chain on the CLI)
    inv deploy               # build --release + install to ~/.cargo/bin
    inv deploy --restart     # ...and SIGTERM the live WM (ENDS your session)
    inv run-nested           # launch a throwaway nested winit WM to test against
    inv create-docs          # Sphinx HTML user guide -> docs/_build/html
    inv create-manpages      # Sphinx man pages -> docs/_build/man

The pre-commit hook already enforces fmt + clippy + test on every commit;
``inv check`` is for running that gate *before* you stage.

The doc tasks shell out to ``sphinx-build`` (via docs/Makefile), so run them
from the docs virtualenv, e.g. ``. ~/data/stars/venv/bin/activate``.
"""

import os
import shutil
from pathlib import Path

from invoke import task

# Every binary the workspace produces, in the order docs/install.rst lists
# them. These are what `deploy` drops on $PATH; the live session resolves them
# from ~/.cargo/bin (see the dev-build-deploy note: NOT /usr/bin).
BINARIES = [
    "shoestring-wm",
    "shoestring-bar",
    "shoestring-menu",
    "shoestring-notify",
    "shoestring-ctl",
    "shoestring-lock",
    "shoestring-screenshot",
    "shoestring-region",
    "shoestring-kill",
    "shoestring-confirm",
]

# Where the live session looks for the binaries. The display manager would use
# a system path, but this user's running session is ~/.cargo/bin.
DEFAULT_PREFIX = "~/.cargo/bin"

# Isolated runtime dir for the nested WM. MUST be separate from the live
# $XDG_RUNTIME_DIR: a nested WM sharing it pollutes the systemd user env and
# clobbers the live IPC socket, crashing the real session (nested-wm-testing
# note).
NESTED_RUNTIME_DIR = "/tmp/sswm-test-rt"


@task
def build(c, release=False, winit_only=False):
    """Compile the workspace (debug by default; --release for an optimized build).

    --winit-only builds just the WM with the winit backend and no DRM/TTY
    stack — the fast inner-loop build for nested testing.
    """
    flags = "--release" if release else ""
    if winit_only:
        c.run(
            f"cargo build {flags} --no-default-features --features winit "
            f"-p shoestring-wm".strip(),
            pty=True,
        )
    else:
        c.run(f"cargo build {flags} --workspace".strip(), pty=True)


@task
def test(c):
    """Run the full workspace test suite (cargo test --workspace)."""
    c.run("cargo test --workspace", pty=True)


@task
def check(c):
    """Static gate: rustfmt --check + clippy with warnings denied.

    Mirrors the fmt/clippy half of the pre-commit hook so you can catch
    problems before staging. Chain with the test task for the full gate:
    ``inv check test``.
    """
    c.run("cargo fmt --all -- --check", pty=True)
    c.run("cargo clippy --workspace --all-targets -- -D warnings", pty=True)


@task(help={"clean": "Wipe docs/_build first for a from-scratch build."})
def create_docs(c, clean=False):
    """Build the HTML user guide with Sphinx (output: docs/_build/html).

    Routes through docs/Makefile, which runs sphinx-build with
    ``-W --keep-going`` so doc warnings fail the build. Needs the docs
    virtualenv active (sphinx-build on PATH) — see the module docstring.
    """
    if clean:
        c.run("make -C docs clean", pty=True)
    c.run("make -C docs html", pty=True)
    print("\nHTML docs: docs/_build/html/index.html")


@task(help={"clean": "Wipe docs/_build first for a from-scratch build."})
def create_manpages(c, clean=False):
    """Build all man pages with Sphinx (output: docs/_build/man).

    Sources live in docs/man/*.rst and are registered in docs/conf.py's
    ``man_pages``. Like create_docs, routes through docs/Makefile (warnings
    are fatal) and needs the docs virtualenv active.
    """
    if clean:
        c.run("make -C docs clean", pty=True)
    c.run("make -C docs man", pty=True)
    print("\nman pages: docs/_build/man/")


@task(
    help={
        "restart": "After installing, SIGTERM the running WM. This ENDS "
        "your Wayland session — only safe from a TTY/SSH."
    }
)
def deploy(c, prefix=DEFAULT_PREFIX, restart=False):
    """Release-build the workspace and install every binary to PREFIX.

    Defaults to ~/.cargo/bin, where the live session resolves them. WM-side
    changes need a restart (logout) to take effect; helper binaries (bar,
    ctl, menu, ...) are picked up the next time the WM spawns them, so most
    changes don't strictly need --restart.
    """
    build(c, release=True)

    dest = Path(prefix).expanduser()
    dest.mkdir(parents=True, exist_ok=True)
    paths = " ".join(f"target/release/{b}" for b in BINARIES)
    # install -D handles perms (0755) and creates the dir; one call for all.
    c.run(f"install -Dm755 -t {dest} {paths}", pty=True)
    print(f"\ninstalled {len(BINARIES)} binaries to {dest}")

    if restart:
        if not shutil.which("pkill"):
            raise SystemExit("pkill not found; cannot restart")
        print("restarting WM (SIGTERM) — your session will end now...")
        # -x exact match so we don't hit shoestring-wm-ctl / an open editor.
        c.run("pkill --signal SIGTERM -x shoestring-wm", warn=True, pty=True)
    else:
        print(
            "Not restarting. WM-side changes take effect after a logout/"
            "restart;\nrun `inv deploy --restart` (from a TTY) or use your "
            "logout binding."
        )


@task(
    help={
        "rebuild": "Rebuild the winit WM before launching (default: on).",
        "runtime-dir": f"Isolated XDG_RUNTIME_DIR for the nested WM "
        f"(default: {NESTED_RUNTIME_DIR}).",
        "release": "Launch the release build instead of debug.",
        "log": "Redirect WM tracing to this file instead of the terminal.",
    }
)
def run_nested(
    c, rebuild=True, runtime_dir=NESTED_RUNTIME_DIR, release=False, log=None
):
    """Launch a throwaway nested winit WM to test changes against live.

    Runs the freshly-built WM as a window inside your current session, in an
    isolated runtime dir so it can't touch the live IPC socket or systemd
    env. Drive it from another terminal with shoestring-ctl pointed at the
    nested socket; the path is printed below. Ctrl-C to stop; the runtime
    dir is recreated on each run.
    """
    parent_rt = os.environ.get("XDG_RUNTIME_DIR")
    wl_display = os.environ.get("WAYLAND_DISPLAY")
    x_display = os.environ.get("DISPLAY")
    if not parent_rt:
        raise SystemExit("XDG_RUNTIME_DIR unset — not inside a session?")
    if not wl_display and not x_display:
        raise SystemExit(
            "neither WAYLAND_DISPLAY nor DISPLAY set — no parent compositor "
            "to nest under"
        )

    if rebuild:
        build(c, release=release, winit_only=True)

    # Fresh isolated runtime dir (0700, like a real $XDG_RUNTIME_DIR).
    rt = Path(runtime_dir)
    if rt.exists():
        shutil.rmtree(rt)
    rt.mkdir(parents=True)
    rt.chmod(0o700)

    env = {"XDG_RUNTIME_DIR": str(rt)}
    if wl_display:
        # Symlink the parent's wayland socket (+ lock) in so the winit
        # backend can connect to it under the isolated runtime dir.
        for name in (wl_display, f"{wl_display}.lock"):
            src = Path(parent_rt) / name
            if src.exists():
                (rt / name).symlink_to(src)
        env["WAYLAND_DISPLAY"] = wl_display
        print(f"nesting under Wayland parent {wl_display}")
    else:
        # X11 parent: the X socket lives in /tmp/.X11-unix (global), so no
        # symlink is needed — just pass DISPLAY through.
        env["DISPLAY"] = x_display
        print(f"nesting under X11 parent {x_display}")
    if log:
        env["SHOESTRING_WM_LOG"] = log

    profile = "release" if release else "debug"
    bin_path = f"target/{profile}/shoestring-wm"
    if not Path(bin_path).exists():
        raise SystemExit(f"{bin_path} missing — run without --no-rebuild")

    # The nested WM creates its own wayland-N + IPC socket in the isolated
    # dir; with the parent's wayland-1 symlinked in, the next free name is
    # typically wayland-2. Tell the user how to find and drive it.
    print(f"\nnested WM IPC socket will appear under {rt}/shoestring-wm-*.sock")
    print("drive it from another terminal, e.g.:")
    print(f"  shoestring-ctl -s {rt}/shoestring-wm-wayland-2.sock windows")
    print(
        "automation is enabled (--enable-automation), so screenshot/inject "
        "work.\nCtrl-C to stop.\n"
    )

    c.run(
        f"{bin_path} --backend winit --enable-automation",
        env=env,
        pty=True,
        warn=True,  # Ctrl-C / non-zero exit shouldn't dump a traceback.
    )
