Running shoestring-wm
=====================

shoestring-wm has three backends. The first two are selected automatically
from the environment; all can be forced with ``--backend``.

============  ===========================================================
**winit**     A nested window inside an existing X11 or Wayland session.
              Used for development and quick demos. Selected when either
              ``$WAYLAND_DISPLAY`` or ``$DISPLAY`` is set.
**tty**       Native DRM/KMS + libinput + libseat. The daily-driver
              path. Selected when both env vars are unset.
**headless**  One virtual output and no physical display - rendered on a
              DRM render node when one is available, else on the CPU
              (pixman) so a host with no ``/dev/dri`` still renders.
              Never auto-detected - request it explicitly with
              ``--backend headless``. The foundation for remote-desktop
              *serve mode*, and useful on its own for CI, the WLCS
              harness, and automation (see :doc:`app-testing`). Included in the default build (it
              adds no dependencies the ``tty`` backend does not already
              pull), so a stock ``cargo install`` / package has it; only a
              ``--no-default-features`` build without ``headless`` omits it.
============  ===========================================================

The headless backend prefers an unprivileged GPU render node - so it needs
no seat, VT, or display manager - and drives its render at a fixed cadence
(there is no vblank). Nothing scans the rendered frame out; it is reached
only through the capture path (``shoestring-ctl screenshot``,
``wlr-screencopy`` / ``ext-image-copy-capture``).

Three renderers can composite the output, chosen with ``[headless] renderer``
in the config (see :doc:`configuration`) or the ``--renderer`` flag:

``gles``
    GLES on the GPU render node. Fails to start without one. The only renderer
    that offers clients ``zwp_linux_dmabuf_v1``, so GPU-accelerated clients can
    hand over dmabuf buffers.

``pixman``
    A CPU compositor (the pixman library the X server uses). Needs no GPU and
    no EGL. For desktop content it costs a small fraction of llvmpipe's CPU:
    an 800x600 window repainting at 30 fps took about a tenth as much.

``llvmpipe``
    GLES on Mesa's software rasterizer (a surfaceless EGL platform). Kept for
    comparison; it runs every frame through a full GL pipeline and a thread
    pool, which costs far more CPU than pixman.

``auto``, the default, takes the GPU when a render node opens and pixman
otherwise (a CPU-only CI runner, or a container started without ``--device``),
logging a warning. Clients render their own content either way; without a GPU
they hand the compositor shared-memory buffers. The software renderers blend and
filter slightly differently from each other and from the GPU, so compare
screenshots taken on the same renderer. Three environment knobs:

``$SHOESTRING_WM_RENDER_NODE``
    Render node to open. Defaults to the first existing of
    ``/dev/dri/renderD128`` - ``renderD130``.

``$SHOESTRING_WM_HEADLESS_SOFTWARE``
    Set to ``1`` and ``auto`` skips the GPU even on a box that has one -
    useful for reproducing the CPU-only path. An explicit ``renderer`` wins.

``$SHOESTRING_WM_HEADLESS_SIZE``
    Virtual output mode size as ``WIDTHxHEIGHT`` (e.g. ``2560x1440``).
    Defaults to ``1920x1080``. The ``--output-size`` flag is sugar for
    this and takes precedence. A future remote client resizes the
    virtual output to its own pixel size.

Because the headless backend needs only an unprivileged render node - no seat,
master, or input devices - it is a natural fit for running the whole session in
a container and viewing it over serve mode. See :doc:`containers`.

Resource use
~~~~~~~~~~~~

With nothing to pace it, the headless backend renders on a ~60 Hz timer. An
idle tick finds no damage and sends no frames to remote viewers, but it still
wakes the WM about 60 times a second. ``[headless] idle_suspend`` (off by
default; see :doc:`configuration`) parks the timer while nothing changes. The
next client commit, input, or IPC request restarts it, so it also renders a
commit sooner than the next tick would.

Repainting is the heavy workload, and the renderer decides its cost: pixman
and a GPU render node cost about the same, while Mesa's llvmpipe costs roughly
ten times as much CPU. While the session is locked, remote viewers get a static
frame rather than the animated lock screen (see :doc:`remote`), and the WM
composites only what the lock screen redraws.

For what a WM-only install leaves out, and the size of each binary, see
:ref:`install-minimal`.

Measured at v0.9.0 with ``scripts/measure-headless.sh``, on a 1920x1080
headless output, on a laptop with a 4-core, 8-thread Intel Core i7-10610U and
its integrated UHD Graphics (Mesa 25.2), 10 seconds per workload. CPU is a
percentage of one core; RSS is the WM's.

