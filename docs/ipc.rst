IPC
===

shoestring-wm exposes a small unix-socket IPC for querying state and
subscribing to events. The protocol is newline-delimited JSON: one
``Request`` object terminated by ``\n`` in, one ``Response`` object out;
for the event-stream request, the server keeps the connection open and
pushes ``Event`` objects forever (one per line) until the client
disconnects.

The connection is **one request per connection**: after it writes the one
response, the server closes the connection (the streaming requests
``event_stream`` and ``metrics_stream`` are the exception — they stay open and
push). So a client issues each request on a fresh connection, exactly as
``shoestring-ctl`` opens one connection per invocation. This is intentional —
it keeps the server's per-client state trivial and bounds it.

Socket location
---------------

The WM listens on
``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`` and exports the
path as ``$SHOESTRING_WM_SOCKET`` to every child process.

Clients should:

1. Prefer ``$SHOESTRING_WM_SOCKET`` when set.
2. Fall back to the conventional path above.

The reference client (``shoestring-ctl``) and the bar both use the
``shoestring-ipc`` crate's ``client_socket_path()`` helper which
implements this fallback.

.. _ipc-client-libraries:

Client libraries
----------------

Besides the Rust ``shoestring-ipc`` crate and the ``shoestring-ctl`` CLI, the
repository ships thin, dependency-free client libraries for **Python**, **Go**,
and **TypeScript/Node** under `clients/ <https://github.com/szarta/shoestring-wm/tree/main/clients>`__
(see ``clients/README.md``). Each one implements the socket discovery above, the
one-request-per-connection round-trip, and an event-stream iterator, with
ergonomic wrappers for every request (``workspaces()``, ``windows()``,
``find_windows(...)``, ``inject_key(...)``, …).

The libraries return responses as the language's native JSON value rather than
a generated schema, so they ride the additive protocol changes described below
for free. They are the non-Rust analogue of the i3/Sway ``i3ipc`` bindings.

.. code-block:: python

   import shoestring_ipc

   wm = shoestring_ipc.Client()           # auto-discovers the socket
   print(wm.workspaces()["active"])
   for event in wm.events():              # blocks, one event per line
       print(event)

.. _ipc-stability:

Stability and versioning
------------------------

The wire format is **stabilizing, not yet frozen**. shoestring-wm is
pre-1.0: no breaking change has been made since the protocol shipped in
0.1.0, and the intent is to keep it that way, but the format is only
contractually frozen at 1.0. From 1.0 onward the wire format follows
semver — breaking changes ride a major bump.

There is **no in-band protocol version** field and no ``version``
request. The single reference point is the WM/CLI version, which tracks
the workspace ``Cargo.toml`` version::

    $ shoestring-ctl --version
    shoestring-ctl 0.4.0

A client that needs to know whether a newer request or field exists
should key off that version, or simply issue the request and handle a
possible ``error`` reply.

Compatibility rules
~~~~~~~~~~~~~~~~~~~~

These are the guarantees the protocol holds *within* a release series,
and the rules new server code follows when it touches the surface:

**Additive evolution.** New request types, response types, event types,
fields, enum variants, and metric names may appear in any release. The
catalogues are **append-only**: an existing request/response/event/field
is never renamed or repurposed without a major bump. The metric-name set
is the original precedent (see :ref:`the metrics note <ipc-metrics>`),
and the same discipline applies to the whole surface.

**New fields are optional and omittable.** Every field added to a
response or event is serialized with ``#[serde(default,
skip_serializing_if = …)]`` — it is left off the wire when empty and
defaults when absent. A client built against an older schema keeps
deserializing newer payloads unchanged. Examples already on the wire:
``WindowSummary.geometry`` / ``.z`` / ``.sticky`` /
``.always_on_top`` and the ``workspaces`` reply's ``names`` array, all
of which older builds simply didn't emit.

**Unknown-tolerant outbound, strict inbound.** Responses and events are
**not** ``deny_unknown_fields``: a client should *ignore* any field — or
any ``type`` discriminator — it does not recognize, so a newer WM can add
to the stream without breaking it. Requests, by contrast, **are**
``deny_unknown_fields``: the WM rejects a request carrying a field it
does not know. Do not send a field to a WM that predates it — gate on the
version above. (This asymmetry is deliberate: tolerant readers make
forward-compat cheap, while strict request parsing turns a typo or a
too-new field into a loud ``error`` instead of a silently-ignored
option.)

**Error text is not an API.** Branch on the presence of
``{"type": "error"}``, not on the human-readable ``message`` string,
which may be reworded at any time. The one exception is explicitly
called out: the automation-gate refusal prefix is *"stable enough to
scrape on"* (see :ref:`the automation gate <ipc-automation-gate>`).

**Experimental / internal surfaces.** Everything documented here is
considered stable under the rules above, with two exceptions still
expected to evolve:

- ``metrics_stream`` interval handling — v1 clamps the push interval *up*
  to the sample interval and cannot push faster than it samples; finer
  control may be added later.
- The media-privacy ``report_media`` request is an internal contract
  between the WM and the trusted ``shoestring-mediad`` monitor, not a
  general client API; its shape may change with that pair.

Requests
--------

Each request is a JSON object with a ``type`` discriminator:

