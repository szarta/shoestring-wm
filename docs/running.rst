Running shoestring-wm
=====================

shoestring-wm has two backends. They are selected automatically from the
environment but can be forced with ``--backend``.

============  ===========================================================
**winit**     A nested window inside an existing X11 or Wayland session.
              Used for development and quick demos. Selected when either
              ``$WAYLAND_DISPLAY`` or ``$DISPLAY`` is set.
**tty**       Native DRM/KMS + libinput + libseat. The daily-driver
              path. Selected when both env vars are unset.
============  ===========================================================

Command-line flags
------------------

``-c, --config PATH``
    Path to a TOML config file. Defaults to
    ``$XDG_CONFIG_HOME/shoestring-wm/config.toml`` (falling back to
    ``$HOME/.config/...``).

``-b, --backend winit|tty``
    Override the auto-detected backend.

``-C, --command CMD``
    Spawn ``CMD`` once the compositor is up. Defaults to
    ``weston-terminal``. The command is split on whitespace; quoting is
    not currently supported.

``--write-default-config``
    Write the bundled default config to the user config path (or to
    ``--config PATH`` if given) and exit. Refuses to overwrite an
    existing file unless ``--force`` is also passed.

``--force``
    Allow ``--write-default-config`` to overwrite an existing file.

``--enable-automation``
    Force the runtime automation gate ON at startup, regardless of
    ``[general].automation_enabled`` in the config. The gate is off by
    default; ``inject_key`` / ``inject_text`` / ``inject_click`` /
    ``move_mouse`` / ``screenshot`` / ``run_command`` /
    ``dispatch_action`` all refuse
    while it is off. The runtime ``shoestring-ctl automation on/off``
    can still flip the gate; the config file is the source of truth at
    next start. See :doc:`ipc`.

``-V, --version``
    Print the binary version.

Environment variables
---------------------

The WM reads these:

``XDG_CONFIG_HOME``, ``HOME``
    Used to resolve the default config path
    ``$XDG_CONFIG_HOME/shoestring-wm/config.toml``.

``RUST_LOG``
    Standard ``tracing_subscriber`` filter. ``info`` is the default;
    ``debug`` is useful when diagnosing input or output issues.

``SHOESTRING_WM_LOG``
    If set, tracing output is appended to this file (with ANSI escapes
    disabled) instead of stderr. Practical when running on a TTY where
    stderr scrolls past on the console.

It exports these for child processes:

``WAYLAND_DISPLAY``
    Set to the compositor's wayland socket name before any child is
    spawned.

``SHOESTRING_WM_SOCKET``
    Path to the IPC socket. Clients (``shoestring-ctl``, the bar) read
    this. See :doc:`ipc`.

Running under winit (development)
---------------------------------

Inside an existing X11 or Wayland session::

    shoestring-wm --command alacritty

A new window opens; that window is the "screen". Spawn more clients
from inside it with the bound keys (``Super+Return`` for a terminal,
``Super+P`` for the launcher).

Quit with ``Super+Shift+Q``.

Running on a TTY (daily driver)
-------------------------------

Switch to a TTY (``Ctrl+Alt+F2``), log in, and launch::

    shoestring-wm

The TTY backend uses **libseat** for session/VT management. With a
``seatd``-style setup (most distros today, including those running
systemd-logind):

- Running as your normal user *just works*: libseat negotiates DRM/input
  access via the active session.
- If you see a "permission denied" opening ``/dev/dri/card0`` or an
  input device, your session is not the active seat. Common fixes:

  - Make sure you actually logged in on this TTY (not via SSH).
  - Confirm ``seatd`` is running, or that you have a systemd-logind
    session: ``loginctl show-session $XDG_SESSION_ID | grep Active``.
  - On distros without systemd, add your user to the ``seat`` group and
    enable the ``seatd`` service.

``Ctrl+Alt+F1..F12`` switches the active VT, as on getty / X /
Openbox. The binds are wired by default.

Running as a systemd user service
----------------------------------

The display-manager session path (the ``.desktop`` entry, see
:doc:`install`) is the usual way in. If you instead drive the session
from a ``systemd`` **user unit** — e.g. a ``shoestring-wm.service`` that
other units order themselves ``After=`` — declare it ``Type=notify``:

.. code-block:: ini

    [Service]
    Type=notify
    ExecStart=/usr/local/bin/shoestring-wm-session

The compositor sends ``sd_notify(READY=1)`` once the wayland socket and
IPC are listening and ``$WAYLAND_DISPLAY`` has been pushed into the user
manager. systemd holds units ordered ``After=`` this service in the
*activating* state until then, so companion services (bars, portals,
notification daemons) no longer race ``$WAYLAND_DISPLAY``. The signal is
keyed on ``$NOTIFY_SOCKET``, which systemd sets only for a ``Type=notify``
service — it is a harmless no-op for the display-manager and nested-winit
launch paths, which leave it unset.

Autostart and companion processes
---------------------------------

The WM spawns an autostart list once the wayland socket is up and
before any user interaction. Configure it via ``[general].autostart``
(see :doc:`configuration`); the default list is ``["shoestring-bar",
"shoestring-mediad"]`` so a fresh user gets the bar plus the
media-privacy monitor on first run. Each entry is split on whitespace
(first token = executable, rest = args). Failures log a warning and
don't block startup.

For one-off launches alongside the WM, ``--command CMD`` still spawns
a single client once the compositor is ready — handy for ``shoestring-wm
--command alacritty`` during nested winit development.

Media privacy (``shoestring-mediad``)
-------------------------------------

``shoestring-mediad`` is the autostarted media-privacy monitor. It links
PipeWire and reports three things to the WM, which the bar surfaces as
**MUTE / MIC / CAM** chips (and rows in the control menu):

- **MUTE** — the default audio *output* is muted. Click the control-menu
  row, bind a key to ``toggle-audio-mute``, or run ``shoestring-ctl media
  audio-mute on|off`` to toggle it. Real, authoritative control.
- **MIC** — the microphone is *live* (unmuted). ``toggle-mic-mute`` /
  ``shoestring-ctl media mic-mute on|off`` mute it. Honest caveat: this
  stream-mutes the source so capturing apps get silence, but it does
  **not** prevent a device open — the same guarantee as a hardware
  mic-mute key.
- **CAM** — a camera is *actively streaming*. **Status only**: there is no
  software camera off-switch, by design. It detects cameras opened through
  PipeWire (the camera portal, browsers/Electron); a raw ``/dev/video*``
  open that bypasses PipeWire is not visible.

The monitor always *reflects live state* — it reads the same default-sink/
source mute that ``pavucontrol``, media keys, and ``wpctl`` change, so the
chips stay accurate no matter who flips the mute. Mute *control* prefers
``wpctl`` (from WirePlumber) for an authoritative result, falling back to a
native PipeWire set when ``wpctl`` is absent. Where PipeWire (or the
``shoestring-mediad`` binary) is missing, the monitor simply doesn't run
and the bar shows no media chips — a graceful no-op. The whole group can be
hidden with ``show_media = false`` in the bar config.

These controls are reachable three ways, like the other privacy gates: the
bar control menu, a keybind (``toggle-audio-mute`` / ``toggle-mic-mute``),
and IPC (``shoestring-ctl media …``).

Quitting
--------

``Super+Shift+Q`` (the default bind for the ``quit`` action) brings up
a modal yes/no confirmation rendered by ``shoestring-confirm``; the
session exits on *Yes* (Enter), stays running on *No* (Escape). On the
TTY backend a confirmed quit drops you back to the console you started
from.
