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
**--pretty** is given.

Several subcommands are gated by the WM's runtime automation gate.
While the gate is off (the default) those subcommands return an
``error`` response with the prefix
``automation disabled: enable with `shoestring-ctl automation on`...``.
Toggle the gate with ``shoestring-ctl automation on``.

Query subcommands
-----------------

**workspaces**
    Print the active workspace, total workspace count, and per-slot
    names.

**windows**
    Print every mapped window: ``id``, ``title``, ``app_id``,
    ``workspace``, ``focused``.

**outputs**
    Print every connected output: ``name``, ``width``, ``height``,
    ``scale``.

**pointer-position**
    Print the current pointer location in compositor-space logical
    coords as ``{"type":"pointer_position","x":...,"y":...}``. Read-
    only, not gated by automation.

**find-windows** [**--title** *RE*] [**--app-id** *RE*]
    Like **windows** but filtered to those whose title and/or app_id
    match the given regular expressions. Each filter is independent
    and AND-ed. Patterns use Rust ``regex`` syntax and are not
    anchored — ``firefox`` matches anywhere; use ``^firefox$`` for
    exact. Bad regex returns an ``error``.

**event-stream**
    Subscribe to events. After a one-line ``ok`` ack the server pushes
    one JSON event per line forever (or until the WM exits).

Window subcommands
------------------

**pick-window**
    Block until the user clicks a toplevel (left-click), cancels
    (Escape or right-click), or clicks a non-toplevel. Prints
    ``picked_window`` with either the targeted window or
    ``window: null``.

**close-window** *ID*
    Send ``xdg_toplevel.close`` to the window with the given
    ``ext-foreign-toplevel-list-v1`` identifier. The client may
    surface a save-prompt rather than exit.

**focus-window** *ID*
    Focus the window with the given identifier. Unminimizes it,
    switches workspaces if needed, raises and activates.

Action subcommands
------------------

**dispatch-action** *ACTION*
    Run a named bind ``Action`` server-side as if a keybind had fired.
    *ACTION* is either a bare kebab-case name for a no-arg action
    (``quit``, ``tile-left``, ``maximize``, ``reload-config``,
    ``lock``, ...) or a JSON object literal for parametric actions
    (e.g. ``'{"type":"focus-workspace","index":3}'``). Gated by the
    automation gate.

**reload-config**
    Re-read the WM's config file and recompile binds. Equivalent to
    the ``reload-config`` keybind action.

**lock**
    Spawn the WM's configured lock binary.

Injection subcommands (gated)
-----------------------------

These three are the xdotool-equivalent surface and require the
automation gate to be on.

**key** *KEYSYM*
    Synthesize a single press + release of *KEYSYM* (an X keysym name
    like ``Return``, ``F5``, ``BackSpace``, ``q``) targeting the
    focused surface.

**type** *TEXT*
    Type a literal string into the focused surface. v1 supports ASCII
    letters, digits, and space; other codepoints return an ``error``.

**click** *BUTTON* [**--x** *F*] [**--y** *F*]
    Click *BUTTON* (``left`` / ``right`` / ``middle`` or a numeric
    ``BTN_*`` code) at the current pointer. Pass ``--x`` and ``--y``
    together to move the pointer to compositor-space coordinates
    first.

**move-mouse** *X* *Y*
    Move the pointer to compositor-space ``(X, Y)`` without clicking.
    Parity with ``xdotool mousemove``; useful for hover-only tests and
    for setting up a drag (``move-mouse`` → ``click``).

**screenshot** [**--output** *NAME*] [**--region** *X,Y,W,H*]
    Capture a PNG via the WM's wlr-screencopy server. ``--output``
    defaults to the first advertised output. ``--region`` is
    output-relative logical pixels and requires ``--output``.

**run-command** [**--timeout-ms** *MS*] **--** *ARGV*...
    Run a command under the WM's environment (inherits
    ``WAYLAND_DISPLAY``, ``SHOESTRING_WM_SOCKET``, ...) and print
    ``command_result`` with the captured stdout / stderr / exit code.
    Output is capped at 64 KiB per stream; over-cap bytes are drained
    but discarded and ``truncated`` is set. ``--timeout-ms`` sends
    ``SIGKILL`` after the given milliseconds.

Automation gate
---------------

**automation on**
    Turn the gate ON. Persists only for the lifetime of this WM
    process — the config file is the source of truth at next start.

**automation off**
    Turn the gate OFF.

**automation status**
    Print ``automation`` with the current state.

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
    Server returned an ``error`` response, or no socket path could be
    resolved.

Other
    I/O or parse error; see stderr.

See also
--------

**shoestring-wm**\(1), **shoestring-wm**\(5), **shoestring-bar**\(1),
**shoestring-menu**\(1)
