shoestring-ctl
==============

Synopsis
--------

| **shoestring-ctl** [**-s** *PATH*] [**-p**] workspaces
| **shoestring-ctl** [**-s** *PATH*] [**-p**] windows
| **shoestring-ctl** [**-s** *PATH*] [**-p**] outputs
| **shoestring-ctl** [**-s** *PATH*] [**-p**] event-stream

Description
-----------

Reference CLI client for the **shoestring-wm**\(1) IPC socket.
Connects to ``$SHOESTRING_WM_SOCKET`` (or the default
``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``), sends one
request, and either prints the response or streams events.

Output is newline-delimited JSON (one object per line) unless
**--pretty** is given.

Subcommands
-----------

**workspaces**
    Print the active workspace index and workspace count.

**windows**
    Print every mapped window: ``id``, ``title``, ``app_id``,
    ``workspace``, ``focused``.

**outputs**
    Print every connected output: ``name``, ``width``, ``height``,
    ``scale``.

**event-stream**
    Subscribe to events. After a one-line ``Ok`` ack the server pushes
    one JSON event per line forever (or until the WM exits).

Options
-------

**-s**, **--socket** *PATH*
    Override the socket path.

**-p**, **--pretty**
    Indent JSON output for human reading.

**-V**, **--version**
    Print version.

**-h**, **--help**
    Print short usage.

Exit status
-----------

**0**
    Normal exit (server replied; event-stream was closed by the server).

**1**
    Server returned an ``error`` response, or no socket path could be
    resolved.

Other
    I/O or parse error; see stderr.

See also
--------

**shoestring-wm**\(1)
