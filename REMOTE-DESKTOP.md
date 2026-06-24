# Remote Desktop — design notes

Status: **design / not yet tasked** (2026-06-22). This captures a design
conversation so it isn't lost; it is not a committed plan or a task breakdown
yet. We close out current tasks before turning this into a separate task load.

## Goal

A shoestring-wm–native remote desktop: connect to another node that is also
running shoestring-wm and have that remote desktop feel like a first-class part
of the local experience rather than "a VNC window I clicked into."

- **Super+J / Super+K** cycle *up and down through connected machines* — a
  second navigation axis, orthogonal to existing workspace switching.
- Both ends run shoestring-wm, so we exploit the shared IPC surface for
  optimizations a generic RDP/VNC can't do.

Primary use cases (these set the engineering tradeoffs):

1. **Remote management + dev.** Drive/observe another shoestring-wm box —
   e.g. one machine has Outlook-in-browser or Teams up and you want to keep an
   eye on it and interact occasionally.
2. **Headless GUI offload.** A beefy headless node runs the apps and
   shoestring-wm; a thin node (a tablet — e.g. a Surface) views and drives it
   over a fast network/USB link. The heavy box does the work; the tablet is
   the display + input.

Explicitly **not** a goal: smooth video / real-time gaming. That would force a
heavy codec (ffmpeg/gstreamer/vaapi) which is against the low-dependency ethos
and a portability burden (FreeBSD). We optimize for mostly-static desktop
content instead.

## UX model: the machine axis

You are always at `(machine, workspace)`.

- **Horizontal** (existing workspace binds) = workspaces *on the machine you're
  currently viewing*.
- **Vertical** (Super+J / Super+K) = which *machine* you're viewing. Position 0
  is `local`; then each connected remote in order.

When the active view is a remote machine, your input drives that machine, so its
*own* keybinds and workspaces work natively. Super+J/K and a single break-out
hotkey are the only things intercepted locally.

## Components

Two standalone binaries plus a small amount of WM-side plumbing.

### `shoestring-remote-server`
Runs on the node being remoted *into*. Added explicitly to the WM's
`autostart`. It is essentially a **privileged IPC bridge** between the local
WM and an ssh-tunneled network protocol:

- On start, **registers with the local WM** over its IPC ("a remote server is
  available"). This is what makes the "enable remote" toggle selectable rather
  than greyed out.
- When the remote gate is enabled, opens its listener (reachable only over the
  ssh tunnel) and accepts client connections.
- **Captures** the served output's frames from the WM and streams them
  (damage tiles, zlib) to the client.
- **Injects** input received from the client back through the WM
  (`inject_*` / `move_mouse` / `virtual_pointer`), and can forward higher-level
  actions via `dispatch_action`.
- Surfaces connection state to the WM so the "being viewed/controlled"
  indicator can light.

Because capture and injection already exist and are already gated, the server
adds little new *surface* — mostly new *plumbing*.

### `shoestring-remote-client`
Runs on the node you sit at. ssh-tunnels to the server, decodes + presents
frames, and forwards local input while the remote is the active view.

- Registers with the **local** WM so it can be placed in the Super+J/K machine
  cycle. When selected, the WM shows the client fullscreen and enters a capture
  mode that forwards input to it.
- Receives + decodes the damage-tile stream and presents it (as a fullscreen
  surface / the local WM's "remote desktop" view).
- Renders the remote cursor locally (structural — see below).

### WM-side additions (the real work on the compositor)
- **Streaming damage-capture subscription** (the key optimization, below).
- **Remote gate + server-registration + viewer indicator** (reusing the
  existing gate/event/bar-chip patterns).
- **Machine-axis nav + remote-capture input mode** (Super+J/K, input grab/
  forward, break-out hotkey).
- **(Phase 2) a way to run with no physical display** for the headless-offload
  case (see Modes).

## Consent & security

Reuse the existing gate model end to end — this is exactly the consent shape we
already have:

- The **remote gate** is effectively a coupling of the **capture gate** (for the
  served output) and the **automation gate** (for injected input) plus "open the
  listener." We already couple automation→capture, so this is the same move.
- The toggle lives in the existing gate menu and is **greyed out until a
  `shoestring-remote-server` has registered** with the WM.
- A **"being viewed / controlled" indicator** lights while a client is
  connected — a new bar chip alongside MUTE/MIC/CAM, same pattern as the capture
  indicator (privacy-opt-in: never silent, always visible).
- **Transport is an ssh tunnel** — reuse existing keys/trust, no new auth
  surface, matches the named-node workflow already in use (dev-106 / dev-107).

## Two modes (share everything but the frame source)

The server streams "an output." What that output *is* differs:

- **Mirror mode** — the served node has a real display; stream an existing
  physical output's scene. (Use case 1: monitor/drive another desktop.) Needs
  **no new backend**; smallest first slice.
- **Serve / headless mode** — the served node has *no* display; the remote view
  **is** the only output, a virtual head sized to the connecting client.
  (Use case 2: tablet drives a headless box.)

Same transport, input injection, and encode for both; only the frame source
(real CRTC vs virtual output) changes.

### Running headless (the one big net-new dependency)
Serve mode needs shoestring-wm to run with no physical display. Two paths:

- **Quick / prototype:** run the existing **winit backend inside Xvfb +
  llvmpipe**. dev-107 already proves this exact stack runs shoestring-wm
  headless for visual tests, so the whole streaming/input loop can be
  prototyped on it with zero new backend code.
- **Proper:** a native **headless backend** — surfaceless EGL on a render node
  (`/dev/dri/renderD128`), one virtual output sized by the client, no DRM
  scanout. More work, but independently valuable (CI, the WLCS harness, and
  automation all want headless) and avoids an Xvfb dependency on a box that's
  meant to be lean.

Leaning: prototype on Xvfb+winit to de-risk, then build the native headless
backend as its own tracked piece.

## Streaming design

Optimized for the fact that both ends are shoestring-wm and that content is
mostly static.

- **Native damage-push, not one-shot screencopy.** The WM already computes
  per-frame damage in its damage tracker. Expose a streaming subscription that,
  on each render, pushes only the *damaged tiles* to the server. An idle remote
  desktop produces zero frames. This is the single biggest payoff of "both run
  shoestring-wm," and it's a contained WM-side addition rather than a new
  protocol. (Alternative: build on the pending `ext-image-copy-capture`
  protocol — standards-compliant, but a native damage-push is simpler and
  strictly more optimal when we control both ends.)
- **Tile + zlib, pure-Rust, low-dep.** Damaged rectangles, RLE/zlib'd (VNC
  Tight/ZRLE in spirit). Great for terminals/code/browser UIs on a LAN; not for
  full-screen video — which is fine per the goals. No heavy codec.
- **Structural cursor.** Send cursor *position + which `CursorIcon`* over the
  channel and render it with the **local** theme — crisp, instant, no
  cursor-in-stream lag. Both ends share the cursor vocabulary.
- **Exact resolution / scale negotiation.** The client tells the server its
  output's pixel size + scale (one field); the server renders/serves a virtual
  output at exactly that size — no rescaling blur. We already have
  wlr-output-management wired to size/spawn a virtual head.
- **Structural window awareness (free).** The remote already exposes `windows`
  (geometry, app_id, title, pid, z-order) and an `event-stream`. Even in Mirror
  mode the local bar could show the remote's window/workspace list, and it sets
  up an eventual "graft one window local↔remote" mode.

## Input forwarding model

**Raw KVM-passthrough** (recommended). While a remote desktop is the active
view, the local WM captures all input and forwards it to the remote, which
injects it — so the *remote's own keybinds Just Work*. Only Super+J/K (switch
machine) and one break-out hotkey are intercepted locally. There's precedent for
the input grab in the picker and keyboard-shortcuts-inhibit paths.

