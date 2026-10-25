Remote desktop
==============

shoestring-wm has a **native** remote desktop: connect to another box that is
*also* running shoestring-wm and have it feel like a first-class part of the
local experience rather than "a VNC window I clicked into." Because both ends
run the same compositor, the implementation exploits the shared :doc:`IPC
surface <ipc>` for optimizations a generic RDP/VNC cannot do - chiefly
damage-push streaming straight off the WM's own damage tracker.

This page is the design reference: the goals, the locked decisions, the
architecture, and the streaming/input/consent model. For the user-facing
specifics see :doc:`bindings` (the machine-axis keys), :doc:`containers`
(running a served session in a container), :doc:`running` (the headless
backend), and :doc:`ipc` (the wire requests and events).

Goals and non-goals
-------------------

The remote-desktop tools connect to other shoestring-wm machines the user
controls. They support these use cases, in priority order:

1. **Compute / storage offload (the headline).** A beefy *headless* box runs the
   apps and shoestring-wm; a thin client - a tablet, e.g. a Surface - views and
   drives it over a fast link. The heavy box does the work; the tablet is the
   display and input. This is **serve mode**, and it is what the design is built
   around.
2. **Terminal continuity + clipboard.** The same terminal experience across
   machines, and copy/paste from a remote terminal to the local box - a cheap,
   decoupled clipboard track (see *Clipboard* below).
3. **Drive an existing desktop.** Keep an eye on / occasionally interact with a
   machine that *has* a display (a laptop running Teams or Outlook-in-browser,
   another Fedora/FreeBSD box). This is **mirror mode**, a later delta on serve
   mode.

Explicitly **not** a goal: smooth video or real-time gaming. That would force a
heavy codec (ffmpeg / gstreamer / vaapi), which is against the low-dependency
ethos and a portability burden (FreeBSD). The design optimizes for
mostly-static desktop content - terminals, code, browser UIs on a LAN - instead.

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
  This sidesteps the cross-compositor problem that the login greeter is not
  exposed to portals.
- **Native damage-push** for the streaming substrate (not the pending
  ``ext-image-copy-capture`` protocol) - simpler and strictly more optimal when
  we control both ends.
- **Raw KVM-passthrough** for input (not structured action-forwarding).
- **Explicit node list** for discovery (not mDNS) - matches the named-node ssh
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
*own* keymap and keybinds apply natively: ``Super+H/L`` walks the remote's
workspaces, and its window keys act on its windows. Only the machine-axis keys
(``Super+J/K``), the break-out hotkey (``Super+Escape``), the clipboard bridge
and the screenshot key stay with your local WM. The keys are documented in
:doc:`bindings`.

Architecture
------------

Two standalone binaries plus WM-side plumbing. For the serve / headless case::

   beefy box (no physical display)            thin client (e.g. a Surface)
     shoestring-wm                              shoestring-wm
      ├─ headless backend                        └─ shoestring-remote-client
      │    GPU (GLES) or CPU (pixman)                 ssh socket forward ↔ server
      │    one virtual output                         decode tiles → fullscreen
      │    timer-driven render loop                   render remote cursor locally
      ├─ damage-capture subscription ─ tiles ─>       forward input while active
      ├─ remote gate + registration + chip       local WM: Super+J/K machine axis
      └─ shoestring-remote-server                       + raw-passthrough capture
           listens on a private Unix socket
           captures the virtual output, injects input

``shoestring-remote-server``
    Runs on the box being remoted *into*, added to the WM ``autostart``. A
    privileged IPC bridge: it ``register_remote_server``\ s with the local WM
    (which makes the remote gate selectable), and while the gate is on it listens
    on a private Unix socket, streams the served output's damage tiles
    (``capture_stream``), and injects the client's input back through the WM.
    It reports the viewer count so the "being viewed" indicator lights.

``shoestring-remote-client``
    Runs on the box you sit at. It connects to a server's socket forwarded over
    ssh, decodes and presents
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

