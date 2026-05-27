Configuration
=============

shoestring-wm reads a single TOML file at
``$XDG_CONFIG_HOME/shoestring-wm/config.toml`` (falling back to
``$HOME/.config/shoestring-wm/config.toml``). Pass ``--config PATH`` to
override.

Bootstrap a starter file:

.. code-block:: console

    $ shoestring-wm --write-default-config
    wrote default config to /home/you/.config/shoestring-wm/config.toml

The file has four sections — ``[general]``, ``[workspaces]``,
``[[bindings]]``, and ``[[window_rules]]`` — all optional. Missing
sections take built-in defaults.

The ``[general]`` section
-------------------------

.. list-table::
   :header-rows: 1
   :widths: 18 12 12 58

   * - Key
     - Type
     - Default
     - Meaning
   * - ``focus_mode``
     - string
     - ``"click-to-focus"``
     - One of ``click-to-focus``, ``follows-mouse``, ``sloppy``. Click
       is the default; the other two are accepted by the parser today
       but partially implemented (``follows-mouse`` / ``sloppy`` are on
       the roadmap).
   * - ``repeat_delay``
     - integer ms
     - ``600``
     - Milliseconds before a held key starts repeating. Matches the
       X11 ``Repeat delay`` default.
   * - ``repeat_rate``
     - integer
     - ``25``
     - Key-repeat rate in repeats per second once repeat kicks in.
   * - ``output_scale``
     - float
     - ``1.0``
     - Scale factor advertised to clients via ``wl_output.scale``.
       Whole values (``1.0``, ``2.0``, …) are sent as integer scales;
       non-integer values use fractional scaling (clients supporting
       ``wp_fractional_scale_v1`` see the exact value, others round up).
       Match this to your ``Xft.dpi`` equivalent so text size carries
       over from an X session.
   * - ``lock_command``
     - string
     - ``"shoestring-lock"``
     - Command spawned by the ``lock`` action and the ``lock`` IPC
       request. Split on whitespace (first token = executable, rest =
       args). The binary itself drives ``ext-session-lock-v1`` and
       reports unlock; if it's missing or fails to spawn the session
       just stays unlocked (logged).
   * - ``autostart``
     - array of strings
     - ``["shoestring-bar"]``
     - Commands spawned once at WM startup, after the wayland socket
       is listening but before user interaction. Each entry is split
       on whitespace like ``lock_command``. Failures log a warning and
       don't block startup. Set to ``[]`` to disable.
   * - ``automation_enabled``
     - bool
     - ``false``
     - Master gate for remote-automation IPC methods (``inject_key`` /
       ``inject_text`` / ``inject_click`` / ``screenshot`` /
       ``run_command`` / ``dispatch_action``). Off by default so an
       attacker with only socket access can't drive the session. The
       CLI flag ``--enable-automation`` and the runtime IPC
       ``set_automation`` both override this without writing back to
       disk — the config file stays the source of truth at next start.

Example::

    [general]
    focus_mode = "click-to-focus"
    repeat_delay = 300
    repeat_rate = 40
    output_scale = 1.5
    lock_command = "shoestring-lock"
    autostart = ["shoestring-bar", "swww init"]
    automation_enabled = false

The ``[workspaces]`` section
----------------------------

.. list-table::
   :header-rows: 1
   :widths: 18 12 12 58

   * - Key
     - Type
     - Default
     - Meaning
   * - ``count``
     - integer
     - ``16``
     - Total workspace count. Must be ``1..=16``. Surfaced over IPC as
       ``workspaces.count`` so a bar can size its workspace strip.
   * - ``names``
     - table ``"N" = "label"``
     - ``{}``
     - Sparse map from 1-based workspace index to a display name.
       Slots without an entry render as the number. Indexes ``> count``
       are rejected at parse time.

Example::

    [workspaces]
    count = 8
    names = { 1 = "main", 2 = "web", 8 = "chat" }

Bindings
--------

Each ``[[bindings]]`` entry specifies modifiers, a key, and an action::

    [[bindings]]
    mods = ["Super"]
    key = "Return"
    action = { type = "spawn", command = "alacritty" }

    [[bindings]]
    mods = ["Super", "Shift"]
    key = "q"
    action = { type = "quit" }

``mods``
    Array of modifier names, case-insensitive. Recognized values:
    ``Super``, ``Ctrl``, ``Alt``, ``Shift``. Order does not matter; an
    empty array is allowed for un-modified keys.

``key``
    An xkb keysym name as a string. Letters, digits and named keys all
    work: ``"q"``, ``"1"``, ``"Return"``, ``"F5"``, ``"Escape"``,
    ``"space"``. Names follow the standard ``xkbcommon`` table — running
    ``xev`` (or ``wev`` under Wayland) shows the keysym name for any key.

    **Important:** binding lookups use the *pre-modifier* keysym (the
    letter on the key cap), not the shifted form. Bind ``Shift+d`` as
    ``key = "d"`` plus ``mods = ["Shift"]``, not ``key = "D"``.

``action``
    Tagged enum: ``{ type = "...", ... }``. The available actions are
    listed below.

Actions
-------

Each action type and its fields:

``spawn``
    Run a command. Fields:

    - ``command`` — the executable name (looked up on ``$PATH``).
    - ``args`` — optional array of string arguments.

    Example::

        action = { type = "spawn", command = "firefox", args = ["--new-window"] }

``quit``
    Bring up a confirm dialog (``shoestring-confirm``); exit the WM
    cleanly on *Yes*, stay running on *No*. The dialog is the only path
    out of the WM today — there is no force-quit action.

``lock``
    Spawn ``[general].lock_command`` (default ``shoestring-lock``). The
    locker binds ``ext-session-lock-v1`` and drives the session-lock
    handshake; a misconfigured / missing binary just logs and leaves
    the session unlocked.

``reload-config``
    Re-read the TOML config from disk and recompile the binding table.
    Config hot-reload via ``notify`` watches the file automatically;
    this action is the manual trigger for the same path.

``tile-left``
    Snap the focused window to the left half of its monitor's usable
    rectangle. Re-pressing while already left-tiled restores the saved
    floating geometry — the Openbox "toggle" feel.

``tile-right``
    As ``tile-left`` but the right half.

``maximize``
    Maximize the focused window to the monitor's usable rect (i.e. minus
    layer-shell exclusive zones). Toggle: re-pressing restores the saved
    floating geometry.

``minimize``
    Hide the focused window. The window is *not* destroyed; restore it
    with ``unminimize``.

``unminimize``
    Restore the most-recently-minimized window of the active workspace.

``close``
    Ask the focused window's client to close gracefully (the equivalent
    of clicking the window-manager close button).

``focus-workspace``
    Switch every output to show workspace ``index`` (1-based, 1..=16).
    Example::

        action = { type = "focus-workspace", index = 3 }

``focus-workspace-relative``
    Switch by a relative offset. ``delta = -1`` is previous,
    ``delta = 1`` is next. Saturates at 1 and 16 — no wrapping.

``move-window-to-workspace``
    Move the focused window to ``index`` (1-based) without switching to
    that workspace.

``move-window-to-workspace-relative``
    Move the focused window by ``delta`` (``-1`` / ``1``). Saturates.

``change-vt``
    Switch to Linux virtual terminal ``vt`` (1..=12). Only effective on
    the TTY backend; logged as a no-op under winit. Default binds map
    ``Ctrl+Alt+F1..F12`` to ``change-vt`` for the matching VT.

``inject-key``
    Synthesize a single keypress (press + release) targeting the focused
    surface. ``keysym`` is an X keysym name. Injected events bypass the
    WM's binding table by design — they go straight to the client.

    ::

        action = { type = "inject-key", keysym = "Return" }

``inject-text``
    Synthesize a sequence of keypresses that types ``text``. v1 supports
    ASCII letters, digits, and space. Useful for snippet-style bindings.

    ::

        action = { type = "inject-text", text = "user@host" }

``inject-click``
    Synthesize a single mouse click at the current pointer location.
    ``button`` is ``"left"`` / ``"right"`` / ``"middle"`` or a numeric
    ``BTN_*`` code as a string.

    ::

        action = { type = "inject-click", button = "middle" }

These three actions are also exposed over IPC as ``inject_key`` /
``inject_text`` / ``inject_click`` requests; see :doc:`ipc`. The
``shoestring-ctl key`` / ``type`` / ``click`` subcommands are the
xdotool-equivalent CLI built on top.

Window rules
------------

Per-app actions evaluated once on first commit after a window maps.
Each rule is a ``match`` predicate plus an ``actions`` table; the
first rule whose match succeeds wins. Reload does *not* re-evaluate
rules against already-mapped windows.

::

    [[window_rules]]
    match = { app_id = "firefox" }
    actions = { workspace = 2 }

    [[window_rules]]
    match = { app_id = "Alacritty", title_contains = "scratch" }
    actions = { position = [100, 100], size = [800, 600] }

``match``
    Sparse predicate. All set fields are AND-ed. An empty matcher
    matches every window — almost never what you want.

    - ``app_id`` (string) — exact match on the toplevel's xdg-shell
      ``app_id`` (the closest Wayland analogue to X11 ``WM_CLASS``).
    - ``title_contains`` (string) — case-sensitive substring match on
      the toplevel title. The substring matcher is deliberate: regex
      lives on the IPC side (``find_windows``) where strings are
      arbitrary; rules favour the simpler form.

``actions``
    Sparse action set. Each field is independently optional.

    - ``workspace`` (1..=count) — move the window to this workspace
      without switching the user's view.
    - ``position`` (``[x, y]``, logical px) — override the
      auto-centered spawn position.
    - ``size`` (``[w, h]``, logical px) — preferred size; sent as part
      of the next configure (client may negotiate a different value).

Pointer bindings
----------------

A few bindings are wired directly in the input layer and are not
user-rebindable today:

================  ===============================================
``Super+Left``    Drag-move the window under the cursor.
``Super+Right``   Drag-resize the window under the cursor (the
                  closest edge follows the pointer).
================  ===============================================

The default keymap
------------------

A reference list of every binding ``--write-default-config`` ships
lives in :doc:`bindings`.