Alternative considered: resolve binds locally and forward *actions*
(`dispatch_action`). More structured but less seamless and more moving parts;
raw passthrough is preferred for v1.

## What we reuse (already in-repo)

- IPC server (newline-JSON over `$SHOESTRING_WM_SOCKET`) + `event-stream`.
- wlr-screencopy + the **capture gate** (`screen_capture_enabled`).
- `inject_key` / `inject_text` / `inject_click`, `move_mouse`, virtual-pointer,
  `dispatch_action`, `run_command` + the **automation gate**.
- The workspaces model (extend for the J/K machine axis).
- wlr-output-management (virtual / resized head).
- The output-sized `MemoryRenderBuffer` fullscreen render path we just built for
  the wallpaper — presenting a remote frame is the same shape.
- The bar chip / indicator pattern (MUTE/MIC/CAM via shoestring-mediad).
- The named-node ssh workflow (dev-106 / dev-107).

## shoestring-specific optimizations (summary)

These are the things a generic RDP can't do, all stemming from "both ends are
shoestring-wm":

1. Damage-push streaming straight off the WM's damage tracker (idle ⇒ 0 frames).
2. Structural cursor (position + `CursorIcon`, rendered locally).
3. Exact resolution/scale negotiation via a virtual output.
4. Input/actions are an existing gated IPC primitive (almost free).
5. Structural window/workspace awareness over the existing query + event stream.
6. Reuse of the existing consent gates as the entire security model.

## Rough phasing (not yet tasks)

- **Phase 1 — Mirror mode.** Client on shoestring-wm; raw-passthrough input;
  native damage-push streaming over ssh; reuse the gates + add the indicator and
  the greyed-until-registered toggle; Super+J/K machine axis. No new backend.
  Shippable slice.
- **Phase 2 — Serve / headless mode.** The tablet-offload case. Prototype on
  Xvfb+winit, then build the native headless backend. Virtual output sized by
  the client.
- **Later — Graft mode.** Pull a single remote *window* into the local
  workspace as a real window (waypipe-style protocol forwarding), using the
  structural window awareness from Phase 1.

## Open questions / decisions pending

1. **Headless approach & timing:** prototype on Xvfb+winit vs. go straight to a
   native headless backend — and is Serve/headless mode a v1 goal or Phase 2?
   (Leaning: Mirror first, headless Phase 2.)
2. **Input model:** confirm raw KVM-passthrough over structured
   action-forwarding. (Leaning: raw passthrough.)
3. **Client always shoestring-wm?** The J/K machine-axis integration assumes the
   client node runs shoestring-wm (the client bin registers with the local WM).
   Do we ever need a *dumb* tablet (no shoestring-wm) as a pure standalone
   viewer? That's a different, WM-less client mode.
4. **Streaming substrate:** native damage-push IPC (preferred) vs. building on
   the pending `ext-image-copy-capture` protocol.
5. **Discovery:** explicit node list in config (matches the named-node habit,
   low-dep) vs. mDNS auto-discovery (a dependency). (Leaning: explicit config.)