Versions and compatibility
--------------------------

The served machine and the viewer are often on different releases, so the
handshake says which protocol each side speaks. The client's first message
(``Hello`` or ``Graft``) and the server's ``Ready`` end with three fields:

- **protocol** - a number that changes only when the wire format changes
  incompatibly. Today it is 1.
- **capabilities** - a bitset of optional features. Each side uses only the
  features both report and ignores bits it doesn't know, so a new optional
  feature adds a bit instead of a protocol version. None are defined yet.
- **software** - the release (``0.10.0``), for messages and logs.

Releases up to 0.9.1 send none of these. Decoders ignore bytes past the fields
they know, so those releases read the new messages unchanged, and a message
without the fields counts as protocol 1 with no capabilities.

When the protocols differ, the server answers with ``Refused`` instead of
``Ready`` and closes the connection. The reason names both sides and says
which one to upgrade, and the client prints it and exits non-zero::

    the server refused: shoestring-remote-server (0.11.0) speaks remote protocol 2,
    but shoestring-remote-client (0.10.0) speaks protocol 1; upgrade the client
    so both match

The server logs the same reason. A client checks the server's ``Ready`` the
same way, so a newer client facing an older server explains the mismatch too.
A client from 0.9.1 or earlier can't read ``Refused``; it just sees the connection
close, and only the server's log says why.

After the handshake, either side skips a whole message whose type it doesn't
know, instead of ending the session, so an addition that an older peer can do
without never disconnects it.

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
  not for full-screen video - which is fine per the goals. No heavy codec.
- **Structural cursor.** The cursor is sent as *position + which* ``CursorIcon``
  and rendered with the **local** theme - crisp, instant, no cursor-in-stream
  lag. Both ends share the cursor vocabulary.
- **Resolution and scale.** The client already sends its output's pixel size
  and scale in ``Hello``, but today the server treats them as advisory: the
  :doc:`headless backend <running>` serves its virtual output at the size it was
  started with (``--output-size`` / ``$SHOESTRING_WM_HEADLESS_SIZE``), and the
  viewer scales the stream to its surface. *Planned:* size the virtual output
  to exactly the client's ``Hello``, so there is no rescale blur.
- **Structural window awareness (free).** The served box already exposes
  ``windows`` (geometry, ``app_id``, title, pid, z-order) and an
  ``event-stream``, so a local bar can show the remote's window/workspace list,
  and it sets up the eventual *Graft* mode.

Input model
-----------

**Raw KVM-passthrough.** While a remote is the active view the local WM captures
all input and forwards it to the remote, which injects it - so the *remote's own
keybinds Just Work*. Only ``Super+J/K`` (switch machine), the break-out hotkey,
the clipboard bridge and the screenshot key are intercepted locally. The grab
reuses the precedent set by the window picker and the keyboard-shortcuts-inhibit
paths. (The alternative - resolving binds locally and forwarding *actions* via
``dispatch_action`` - is more structured but less seamless, and was rejected
for v1.)

**Whose keybinds answer a key** depends on the mode, and the rule is that
exactly one WM does:

- **Serve mode:** the remote's. The viewer is a fullscreen overlay standing in
  for that machine's whole desktop, so a forwarded key takes the same path a
  key on the remote's own keyboard would: its bindings, its window picker, its
  lock screen. The local WM has already kept the few keys above, so the two
  never compete, and no remote window can be maximized over your local
  workspace keys.
- **Graft mode:** yours. A grafted window is one window among your local ones,
  so your WM's bindings apply first, and only the keys it lets through reach
  the grafted app - past the remote WM's binding table, which never acts on
  them. Otherwise a key your local WM doesn't bind could switch workspaces or
  close windows on a remote desktop nobody is looking at.

Clicks follow the same rule. In serve mode a click takes the remote's own
pointer path: it focuses and raises the window clicked, Super+drag moves or
resizes there, and an armed window picker takes the click. In graft mode the
click goes to the grafted window as it is.

