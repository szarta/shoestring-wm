shoestring-bar
==============

Synopsis
--------

**shoestring-bar**

Description
-----------

A lightweight Wayland status bar for **shoestring-wm**\(1). Attaches to
the bottom edge of the first output via ``zwlr_layer_shell_v1`` and
shows workspaces, the open-window list, the focused window, and a
clock. Window data comes from ``ext-foreign-toplevel-list-v1``;
workspace and focus state come from the WM's IPC stream.

There are no command-line flags in v1; configuration is via environment
variables. The bar exits with a non-zero status if a required Wayland
global is missing (no layer-shell, no compositor).

Environment
-----------

``WAYLAND_DISPLAY``
    Required.

``SHOESTRING_WM_SOCKET``
    Optional. When set, the bar subscribes to WM events (workspace
    changes, focus changes). Without it the bar still renders the
    window list and the clock.

``SHOESTRING_BAR_FONT``
    Path to a TTF file overriding the bundled font search. The default
    search picks the first match from a hard-coded list of common
    Debian/Arch/Fedora/FreeBSD sans-serif paths (``DejaVuSans.ttf``,
    ``LiberationSans-Regular.ttf``, ``NotoSans-Regular.ttf``).

``RUST_LOG``
    ``tracing_subscriber`` filter; default ``info``.

``SHOESTRING_BAR_LOG``
    Optional. When set, tracing is appended to this file (with ANSI
    escapes disabled) instead of stderr. Mirrors ``SHOESTRING_WM_LOG``;
    the practical way to observe the bar's IPC event flow and which WM
    socket it connected to. Pair with ``RUST_LOG=debug``.

Limitations
-----------

- Only the first output is rendered to.
- Position (bottom edge), height (24 px), and colors are baked in.
- No system-tray support and no plan to add one.

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1), **shoestring-menu**\(1)
