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

Validate a file before relying on it — this parses the TOML, compiles the
keybindings, reports any problems, and exits without starting a compositor
(so it is safe to run against a live session):

.. code-block:: console

    $ shoestring-wm --check-config
    /home/you/.config/shoestring-wm/config.toml: OK — 48 binding(s), 0 warning(s)

It exits non-zero on a parse error or a missing file, making it suitable
for a pre-commit hook or a CI check on a dotfiles repo.

The file's main sections — ``[general]``, ``[workspaces]``,
``[[bindings]]``, ``[[window_rules]]``, ``[outputs.<name>]``, and
``[input]`` — are all optional. Missing sections take built-in defaults.

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
     - One of ``click-to-focus``, ``follows-mouse``, ``sloppy``.
       ``click-to-focus`` (default): keyboard focus only changes on a
       press. ``follows-mouse``: focus tracks the window under the
       pointer; pointer over empty space clears focus. ``sloppy``: like
       ``follows-mouse`` but the previous window keeps focus while the
       pointer is over empty space (it's only stolen when the pointer
       enters another window). Pointer-driven focus updates do NOT
       raise — clicks still do.
   * - ``repeat_delay``
     - integer ms
     - ``600``
     - Milliseconds before a held key starts repeating. Matches the
       X11 ``Repeat delay`` default.
   * - ``repeat_rate``
     - integer
     - ``25``
     - Key-repeat rate in repeats per second once repeat kicks in.
   * - ``desktop_scroll_notches``
     - integer
     - ``1``
     - Mouse-wheel detents required to switch one workspace when scrolling
       the bare desktop (no window under the pointer). ``1`` switches per
       notch; higher values slow it down. High-resolution wheels are
       accumulated so a single notch never overshoots. Touchpad scrolling
       is unaffected.
   * - ``output_scale``
     - float
     - ``1.0``
     - Global scale factor fallback, used for any output that does not
       have a per-output ``scale`` entry under ``[outputs.<name>]``.
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
     - ``["shoestring-bar", "shoestring-mediad"]``
     - Commands spawned once at WM startup, after the wayland socket
       is listening but before user interaction. Each entry is split
       on whitespace like ``lock_command``. Failures log a warning and
       don't block startup. Set to ``[]`` to disable. The default starts
       the status bar and the media-privacy monitor
       (``shoestring-mediad``, which feeds the bar's MUTE/MIC/CAM
       indicators); the monitor links PipeWire and is a harmless no-op
       where PipeWire or the binary is absent.
   * - ``automation_enabled``
     - bool
     - ``false``
     - Master gate for remote-automation IPC methods (``inject_key`` /
       ``inject_text`` / ``inject_click`` / ``move_mouse`` /
       ``screenshot`` / ``run_command`` / ``dispatch_action``). Off by default so an
       attacker with only socket access can't drive the session. The
       CLI flag ``--enable-automation`` and the runtime IPC
       ``set_automation`` both override this without writing back to
       disk — the config file stays the source of truth at next start.
   * - ``screen_capture_enabled``
     - bool
     - ``false``
     - Gate for screen capture via the ``zwlr_screencopy`` protocol — the
       path tools like OBS, ``grim`` and the
       ``xdg-desktop-portal-shoestring`` screencast/screenshot backend use
       to read the screen. Off by default: unlike
       X11, Wayland isolates clients, so leaving it off means a stray or
       malicious client simply cannot capture the screen. When ``false``
       the ``zwlr_screencopy_manager_v1`` global is not advertised and any
       capture is refused. The runtime IPC ``set_screen_capture`` (and
       ``shoestring-ctl screen-capture on``) override it without writing
       to disk. Independent of the ``screenshot`` IPC request, which is
       behind ``automation_enabled``.
   * - ``idle_notifications_enabled``
     - bool
     - ``false``
     - Advertise the ``ext_idle_notify_v1`` global so idle daemons,
       screen-dimmers and auto-lockers (e.g. ``swayidle``) can request a
       notification after N milliseconds without input. Off by default:
       on a desktop that never sleeps, idle behaviour is mostly an
       annoyance, and not advertising the global means a stray idle
       client simply finds nothing to talk to. Set ``true`` on a laptop
       where you do want idle dimming/locking. Enabling this also
       advertises ``zwp_idle_inhibit_manager_v1`` so apps (video players,
       browsers) can suppress idle while a visible surface requests it —
       inhibition only has meaning when something advertises idle, so the
       two are paired.
   * - ``xkb_layout``
     - string
     - ``""``
     - Comma-separated XKB layout codes — ``"us"``, ``"us,de"``,
       ``"fr,ru"``. With more than one, the ``cycle-layout`` action
       (Super+Space by default) switches between them at runtime. Empty
       uses xkbcommon's default (``XKB_DEFAULT_LAYOUT``, usually ``us``).
   * - ``xkb_variant``
     - string
     - ``""``
     - Comma-separated variants, one per layout — e.g. ``"dvorak"`` or
       ``",nodeadkeys"`` (default variant for the first layout, nodeadkeys
       for the second). Empty uses each layout's default variant.
   * - ``xkb_options``
     - string
     - unset
     - Comma-separated XKB options — non-layout tweaks like
       ``"ctrl:nocaps"`` (Caps Lock acts as Ctrl) or
       ``"grp:alt_shift_toggle"`` (also switch layouts with Alt+Shift).
   * - ``xkb_rules`` / ``xkb_model``
     - string
     - ``""``
     - Rarely changed. Rules file (usually ``"evdev"``) and keyboard model
       (e.g. ``"pc105"``). Empty uses the xkbcommon defaults.