So does the mouse wheel. In serve mode a wheel over the remote's bare desktop
switches its workspaces, as it would locally. A touchpad scroll doesn't,
there or locally, so the viewer tells the server which one it was. A viewer
from before that told it nothing, so with an older viewer every scroll
reaches the remote as a plain scroll.

Over IPC this is ``inject_input``'s ``bindings`` flag, which the server sets
for serve sessions; graft uses ``inject_input_to_window``, which never applies
bindings, and automation's raw keys and buttons skip them too (see :doc:`ipc`).

Consent and security
--------------------

The remote gate reuses the existing capability-gate model end to end:

- **The gates are consent, not protection.** Any process running as you on
  the served box can reach the WM's IPC socket and turn the remote gate on
  itself, which opens the listener. The gate keeps sharing off until someone
  means it and shows when it is on; it does not stop software already running
  as you. Keep software you don't trust off that account, or in a container
  without your ``$XDG_RUNTIME_DIR``. See :ref:`ipc-trust-model`.
- The **remote gate** couples the **capture gate** (for the served output) and
  the **automation gate** (for injected input), then opens the listener. It is
  off by default and **greyed until a** ``shoestring-remote-server`` **has
  registered**. Flip it from the bar chip, with ``shoestring-ctl remote on``,
  or - for a headless box or container - from the entrypoint. See
  :doc:`ipc` for ``set_remote`` / ``remote_status`` / ``remote_changed``.
- **Sharing can end itself.** By default the gate stays on until someone
  turns it off, which suits containers and always-on serve boxes. On a
  headless box with no bar to show the "being viewed" chip, give it a lease
  instead: ``shoestring-ctl remote on --for 2h`` turns it off after two
  hours, and ``--until-idle 10m`` once nobody has been connected for ten
  minutes, counting from now if nobody is yet. Both together end it at
  whichever comes first, and each is at most 30 days. When the lease ends, the WM turns the gate off as
  ``remote off`` would: sessions end, the capture and automation gates
  close, and held keys are released. ``shoestring-ctl remote status`` shows
  the time left. There is no config default, since the gate is never turned
  on from the config either; the command that turns it on sets the lease.
- **Nothing a viewer held outlives it.** The server tracks which keys and
  buttons each session holds. When a session ends with some still down (a
  dropped connection mid-chord or mid-drag), it releases the ones no other
  viewer holds. Closing the remote gate, or the server exiting or crashing,
  closes the automation gate, and the WM then releases everything injection
  held itself, since injection is refused from that point.
- **Closing the gate ends every session**, a stalled one included. Each viewer
  gets a ``Bye`` and the connection closes. A viewer that has stopped reading
  (a frozen client, a link that went quiet) can't take that ``Bye``, so after
  half a second the server closes its connection anyway rather than keep it,
  and its session slot, open past the consent. Such a viewer never slows the
  WM or the other viewers either: the WM skips frames for it and sends one
  full frame once it reads again.
- A **"being viewed / controlled" indicator** lights while a client is connected
  - a bar chip alongside MUTE/MIC/CAM, the same privacy-opt-in pattern as the
  capture indicator: never silent, always visible.
- **ssh login is the authentication.** The server listens on a Unix socket in
  ``$XDG_RUNTIME_DIR`` (mode 0600) and accepts a connection only from a process
  running as the same user, checked with ``SO_PEERCRED`` / ``getpeereid``. A
  viewer forwards that socket with ``ssh -L local.sock:remote.sock``, which
  runs as the logged-in user, so reaching the session takes an ssh login as
  that user. Other users on the served box can't connect, and the viewer's end
  of the forward is a 0600 socket too, so other users there can't ride the
  tunnel. That mode comes from ssh's default ``StreamLocalBindMask 0177``, and
  it is the only guard on the viewer's side: the server sees every connection
  through the tunnel as the ssh user, so leave the mask alone and don't loosen
  the forwarded socket. No new keys or passwords, matching the named-node
  workflow. For a container see :doc:`containers`.
