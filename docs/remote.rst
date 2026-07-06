Remote desktop
==============

shoestring-wm has a **native** remote desktop: connect to another box that is
*also* running shoestring-wm and have it feel like a first-class part of the
local experience rather than "a VNC window I clicked into." Because both ends
run the same compositor, the implementation exploits the shared :doc:`IPC
surface <ipc>` for optimizations a generic RDP/VNC cannot do — chiefly
damage-push streaming straight off the WM's own damage tracker.

This page is the design reference: the goals, the locked decisions, the
architecture, and the streaming/input/consent model. For the user-facing
specifics see :doc:`bindings` (the machine-axis keys), :doc:`containers`
(running a served session in a container), :doc:`running` (the headless
backend), and :doc:`ipc` (the wire requests and events).

Goals and non-goals
-------------------

The goal is a seamless connection to *other shoestring-wm machines the user
controls*. The three use cases, in priority order:

1. **Compute / storage offload (the headline).** A beefy *headless* box runs the
   apps and shoestring-wm; a thin client — a tablet, e.g. a Surface — views and
   drives it over a fast link. The heavy box does the work; the tablet is the
   display and input. This is **serve mode**, and it is what the design is built
   around.
2. **Terminal continuity + clipboard.** The same terminal experience across
   machines, and copy/paste from a remote terminal to the local box — a cheap,
   decoupled clipboard track (see *Clipboard* below).
3. **Drive an existing desktop.** Keep an eye on / occasionally interact with a
   machine that *has* a display (a laptop running Teams or Outlook-in-browser,
   another Fedora/FreeBSD box). This is **mirror mode**, a later delta on serve
   mode.

Explicitly **not** a goal: smooth video or real-time gaming. That would force a
heavy codec (ffmpeg / gstreamer / vaapi), which is against the low-dependency
ethos and a portability burden (FreeBSD). The design optimizes for
mostly-static desktop content — terminals, code, browser UIs on a LAN — instead.

Locked decisions
----------------

- **Native serve mode, not protocol forwarding.** A full shoestring-wm desktop
  is served as pixels, rather than proxying individual Wayland clients
  (waypipe-style). The user wants the whole shoestring experience, not per-app
  offload. (waypipe stays a reference for a possible later *Graft* mode.)
- **Headless-first.** The first slice is headless serve mode, so the native
  :doc:`headless backend <running>` is the keystone and came first.
- **Consent is "log in, then enable."** The user logs into the box and
  explicitly enables remote mode; there is no pre-login / unattended access.
  This sidesteps the cross-compositor problem that the login greeter isn't
  exposed to portals.
- **Native damage-push** for the streaming substrate (not the pending
  ``ext-image-copy-capture`` protocol) — simpler and strictly more optimal when
  we control both ends.
- **Raw KVM-passthrough** for input (not structured action-forwarding).
- **Explicit node list** for discovery (not mDNS) — matches the named-node ssh
  habit and adds no dependency.
- **The client node runs shoestring-wm**, so the remote joins the local
  machine axis as a real navigable view.

The machine axis
----------------

The UX model is a second navigation axis. You are always at
``(machine, workspace)``:

- **Horizontal** (the existing workspace binds) = workspaces *on the machine
  you are currently viewing*.
- **Vertical** (``Super+J`` / ``Super+K``) = *which machine* you are viewing.
  Index 0 is ``local``; each connected remote is the next index.

When the active view is a remote machine, your input drives that machine, so its
*own* keymap and keybinds apply natively — only ``Super+J/K`` and a single
break-out hotkey (``Super+Escape``) are intercepted locally. The keys and the
clipboard-bridge binds are documented in :doc:`bindings`.

Architecture
------------

