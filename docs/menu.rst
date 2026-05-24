shoestring-menu
===============

shoestring-menu is a dmenu-style launcher: a Wayland layer-shell panel
that filters a list of candidates as you type, runs the selection, and
exits. It has two modes — **commands** and **bookmarks** — sharing the
same UI.

Like the bar, the menu is a thin ``wayland-client`` program that lives
in its own repo at ``~/data/shoestring-menu``.

Modes
-----

**commands** (default)
    Reads one command per line from the source file, displays it,
    spawns it on selection. Comment lines (``#``) and blank lines are
    skipped. Args are split on whitespace at exec time.

**bookmarks**
    Reads a markdown bookmarks file. Each line is parsed for a
    ``[label](url)`` segment; the full line is displayed (so tags and
    URL are fuzzy-searchable) and the URL is launched via ``xdg-open``.
    Lines without a parseable link are skipped.

Source files
------------

By default, both modes read from
``$XDG_CONFIG_HOME/shoestring-wm/``:

============================  =========================================
**commands** mode             ``executables`` — one command per line.
**bookmarks** mode            ``bookmarks`` — markdown link list.
============================  =========================================

Both files share the WM's config directory deliberately: the menu and
the WM are configured side by side.

A ``bookmarks`` file looks like::

    # Reference
    - [Smithay docs](https://smithay.github.io/smithay/) <!-- TAGS: wayland rust -->
    - [niri](https://github.com/YaLTeR/niri) <!-- TAGS: wayland reference -->

    # Daily
    - [Mail](https://mail.example.com) <!-- TAGS: web personal -->

Tags in comments are fuzzy-searchable because the entire line is
displayed and searched.

Command-line
------------

``--mode commands|bookmarks``
    Select the mode. Defaults to ``commands``.

``--source PATH``
    Override the candidate source file. Useful for ad-hoc menus
    (``shoestring-menu --source /tmp/choices.txt``).

``-h, --help``
    Print short usage.

Default bindings (from ``shoestring-wm --write-default-config``):

================  =================================================
``Super+P``       ``shoestring-menu`` (commands mode).
``Super+B``       ``shoestring-menu --mode bookmarks``.
================  =================================================

Environment
-----------

``WAYLAND_DISPLAY``
    Required.

``XDG_CONFIG_HOME``, ``HOME``
    Used to resolve the default source files.

``SHOESTRING_MENU_FONT``
    Optional path to a TTF (same idea as ``SHOESTRING_BAR_FONT``).

``SHOESTRING_MENU_LOG``
    If set, tracing output goes to this file (handy on a TTY).

``RUST_LOG``
    Standard tracing filter.

Behavior notes
--------------

- Pressing ``Escape`` or losing focus (the compositor closing the layer
  surface) exits without launching anything.
- Selections are run *detached* (``setsid``), so closing the menu doesn't
  kill the spawned program.
- ``Enter`` runs the highlighted item; arrow keys move the highlight;
  typing filters the list.