.. list-table::
   :header-rows: 1

   * - Workload
     - pixman
     - llvmpipe
     - gles (GPU)
   * - Idle, one window: WM CPU, wakeups/s
     - 1.6%, 60
     - 2.0%, 59
     - 1.9%, 59
   * - Idle, one window, ``idle_suspend`` on
     - 0.1%, 1
     - 0.0%, 1
     - 0.0%, 1
   * - 800x600 window repainting at 30 fps: WM CPU
     - 7.9%
     - 92%
     - 11%
   * - The same, viewer attached: WM CPU
     - 16%
     - 101%
     - 21%
   * - Locked (maze screensaver), viewer attached: WM CPU
     - 2.1%
     - 9.8%
     - 6.9%
   * - Commit-to-viewer latency, median / p95
     - 23 / 40 ms
     - 27 / 45 ms
     - 40 / 42 ms
   * - The same, ``idle_suspend`` on
     - 19 / 21 ms
     - 23 / 25 ms
     - 22 / 23 ms
   * - WM RSS
     - 26-56 MiB
     - 119-159 MiB
     - 91-118 MiB
   * - WM threads
     - 4
     - 21
     - 8

On every renderer the stream for the repainting window ran at about 280 KiB/s
and 29.5 frames/s; an idle or locked desktop streamed nothing. The remote
server stayed under 0.5% CPU and 4 MiB, and slept entirely while idle;
``shoestring-lock`` used about 1% CPU and 47 MiB.

To measure on your own machine, run::

    scripts/measure-headless.sh

It starts a fresh headless WM for each fixed workload: idle, a window
repainting at 30 fps, a remote viewer attached, commit-to-viewer latency, and
the lock screen. It reports CPU, wakeups, memory, stream bandwidth, and latency
for the WM and each helper process. ``SHOESTRING_MEASURE_IDLE_SUSPEND=both``
compares ``idle_suspend`` off and on, and ``SHOESTRING_E2E_BIN_DIR`` measures
another build's binaries, such as a release tag's. Numbers depend on the
machine and the renderer, so compare runs on the same host.

These measurements back the project's budgets for idle CPU, memory and
start time: at most 3% of a core idle (0.5% and 2 wakeups a second with
``idle_suspend``), at most 64 MiB of RSS on pixman and 128 MiB on the GPU,
and at most 100 ms from start to the first IPC reply. The budgets, with
those for binary size, dependencies and system packages, are in the
architecture guide (``docs/architecture.md``, *Lightweight budgets*). A change
that crosses one is fixed, moved behind a Cargo feature, or raises the
budget there with the reason.

Command-line flags
------------------

``-c, --config PATH``
    Path to a TOML config file. Defaults to
    ``$XDG_CONFIG_HOME/shoestring-wm/config.toml`` (falling back to
    ``$HOME/.config/...``).

``-b, --backend winit|tty|headless``
    Override the auto-detected backend. All three are in the default
    build; only a ``--no-default-features`` build can drop one (a build
    without the ``headless`` feature errors at startup if you pass
    ``--backend headless``).

``-C, --command CMD``
    Spawn ``CMD`` once the compositor is up, after ``[general] autostart``.
    Nothing is spawned without it. The command is split on whitespace;
    quoting is not currently supported.

``--output-size WxH``
    Headless backend only: fix the virtual output's mode size in pixels
    (e.g. ``1360x856``) instead of the ``1920x1080`` default. Gives batch
    automation a stable, reproducible output so coordinate-based clicks
    stay valid across runs. The logical size clients see is this divided
    by the output scale, so automation usually pairs it with scale 1.
    Sugar for ``$SHOESTRING_WM_HEADLESS_SIZE``; the flag wins. An invalid
    spec fails startup rather than silently defaulting.

``--renderer auto|gles|llvmpipe|pixman``
    Headless backend only: what composites the output (see above).
    Overrides ``[headless] renderer``.

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

    The gate is consent, not protection: any process running as your user can
    reach the IPC socket and turn it on itself, and any Wayland client can
    synthesize input through the virtual-pointer and virtual-keyboard protocols
    whatever the gate says. To keep software you don't trust out of the
    session, run it as another user, or in a container or sandbox without your
    ``$XDG_RUNTIME_DIR``. See :ref:`ipc-trust-model`.

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

Profiling with Tracy
--------------------

For diagnosing frame drops and render-loop stalls, the WM ships an optional
`Tracy <https://github.com/wolfpld/tracy>`_ profiler integration behind the
``profile-with-tracy`` Cargo feature. It is **off by default and compiled out
entirely** - a normal build carries no extra dependency and no per-frame cost;
the instrumentation macros become no-ops.

Build with the feature and run under a Tracy capture::

    cargo build --release --features profile-with-tracy
    ./target/release/shoestring-wm        # then connect tracy-profiler

The integration adds a ``tracing-tracy`` layer that forwards the WM's spans to
Tracy, a zone for each render step (``render_surface`` / ``render_frame`` on
the TTY backend, ``render_output`` under winit), and a frame mark on every
*drawn* frame - empty no-damage polls do not mark, so Tracy's frame timeline
tracks real presented frames, matching the ``render.fps`` metric. The Tracy
layer captures ``info,shoestring_wm=trace`` by default (so it sees the profile
zones without smithay/calloop trace spam); override it with the
``SHOESTRING_WM_TRACY`` environment variable, which takes the same syntax as
``RUST_LOG``. This complements the always-on metrics subsystem (``render.fps``,
frame-time gauges over IPC): metrics give you the numbers in production, Tracy
gives you the per-zone flame graph when you need to find *where* a frame went.

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
from a ``systemd`` **user unit** - e.g. a ``shoestring-wm.service`` that
other units order themselves ``After=`` - declare it ``Type=notify``:

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
service - it is a harmless no-op for the display-manager and nested-winit
launch paths, which leave it unset.

