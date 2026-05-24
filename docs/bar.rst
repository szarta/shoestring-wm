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

There are no command-line flags in v1; launching it is enough:

.. code-block:: console

    $ shoestring-bar

The bar binds to whatever Wayland compositor ``$WAYLAND_DISPLAY``
points at. It anchors to the bottom edge of the first output and
reserves a 24-pixel exclusive zone.

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
``NotoSans-Regular.ttf``). Use ``SHOESTRING_BAR_FONT`` to point at any
other TTF.

Limitations (v1)
----------------

- Position, height, and colors are baked in. Making them configurable
  is a tracked roadmap item.
- Only one monitor is rendered to. Multi-output is on the roadmap.
- There is no system-tray support and no plan to add one.
