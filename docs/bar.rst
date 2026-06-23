shoestring-bar
==============

shoestring-bar is the status bar that ships alongside shoestring-wm. It
is a ``wayland-client`` program (no Smithay, no GTK toolkit): it uses
``zwlr_layer_shell_v1`` to attach itself to the bottom edge of an
output, ``ext-foreign-toplevel-list-v1`` to track the open windows, and
the shoestring-wm IPC stream for the active workspace and the currently
focused window. When a session D-Bus is present it additionally hosts a
:ref:`system tray <bar-system-tray>`; without one it runs trayless.

The bar lives in the shoestring-wm workspace at
``crates/shoestring-bar/`` (it used to be a standalone sibling repo;
folded in during the monorepo migration so a single
``cargo build --workspace`` produces it alongside everything else).

Running
-------

The bar takes essentially no flags — launching it is enough:

.. code-block:: console

    $ shoestring-bar

The only non-runtime flag is ``--write-default-config`` (covered in the
:ref:`bar-configuration` section), which writes a commented starter
config and exits.

The bar binds to whatever Wayland compositor ``$WAYLAND_DISPLAY``
points at. Without a config file it anchors to the bottom edge of the
first output and reserves a 24-pixel exclusive zone; the
:ref:`bar-configuration` section below covers how to change that.

If the bar starts before the WM (or against a compositor that doesn't
support ``zwlr_layer_shell_v1``), it exits with an error explaining
which global was missing.

Environment
-----------

``WAYLAND_DISPLAY``
    Required — the bar is a Wayland client.

``SHOESTRING_WM_SOCKET``
    Optional. When present, the bar subscribes to WM events for
    workspaces and focus changes. If unset (or if the IPC connection
    fails), the bar still renders the window list and the clock — just
    without workspace state or focused-window highlighting.

``SHOESTRING_BAR_FONT``
    Optional override path to a TTF file. Useful for testing or for
    distros whose default font search path differs from the bundled
    candidates.

``RUST_LOG``
    Standard ``tracing_subscriber`` filter (``info`` by default).

Font discovery
--------------

To stay free of fontconfig, the bar walks a hard-coded list of common
sans-serif paths and uses the first one that exists. The list covers
Debian/Ubuntu, Arch, Fedora/RHEL and FreeBSD defaults
(``DejaVuSans.ttf``, ``LiberationSans-Regular.ttf``,
``NotoSans-Regular.ttf``). Use ``SHOESTRING_BAR_FONT`` (or the
``[bar].font`` config key) to point at any other TTF.

.. _bar-configuration:

Configuration
-------------

The bar reads an optional TOML file at
``$XDG_CONFIG_HOME/shoestring-bar/config.toml`` (defaulting to
``~/.config/shoestring-bar/config.toml``). When the file is missing, the
bar starts with the same hardcoded defaults it shipped with — every
field below is optional.

To drop a fully-commented starter file at that path, run:

.. code-block:: console

    $ shoestring-bar --write-default-config

It refuses to overwrite an existing file unless ``--force`` is also
passed. The written contents match the schema below.

.. code-block:: toml

    [bar]
    position        = "bottom"   # "bottom" | "top"
    height          = 24
    background      = "#222222"  # #RGB, #RRGGBB, or #AARRGGBB
    foreground      = "#ffffff"
    font            = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
    font_size       = 14.0
    show_workspaces = true       # hide the box cluster + active name

    [clock]
    format = "%a %b %d  %H:%M"   # strftime(3) pattern
    # format = "24h-short"        # alias for "%H:%M"
    # format = "iso"              # alias for "%Y-%m-%d %H:%M:%S"

    [battery]
    show               = true             # auto-hidden when no battery
    format             = "{pct}%{sign}"   # 85%- discharging, 85%+ charging
    low_threshold      = 20               # orange at or below
    critical_threshold = 10               # red at or below

Colors are parsed as hex; an opaque alpha is assumed unless you spell out
all eight digits. Font resolution order is ``[bar].font`` →
``$SHOESTRING_BAR_FONT`` → built-in candidate paths.

The clock format is passed straight to ``libc::strftime``; the two
named aliases above are the only special-cased values.

Battery indicator
-----------------

When present, the battery readout is drawn immediately to the left of
the clock (and to the left of the ``AUTO`` automation chip when that's
shown). It re-polls on the bar's existing 1-second tick — no extra
file descriptors.

Source detection runs once at startup and falls through per platform:

- **Linux**: the first ``/sys/class/power_supply/BAT*`` entry
  (``capacity`` + ``status`` files).
- **FreeBSD**: ``sysctlbyname`` against
  ``hw.acpi.battery.{life,state,units}``. ``units == 0`` is treated
  as "no battery present".
- **Other OSes** (and BSDs without an ACPI battery): the indicator is
  hidden entirely so the bar layout doesn't reserve dead space.

Format placeholders:

``{pct}``
    Current capacity as an integer (e.g. ``85``).

``{sign}``
    ``+`` while charging, ``-`` while discharging, empty string when
    full or in an unknown state.

Color override: below ``low_threshold`` the indicator paints orange;
below ``critical_threshold`` it paints red. Both thresholds are only
applied while discharging — a charging or full battery stays in the
normal foreground color even at low capacity.

.. _bar-system-tray:

System tray
-----------

When a session D-Bus is reachable, the bar becomes a
``org.kde.StatusNotifierWatcher`` + host and shows tray icons on the
right, just left of the clock cluster. If another process already owns
the watcher, the bar bows out and runs trayless — it never fights an
existing tray. There is no configuration; the tray appears whenever
items register.

Icons resolve in two ways:

- **Themed name** (``IconName``): appindicator/ayatana apps (nm-applet,
  blueman) advertise a freedesktop icon *name*, resolved against the
  active icon theme (PNG or SVG) — crisp and theme-following.
- **Raw pixmap** (``IconPixmap``): KDE/Qt apps that ship ARGB32 pixmaps
  instead of a name are blitted directly. The name path wins when an
  item offers both.

Left-clicking an item opens its ``com.canonical.dbusmenu`` context menu
as a cascade of popups; items without a menu get an ``Activate`` call
(the StatusNotifier left-click default) instead. In the menu, hovering a
row with a submenu arrow opens it, and the menu live-updates while open
when the application changes its layout. Keyboard navigation (arrows /
Enter / Esc) works alongside the pointer.

Limitations (v1)
----------------

- The accent (focused-workspace) and dim (inactive) colors are baked
  in; only background and foreground are user-configurable.
- Only horizontal bars are supported. Vertical (left/right) bars are
  not planned.
- Only one monitor is rendered to. Multi-output is on the roadmap.
