shoestring-ctl
==============

Synopsis
--------

| **shoestring-ctl** [**-s** *PATH*] [**-p**] *SUBCOMMAND* [*ARGS*...]

Description
-----------

Reference CLI client for the **shoestring-wm**\(1) IPC socket.
Connects to ``$SHOESTRING_WM_SOCKET`` (or the default
``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``), sends one
request, and either prints the response or streams events.

Output is newline-delimited JSON (one object per line) unless
**--pretty** is given. Every subcommand has its own **--help**.

Several subcommands are gated by the WM's runtime automation gate.
While the gate is off (the default) those subcommands return an
``error`` response with the prefix
``automation disabled: enable with `shoestring-ctl automation on`...``.
Toggle the gate with ``shoestring-ctl automation on``. The gates are
consent, not protection: any process running as you can reach the
socket and turn them on. The full protocol, including which request
each subcommand sends, is in the IPC chapter of the manual.

Query subcommands
-----------------

None of these are gated.

**server-info**
    Describe the running WM: ``version``, ``backend`` (``tty``,
    ``winit`` or ``headless``), ``locked``, the ``gates`` (automation,
    screen capture, remote, and whether a remote server has registered),
    and ``requests``, every request type it understands. Use it to check
    what a WM supports before relying on a request.

**workspaces**
    Print the active workspace, total workspace count, and per-slot
    names.

**windows**
    Print every mapped window: ``id``, ``title``, ``app_id``,
    ``workspace``, ``focused``.

**find-windows** [**-t**, **--title** *RE*] [**-a**, **--app-id** *RE*]
    Like **windows** but filtered to those whose title and/or app_id
    match the given regular expressions. Each filter is independent
    and AND-ed; an omitted filter matches everything. Patterns use Rust
    ``regex`` syntax and are not anchored - ``firefox`` matches
    anywhere; use ``^firefox$`` for exact. Bad regex returns an
    ``error``.

**outputs**
    Print every connected output with its mode and scale.

**inputs**
    Print every connected input device (keyboards, pointers, touchpads,
    tablets) with its libinput identity and capabilities. Reports
    nothing under the nested winit backend, which has no libinput
    devices.

**tree**
    Dump the full window tree: outputs with their logical placement,
    plus each workspace and the windows on it (geometry, stacking
    order, and the output each window sits on). The
    ``swaymsg -t get_tree`` analogue, for layout scripting.

**metrics** [**-w**, **--watch**] [**--interval** *MS*]
    Print the WM's diagnostics metrics: process gauges
    (``process.open_fds``, ``process.rss_kb``, ``process.fd_limit``)
    plus WM counts. With **--watch**, subscribe and print a line per
    sample until the WM closes the socket; **--interval** asks for a
    push interval, clamped up to ``[diagnostics] sample_interval_ms``.

**pointer-position**
    Print the current pointer location in compositor-space logical
    coords as ``{"type":"pointer_position","x":...,"y":...}``.

**event-stream**
    Subscribe to events. After a one-line ``ok`` ack the server pushes
    one JSON event per line until the WM exits.

Waiting
-------

Both exit with status 2 on timeout, so a script can tell "timed out"
from an error. Not gated.

**wait-window** [**-t**, **--title** *RE*] [**-a**, **--app-id** *RE*] [**--unmap**] [**--timeout** *MS*]
    Block until a toplevel matching the filters maps, then print it.
    The filters work as in **find-windows**. A map wait resolves once
    the window has its first frame and initial placement, so its
    reported geometry is where it is drawn. With **--unmap**, wait
    until every currently-matching window has closed instead.
    **--timeout** defaults to 10000; 0 waits forever.

**wait-ready** [**--xwayland**] [**--timeout** *MS*]
    Block until the compositor answers. With **--xwayland**, also wait
    until XWayland is up and ``$DISPLAY`` is exported, so an X11 app
    launched at startup doesn't race it. **--timeout** defaults to
    10000; 0 waits forever.