- **A locked session streams almost nothing.** While the served box is
  locked, a serve viewer sees one static dark frame, not the lock screen and
  not the windows behind it, and a graft session pauses (a new one is
  refused). Input still reaches the lock screen: once you type, the viewer
  sees the live lock screen for ``[general] lock_preview_secs`` (default 8),
  long enough to watch the password prompt, then the static frame again.
  The desktop comes back on unlock. See :ref:`ipc-session-lock`.
- **TCP is opt-in and unchecked.** ``--tcp ADDR:PORT`` (or
  ``SHOESTRING_REMOTE_TCP``) listens on TCP instead. Nothing checks who connects
  there: on loopback any local user, and on any other address any host that can
  reach it, gets the session. The server warns at startup.
- **TCP beyond loopback takes a second opt-in.** The server refuses an address
  other than loopback, ``0.0.0.0`` included, unless ``--tcp-any-host`` (or
  ``SHOESTRING_REMOTE_TCP_ANY_HOST=1``) is also given. A host name is resolved
  once at startup and checked by the address it resolves to.
  ``shoestring-ctl remote status`` reports where the server listens in
  ``endpoint``.

Clipboard
---------

Cross-machine copy/paste rides the remote session directly: ``Super+Shift+C``
pushes the local selection to the machine you are viewing and ``Super+Shift+V``
pulls the remote's selection into the local one - both explicit and gated by the
remote share. Locally, a ``wlr-data-control`` global lets an out-of-focus
clipboard manager (cliphist / wl-clipboard) observe selections. See
:doc:`bindings` for the keys.

Modes
-----

All three modes share the transport and the tile codec; serve and mirror also
share whole-output capture and global input injection, while graft scopes both
to a single window (see its entry).

- **Serve / headless mode** *(shipped).* The served box has no display; the
  remote view **is** the only output - a virtual head (sized at WM startup for
  now; sizing it to the connecting client is planned) for the connecting
  client. The compute-offload case.
- **Mirror mode** *(planned).* The served box has a real display; the server
  streams an existing physical output's scene. The "drive an existing desktop"
  case. Needs no new backend - only a real-CRTC frame source for the server.
- **Graft mode** *(shipped).* Pull a single remote *window* into the local
  workspace as a real, drivable local window. Rather than proxying the app's
  Wayland connection (the waypipe approach), graft **scopes the serve-mode
  pixel stream to one window**: the served WM renders just that window's surface
  tree to its own offscreen buffer and streams its damage tiles, so the capture
  is correct even when the window is occluded, minimized, or on a non-active
  workspace. The viewer presents it as an ordinary ``xdg_toplevel`` the local WM
  tiles/moves/closes like any app, and forwards the natural keyboard/pointer
  input the window receives back to the served window (keyboard focus pinned to
  it, pointer coordinates window-local). Your local keybinds apply to it; the
  remote WM's don't (see *Input model*). Run it with
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
- **gnome-remote-desktop / KRdp / wayvnc** are pixel-streamers (RDP/VNC) - good
  for whole-desktop and for mirroring, and their encodings (VNC Tight / ZRLE)
  validate the tile + zlib plan. The shoestring difference is that automation and
  observation are first-class *in the compositor*, and the serve protocol is a
  native, damage-tracked tile stream rather than generic VNC.

shoestring-specific wins (all from "both ends are shoestring-wm"): damage-push
straight off the damage tracker (idle ⇒ 0 frames); a structural cursor rendered
locally; a virtual output (sized to the client, once that lands); input as an existing gated
IPC primitive; structural window/workspace awareness over the query + event
stream; and reuse of the existing consent gates as the entire security model.

See also
--------

- :doc:`bindings` - the machine-axis keys and clipboard binds.
- :doc:`containers` - running a served session in a container.
- :doc:`running` - the headless backend the served box runs on.
- :doc:`ipc` - the remote requests, responses, and events.