Layout/variant/options/rules/model changes apply on config reload
(rebuilding the keymap); an invalid combination is rejected with a warning,
leaving the previous keymap in place.

Example::

    [general]
    focus_mode = "click-to-focus"
    repeat_delay = 300
    repeat_rate = 40
    output_scale = 1.5
    lock_command = "shoestring-lock"
    autostart = ["shoestring-bar", "swww init"]
    automation_enabled = false
    screen_capture_enabled = false
    idle_notifications_enabled = false
    xkb_layout = "us,de"
    xkb_options = "grp:alt_shift_toggle"

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

The ``[outputs.<name>]`` section
--------------------------------

Per-output overrides. ``<name>`` is the connector name the WM logs when an
output is connected (e.g. ``DP-1``, ``HDMI-A-1``, ``eDP-1``). All fields
are optional; unset fields fall back to the matching ``[general]`` default.

.. list-table::
   :header-rows: 1
   :widths: 10 8 8 74

   * - Key
     - Type
     - Default
     - Meaning
   * - ``scale``
     - float
     - ``general.output_scale``
     - Scale factor for this output only. Same semantics as
       ``general.output_scale`` — whole values use integer scaling,
       fractional values use ``wp_fractional_scale_v1``. Useful on a
       mixed-DPI setup where one monitor is HiDPI and another is not.
   * - ``position``
     - ``[x, y]`` integer array
     - auto (left-to-right)
     - Fixed compositor-space position for this output. Overrides the
       automatic left-to-right stacking that occurs when no position is
       set. Use this to declare a stable multi-monitor arrangement that
       is independent of plug-in order. Coordinates are in logical pixels
       (before scaling).
   * - ``adaptive_sync``
     - bool
     - ``false``
     - Enable variable refresh rate (VRR / adaptive sync, a.k.a.
       FreeSync / G-Sync) on this output. Opt-in per connector because
       VRR can cause visible flicker on some panels. ``true`` only takes
       effect if the monitor and driver actually advertise support;
       otherwise the WM logs a warning and leaves it off. Only honored on
       the DRM/KMS (TTY) backend — ignored when running nested under
       winit. ``shoestring-ctl outputs`` reports the resolved state in
       each output's ``adaptive_sync`` field.
   * - ``transform``
     - string
     - ``"normal"``
     - Rotate or flip this output. Accepted values match ``wlr-randr`` /
       ``wl_output``: ``"normal"``, ``"90"``, ``"180"``, ``"270"``
       (clockwise rotations), and the mirrored variants ``"flipped"``,
       ``"flipped-90"``, ``"flipped-180"``, ``"flipped-270"`` (mirror
       horizontally, then rotate). A ``"90"`` / ``"270"`` transform swaps
       the logical width and height — a 1920×1080 panel becomes a
       1080×1920 portrait workspace. Applied at output creation. Only
       honored on the DRM/KMS (TTY) backend; ignored under nested winit. A
       later ``wlr-output-management`` apply (e.g. ``wlr-randr --transform``)
       overrides whatever is set here. ``shoestring-ctl outputs`` reports
       the live orientation in each output's ``transform`` field.