Window subcommands
------------------

*ID* is a window's ``ext-foreign-toplevel-list-v1`` identifier, the
``id`` field of **windows** and of window events. None of these are
gated.

**pick-window**
    Block until the user clicks a toplevel (left-click), cancels
    (Escape or right-click), or clicks a non-toplevel. Prints
    ``picked_window`` with either the targeted window or
    ``window: null``.

**close-window** *ID*
    Send ``xdg_toplevel.close``. The client may surface a save-prompt
    rather than exit.

**kill-window** *ID*
    Kill the process that owns the window (SIGKILL) instead of asking
    it to close, for windows that ignore a close. Backs
    ``shoestring-kill -f``.

**focus-window** *ID*
    Focus the window: unminimize it, switch to its workspace if needed,
    raise and activate it.

**raise-window** *ID*
    Raise the window to the top of the stacking order without changing
    focus or the active workspace.

**lower-window** *ID*
    Lower the window to the bottom of the stacking order.

**move-window** *ID* *INDEX*
    Move the window to workspace *INDEX* (1-based), leaving the active
    workspace and focus alone.

**set-sticky** *ID* **true**\|\ **false**
    Show the window on every workspace, or stop.

**set-always-on-top** *ID* **true**\|\ **false**
    Keep the window above others, or stop.

**set-minimized** *ID* **true**\|\ **false**
    Minimize or restore the window, whichever window has focus.

**set-maximized** *ID* **true**\|\ **false**
    Maximize the window to the work area, or restore its floating
    rectangle.

**set-name** *ID* *NAME*
    Override the window's display name everywhere the WM reports a
    title (**windows**, **tree**, **find-windows**, the
    ``window_title_changed`` event). An empty *NAME* clears it.

Action and session subcommands
------------------------------

**dispatch-action** *ACTION*
    Run a named bind ``Action`` server-side as if a keybind had fired.
    *ACTION* is either a bare kebab-case name for a no-arg action
    (``quit``, ``tile-left``, ``maximize``, ``reload-config``,
    ``lock``, ...) or a JSON object literal for parametric actions
    (e.g. ``'{"type":"focus-workspace","index":3}'``). Gated by the
    automation gate.

**reload-config**
    Re-read the WM's config file, recompile binds, and broadcast
    ``config_reloaded``. Equivalent to the ``reload-config`` keybind
    action.

**lock**
    Spawn the WM's configured lock binary
    (``[general] lock_command``). The reply doesn't mean the session is
    locked; **server-info** reports ``locked``.

**shutdown**
    Quit the WM without a confirmation prompt (not the machine). Its
    child processes exit with it, and its sockets are removed. Gated by
    the automation gate.

Injection subcommands (gated)
-----------------------------

The xdotool-equivalent surface. All of these require the automation
gate. Coordinates are compositor-space logical pixels unless a window
is named.

**key** *KEYSYM* [**-m**, **--mod** *NAME*]...
    Synthesize a press + release of *KEYSYM* (an X keysym name like
    ``Return``, ``F5``, ``BackSpace``, ``q``) targeting the focused
    surface. *KEYSYM* may also be a ``+``-joined chord like
    ``super+shift+q`` - the last token is the keysym and the rest are
    treated as modifiers, xdotool-compatible. ``--mod`` (repeatable,
    case-insensitive: ``super``/``logo``/``mod4``/``win``, ``ctrl``/
    ``control``, ``alt``/``mod1``, ``shift``, or a raw keysym name like
    ``Hyper_L``) appends extra modifiers; chord-derived modifiers are
    pressed after any ``--mod`` and released first. Chords the WM
    consumes (e.g. ``super+shift+q``) will not reach the focused surface
    - use **dispatch-action** for those.