Autostart and companion processes
---------------------------------

The WM spawns an autostart list once the wayland socket is up and
before any user interaction. Configure it via ``[general].autostart``
(see :doc:`configuration`); the default list is ``["shoestring-bar",
"shoestring-mediad"]`` so a fresh user gets the bar plus the
media-privacy monitor on first run. Each entry is split on whitespace
(first token = executable, rest = args). Failures log a warning and
do not block startup.

For one-off launches alongside the WM, ``--command CMD`` still spawns
a single client once the compositor is ready - handy for ``shoestring-wm
--command alacritty`` during nested winit development.

Media privacy (``shoestring-mediad``)
-------------------------------------

``shoestring-mediad`` is the autostarted media-privacy monitor. It links
PipeWire and reports three things to the WM, which the bar surfaces as
**MUTE / MIC / CAM** chips (and rows in the control menu):

- **MUTE** - the default audio *output* is muted. Click the control-menu
  row, bind a key to ``toggle-audio-mute``, or run ``shoestring-ctl media
  audio-mute on|off`` to toggle it. Real, authoritative control.
- **MIC** - the microphone is *live* (unmuted). ``toggle-mic-mute`` /
  ``shoestring-ctl media mic-mute on|off`` mute it. Honest caveat: this
  stream-mutes the source so capturing apps get silence, but it does
  **not** prevent a device open — the same guarantee as a hardware
  mic-mute key.
- **CAM** - a camera is *actively streaming*. **Status only**: there is no
  software camera off-switch, by design. It detects cameras opened through
  PipeWire (the camera portal, browsers/Electron); a raw ``/dev/video*``
  open that bypasses PipeWire is not visible.

The monitor always *reflects live state* - it reads the same default-sink/
source mute that ``pavucontrol``, media keys, and ``wpctl`` change, so the
chips stay accurate no matter who flips the mute. Mute *control* prefers
``wpctl`` (from WirePlumber) for an authoritative result, falling back to a
native PipeWire set when ``wpctl`` is absent. Where PipeWire (or the
``shoestring-mediad`` binary) is missing, the monitor simply does not run
and the bar shows no media chips - a graceful no-op. The whole group can be
hidden with ``show_media = false`` in the bar config.

These controls are reachable three ways, like the other privacy gates: the
bar control menu, a keybind (``toggle-audio-mute`` / ``toggle-mic-mute``),
and IPC (``shoestring-ctl media …``).

Input methods and on-screen keyboards
-------------------------------------

The WM advertises the three cooperating globals an input method (IME) or
on-screen keyboard (OSK) needs, always on and with no configuration:

- ``zwp_text_input_manager_v3`` — the application/toolkit side that wants
  text (GTK, Qt, Chromium…).
- ``zwp_input_method_manager_v2`` — the IME side that composes and commits
  it (``fcitx5``, ``ibus`` with its Wayland front-end).
- ``zwp_virtual_keyboard_manager_v1`` - the side that *injects* keystrokes:
  an IME's latin pass-through, or an OSK turning taps into key events.

**IMEs** (CJK and other composed input) work out of the box: smithay bridges
text-input and input-method on keyboard focus, and the WM draws the candidate
popup on the normal window-popup path, anchored to the client's reported
text-cursor rectangle. The WM's keybind filter runs *before* the IME keyboard
grab, so ``Super`` binds keep working while an IME is active. Launch your IME
daemon from ``[general].autostart`` (above) like any other companion process.

**On-screen keyboards** - ``squeekboard``, ``wvkbd`` and similar - bind
``zwp_virtual_keyboard_v1`` to inject and ``zwlr_layer_shell_v1`` to place
themselves. Two behaviors make them work without any OSK-specific config:

- *Injection reaches the app, not the WM.* Virtual-keyboard input is routed
  straight to the surface that holds keyboard focus, **bypassing the WM's
  keybind filter**. An OSK types into the focused application and cannot
  accidentally trip ``Super`` binds; conversely the WM never swallows what it
  injects.
- *The keyboard does not steal focus.* An OSK maps as a layer surface with
  ``keyboard_interactivity = none``; the WM only ever transfers keyboard focus
  to a layer surface that asks for it (``exclusive``), so the keyboard stays
  visible while the application underneath keeps focus and receives the keys.
  A layer surface with an exclusive zone (an OSK anchored to the bottom edge)
  also reserves its space in the layout, so tiled windows resize around it.

There is nothing to enable: any client that binds these globals - an OSK, an
IME, or an automation tool - works against a running session immediately.

Quitting
--------

``Super+Shift+Q`` (the default bind for the ``quit`` action) brings up
a modal yes/no confirmation rendered by ``shoestring-confirm``; the
session exits on *Yes* (Enter), stays running on *No* (Escape). On the
TTY backend a confirmed quit drops you back to the console you started
from.