Two standalone binaries plus WM-side plumbing. For the serve / headless case::

   beefy box (no physical display)            thin client (e.g. a Surface)
     shoestring-wm                              shoestring-wm
      ├─ headless backend                        └─ shoestring-remote-client
      │    surfaceless EGL (GPU or llvmpipe)          ssh-tunnel ↔ server
      │    one virtual output, sized by client        decode tiles → fullscreen
      │    timer-driven render loop                   render remote cursor locally
      ├─ damage-capture subscription ─ tiles ─>       forward input while active
      ├─ remote gate + registration + chip       local WM: Super+J/K machine axis
      └─ shoestring-remote-server                       + raw-passthrough capture
           opens ssh-tunneled listener
           captures the virtual output, injects input

``shoestring-remote-server``
    Runs on the box being remoted *into*, added to the WM ``autostart``. A
    privileged IPC bridge: it ``register_remote_server``\ s with the local WM
    (which makes the remote gate selectable), and while the gate is on it opens
    an ssh-reachable listener, streams the served output's damage tiles
    (``capture_stream``), and injects the client's input back through the WM.
    It reports the viewer count so the "being viewed" indicator lights.

``shoestring-remote-client``
    Runs on the box you sit at. It ssh-tunnels to a server, decodes and presents
    the tile stream as a fullscreen surface (reusing the wallpaper
    ``MemoryRenderBuffer`` path), renders the remote cursor locally, and
    forwards local input while the remote is the active view. It
    ``register_remote_client``\ s with the **local** WM so it takes a slot on
    the ``Super+J/K`` machine axis.

The compositor-side additions are the streaming damage-capture subscription, the
remote gate + server-registration + viewer indicator (reusing the existing
gate / event / bar-chip pattern), and the machine-axis nav + raw-passthrough
input mode. The wire protocol and tile/zlib codec live in the shared,
low-dependency ``shoestring-remote`` crate (in the spirit of ``shoestring-ipc``).

Streaming design
----------------

Optimized for the fact that both ends are shoestring-wm and content is mostly
static:

- **Native damage-push, not one-shot screencopy.** The WM already computes
  per-frame damage. The ``capture_stream`` subscription pushes only the
  *damaged tiles* on each render, so an idle remote desktop produces **zero**
  frames. This is the single biggest payoff of "both run shoestring-wm."
- **Tile + zlib, pure-Rust, low-dep.** Damaged rectangles, zlib-compressed (VNC
  Tight / ZRLE in spirit). Great for terminals / code / browser UIs on a LAN;
  not for full-screen video — which is fine per the goals. No heavy codec.
- **Structural cursor.** The cursor is sent as *position + which* ``CursorIcon``
  and rendered with the **local** theme — crisp, instant, no cursor-in-stream
  lag. Both ends share the cursor vocabulary.
- **Exact resolution / scale negotiation.** The client sends its output's pixel
  size + scale; the server sizes the virtual output to exactly that, so there is
  no rescale blur. The :doc:`headless backend <running>` mints the virtual head.
- **Structural window awareness (free).** The served box already exposes
  ``windows`` (geometry, ``app_id``, title, pid, z-order) and an
  ``event-stream``, so a local bar can show the remote's window/workspace list,
  and it sets up the eventual *Graft* mode.

Input model
-----------

**Raw KVM-passthrough.** While a remote is the active view the local WM captures
all input and forwards it to the remote, which injects it — so the *remote's own
keybinds Just Work*. Only ``Super+J/K`` (switch machine) and the break-out
hotkey are intercepted locally. The grab reuses the precedent set by the window
picker and the keyboard-shortcuts-inhibit paths. (The alternative — resolving
binds locally and forwarding *actions* via ``dispatch_action`` — is more
structured but less seamless, and was rejected for v1.)

Consent and security
--------------------

The remote gate reuses the existing capability-gate model end to end:

- The **remote gate** couples the **capture gate** (for the served output) and
  the **automation gate** (for injected input), then opens the listener. It is
  off by default and **greyed until a** ``shoestring-remote-server`` **has
  registered**. Flip it from the bar chip, with ``shoestring-ctl remote on``,
  or — for a headless box or container — from the entrypoint. See
  :doc:`ipc` for ``set_remote`` / ``remote_status`` / ``remote_changed``.