**type** *TEXT* [**--via-clipboard** [**--paste-key** *CHORD*]]
    Type a literal string into the focused surface. Typed one key at a
    time it supports ASCII letters, digits, and space; other characters
    return an ``error``. **--via-clipboard** sets the selection to
    *TEXT* and pastes it instead, which handles any text; the paste
    chord defaults to ``Ctrl+v``.

**paste** [**--key** *CHORD*]
    Paste the current selection into the focused surface by
    synthesizing the paste chord (default ``Ctrl+v``; terminals may
    want ``Ctrl+Shift+v`` or ``Shift+Insert``).

**click** *BUTTON* [**--x** *X* **--y** *Y* \| **--window** *ID* **--wx** *X* **--wy** *Y*] [**--count** *N*]
    Click *BUTTON* (``left`` / ``right`` / ``middle`` or a numeric
    ``BTN_*`` code) at the current pointer. **--x**/**--y** move the
    pointer there first. **--window** with **--wx**/**--wy** clicks at
    window-local coordinates (0,0 is the window's top-left), wherever
    the window was placed. **--count** 2 double-clicks.

**drag** **--from** *X,Y* **--to** *X,Y* [**--button** *BUTTON*] [**--window** *ID*]
    Press *BUTTON* (default ``left``) at **--from**, move to **--to**,
    and release. With **--window** both points are window-local.

**move-mouse** *X* *Y*
    Move the pointer without clicking. Parity with
    ``xdotool mousemove``; useful for hover-only tests.

Capture and clipboard (gated)
-----------------------------

All of these require the automation gate.

**screenshot** [**-o**, **--output** *NAME* [**--region** *X,Y,W,H*] \| **--window** *ID*] [**--path** *FILE* \| **--stdout**]
    Capture a PNG via the WM and print its path, by default
    ``$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png``. **--output**
    defaults to the first output; **--region** is in that output's
    logical pixels and requires **--output**. **--window** captures
    one toplevel, cropped to it, even when it is covered or off-screen.
    **--path** writes the PNG elsewhere (creating parent directories);
    **--stdout** writes the PNG bytes to standard output.

**get-pixel** *X* *Y* [**-o**, **--output** *NAME*]
    Print one pixel's colour at output-logical *X*, *Y* as
    ``{"type":"pixel","r":..,"g":..,"b":..,"a":..}``. winit and
    headless backends only.

**hash-region** [**--region** *X,Y,W,H*] [**-o**, **--output** *NAME*]
    Print a hash of an output region (default: the whole output);
    compare two runs to tell whether it repainted. winit and headless
    backends only.

**get-clipboard** [**--primary**]
    Read the WM's current selection and write the raw bytes to stdout
    (the chosen mime goes to stderr). The WM picks the best text mime
    the owner offers. **--primary** reads the primary (middle-click)
    selection. A focus-free ``wl-paste``.

**set-clipboard** [*TEXT*] [**--primary**] [**--mime** *MIME*]
    Set the WM's selection to *TEXT* (or stdin if omitted) and serve it
    to anything that pastes. **--mime** defaults to UTF-8 text (text
    mimes fan out to the standard aliases server-side); **--primary**
    sets the primary selection. A focus-free ``wl-copy``.

**run-command** [**--timeout-ms** *MS*] [**--cwd** *DIR*] [**--env** *K=V*]... [**--detach**] **--** *ARGV*...
    Run a command under the WM's environment (inherits
    ``WAYLAND_DISPLAY``, ``SHOESTRING_WM_SOCKET``, ...) and print
    ``command_result`` with the captured stdout / stderr / exit code.
    Output is capped at 64 KiB per stream; over-cap bytes are drained
    but discarded and ``truncated`` is set. **--timeout-ms** sends
    ``SIGKILL`` after that many milliseconds. **--cwd** and **--env**
    (repeatable) set the working directory and add variables.
    **--detach** starts the command in its own session with stdio on
    ``/dev/null`` and prints its PID without waiting.

Gates
-----

