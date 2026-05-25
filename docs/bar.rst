shoestring-bar
==============

shoestring-bar is the status bar that ships alongside shoestring-wm. It
is a pure ``wayland-client`` program (no Smithay, no GTK, no system
bus): it uses ``zwlr_layer_shell_v1`` to attach itself to the bottom
edge of an output, ``ext-foreign-toplevel-list-v1`` to track the open
windows, and the shoestring-wm IPC stream for the active workspace and
the currently focused window.

The bar lives in its own repo at ``~/data/shoestring-bar`` and is
released in lockstep with the WM.

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
    position    = "bottom"   # "bottom" | "top"
    height      = 24
    background  = "#222222"  # #RGB, #RRGGBB, or #AARRGGBB
    foreground  = "#ffffff"
    font        = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
    font_size   = 14.0

    [clock]
    format = "%a %b %d  %H:%M"  # strftime(3) pattern
    # format = "24h-short"       # alias for "%H:%M"
    # format = "iso"             # alias for "%Y-%m-%d %H:%M:%S"

Colors are parsed as hex; an opaque alpha is assumed unless you spell out
all eight digits. Font resolution order is ``[bar].font`` →
``$SHOESTRING_BAR_FONT`` → built-in candidate paths.

The clock format is passed straight to ``libc::strftime``; the two
named aliases above are the only special-cased values.

Limitations (v1)
----------------

- The accent (focused-workspace) and dim (inactive) colors are baked
  in; only background and foreground are user-configurable.
- Only horizontal bars are supported. Vertical (left/right) bars are
  not planned.
- Only one monitor is rendered to. Multi-output is on the roadmap.
- There is no system-tray support and no plan to add one.
