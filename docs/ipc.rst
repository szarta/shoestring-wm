IPC
===

shoestring-wm exposes a small unix-socket IPC for querying state and
subscribing to events. The protocol is newline-delimited JSON: one
``Request`` object terminated by ``\n`` in, one ``Response`` object out;
for the event-stream request, the server keeps the connection open and
pushes ``Event`` objects forever (one per line) until the client
disconnects.

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
     - Snapshot the diagnostics registry — process resource gauges plus
       WM counts. Reply is ``metrics``. Read-only and not gated by
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
     - Synthesize a single mouse click. ``button`` is one of ``"left"`` /
       ``"right"`` / ``"middle"``, or a numeric Linux ``BTN_*`` code as a
       string. Pass ``"x"`` and ``"y"`` (both, as numbers) to move the
       pointer to those compositor-space coordinates first.
   * - ``{"type": "move_mouse", "x": 100.0, "y": 200.0}``
     - Move the pointer to compositor-space ``(x, y)`` without clicking.
       Parity with ``xdotool mousemove``; useful for hover-only tests
       and for composing drags (``move_mouse`` → ``inject_click``).
       Does not change keyboard focus.
   * - ``{"type": "pointer_position"}``
     - Read the current pointer location. Reply is ``pointer_position``.
       Read-only and not gated by automation.
   * - ``{"type": "pick_window"}``
     - Enter interactive picker mode: the WM waits for the user's next
       click and replies with ``picked_window``. Pointer/keyboard input
       is intercepted while the picker is up — left-click resolves to a
       toplevel, right-click and Escape cancel, other keys are
       swallowed. Only one picker may be active at a time. If the
       client disconnects mid-pick the picker is cancelled.
   * - ``{"type": "close_window", "id": "..."}``
     - Send ``xdg_toplevel.close`` to the window with the given
       ``ext-foreign-toplevel-list-v1`` identifier. The client may
       still surface a save-prompt rather than exiting immediately.
       Returns ``error`` if no window matches.
   * - ``{"type": "focus_window", "id": "..."}``
     - Focus the window with the given identifier. Unminimizes it if
       needed and switches to its workspace if it lives elsewhere,
       then raises + activates it the same way a click does. Returns
       ``error`` if no window matches.
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
   * - ``{"type": "set_automation", "enabled": true}``
     - Flip the runtime automation gate. Reply is ``automation`` with
       the new state; ``automation_changed`` is broadcast to
       subscribers when the value actually changes. Not persisted to
       disk.
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
   * - ``{"type": "reload_config"}``
     - Re-read the TOML config the WM was launched with and recompile
       the binding table. Broadcasts ``config_reloaded`` on success.
       Reply is ``ok`` or ``error``.
   * - ``{"type": "screenshot", "output": null, "region": null}``
     - Capture a PNG via the WM's wlr-screencopy server. ``output`` is
       the output name (omit / null → first advertised output).
       ``region`` is ``{"x":..,"y":..,"w":..,"h":..}`` in the named
       output's logical coords and requires ``output`` to be set. Reply
       is ``screenshot`` with the absolute path. Gated by the
       automation gate.
   * - ``{"type": "run_command", "argv": ["...", ...], "timeout_ms": null}``
     - Spawn a child process under the WM's environment and return its
       captured output once it exits. ``argv`` must be non-empty;
       ``timeout_ms`` (optional) sends ``SIGKILL`` after the given
       milliseconds. Output is capped at 64 KiB per stream; extra bytes
       are drained but discarded and ``truncated`` is set. Reply is
       ``command_result``. Gated by the automation gate.
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

Automation gate
~~~~~~~~~~~~~~~

The following requests refuse with an ``error`` while
``[general].automation_enabled`` is off (and the CLI flag
``--enable-automation`` / the IPC ``set_automation`` haven't flipped
it): ``inject_key``, ``inject_text``, ``inject_click``, ``move_mouse``,
``dispatch_action``, ``screenshot``, ``run_command``. The error message
is stable enough to scrape on:
``automation disabled: enable with `shoestring-ctl automation on`...``.

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

``screenshot``
    ``{"type": "screenshot", "path": "/absolute/path.png"}``.

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
    start. v1 emits these gauges:

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

    The WM warns in its log when ``process.open_fds`` crosses
    ``[diagnostics].fd_warn_fraction`` of ``process.fd_limit`` or climbs
    monotonically — an early signal of a file-descriptor leak before it
    can exhaust the limit and crash the session.

``WindowSummary``::

    {
      "id":        "stable-string-id",
      "title":     "Window title",
      "app_id":    "alacritty",
      "workspace": 3,
      "focused":   true,
      "geometry":  {"x": 0, "y": 0, "w": 960, "h": 1080}
    }

``id`` matches the ``identifier`` event from ``ext-foreign-toplevel-list-v1``,
so a bar can cross-reference between protocols. ``geometry`` is the window's
on-screen rectangle in compositor-global logical coords (same system as
``move_mouse`` / ``pointer_position``); it is **omitted** when the window is
minimized or on a non-active workspace (only the active workspace is mapped,
so an unmapped window has no rectangle). Older WM builds that pre-dated the
field omit it too — clients should treat "absent" and "off-screen"
identically.

``OutputSummary``::

    {
      "name":   "DP-1",
      "width":  3840,
      "height": 2160,
      "scale":  1.5
    }

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
    ``{"type": "window_title_changed", "id": "...", "title": "...", "app_id": "..."}``

``window_moved_to_workspace``
    ``{"type": "window_moved_to_workspace", "id": "...", "workspace": <1..count>}``.
    Fired when a window is reassigned to a different workspace via the
    move-to-workspace bindings or a ``[[window_rules]]`` action.

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
to ``close_window`` on success. Bind it via the WM config or invoke it
from a menu.

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