All three are runtime-only: the config file sets each at the next WM
start, and nothing here edits it. Flipping a gate broadcasts an event.

**automation on** \| **off** \| **status**
    Turn the automation gate on or off, or print it. It covers the
    gated subcommands above.

**screen-capture on** \| **off** \| **status**
    Turn the screen-capture gate on or off, or print it. While it is on
    the WM advertises ``zwlr_screencopy``, so tools such as OBS, grim
    or the screen-share portal can read the screen.

**remote on** [**--for** *DURATION*] [**--until-idle** *DURATION*]
    Turn the remote-desktop gate on: the registered
    **shoestring-remote-server**\(1) may stream the output and replay
    viewers' input, and the capture and automation gates turn on with
    it. Refused until a server has registered. Without options the
    gate stays on until turned off. **--for** turns it off after that
    long, viewers or not; **--until-idle** turns it off once nobody has
    been connected for that long, counting from now if nobody is. With
    both, whichever comes first ends it. *DURATION* is a number of
    seconds or a sum of units such as ``90s``, ``10m``, ``2h``,
    ``1h30m`` or ``1d``, at most 30 days.

**remote off**
    Turn the remote gate off. Viewers are disconnected, the server's
    listener closes, and the capture and automation gates turn off.

**remote status**
    Print the remote gate's state: ``enabled``, ``server_available``,
    ``viewers``, ``endpoint`` (where the server listens) and ``lease``
    (time left on **--for** or **--until-idle**, if set).

Machine axis
------------

**remote-clients**
    List the remote machines registered as viewable (index 1..) and the
    active view (0 is this machine). Not gated.

**set-view** *INDEX*
    Switch the active view to *INDEX*, clamped to the machine count:
    the programmatic ``Super+J``/``Super+K``. While a remote machine is
    the active view the WM forwards all input to it.

Media and gamma
---------------

**media status**
    Print the media-privacy snapshot (output mute, microphone mute,
    camera in use), or ``"state": null`` before
    **shoestring-mediad** has reported.

**media audio-mute on** \| **off**
    Mute or unmute the default audio output, through
    **shoestring-mediad**.

**media mic-mute on** \| **off**
    Mute or unmute the default microphone. This mutes the stream; it
    doesn't stop an app opening the device, like a hardware mic key.

**set-gamma** **-t**, **--temperature** *KELVIN* [**-o**, **--output** *NAME*] [**-b**, **--brightness** *F*] [**-g**, **--gamma** *F*]
    Set a gamma ramp from a colour temperature (1000-25000; about 6500
    is neutral, lower is warmer), with an optional brightness
    multiplier (0.1-1.0) and gamma exponent (0.1-10.0). Applies to
    every gamma-capable output unless **--output** names one, and takes
    over from any wlr-gamma-control client there. udev/TTY backend
    only.

**reset-gamma** [**-o**, **--output** *NAME*]
    Restore the original ramp on outputs the WM set (default: all of
    them). Leaves wlr-gamma-control clients alone.

Options
-------

**-s**, **--socket** *PATH*
    Override the socket path.

**-p**, **--pretty**
    Indent JSON output for human reading.

**-V**, **--version**
    Print version.

**-h**, **--help**
    Print short usage.

Exit status
-----------

**0**
    Normal exit (server replied; event-stream was closed by the server).

**1**
    Server returned an ``error`` response, no socket path could be
    resolved, or an I/O or parse error occurred; see stderr.

**2**
    **wait-window** or **wait-ready** timed out.

Environment
-----------

``SHOESTRING_WM_SOCKET``
    The IPC socket to connect to.

``XDG_RUNTIME_DIR``, ``WAYLAND_DISPLAY``
    Locate the default socket when ``SHOESTRING_WM_SOCKET`` is unset.

See also
--------

**shoestring-wm**\(1), **shoestring-wm**\(5), **shoestring-bar**\(1),
**shoestring-menu**\(1), **shoestring-remote-server**\(1)