Example — a HiDPI laptop panel at 2× on the left, 1× external (a VRR gaming
monitor) on the right::

    [general]
    output_scale = 1.0      # fallback for any unspecified output

    [outputs.eDP-1]
    scale = 2.0
    position = [0, 0]

    [outputs.DP-1]
    scale = 1.0
    position = [1920, 0]
    adaptive_sync = true

A portrait monitor rotated 90° clockwise, sat to the right of a landscape
panel. Note the ``position`` x-offset is the landscape panel's logical width,
and the rotated panel now occupies a 1080-wide column::

    [outputs.eDP-1]
    position = [0, 0]               # 1920×1080 landscape

    [outputs.DP-2]
    transform = "90"                # 1920×1080 panel → 1080×1920 portrait
    position = [1920, 0]

.. note::

   Connector names are printed at WM startup in the log (``tracing``
   at ``INFO`` level) and in the ``output-added`` IPC event. Running
   ``shoestring-ctl outputs`` shows the current list.

The ``[input]`` section
-----------------------

libinput device tuning — touchpad and pointer behaviour you would otherwise
set with udev/kernel rules. Settings are applied to every applicable device
as it connects, and re-applied to all connected devices on config hot-reload.

The section is declarative: every key is optional, and an omitted key
applies the device's libinput default. So editing or deleting a key and
reloading takes effect right away — removing ``accel_speed`` snaps the
pointer back to the default speed rather than leaving the last value. A
setting a device doesn't support (e.g. ``tap_to_click`` on a wired mouse) is
silently ignored for that device, so this single global section is safe
across mixed hardware. **TTY/udev backend only** — the nested winit backend
has no real input devices and ignores this section.

.. list-table::
   :header-rows: 1
   :widths: 18 14 68

   * - Key
     - Type
     - Meaning
   * - ``tap_to_click``
     - bool
     - Tap the touchpad to click.
   * - ``tap_button_map``
     - enum
     - Which button 1/2/3-finger taps emit: ``left-right-middle`` or
       ``left-middle-right``.
   * - ``tap_and_drag``
     - bool
     - A tap immediately followed by a finger-down starts a drag.
   * - ``drag_lock``
     - bool
     - Keep dragging after the finger lifts until the next tap.
   * - ``natural_scroll``
     - bool
     - Reverse the scroll direction (content follows the fingers).
   * - ``scroll_method``
     - enum
     - How scrolling is produced: ``two-finger``, ``edge``,
       ``on-button-down``, or ``none``.
   * - ``scroll_button``
     - integer
     - evdev button code used for ``on-button-down`` scrolling (e.g.
       ``274`` for middle).
   * - ``click_method``
     - enum
     - Clickpad software-button method: ``button-areas`` or
       ``clickfinger``.
   * - ``accel_speed``
     - float
     - Pointer acceleration speed in ``[-1.0, 1.0]`` (``0`` = libinput
       default). Out-of-range values are clamped.
   * - ``accel_profile``
     - enum
     - ``adaptive`` (speed-dependent, the usual default) or ``flat``
       (constant factor, no acceleration).
   * - ``disable_while_typing``
     - bool
     - Suppress the touchpad while typing on the internal keyboard.
   * - ``left_handed``
     - bool
     - Swap the left and right buttons.
   * - ``middle_emulation``
     - bool
     - Treat a simultaneous left+right click as a middle click.

Example — a typical laptop touchpad::

    [input]
    tap_to_click = true
    natural_scroll = true
    disable_while_typing = true
    accel_speed = 0.3

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

``cycle-windows``
    Move keyboard focus to the next window on the active workspace,
    raising it (the Alt+Tab switcher). Repeated presses round-robin
    through every window and wrap around. Does nothing when the
    workspace has fewer than two windows.

``raise``
    Raise the focused window to the top of the stacking order, leaving
    keyboard focus where it is (a pure restack). Bound to Super+Up by
    default. A no-op when no window is focused.

``lower``
    Lower the focused window to the bottom of the stacking order — the
    complement of ``raise``. Bound to Super+Shift+Up by default. Handy for
    pushing the current window behind the others without moving the
    pointer.

