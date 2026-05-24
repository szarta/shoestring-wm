shoestring-menu
===============

Synopsis
--------

| **shoestring-menu** [**--mode** commands|bookmarks] [**--source** *PATH*]
| **shoestring-menu** **-h** | **--help**

Description
-----------

A dmenu-style launcher for **shoestring-wm**\(1). Displays a Wayland
layer-shell panel of candidates filtered as you type, runs the
selection, and exits.

Two modes share the same UI:

**commands**
    Reads one command per line from the source file. Blank lines and
    lines beginning with ``#`` are skipped. The selected line is
    whitespace-split and exec'd, detached via ``setsid``.

**bookmarks**
    Reads a markdown bookmarks file. Each line is parsed for a
    ``[label](url)`` segment; the entire line (including any HTML
    comment tags) is displayed and searchable. The URL is opened via
    ``xdg-open`` on selection.

Options
-------

**--mode** commands|bookmarks
    Select the mode. Default ``commands``.

**--source** *PATH*
    Override the default candidate source file.

**-h**, **--help**
    Print short usage.

Default source files
--------------------

When **--source** is not given:

``$XDG_CONFIG_HOME/shoestring-wm/executables``
    Default for ``--mode commands``.

``$XDG_CONFIG_HOME/shoestring-wm/bookmarks``
    Default for ``--mode bookmarks``.

Both files share the WM's config directory by design.

Environment
-----------

``WAYLAND_DISPLAY``
    Required.

``XDG_CONFIG_HOME``, ``HOME``
    Resolve the default source files.

``SHOESTRING_MENU_FONT``
    Override the font search path (a TTF file). Same idea as
    ``$SHOESTRING_BAR_FONT``.

``SHOESTRING_MENU_LOG``
    If set, tracing output goes to this file instead of stderr.

``RUST_LOG``
    ``tracing_subscriber`` filter; default ``info``.

Keybindings (inside the menu)
-----------------------------

``Enter``
    Run the highlighted candidate.

``Escape``
    Exit without running anything.

``Up`` / ``Down``
    Move highlight.

Any printable key
    Type to filter the candidate list.

See also
--------

**shoestring-wm**\(1), **shoestring-bar**\(1), **xdg-open**\(1)
