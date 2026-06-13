shoestring-menu
===============

Synopsis
--------

| **shoestring-menu** [**--mode** commands|bookmarks|windows] [**--source** *PATH*]
| **shoestring-menu** **-h** | **--help**

Description
-----------

A dmenu-style launcher for **shoestring-wm**\(1). Displays a Wayland
layer-shell panel of candidates filtered as you type, acts on the
selection, and exits.

Three modes share the same UI:

**commands**
    Reads one command per line from the source file. Blank lines and
    lines beginning with ``#`` are skipped. The selected line is
    whitespace-split and exec'd, detached via ``setsid``.

**bookmarks**
    Reads a markdown bookmarks file. Each line is parsed for a
    ``[label](url)`` segment; the entire line (including any HTML
    comment tags) is displayed and searchable. The URL is opened via
    ``xdg-open`` on selection.

**windows**
    A live window switcher. Candidates come from the running WM over its
    IPC socket (not a file): one row per mapped window across every
    workspace, labelled ``[workspace] app-id — title`` (focused window
    marked ``*``). Selecting a row switches to that window's workspace and
    focuses it — unminimizing if needed — via the ``FocusWindow`` IPC
    request. ``--source`` is ignored.

Options
-------

**--mode** commands|bookmarks|windows
    Select the mode. Default ``commands``.

**--source** *PATH*
    Override the default candidate source file (commands/bookmarks only).

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

``SHOESTRING_WM_SOCKET``
    Path to the WM IPC socket, used by ``--mode windows`` to list and
    focus windows. Falls back to
    ``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``. Unused by
    the other modes.

``XDG_CONFIG_HOME``, ``HOME``
    Resolve the default source files (commands/bookmarks modes).

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
    Act on the highlighted candidate (spawn it, open the URL, or focus
    the window, depending on mode).

``Escape``
    Exit without running anything.

``Up`` / ``Down``
    Move highlight.

Any printable key
    Type to filter the candidate list.

See also
--------

**shoestring-wm**\(1), **shoestring-bar**\(1), **xdg-open**\(1)