``toggle-sticky``
    Toggle "show on all workspaces" for the focused window. A sticky
    window stays mapped (and keeps its position) across workspace
    switches instead of being hidden with the rest of its workspace —
    useful for a reference doc or a picture-in-picture video. Bound to
    Super+S by default. Moving a sticky window to a specific workspace is
    ignored (it's already on all of them); un-stick it first. The same
    flag is settable per-app via the ``sticky`` window rule.

``toggle-always-on-top``
    Toggle "always on top" for the focused window. An always-on-top
    window stays above all ordinary windows regardless of focus —
    clicking another window raises it only as far as just below the
    always-on-top layer (bars and pop-up menus still sit above it). Bound
    to Super+A by default. Combine with ``toggle-sticky`` for a
    picture-in-picture window. The same flag is settable per-app via the
    ``always_on_top`` window rule.

``cycle-layout``
    Switch to the next keyboard layout listed in ``[general].xkb_layout``,
    wrapping at the end. Bound to Super+Space by default. A no-op when only
    one layout is configured. Only the active layout index changes — no
    keymap rebuild — and the new state is sent to the focused client
    immediately.

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

    [[window_rules]]
    match = { app_id_regex = "^(firefox|chromium)$" }
    actions = { output = "DP-1", layout = "maximized" }

``match``
    Sparse predicate. All set fields are AND-ed. An empty matcher
    matches every window — almost never what you want.

    - ``app_id`` (string) — exact match on the toplevel's xdg-shell
      ``app_id`` (the closest Wayland analogue to X11 ``WM_CLASS``).
    - ``title_contains`` (string) — case-sensitive substring match on
      the toplevel title.
    - ``app_id_regex`` (string) — regex match on ``app_id``. Rust ``regex``
      syntax (Perl-like, no backrefs), **unanchored** — ``firefox`` matches
      anywhere; use ``^firefox$`` for an exact match. Same engine as the
      IPC ``find_windows`` filters.
    - ``title_regex`` (string) — regex match on the title (same syntax).

    An invalid pattern matches nothing and is reported at startup / reload
    (and by ``--check-config``); the rest of the config still loads.

``actions``
    Sparse action set. Each field is independently optional.

    - ``workspace`` (1..=count) — move the window to this workspace
      without switching the user's view.
    - ``position`` (``[x, y]``, logical px) — override the
      auto-centered spawn position.
    - ``size`` (``[w, h]``, logical px) — preferred size; sent as part
      of the next configure (client may negotiate a different value).
    - ``sticky`` (bool) — when ``true``, pin the window to all
      workspaces (see the ``toggle-sticky`` action). Applied before
      ``workspace``, which a sticky window then ignores — so set one or
      the other, not both. ``mpv`` and other PiP players are the
      typical use.
    - ``always_on_top`` (bool) — when ``true``, keep the window above
      ordinary windows (see the ``toggle-always-on-top`` action). Pair
      with ``sticky`` for a picture-in-picture window that floats on top
      of every workspace.
    - ``output`` (string) — place the window on the named output (e.g.
      ``"DP-1"``), centered in its usable area. Applied before
      ``position``, so an explicit ``position`` still wins. A name that
      matches no connected output is ignored with a warning.
    - ``layout`` (string) — give the window an initial tiling layout
      instead of leaving it floating: ``floating`` (the default),
      ``tiled-left``, ``tiled-right``, or ``maximized``. Computed against
      whichever output the window ends up on, so combine with ``output``
      to tile on a specific monitor.

Screen-sharing portal
---------------------

``[portal]`` configures the ``xdg-desktop-portal-shoestring`` screen-sharing
backend. That backend is a separate process, but it reads this same
``config.toml`` so the screencast output choice lives in one place. Both keys
are optional; with no ``[portal]`` section the backend uses the region chooser
when more than one output is connected. See :doc:`portals`.

::

    [portal]
    screencast_output = "DP-2"
    screencast_chooser = "region"

``screencast_output``
    Pin screencast to one output by connector name (e.g. ``"DP-2"``). When set,
    the chooser is skipped and this output is always shared. Unset (the default)
    ⇒ the chooser runs whenever more than one output is connected. The
    ``$SHOESTRING_SCREENCAST_OUTPUT`` environment variable overrides this.

``screencast_chooser``
    How to choose the output when none is pinned and more than one is connected.
    Defaults to ``"region"``. Overridden by ``$SHOESTRING_SCREENCAST_CHOOSER``.

    - ``"region"`` — pop the ``shoestring-region`` overlay so you click/drag the
      monitor to share.
    - ``"none"`` — silently share the first output (a warning names it and how
      to pin one).
    - anything else — run as a dmenu-style command: the connector names are
      written to its stdin, one per line, and the line it prints on stdout is
      taken as the chosen output (e.g. ``"wofi --dmenu"``).

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