.. list-table::
   :header-rows: 1
   :widths: 22 78

   * - Request
     - Meaning
   * - ``{"type": "workspaces"}``
     - List workspaces and which one is currently active.
   * - ``{"type": "windows"}``
     - List every mapped window (across all workspaces).
   * - ``{"type": "outputs"}``
     - List every connected output.
   * - ``{"type": "inputs"}``
     - List every connected input device (keyboards, pointers, touchpads,
       tablets) with its libinput identity and capabilities — the input
       analogue of ``outputs``. Reply is ``inputs``. Read-only and not gated
       by automation. Empty under the nested winit backend, which has no
       libinput devices.
   * - ``{"type": "get_tree"}``
     - Snapshot the full window tree: outputs with their logical
       placement, plus each workspace and the windows on it (geometry,
       stacking order, and the output each window sits on). Reply is
       ``tree``. The ``swaymsg -t get_tree`` analogue — the canonical
       query for layout scripting. Read-only and not gated by automation.
   * - ``{"type": "event_stream"}``
     - Switch into streaming mode. Server replies once with ``Ok`` then
       pushes events.
   * - ``{"type": "metrics"}``
     - Snapshot the diagnostics registry — process resource gauges, WM
       counts, and per-client surface gauges. Reply is ``metrics``. Read-only and not gated by
       automation. Sampled fresh on demand, so it answers even when
       ``[diagnostics].enabled`` is off.
   * - ``{"type": "metrics_stream", "interval_ms": 1000}``
     - Subscribe to a stream of ``metrics`` events: the server replies
       once with ``ok`` then pushes one sample per tick until the client
       disconnects (the turn-on/off diagnostics pipe). ``interval_ms``
       (optional) is the desired push interval, clamped *up* to
       ``[diagnostics].sample_interval_ms`` — v1 can't push faster than
       it samples. Requires ``[diagnostics].enabled = true``; returns
       ``error`` otherwise.
   * - ``{"type": "inject_key", "keysym": "Return", "modifiers": ["Super", "Shift"]}``
     - Synthesize a keypress (press + release) targeting the focused
       surface. ``keysym`` is any X keysym name understood by
       ``xkb_keysym_from_name`` (e.g. ``"Return"``, ``"F5"``,
       ``"BackSpace"``, ``"q"``). ``modifiers`` (optional, default
       ``[]``) are pressed before the keysym and released after in
       reverse order — the focused surface sees the chord with the
       right modifier mask, the same way ``xdotool key super+shift+q``
       works. Modifier names are case-insensitive and follow the
       keybind alias table: ``super`` / ``logo`` / ``mod4`` / ``win``,
       ``ctrl`` / ``control``, ``alt`` / ``mod1``, ``shift``; anything
       else falls through to a raw keysym name (``Hyper_L`` etc.).
       Chords the WM consumes (e.g. ``Super+Shift+Q``) won't reach the
       focused surface — use ``dispatch_action`` for those.
   * - ``{"type": "inject_text", "text": "hello"}``
     - Type a literal string into the focused surface. v1 supports ASCII
       letters, digits, and space; other codepoints return an ``error``
       so the caller knows to break the string up.
   * - ``{"type": "inject_click", "button": "left"}``
     - Synthesize a mouse click. ``button`` is one of ``"left"`` /
       ``"right"`` / ``"middle"``, or a numeric Linux ``BTN_*`` code as a
       string. Pass ``"x"`` and ``"y"`` (both, as numbers) to move the
       pointer to those compositor-space coordinates first. Optional
       ``"count"`` repeats the press/release cycle at the same spot —
       ``2`` for a double-click; absent means 1.
   * - ``{"type": "inject_click_to_window", "id": "<ft-id>", "button": "left", "wx": 12.0, "wy": 34.0}``
     - Like ``inject_click`` but ``wx`` / ``wy`` are **window-local** logical
       pixels (``0,0`` = the window's top-left) relative to the toplevel named
       by ``id`` (a foreign-toplevel identifier from ``windows``). The WM
       translates them by the window's current on-screen origin, so callers stay
       immune to where the compositor placed the window — the key primitive for
       automating apps whose dialogs spawn at variable positions. Runs the same
       click-to-focus + press/release as ``inject_click`` (so a following
       ``inject_text`` lands in the clicked window); ``"count"`` gives a
       double-click. Returns an ``error`` if the window is unknown or not
       currently mapped. Gated by the automation gate.
   * - ``{"type": "drag", "from_x": 10.0, "from_y": 20.0, "to_x": 200.0, "to_y": 20.0}``
     - Press ``button`` (default ``"left"``) at ``from``, move to ``to`` with
       interpolated motion, then release — the press/move/release the space map
       and other drag surfaces need, without hand-composing it from ``move_mouse``
       and raw button events. Coordinates are compositor-space, unless ``"id"`` (a
       foreign-toplevel identifier) is set, in which case ``from`` / ``to`` are
       window-local like ``inject_click_to_window``. Focus is pinned to the
       surface under the press for the whole gesture (a real implicit grab), so a
       drag that slides off the pressed widget still reads as one continuous
       drag. Returns an ``error`` if ``id`` is unknown/unmapped or ``button`` is
       bad. Gated by the automation gate.
   * - ``{"type": "move_mouse", "x": 100.0, "y": 200.0}``
     - Move the pointer to compositor-space ``(x, y)`` without clicking.
       Parity with ``xdotool mousemove``; useful for hover-only tests.
       For a button-held drag prefer ``drag`` over composing this by hand.
       Does not change keyboard focus.
   * - ``{"type": "inject_input", "event": {"kind": "key", "keycode": 30, "pressed": true}}``
     - Inject one **raw input transition** straight into the seat — the
       KVM-passthrough primitive behind remote-desktop serve mode. ``event``
       is a tagged object selected by ``"kind"``: ``key`` (``keycode`` is an
       evdev keycode *before* XKB's ``+8``; forwarded past the binding table so
       the served machine's own keymap/binds apply), ``button``
       (``button`` is an evdev code, ``BTN_LEFT`` = ``272``), ``motion``
       (``x`` / ``y`` compositor-space), and ``axis`` (``horizontal`` /
       ``vertical`` scroll deltas). Each carries ``"pressed"`` for the
       down/up variants. Unlike ``inject_key`` / ``inject_click`` (atomic
       tap / press+release) each is a single event, so a held key, a chord,
       or a drag reproduces exactly. Gated by the automation gate.
   * - ``{"type": "inject_input_to_window", "id": "<ft-id>", "event": {…}}``
     - Like ``inject_input`` but delivered to the **single window** named by
       ``id`` (a foreign-toplevel identifier from ``windows``) rather than the
       global seat — the input primitive behind remote-desktop *graft mode*.
       Keyboard focus is pinned to that window and pointer ``motion`` ``x`` /
       ``y`` are interpreted as **window-local** logical pixels, so input lands
       correctly even when the window is occluded or on a non-active workspace.
       ``event`` is the same tagged ``key`` / ``button`` / ``motion`` / ``axis``
       currency as ``inject_input``. Gated by the automation gate. Because it
       drives the single global seat, it moves the served box's own focus to the
       target window (one graft at a time).
   * - ``{"type": "get_clipboard"}`` (optional ``"primary": true``)
     - Read the WM's current selection and reply with ``clipboard``. The WM
       picks the best text mime the selection owner offers
       (``text/plain;charset=utf-8`` → ``text/plain`` → ``UTF8_STRING`` → …),
       reads the bytes out of band, and returns them. ``primary`` reads the
       primary (middle-click) selection instead of the clipboard. The reply is
       **deferred** — the WM holds the connection until the owning client
       finishes writing. An empty or non-text selection replies with no ``mime``
       and empty ``data``. This is the viewer-box read behind cross-machine
       copy/paste; locally it is a focus-free ``wl-paste``. Gated by automation.
   * - ``{"type": "set_clipboard", "mime": "text/plain;charset=utf-8", "data": [...]}``
       (optional ``"primary": true``)
     - Take ownership of the selection and serve ``data`` (a byte array) under
       ``mime`` to anything that pastes. A text ``mime`` fans out to the standard
       text aliases (``text/plain``, ``UTF8_STRING``, ``STRING``, ``TEXT``) so
       every paster finds the atom it asks for. ``primary`` sets the primary
       selection. The receiving half of cross-machine copy/paste; locally a
       focus-free ``wl-copy``. Reply is ``ok``. Gated by automation.
   * - ``{"type": "paste", "text": "…", "keysym": "v", "modifiers": ["Ctrl"]}``
     - Enter text into the focused surface **via the clipboard**. If ``text`` is
       present, set the selection to it (UTF-8 ``text/plain``) first; then
       synthesize the paste chord (``modifiers`` + ``keysym``, defaulting to
       ``Ctrl`` + ``v``) so the focused client requests the selection back. All
       three fields are optional: omit ``text`` to paste the current selection,
       and override ``keysym`` / ``modifiers`` for apps whose paste isn't
       ``Ctrl+V`` (e.g. ``Ctrl+Shift+v`` or ``Shift+Insert`` for terminals).
       Unlike ``inject_text`` (ASCII letters/digits/space only, per keystroke),
       this carries arbitrary Unicode, punctuation, and long strings, since the
       bytes travel through the selection rather than the keymap. ``shoestring-ctl
       type --via-clipboard`` and ``shoestring-ctl paste`` drive it. Reply is
       ``ok`` (or ``error`` if the paste chord isn't in the current keymap).
       Gated by automation.
   * - ``{"type": "pointer_position"}``
     - Read the current pointer location. Reply is ``pointer_position``.
       Read-only and not gated by automation.
   * - ``{"type": "pick_window"}``
     - Enter interactive picker mode: the WM waits for the user's next
       click and replies with ``picked_window``. Pointer/keyboard input
       is intercepted while the picker is up — left-click resolves to a
       toplevel, right-click and Escape cancel, other keys are
       swallowed. While the picker is active the pointer shows an
       ``xkill``-style kill cursor (a red ``×``) on every output,
       reverting to the normal cursor once it resolves. Only one picker
       may be active at a time. If the client disconnects mid-pick the
       picker is cancelled. Synthetic input resolves the picker exactly
       like real input: an armed picker intercepts ``inject_click``
       (left-click picks the toplevel under the pointer, any other button
       cancels) and ``inject_key`` (Escape cancels, other keys are
       swallowed) before the event reaches the focused surface — so an
       agent can arm and resolve the picker entirely over IPC.
   * - ``{"type": "close_window", "id": "..."}``
     - Send ``xdg_toplevel.close`` to the window with the given
       ``ext-foreign-toplevel-list-v1`` identifier. The client may
       still surface a save-prompt rather than exiting immediately.
       Returns ``error`` if no window matches.
   * - ``{"type": "kill_window", "id": "..."}``
     - Force-kill that window — the SIGKILL to ``close_window``'s polite
       request. The WM terminates the *owning process* rather than asking
       the client to close: the peer-credential pid for a Wayland client,
       or the window's real X client pid (XRes ``LOCAL_CLIENT_PID``, **not**
       XWayland's) for an X11 window. For windows that ignore a close
       (mid-session games, hung apps). Backs ``shoestring-kill -f``. Returns
       ``error`` if no window matches or the owning pid can't be resolved.
   * - ``{"type": "focus_window", "id": "..."}``
     - Focus the window with the given identifier. Unminimizes it if
       needed and switches to its workspace if it lives elsewhere,
       then raises + activates it the same way a click does. Returns
       ``error`` if no window matches.
   * - ``{"type": "raise_window", "id": "..."}``
     - Raise the window with the given identifier to the top of the
       stacking order. A **pure restack**: keyboard focus and the active
       workspace are left untouched (contrast ``focus_window``, which
       raises *and* focuses). A no-op when the window is minimized or on a
       non-active workspace — it isn't in the mapped stack — but the
       lookup still succeeds. Broadcasts a ``window_restacked`` event.
       Returns ``error`` if no window matches. Not gated by automation.
   * - ``{"type": "lower_window", "id": "..."}``
     - Lower the window with the given identifier to the bottom of the
       stacking order. The complement of ``raise_window``; same
       focus/gating/no-op semantics.
   * - ``{"type": "set_window_sticky", "id": "...", "sticky": <bool>}``
     - Set or clear the **sticky** flag (show on all workspaces) on the
       window with the given identifier. A sticky window stays mapped across
       workspace switches and is reported on whatever workspace is active.
       Broadcasts a ``window_sticky_changed`` event. Returns ``error`` if no
       window matches. Not gated by automation.
   * - ``{"type": "set_window_always_on_top", "id": "...", "always_on_top": <bool>}``
     - Set or clear the **always-on-top** flag on the window with the given
       identifier. An always-on-top window stays above all ordinary windows
       in the stacking order (but below layer-shell bars/menus). Broadcasts a
       ``window_always_on_top_changed`` event. Returns ``error`` if no window
       matches. Not gated by automation.
   * - ``{"type": "set_window_name", "id": "...", "name": "..."}``
     - Override the display name of the window with the given identifier.
       The override wins over the client's ``xdg_toplevel`` title
       everywhere the WM reports a title — the ``windows`` and
       ``get_tree`` snapshots, the ``find_windows`` match surface, and
       the ``window_title_changed`` event — so a bar or window-jump menu
       shows and matches on it. An empty ``name`` clears the override and
       reverts to the client's own title. ``app_id`` is never affected,
       and the override is **not** forwarded to
       wlr-foreign-toplevel-management taskbars (they keep the raw client
       title). The override is keyed by the live window and dropped when
       it closes — it does not persist. Setting or clearing it broadcasts
       a ``window_title_changed`` event with the new effective title.
       Returns ``error`` if no window matches. Not gated by automation.
   * - ``{"type": "move_window_to_workspace", "id": "...", "index": <1-based>}``
     - Move the window with the given identifier to workspace ``index``
       (1-based). The per-id, focus-independent counterpart to the
       ``move-window-to-workspace`` *action* (which only moves the focused
       window): this targets an arbitrary window and does **not** switch the
       active workspace or steal focus, so a TUI or taskbar can shuffle a
       background window without disturbing the user's view. Moving a window
       *onto* the active workspace maps it into view; moving it *off* unmaps it
       (refocusing the next window if the moved one held focus). A no-op when
       the window is already on ``index``. Broadcasts a
       ``window_moved_to_workspace`` event. Returns ``error`` if no window
       matches, ``index`` is out of range, or the window is sticky (un-stick it
       first) or minimized (restore it first). Not gated by automation.
   * - ``{"type": "set_window_minimized", "id": "...", "minimized": <bool>}``
     - Minimize (hide) or restore the window with the given identifier,
       regardless of which window has focus — the per-id counterpart to the
       ``minimize`` action (which toggles the *focused* window). Idempotent.
       Returns ``error`` if no window matches. Not gated by automation.
   * - ``{"type": "set_window_maximized", "id": "...", "maximized": <bool>}``
     - Maximize (fill the work area) or restore the saved floating rectangle of
       the window with the given identifier, regardless of focus — the per-id
       counterpart to the ``maximize`` action. Idempotent. Returns ``error`` if
       no window matches. Not gated by automation.
   * - ``{"type": "find_windows", "title": "...", "app_id": "..."}``
     - List every mapped window whose ``title`` and ``app_id`` match
       the supplied regular expressions. Each filter is independent and
       both are AND-ed; an unset filter matches everything. Patterns
       use Rust ``regex`` syntax (Perl-like, no backrefs) and are not
       anchored — ``firefox`` matches anywhere; use ``^firefox$`` for
       exact match. Reply reuses the ``windows`` response shape; bad
       regex returns ``error``.
   * - ``{"type": "dispatch_action", "action": {...}}``
     - Run a named bind ``Action`` server-side, exactly as if a keybind
       had fired. Unlike ``inject_key``, this bypasses the focused
       surface and goes through the WM's action dispatcher —
       ``Super+Shift+Q`` is consumed by the WM and won't fire from
       ``inject_key``, but ``{"type":"dispatch_action","action":{"type":"quit"}}``
       will. ``action`` is the same JSON shape used by ``[[bindings]]``
       entries in the config file (e.g. ``{"type":"focus-workspace","index":3}``).
       Gated by the automation gate.
   * - ``{"type": "lock"}``
     - Spawn ``[general].lock_command``. Replies ``ok`` immediately;
       the locker drives the ``ext-session-lock-v1`` handshake itself.
   * - ``{"type": "quit"}``
     - Exit the WM, via the same ``confirm_action`` dialog as the
       ``Super+Shift+Q`` keybind — the WM only quits on the user's *Yes*.
       Replies ``ok`` once the dialog is shown (not on acceptance).
       **Ungated**: the human confirmation *is* the authorization, so the
       bar's control menu can offer Log out without the automation gate
       that ``dispatch_action {"type":"quit"}`` requires.
   * - ``{"type": "power_off"}`` / ``{"type": "reboot"}`` / ``{"type": "suspend"}``
     - Shut down / reboot / suspend the machine. Each pops the same
       confirm dialog as ``quit`` and only acts on *Yes*; reply is ``ok``
       once the dialog is shown. **Ungated** for the same reason as
       ``quit``. The WM owns no power policy — it shells out to the first
       available tool in a fallback chain (``systemctl`` → ``loginctl`` →
       ``shutdown(8)``/``zzz``) so systemd, elogind, and bare-init/FreeBSD
       all work. Note this is the *menu*/IPC path; the hardware power key
       is still handled by ``logind`` unless you set ``HandlePowerKey`` —
       see :doc:`install`.
   * - ``{"type": "set_automation", "enabled": true}``
     - Flip the runtime automation gate. Reply is ``automation`` with
       the new state; ``automation_changed`` is broadcast to
       subscribers when the value actually changes. Not persisted to
       disk. Automation is a superset of screen capture: this flips the
       screen-capture gate the same way (so ``screenshot`` works under
       automation alone, and turning automation off returns the session
       to no-capture), emitting ``screen_capture_changed`` when capture
       follows.
   * - ``{"type": "automation_status"}``
     - Read the current automation gate state without changing it.
       Reply is ``automation``.
   * - ``{"type": "set_screen_capture", "enabled": true}``
     - Flip the runtime screen-capture gate. When enabled the WM
       advertises the ``zwlr_screencopy_manager_v1`` global so capture
       tools (OBS, grim, the screen-share portal) can read the screen;
       when disabled the global is withdrawn and any capture is refused.
       Reply is ``screen_capture`` with the new state;
       ``screen_capture_changed`` is broadcast when the value actually
       changes. Not persisted to disk.
   * - ``{"type": "screen_capture_status"}``
     - Read the current screen-capture gate state without changing it.
       Reply is ``screen_capture``.
   * - ``{"type": "capture_stream", "output": null}``
     - Subscribe to a **streaming damage capture** of an output — the
       native damage-push primitive behind remote-desktop serve mode.
       ``output`` is the output name (omit / null → first output).
       **Gated by the screen-capture gate**: returns ``error`` when the
       gate is off, and an active stream is torn down (a ``Bye`` frame,
       then disconnect) if the gate is flipped off mid-stream. Unlike
       every other request the reply is **not** pure newline-JSON: the
       server writes one ``ok`` line, then *upgrades the connection* to a
       binary stream of length-prefixed ``shoestring_remote::ServerMessage``
       frames — a ``Ready`` (served size/scale/format) and full-output
       first frame, then one ``Frame`` of only the *damaged tiles* per
       render (zlib-compressed), plus ``Resize`` on output change. An idle
       desktop produces no frames. Consumed by ``shoestring-remote-server``,
       which relays the frames over an ssh tunnel; see the
       ``shoestring-remote`` crate for the wire format.
   * - ``{"type": "capture_window", "id": "<ft-id>"}``
     - Subscribe to a **streaming damage capture of a single window** — the
       capture primitive behind remote-desktop *graft mode*. ``id`` is a
       foreign-toplevel identifier from ``windows``. Unlike ``capture_stream``
       the window is rendered to its own offscreen buffer, so the stream is
       correct even when the window is occluded, minimized, or on a non-active
       workspace, and tiles carry **window-local** coordinates. **Gated by the
       screen-capture gate** and torn down on gate-off, identically to
       ``capture_stream``; the reply uses the same connection upgrade to a binary
       ``ServerMessage`` stream (``Ready``, a ``Meta`` with the window's
       ``app_id``/``title``, then ``Frame``/``Resize``, and a ``Bye`` when the
       window closes).
   * - ``{"type": "register_remote_server"}``
     - Register this connection as **the** remote-desktop server
       (``shoestring-remote-server``), the served-box handshake. Sent once at
       the server's startup so the WM knows a server is available — which is
       what makes the **remote gate** selectable (greyed until then). The
       registration lives for the connection's lifetime: when the server
       disconnects the WM clears it and forces the gate off. One server
       registers at a time; a second registration replaces the first. Reply is
       ``remote``, then ``remote_changed`` is pushed to this connection so the
       server learns of later gate flips.
   * - ``{"type": "set_remote", "enabled": true}``
     - Toggle the runtime **remote gate** — the consent switch for serve mode.
       Enabling *couples* the capture gate (so the served output can be
       streamed) and the automation gate (so client input can be injected) and
       opens the server's listener. Refused with ``error`` unless a server has
       registered (``register_remote_server``). Reply is ``remote``;
       ``remote_changed`` is broadcast when the value changes. Runtime-only, not
       persisted — driven by the bar chip on a desktop, or by
       ``shoestring-ctl remote on`` (the headless/container entrypoint). See
       :doc:`remote`.
   * - ``{"type": "remote_status"}``
     - Read the remote-gate state without changing it. Reply is ``remote``
       (``enabled`` / ``server_available`` / ``viewers``). Read-only, not gated.
   * - ``{"type": "report_remote_viewers", "viewers": 2}``
     - Report the number of connected viewers/controllers. Sent by the
       ``shoestring-remote-server`` as clients connect and leave; the WM caches
       it and broadcasts ``remote_changed`` so the bar's "being viewed"
       indicator can light. Reply is ``remote``.
   * - ``{"type": "register_remote_client", "label": "dev-107", "width": 1920, "height": 1080}``
     - Register this connection as a **viewable machine** on the local
       machine-axis — the viewer-box half of remote desktop. A
       ``shoestring-remote-client`` that has tunnelled out to a remote box
       registers here so the user can ``Super+J/K`` to it; index 0 is always the
       local machine and registrations take index 1.. in arrival order. Unlike
       ``register_remote_server`` (one served box, single registration) **many**
       clients may register. ``width`` / ``height`` are the remote's negotiated
       pixel size, used to clamp the forwarded virtual pointer. The connection
       becomes an event subscriber and is added to the axis for its lifetime:
       disconnecting removes the machine and, if it was the active view, returns
       input to local. While this client is the active view the WM pushes every
       local input event to it as ``captured_input``. Reply is ``remote_clients``.
   * - ``{"type": "remote_client_status"}``
     - Read the machine-axis without changing it: the registered remote machines
       (index 1..) and the active view index (0 = local). Reply is
       ``remote_clients``. Read-only, not gated.
   * - ``{"type": "set_view", "index": 1}``
     - Switch the active machine-axis view to ``index`` (0 = local, 1.. = a
       registered remote), clamped to the machine count. The same internal path
       the ``Super+J/K`` / break-out keybinds drive, exposed for clients and
       tests. Entering a remote view starts input capture; ``index`` 0 stops it.
       Broadcasts ``view_changed``. Reply is ``remote_clients``.
   * - ``{"type": "media_status"}``
     - Read the last media-privacy snapshot the WM holds (default-sink
       mute, microphone mute, camera-in-use). Reply is ``media``, whose
       ``state`` is ``null`` until the ``shoestring-mediad`` monitor first
       reports. Read-only, not gated.
   * - ``{"type": "set_audio_mute", "enabled": true}``
     - Mute/unmute the default audio output. The WM does not mute anything
       itself — it spawns ``shoestring-mediad`` to flip PipeWire's real
       default-sink mute; the new live state then arrives back via
       ``report_media`` and a ``media_changed`` event. Reply is ``ok``.
   * - ``{"type": "set_mic_mute", "enabled": true}``
     - Mute/unmute the default microphone (the mic analogue of
       ``set_audio_mute``). Honest caveat: this stream-mutes the source —
       capturing apps get silence — but does **not** prevent a device
       open, the same guarantee as a hardware mic key. Reply is ``ok``.
   * - ``{"type": "report_media", "audio_muted": false, "mic_muted": false, "camera_active": false}``
     - Push a fresh media-privacy snapshot to the WM. Sent by the trusted
       ``shoestring-mediad`` monitor (which links PipeWire), **not** by
       ordinary clients: the WM caches it and broadcasts ``media_changed``
       when it differs. There is no camera off-switch — ``camera_active``
       is status only. Reply is ``ok``.
   * - ``{"type": "reload_config"}``
     - Re-read the TOML config the WM was launched with and recompile
       the binding table. Broadcasts ``config_reloaded`` on success.
       Reply is ``ok`` or ``error``.
   * - ``{"type": "screenshot", "output": null, "region": null, "path": null}``
     - Capture a PNG via the WM's wlr-screencopy server. ``output`` is
       the output name (omit / null → first advertised output).
       ``region`` is ``{"x":..,"y":..,"w":..,"h":..}`` in the named
       output's logical coords and requires ``output`` to be set.
       ``path`` (optional) writes the PNG there instead of the default
       ``$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png`` (parent dirs are
       created). Reply is ``screenshot`` with the absolute path. Gated by
       the automation gate.
   * - ``{"type": "screenshot_window", "id": "<ft-id>", "path": null}``
     - Capture a **single toplevel**, cropped to just that window, as a PNG.
       ``id`` is a foreign-toplevel identifier from ``windows``. The window's
       own surface tree is rendered to an offscreen buffer, so the capture is
       correct even when the window is occluded or on another workspace.
       ``path`` behaves as on ``screenshot``. Reply is ``screenshot`` with the
       absolute path (same shape as ``screenshot``), or an ``error`` if the
       window is unknown or the active backend can't render a window offscreen
       (the ``udev``/DRM backend can't; ``winit`` and ``headless`` can). Gated
       by the automation gate. Both ``screenshot`` and ``screenshot_window``
       back ``shoestring-ctl screenshot --stdout``: ``ctl`` passes a
       ``$XDG_RUNTIME_DIR`` (tmpfs) ``path``, then streams that file to its own
       stdout and unlinks it — so a capture pipes without a large PNG crossing
       the IPC socket.
   * - ``{"type": "run_command", "argv": ["...", ...], "timeout_ms": null}``
     - Spawn a child process under the WM's environment and return its
       captured output once it exits. ``argv`` must be non-empty;
       ``timeout_ms`` (optional) sends ``SIGKILL`` after the given
       milliseconds. Output is capped at 64 KiB per stream; extra bytes
       are drained but discarded and ``truncated`` is set. Reply is
       ``command_result``. Optional ``"cwd"`` runs the child in that working
       directory (Stars!'s save dialog defaults to the process cwd); optional
       ``"env"`` is a list of ``["KEY", "VALUE"]`` pairs layered onto the
       inherited environment. Optional ``"detach": true`` spawns the child in
       its own session (``setsid``, stdio to ``/dev/null``) and replies
       immediately with ``command_started`` carrying its PID — the WM does not
       wait for it or capture output, so one long-lived WM can launch many
       independent jobs. ``timeout_ms`` is ignored with ``detach``. Gated by the
       automation gate.
   * - ``{"type": "set_gamma", "output": null, "temperature": 3000, "brightness": null, "gamma": null}``
     - Drive the color temperature of an output's CRTC gamma ramp (the
       same machinery ``zwlr_gamma_control_v1`` clients like gammastep
       use). ``output`` is the output name (omit / null → every KMS
       output). ``temperature`` is the whitepoint in kelvin
       (1000–25000; 6500 ≈ neutral, lower = warmer). ``brightness``
       (optional, 0.1–1.0, default 1.0) scales the ramp peak; ``gamma``
       (optional, 0.1–10.0, default 1.0) is the gamma exponent. Takes
       exclusive ownership of each target output, evicting any
       wlr-gamma-control client holding it. KMS-only — returns ``error``
       on a winit/non-DRM output or build. Reply is ``ok`` with the
       count of outputs affected. **Not** gated by the automation gate.
   * - ``{"type": "reset_gamma", "output": null}``
     - Restore the original (pre-``set_gamma``) ramp on outputs whose
       gamma this IPC owns and release them. ``output`` selects one by
       name (omit / null → all IPC-owned outputs). Does not disturb
       outputs currently held by a wlr-gamma-control client. Reply is
       ``ok`` with the count restored. KMS-only. **Not** gated by the
       automation gate.

Injected key and click events bypass the WM's binding table — a scripted
``Super+q`` will NOT trigger the ``Quit`` binding. Use
``dispatch_action`` (above) for that path.

.. _ipc-automation-gate:

Automation gate
~~~~~~~~~~~~~~~

The following requests refuse with an ``error`` while
``[general].automation_enabled`` is off (and the CLI flag
``--enable-automation`` / the IPC ``set_automation`` haven't flipped
it): ``inject_key``, ``inject_text``, ``inject_click``,
``inject_click_to_window``, ``drag``, ``move_mouse``,
``inject_input``, ``get_clipboard``, ``set_clipboard``, ``paste``,
``dispatch_action``,
``screenshot``, ``screenshot_window``, ``run_command``. The error message
is stable enough to scrape on:
``automation disabled: enable with `shoestring-ctl automation on`...``.

Because ``screenshot`` also needs the ``zwlr_screencopy_manager_v1``
global (raised only by the screen-capture gate), enabling automation
*also* enables screen capture — automation is a superset, so
``automation on`` alone is enough to capture and ``automation off``
returns the session to no-capture.

Responses
---------

The server replies with a single JSON object tagged by ``type``:

``ok``
    Generic success acknowledgement. Sent in reply to ``event_stream``
    before the event stream starts.

``workspaces``
    ``{"type": "workspaces", "active": <1..count>, "count": <int>, "names": [<str>, ...]}``.
    ``count`` follows ``[workspaces].count`` in the WM config (default
    16). ``names`` is a length-``count`` array of display strings; empty
    string means "use the number". The field is omitted (or empty) on
    older WM builds that pre-dated workspace naming; clients with
    ``#[serde(default)]`` deserialize either shape.

``windows``
    ``{"type": "windows", "windows": [WindowSummary, ...]}``

``outputs``
    ``{"type": "outputs", "outputs": [OutputSummary, ...]}``

``inputs``
    ``{"type": "inputs", "inputs": [InputSummary, ...]}``

``tree``
    ``{"type": "tree", "outputs": [OutputNode, ...], "workspaces": [WorkspaceNode, ...], "minimized": [WindowNode, ...]}``.
    Returned in reply to ``get_tree``. Only workspaces that have windows —
    plus the active one — appear in ``workspaces``. ``minimized`` holds
    windows that are minimized and therefore belong to no workspace
    (minimizing drops a window's workspace assignment; unminimizing
    re-assigns it to the active workspace). It is empty when nothing is
    minimized.

``picked_window``
    ``{"type": "picked_window", "window": WindowSummary | null}``.
    Returned in reply to ``pick_window`` once the user resolves the
    picker. ``null`` on cancel or a click that didn't land on a
    toplevel.

``automation``
    ``{"type": "automation", "enabled": <bool>}``. Returned for both
    ``set_automation`` and ``automation_status``.

``screen_capture``
    ``{"type": "screen_capture", "enabled": <bool>}``. Returned for both
    ``set_screen_capture`` and ``screen_capture_status``.

``media``
    ``{"type": "media", "state": {"audio_muted": <bool>, "mic_muted":
    <bool>, "camera_active": <bool>}}``. Returned for ``media_status``.
    ``state`` is omitted (``null``) when no ``shoestring-mediad`` monitor
    has reported yet — distinct from "reported, all false".

``remote``
    ``{"type": "remote", "enabled": <bool>, "server_available": <bool>,
    "viewers": <int>}``. The remote-gate snapshot, returned for
    ``register_remote_server``, ``set_remote``, ``remote_status``, and
    ``report_remote_viewers``. ``enabled`` is the gate; ``server_available`` is
    whether a ``shoestring-remote-server`` has registered (the bar greys the
    toggle until then); ``viewers`` is the connected-client count (the "being
    viewed" chip lights when > 0).

``remote_clients``
    ``{"type": "remote_clients", "machines": [{"index": 1, "label":
    "dev-107"}, ...], "active": <int>}``. Returned for
    ``register_remote_client``, ``remote_client_status``, and ``set_view``.
    ``machines`` are the registered remote machines (index 1.. ; the local
    machine is the implicit index 0 and is not listed); ``active`` is the
    current machine-axis view index (0 = local).

``screenshot``
    ``{"type": "screenshot", "path": "/absolute/path.png"}``. Returned for
    both ``screenshot`` and ``screenshot_window``.

``clipboard``
    ``{"type": "clipboard", "mime": "text/plain;charset=utf-8", "data": [...]}``.
    Returned in reply to ``get_clipboard``. ``data`` is the selection bytes;
    ``mime`` is the type they're in and is omitted when the selection was empty
    or had no text the WM could read (``data`` is then empty).

``pointer_position``
    ``{"type": "pointer_position", "x": <f64>, "y": <f64>}``. Returned
    in reply to ``pointer_position``; same coordinate system as
    ``move_mouse`` / ``inject_click``.

``command_result``
    ``{"type": "command_result", "exit_code": <int>, "stdout": "...",
    "stderr": "...", "truncated": <bool>}``. ``exit_code`` is the
    child's real code; ``-1`` means killed by signal (typically the
    timeout-driven ``SIGKILL``). ``truncated`` is true if either
    stream exceeded the 64 KiB cap.

``command_started``
    ``{"type": "command_started", "pid": <int>}``. The reply to a
    ``run_command`` with ``"detach": true`` — the child's PID. The WM does not
    wait for it or capture output; the caller owns its lifetime.

.. _ipc-metrics:

``metrics``
    ``{"type": "metrics", "ts_ms": <u64>, "metrics": {<name>: MetricValue, ...}}``.
    ``ts_ms`` is the sample's wall-clock time (ms since the Unix epoch).
    ``metrics`` maps a dotted metric name to a ``MetricValue`` (below).
    Keys are sorted, and the set is append-only — new metrics may appear
    in later builds, so consumers should ignore unknown names rather than
    fail. Returned for ``metrics`` and carried verbatim by the ``metrics``
    event.

``error``
    ``{"type": "error", "message": "..."}``. The client should print the
    message and exit non-zero.

``MetricValue``
    A self-describing tagged value: ``{"kind": "gauge", "value": <i64>}``
    for an instantaneous reading (open fds, RSS) that can rise or fall, or
    ``{"kind": "counter", "value": <u64>}`` for a monotonic count since WM
    start. The WM emits these gauges:

    .. list-table::
       :header-rows: 1
       :widths: 30 70

       * - Name
         - Meaning
       * - ``process.open_fds``
         - Open file descriptors (``/proc/self/fd``).
       * - ``process.fd_limit``
         - ``RLIMIT_NOFILE`` soft limit.
       * - ``process.rss_kb``
         - Resident set size in KiB.
       * - ``wm.windows``
         - Mapped toplevels across all workspaces.
       * - ``wm.clients``
         - Connected IPC clients.
       * - ``ipc.subscribers``
         - Long-lived stream subscribers (event + metrics).
       * - ``wm.idle_inhibitors``
         - Tracked ``zwp_idle_inhibitor_v1`` surfaces. Present only when
           idle notifications are enabled (the inhibit manager is
           advertised alongside the notifier).
       * - ``wm.idle_inhibited``
         - ``1`` while at least one inhibitor surface is *visible* (mapped
           on the active workspace), so idle/lock is currently suppressed;
           ``0`` otherwise. Answers "why won't the screen lock?".
       * - ``client.<name>.surfaces``
         - Live ``wl_surface`` count for one Wayland client. ``<name>`` is
           the client's process ``comm`` joined with its pid
           (``shoestring-bar-1234``); the pid keeps it unique when two
           instances share a name. One row appears per client that holds at
           least one surface and vanishes when its last surface is
           destroyed. On platforms without ``/proc`` the name degrades to
           ``pid-<n>``.
       * - ``render.last_frame_us``
         - Wall-clock microseconds the most recent frame's render call took
           (winit ``render_output`` / udev ``render_frame``).
       * - ``render.fps``
         - Instantaneous frames per second, derived from the interval
           between the last two rendered frames. Appears after the second
           frame.

    …and these counters, monotonic since WM start (all seeded at ``0`` so
    they appear from the first sample):

    .. list-table::
       :header-rows: 1
       :widths: 30 70

       * - Name
         - Meaning
       * - ``render.frames_total``
         - Frames actually rendered (damage present) across all outputs.
       * - ``input.events_total``
         - Input events dispatched (keyboard, pointer, touch, tablet,
           device hotplug).
       * - ``ipc.requests_total``
         - Well-formed IPC requests parsed and handled.
       * - ``ipc.subscribers_dropped``
         - Stream subscribers hung up on after a failed write
           (backpressure), as opposed to a clean disconnect.

    The WM warns in its log when ``process.open_fds`` crosses
    ``[diagnostics].fd_warn_fraction`` of ``process.fd_limit`` or climbs
    monotonically — an early signal of a file-descriptor leak before it
    can exhaust the limit and crash the session. The per-client
    ``client.<name>.surfaces`` gauges turn that process-wide signal into
    attribution: a client whose surface count climbs without bound is the
    likely offender.

    .. note::

       v1 attributes ``wl_surface`` only. Smithay exposes surface
       create/destroy hooks but no creation hook for ``wl_buffer`` /
       ``wl_shm_pool``, so those resource kinds can't yet be counted
       per-client without reimplementing its shm dispatch. Surfaces are the
       resource the compositor owns end-to-end.

``WindowSummary``::

    {
      "id":        "stable-string-id",
      "title":     "Window title",
      "app_id":    "alacritty",
      "workspace": 3,
      "focused":   true,
      "geometry":     {"x": 0, "y": 0, "w": 960, "h": 1080},
      "z":            2,
      "sticky":       true,
      "always_on_top": true,
      "pid":          1234
    }

``id`` matches the ``identifier`` event from ``ext-foreign-toplevel-list-v1``,
so a bar can cross-reference between protocols. ``geometry`` is the window's
on-screen rectangle in compositor-global logical coords (same system as
``move_mouse`` / ``pointer_position``); it is **omitted** when the window is
minimized or on a non-active workspace (only the active workspace is mapped,
so an unmapped window has no rectangle). ``z`` is the window's stacking
position within the mapped stack — ``0`` is bottom-most, higher is closer to
the top — and the same value the ``get_tree`` ``WindowNode`` carries; it is
**omitted** for unmapped windows (same condition as ``geometry``). ``sticky``
is ``true`` when the window is pinned to all workspaces; ``always_on_top`` is
``true`` when it is kept above ordinary windows. Both are **omitted**
(defaulting to ``false``) for ordinary windows. Older WM builds that pre-dated
these fields omit them too — clients should treat "absent" and "off-screen"
(and "not sticky" / "not always-on-top") identically.

``pid`` is the operating-system process id of the window's owning client,
letting a script resolve a window (matched by ``title`` / ``app_id`` via
``find_windows``) to a process — e.g. to find a specific nested
``shoestring-wm``'s pid (give it a distinct title with
``SHOESTRING_WM_WINIT_TITLE`` first, since the default title collides across
instances). It is **omitted** when the compositor can't resolve a pid: clients
whose peer credentials carry none (e.g. FreeBSD ``LOCAL_PEERCRED``) and older
WM builds. For **X11** windows it is XWayland's pid — the wayland surface
belongs to the XWayland connection, not the X application.

``OutputSummary``::

    {
      "name":   "DP-1",
      "width":  3840,
      "height": 2160,
      "scale":  1.5,
      "adaptive_sync": false,
      "transform": "normal"
    }

``adaptive_sync`` is ``true`` only when the output opted in via
``[outputs.<name>] adaptive_sync = true`` *and* the connector advertised VRR
support. It is always ``false`` under the nested winit backend.

``transform`` is the output's orientation as a ``wlr-randr``-style name —
``"normal"``, ``"90"``, ``"180"``, ``"270"``, or the mirrored ``"flipped"`` /
``"flipped-90"`` / ``"flipped-180"`` / ``"flipped-270"``. It reflects the live
state, whether set via ``[outputs.<name>] transform`` or a later
``wlr-output-management`` apply. ``width`` and ``height`` are the raw mode
dimensions; a ``"90"`` / ``"270"`` transform swaps the logical usable area.

``InputSummary``::

    {
      "name":     "SynPS/2 Synaptics TouchPad",
      "sysname":  "event5",
      "vendor":   2,
      "product":  7,
      "capabilities": ["pointer", "gesture"],
      "size_mm":  [97.33, 66.86]
    }

Properties come straight off the live libinput device handle the WM holds for
``[input]`` config application, so this enumerates exactly the devices the
compositor is driving. ``vendor`` / ``product`` are the USB/Bluetooth ids (``0``
when the bus exposes none). ``capabilities`` is the set of libinput
capabilities the device advertises, drawn from ``"keyboard"``, ``"pointer"``,
``"touch"``, ``"tablet_tool"``, ``"tablet_pad"``, ``"gesture"``, ``"switch"`` —
a single device may report several. ``size_mm`` is the physical ``[width,
height]`` in millimetres for devices that measure it (touchpads, touchscreens,
tablets) and is **omitted** otherwise. ``output`` (omitted unless set) names the
output a device is pinned to, when libinput knows the mapping.

Only the TTY/udev backend tracks real libinput devices, so under the nested
winit backend ``inputs`` is always ``[]``.

``tree`` payload types
~~~~~~~~~~~~~~~~~~~~~~~

``OutputNode`` carries the output's *logical placement* (position + size in
the global coordinate space), which ``OutputSummary`` lacks — that's what lets
a script relate a window's ``geometry`` to the output it lands on::

    {
      "name":  "DP-1",
      "x":     0,
      "y":     0,
      "w":     2560,
      "h":     1440,
      "scale": 1.0
    }

``WorkspaceNode``::

    {
      "index":   3,
      "name":    "",
      "focused": true,
      "windows": [WindowNode, ...]
    }

``index`` is 1-based; ``name`` is empty when unset. ``focused`` marks the
active (mapped/visible) workspace. For the active workspace, ``windows`` is
ordered bottom-to-top by stacking order; for others the order is unspecified.

``WindowNode``::

    {
      "id":        "stable-string-id",
      "title":     "Window title",
      "app_id":    "alacritty",
      "geometry":  {"x": 0, "y": 0, "w": 960, "h": 1080},
      "output":    "DP-1",
      "z":         0,
      "focused":   true,
      "minimized": false,
      "layout":    "tiled_left"
    }

``geometry``, ``output``, and ``z`` are **omitted** for windows that aren't
currently mapped (minimized, or on a non-active workspace) — only the active
workspace's windows have an on-screen rectangle, a host output, and a stacking
position. ``z`` is the stacking index within the mapped stack: ``0`` is
bottom-most, higher is closer to the top. ``layout`` is one of ``floating``,
``tiled_left``, ``tiled_right``, or ``maximized``.

Events
------

Pushed continuously on an ``event_stream`` connection, one per line.
Each event is tagged by ``type``.

``workspace_changed``
    ``{"type": "workspace_changed", "active": <1..16>}``

``window_opened``
    ``{"type": "window_opened", "id": "...", "title": "...", "app_id": "...", "workspace": 1}``

``window_closed``
    ``{"type": "window_closed", "id": "..."}``

``window_focused``
    ``{"type": "window_focused", "id": "..." | null}``. ``null`` when no
    window holds keyboard focus.

``window_title_changed``
    ``{"type": "window_title_changed", "id": "...", "title": "...", "app_id": "..."}``.
    ``title`` is the *effective* title: the override set via
    ``set_window_name`` when one is active, otherwise the client's own
    title. Fired both when the client changes its title and when an
    override is set or cleared.

``window_moved_to_workspace``
    ``{"type": "window_moved_to_workspace", "id": "...", "workspace": <1..count>}``.
    Fired when a window is reassigned to a different workspace via the
    move-to-workspace bindings or a ``[[window_rules]]`` action.

``window_restacked``
    ``{"type": "window_restacked", "id": "..."}``. Fired when a window's
    stacking order changes — it was raised to the top or lowered to the
    bottom via the ``raise`` / ``lower`` actions or the ``raise_window`` /
    ``lower_window`` requests. A raise or lower shifts the ``z`` index of
    every other mapped window too, so the event only names the window that
    moved; re-query ``windows`` or ``get_tree`` for the updated stack.

``window_sticky_changed``
    ``{"type": "window_sticky_changed", "id": "...", "sticky": <bool>}``.
    Fired when a window is pinned to (or released from) "show on all
    workspaces" — via the ``toggle-sticky`` action, a ``[[window_rules]]``
    ``sticky`` action, or the ``set_window_sticky`` request.

``window_always_on_top_changed``
    ``{"type": "window_always_on_top_changed", "id": "...", "always_on_top": <bool>}``.
    Fired when a window is pinned to (or released from) the always-on-top
    layer — via the ``toggle-always-on-top`` action, a ``[[window_rules]]``
    ``always_on_top`` action, or the ``set_window_always_on_top`` request.

``window_activation_requested``
    ``{"type": "window_activation_requested", "id": "...", "granted": <bool>}``.
    Fired when a client uses ``xdg_activation_v1`` to ask that one of its
    surfaces be focused — typically an app launched by another app (a link
    opened from a chat client, a file opened from a file manager) asking to
    come to the front. ``granted`` is the WM's focus-stealing-prevention
    verdict: ``true`` when the request was honored (the window was activated
    and focused, so a ``window_focused`` event follows), ``false`` when it
    was suppressed because the request wasn't user-driven — focus stayed put
    and a bar may flag the window as demanding attention. The WM trusts an
    activation only when its token is recent and either carries a real input
    serial or was requested by the currently focused surface. Emitted only
    for a window the WM tracks (one with an ``id``).

``output_added``
    Same shape as ``OutputSummary``.

``output_removed``
    ``{"type": "output_removed", "name": "DP-1"}``

``automation_changed``
    ``{"type": "automation_changed", "enabled": <bool>}``. Fired when
    the runtime automation gate flips.

``screen_capture_changed``
    ``{"type": "screen_capture_changed", "enabled": <bool>}``. Fired when
    the runtime screen-capture gate flips — subscribers (e.g. a bar) can
    surface whether capture is *possible* without polling.

``screen_captured``
    ``{"type": "screen_captured", "output": "<name>"}``. Fired when a
    capture frame is actually delivered to a client — the live "your
    screen is being read right now" signal, distinct from the gate merely
    being enabled. Rate-limited by the WM (a few per second) so a high-FPS
    cast doesn't flood subscribers; a bar can light a "recording" dot and
    let it decay after the events stop.

``remote_changed``
    ``{"type": "remote_changed", "enabled": <bool>, "server_available":
    <bool>, "viewers": <int>}``. Fired when the remote gate flips, a server
    registers or drops, or the viewer count changes — same payload as the
    ``remote`` response. The ``shoestring-remote-server`` watches it to open or
    close its listener; a bar uses it for the remote toggle and the "being
    viewed" chip. See :doc:`remote`.

``media_changed``
    ``{"type": "media_changed", "audio_muted": <bool>, "mic_muted":
    <bool>, "camera_active": <bool>}``. Fired when the media-privacy
    snapshot changes — the default sink/source mute or camera-in-use state
    moved, as reported by ``shoestring-mediad``. Carries the full snapshot
    so a subscriber (the bar's MUTE/MIC/CAM chips) needs no follow-up
    ``media_status``.

``view_changed``
    ``{"type": "view_changed", "index": <int>, "label": "<machine>"}``.
    Fired when the active machine-axis view changes — the user switched which
    machine they're driving (``Super+J/K``, the break-out hotkey, a ``set_view``,
    or a registration/disconnect that shifted the active index). ``index`` 0 =
    local, 1.. = a registered remote; ``label`` is that machine's name and is
    omitted at the local view. Broadcast to every subscriber: the bar lights a
    "driving <machine>" chip, and a ``shoestring-remote-client`` uses it to
    reveal/hide its own surface as it becomes (or stops being) the active view.
    Re-query ``remote_client_status`` for the full machine list.

``captured_input``
    ``{"type": "captured_input", "event": {"kind": "key", "keycode": 30,
    "pressed": true}}``. Pushed **only to the connection that is the active
    machine-axis view** — the viewer-box mirror of ``inject_input``. While a
    remote machine is the active view the WM swallows every local input event and
    forwards it here as the same raw ``event`` currency ``inject_input`` injects
    (``key`` / ``button`` / ``motion`` / ``axis``), so the client can relay it
    over its tunnel for the remote's own keymap/binds to apply (raw KVM
    passthrough). ``key`` carries an evdev keycode (before XKB's ``+8``);
    ``motion`` is absolute in the remote's pixel space (clamped to the size from
    ``register_remote_client``). The axis keys (``Super+J/K``) and the break-out
    hotkey are handled locally and never forwarded.

``remote_clipboard``
    ``{"type": "remote_clipboard", "op": "push"|"pull"}``. Pushed **only to the
    connection that is the active machine-axis view** when the user triggers a
    remote clipboard transfer (``Super+Shift+C`` = ``push``, ``Super+Shift+V`` =
    ``pull``). It is viewer-initiated and gated by remote sharing — never a
    silent always-on stream. ``push`` tells the client to read *this* (viewer)
    machine's selection (``get_clipboard``) and ship it to the remote, which
    installs it (``set_clipboard``); ``pull`` tells the client to fetch the
    remote's selection and install it locally. Because both directions are
    initiated by the one active viewer, the served box only ever replies to the
    requester — it never has to choose among its connected clients. Emitted only
    while a remote machine is the active view; a no-op at the local view.

``config_reloaded``
    ``{"type": "config_reloaded"}``. Fired after a successful config
    re-read (file-watcher or explicit ``reload_config`` trigger).
    Subscribers should re-query anything derived from the config.

``metrics``
    ``{"type": "metrics", "ts_ms": <u64>, "metrics": {...}}``. One
    diagnostics sample, pushed only to ``metrics_stream`` subscribers
    (*not* to plain ``event_stream`` connections). Same payload as the
    ``metrics`` response.

Reference client
----------------

``shoestring-ctl`` is a thin CLI wrapper around the protocol. Each
subcommand maps to one request:

.. code-block:: console

    $ shoestring-ctl workspaces
    {"type":"workspaces","active":3,"count":16}

    $ shoestring-ctl --pretty windows
    {
      "type": "windows",
      "windows": [
        { "id": "...", "title": "tmux", "app_id": "alacritty",
          "workspace": 1, "focused": true,
          "geometry": { "x": 0, "y": 0, "w": 960, "h": 1080 } }
      ]
    }

    $ shoestring-ctl tree                # full layout tree (alias: get-tree)
    {"type":"tree","outputs":[{"name":"DP-1","x":0,"y":0,"w":2560,"h":1440,"scale":1.0}],
     "workspaces":[{"index":1,"name":"","focused":true,"windows":[
       {"id":"...","title":"tmux","app_id":"alacritty",
        "geometry":{"x":0,"y":0,"w":960,"h":1080},"output":"DP-1","z":0,
        "focused":true,"minimized":false,"layout":"tiled_left"}]}],"minimized":[]}

    $ shoestring-ctl event-stream
    {"type":"workspace_changed","active":4}
    {"type":"window_focused","id":"abcd-1234"}
    ...

    $ shoestring-ctl -p metrics          # one-shot diagnostics snapshot
    {
      "type": "metrics",
      "ts_ms": 1733800000000,
      "metrics": {
        "process.open_fds": { "kind": "gauge", "value": 142 },
        "process.fd_limit": { "kind": "gauge", "value": 1024 },
        "process.rss_kb":   { "kind": "gauge", "value": 85320 }
      }
    }

    $ shoestring-ctl metrics --watch     # tail samples (Ctrl-C to stop)
    {"type":"metrics","ts_ms":...,"metrics":{...}}
    ...

    $ shoestring-ctl key Return         # synthesize a single Enter press
    $ shoestring-ctl key super+shift+q  # chord; +-syntax matches xdotool
    $ shoestring-ctl key q -m ctrl -m shift   # equivalent, no parsing
    $ shoestring-ctl type "hello 123"   # type ASCII into focused surface
    $ shoestring-ctl click left         # click at the current pointer
    $ shoestring-ctl click left --x 200 --y 400

    $ shoestring-ctl pick-window        # blocks until user clicks
    {"type":"picked_window","window":{"id":"...","title":"...", ...}}
    $ shoestring-ctl close-window <id>  # ask that toplevel to close
    $ shoestring-ctl kill-window <id>   # SIGKILL the owning process
    $ shoestring-ctl focus-window <id>  # focus + raise + switch workspace

    $ shoestring-ctl find-windows --app-id '^Alacritty$'
    {"type":"windows","windows":[ ... ]}
    $ shoestring-ctl find-windows --title '(?i)slack'

    $ shoestring-ctl dispatch-action quit                                  # bare name
    $ shoestring-ctl dispatch-action '{"type":"focus-workspace","index":3}'

    $ shoestring-ctl automation status
    {"type":"automation","enabled":false}
    $ shoestring-ctl automation on

    $ shoestring-ctl screenshot --output eDP-1 --region 100,100,800,600
    {"type":"screenshot","path":"/home/you/Pictures/Screenshot-AUTO-...png"}

    $ shoestring-ctl run-command --timeout-ms 500 -- echo hi
    {"type":"command_result","exit_code":0,"stdout":"hi\n",...}

    $ shoestring-ctl lock           # spawn the configured locker
    $ shoestring-ctl reload-config  # re-read the TOML config

A higher-level binary, ``shoestring-kill`` (xkill equivalent), chains
the two: it sends ``pick_window``, then forwards the resulting ``id``
to ``close_window`` on success — or to ``kill_window`` when run with
``-f`` / ``--force``, which force-kills the owning process instead of
asking it to close (use it on a window that ignores a close). Bind it via
the WM config or invoke it from a menu.

Flags:

``-s, --socket PATH``
    Override the socket path (otherwise ``$SHOESTRING_WM_SOCKET`` or the
    default).

``-p, --pretty``
    Pretty-print JSON output instead of one-line-per-object.

Writing your own client
-----------------------

A minimal Python client::

    import os, json, socket

    s = socket.socket(socket.AF_UNIX)
    s.connect(os.environ["SHOESTRING_WM_SOCKET"])
    s.sendall(b'{"type":"event_stream"}\n')

    f = s.makefile("r")
    # First line is the Ok ack:
    print("ack:", f.readline().strip())
    for line in f:
        print(json.loads(line))

The protocol is one-shot per connection except for ``event_stream``;
open a new connection for each ad-hoc query.

Changelog
---------

Wire-format changes by release, newest first. Versions are the WM/CLI
version (``shoestring-ctl --version``). Per the :ref:`stability policy
<ipc-stability>` every entry below is an *additive* change — nothing has
been renamed, removed, or repurposed since 0.1.0 — so a client written
against any prior version still works against a newer WM.

0.8.0
~~~~~

- ``get_clipboard`` / ``set_clipboard`` requests + ``clipboard`` response: read
  and set the WM's selection (clipboard or primary) over IPC, gated by
  automation. ``get_clipboard`` reads the owner's bytes out of band and defers
  its reply; ``set_clipboard`` takes ownership and fans text mimes out to the
  standard aliases. A focus-free ``wl-paste`` / ``wl-copy``, and the native
  plumbing behind cross-machine remote clipboard sync.
- ``remote_clipboard`` event (``{op: "push"|"pull"}``): pushed to the active
  machine-axis view when the user triggers ``Super+Shift+C`` / ``Super+Shift+V``,
  telling a ``shoestring-remote-client`` to move the clipboard over its tunnel.
  Both directions are viewer-initiated, so the served box only replies to the
  requester. The ``clipboard-push`` / ``clipboard-pull`` / ``screenshot``
  dispatchable actions back the binds; ``screenshot`` stays local while viewing
  a remote.

0.7.0
~~~~~

- ``move_window_to_workspace`` request (``{id, index}``): move an arbitrary
  window to a workspace by FT id, focus-independent — does not switch the
  active workspace or steal focus. The per-id counterpart to the focused-only
  ``move-window-to-workspace`` action. Reuses the existing
  ``window_moved_to_workspace`` event.
- ``set_window_minimized`` / ``set_window_maximized`` requests
  (``{id, <bool>}``): minimize/restore and maximize/unmaximize an arbitrary
  window by FT id, regardless of focus. Per-id counterparts to the focused-only
  ``minimize`` / ``maximize`` actions.
- These back ``shoestring-tasks``, the new console (TUI) window/workspace
  manager — see :manpage:`shoestring-tasks(1)`.

0.5.0
~~~~~

- ``inputs`` request + ``inputs`` response (``InputSummary`` list): the
  input-device analogue of ``outputs``. Read-only, not gated.
- Media-privacy surface: ``media_status`` / ``set_audio_mute`` /
  ``set_mic_mute`` requests, the ``report_media`` ingest request (WM ⇆
  ``shoestring-mediad`` only), the ``media`` response, and the
  ``media_changed`` event.
- ``OutputSummary.transform`` — the active output rotation/flip.
- New metric names: render, input, and IPC counters (append-only; older
  consumers ignore unknown names).

0.4.0
~~~~~

- ``raise_window`` / ``lower_window`` requests + ``window_restacked``
  event; ``WindowSummary.z`` (stacking order) added to ``windows``.
- ``set_window_sticky`` request + ``window_sticky_changed`` event;
  ``WindowSummary.sticky`` added.
- ``set_window_always_on_top`` request + ``window_always_on_top_changed``
  event; ``WindowSummary.always_on_top`` added.
- Per-client ``wl_surface`` gauges added to the ``metrics`` set.

0.3.0
~~~~~

- ``metrics`` / ``metrics_stream`` requests, the ``metrics`` response and
  event, and the ``MetricValue`` tagged shape (the append-only
  metric-name set originates here).
- Screen-capture gate: ``set_screen_capture`` / ``screen_capture_status``
  requests, the ``screen_capture`` response, and the
  ``screen_capture_changed`` event. Enabling automation now also enables
  screen capture (automation is a superset).
- ``set_gamma`` / ``reset_gamma`` requests (not automation-gated).
- ``get_tree`` request + ``tree`` response — the outputs→workspaces→
  windows layout snapshot.
- ``set_window_name`` request — override a window's display title;
  reflected in ``windows`` / ``get_tree`` / ``find_windows`` and the
  ``window_title_changed`` event.

0.2.0
~~~~~

- No wire-format changes.

0.1.0
~~~~~

- Initial IPC surface. Read-only queries (``workspaces`` — including the
  ``names`` array, ``windows``, ``outputs``, ``pointer_position``),
  ``find_windows``, ``focus_window``, ``close_window``, ``event_stream``,
  ``lock``, and ``reload_config``.
- The automation gate, plus the automation-gated primitives:
  ``inject_key`` / ``inject_text`` / ``inject_click``, ``move_mouse``,
  ``dispatch_action``, ``pick_window``, ``screenshot``, and
  ``run_command``; ``set_automation`` / ``automation_status``.