- A **"being viewed / controlled" indicator** lights while a client is connected
  — a bar chip alongside MUTE/MIC/CAM, the same privacy-opt-in pattern as the
  capture indicator: never silent, always visible.
- **Transport is an ssh tunnel** — reuse existing keys and trust, no new auth
  surface, matching the named-node workflow already in use. The server binds
  loopback; a viewer reaches it over ``ssh -L`` (or, for a container, a
  loopback-published port — see :doc:`containers`).

Clipboard
---------

Cross-machine copy/paste rides the remote session directly: ``Super+Shift+C``
pushes the local selection to the machine you are viewing and ``Super+Shift+V``
pulls the remote's selection into the local one — both explicit and gated by the
remote share. Locally, a ``wlr-data-control`` global lets an out-of-focus
clipboard manager (cliphist / wl-clipboard) observe selections. See
:doc:`bindings` for the keys.

Modes
-----

All three modes share the transport and the tile codec; serve and mirror also
share whole-output capture and global input injection, while graft scopes both
to a single window (see its entry).

- **Serve / headless mode** *(shipped).* The served box has no display; the
  remote view **is** the only output — a virtual head sized to the connecting
  client. The compute-offload case.
- **Mirror mode** *(planned).* The served box has a real display; the server
  streams an existing physical output's scene. The "drive an existing desktop"
  case. Needs no new backend — only a real-CRTC frame source for the server.
- **Graft mode** *(shipped).* Pull a single remote *window* into the local
  workspace as a real, drivable local window. Rather than proxying the app's
  Wayland connection (the waypipe approach), graft **scopes the serve-mode
  pixel stream to one window**: the served WM renders just that window's surface
  tree to its own offscreen buffer and streams its damage tiles, so the capture
  is correct even when the window is occluded, minimized, or on a non-active
  workspace. The viewer presents it as an ordinary ``xdg_toplevel`` the local WM
  tiles/moves/closes like any app, and forwards the natural keyboard/pointer
  input the window receives back to the served window (keyboard focus pinned to
  it, pointer coordinates window-local). Run it with
  ``shoestring-remote-client --graft <selector>`` where ``selector`` matches a
  remote window's ``app_id`` / ``title`` / foreign-toplevel id; the server
  resolves it against the served WM's window list. *v1 limitations:* one graft
  at a time drives cleanly (it moves the served box's single seat); sizing is
  remote-authoritative (a local resize viewport-scales); it is pixels, not
  protocol (no client-side GPU; the cursor is the local one over the toplevel).

Why native serve mode
---------------------

The key fork is **pixel-streaming** (VNC-like) vs **Wayland-protocol-forwarding**
(waypipe-like):

- **waypipe** proxies Wayland clients over ssh and feels local, but it is
  *per-app*, not "a whole shoestring-wm desktop," and cannot mirror an
  already-displayed session. We want the full shoestring experience, so we build
  native serve mode. (waypipe remains the reference for *Graft* mode.)
- **gnome-remote-desktop / KRdp / wayvnc** are pixel-streamers (RDP/VNC) — good
  for whole-desktop and for mirroring, and their encodings (VNC Tight / ZRLE)
  validate the tile + zlib plan. The shoestring difference is that automation and
  observation are first-class *in the compositor*, and the serve protocol is a
  native, damage-tracked tile stream rather than generic VNC.

shoestring-specific wins (all from "both ends are shoestring-wm"): damage-push
straight off the damage tracker (idle ⇒ 0 frames); a structural cursor rendered
locally; exact resolution/scale via a virtual output; input as an existing gated
IPC primitive; structural window/workspace awareness over the query + event
stream; and reuse of the existing consent gates as the entire security model.

See also
--------

- :doc:`bindings` — the machine-axis keys and clipboard binds.
- :doc:`containers` — running a served session in a container.
- :doc:`running` — the headless backend the served box runs on.
- :doc:`ipc` — the remote requests, responses, and events.
