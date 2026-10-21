# CLAUDE.md

@AGENTS.md

Agent notes for working on **shoestring-wm**. See `README.md` for the
project overview and `docs/` for the full manual. This file covers the
things that aren't obvious from the source: how to drive the **running**
WM over its IPC socket.

## Driving the live WM over IPC

The WM exposes a unix-socket IPC (newline-delimited JSON). When a
shoestring-wm session is running, you can query its state and synthesize
input/screenshots without rebuilding anything — useful for verifying
changes against the real compositor.

- **Client:** `shoestring-ctl` (built from `crates/shoestring-ctl`, on
  `$PATH` after install). Run `shoestring-ctl --help` for the full
  subcommand list. Add `-p` for pretty JSON.
- **Socket:** the client auto-discovers it via `$SHOESTRING_WM_SOCKET`,
  falling back to `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`.
  Override with `-s <path>`. If neither resolves, no WM is running.
- **Canonical protocol reference:** `docs/ipc.rst` — every request,
  response, event, and the automation-gate semantics. Read it before
  changing the IPC surface or scripting against it.

Read-only queries (never gated): `workspaces`, `windows`, `outputs`,
`pointer-position`, `event-stream`.

```sh
shoestring-ctl -p windows          # list mapped windows + focus flag
shoestring-ctl -p outputs          # outputs, modes, scales
shoestring-ctl event-stream        # tail events (blocks; one JSON/line)
```

## The automation gate

Input synthesis and capture are gated behind a runtime **automation
gate**, so nothing is driven or captured until someone turns it on. The
gate is consent, not authorization: any IPC client may flip it, and only
the user's own processes can reach the socket (ADR 0005, and "Trust
model" in `docs/ipc.rst`). These requests refuse with a stable
`automation disabled: ...` error while the gate is off:
`inject_key`/`type`/`click`, `move_mouse`, `dispatch_action`,
`screenshot`, `run_command`.

The gate is **runtime-only** — `shoestring-ctl automation on` does NOT
edit any file. At next WM start it resets to `[general].automation_enabled`
in the config (`~/.config/shoestring-wm/config.toml`), which is the
source of truth. To make it default-on, set that key; to enable for one
session, flip it over IPC.

```sh
shoestring-ctl automation status       # {"enabled": false}
shoestring-ctl automation on           # flip ON for this WM session
shoestring-ctl automation off          # flip back OFF when done
```

Flipping the gate broadcasts an `automation_changed` event to subscribers.

## Common recipe: gate on → screenshot → gate off

`screenshot` captures via the WM and writes
`$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png`, printing the absolute path.
Full screen of the default output:

```sh
shoestring-ctl automation on
shoestring-ctl screenshot              # prints {"type":"screenshot","path":"/…/Screenshot-AUTO-….png"}
shoestring-ctl automation off
```

Variants: `--output eDP-1` to pick an output; `--output eDP-1 --region X,Y,W,H`
for a rectangle in that output's logical coords.

Other gated primitives once the gate is on: `shoestring-ctl key <Keysym>`,
`shoestring-ctl type "<text>"`, `shoestring-ctl click <left|right|middle>`,
and `shoestring-ctl run-command -- <argv>` (runs under the WM's env,
returns stdout/stderr/exit as JSON). See `docs/ipc.rst` for the rest.

## Project conventions

- The maintainer's task list lives outside the repository, managed via
  `todo-sqlite-cli`; a gitignored `.todo-sqlite-cli` marker at the repo
  root names the database. Use the CLI, never query the DB directly, and
  keep task ids and titles out of public files (see `AGENTS.md`).
- Docs are reStructuredText under `docs/` (Sphinx). Keep `docs/ipc.rst`
  in sync when the IPC surface changes.
- Settled policy lives in `docs/adr/` (Markdown, outside the Sphinx
  toctree). Read the relevant ADR before changing what it covers, such
  as repo sanitization (0001) or vendoring vs. forking Smithay (0002).
