shoestring-notify
=================

Synopsis
--------

| **shoestring-notify**
| **shoestring-notify** **--write-default-config** [**--force**]

Description
-----------

A lightweight notification daemon for **shoestring-wm**\(1). It claims the
``org.freedesktop.Notifications`` name on the session bus and implements
the freedesktop Desktop Notifications specification (v1.2), rendering each
toast as a Wayland layer-shell surface.

Run with no arguments it starts the daemon in the foreground (it does not
fork). On startup it takes over the notification name from any daemon
already holding it. It exits cleanly on ``SIGINT`` / ``SIGTERM``.

Options
-------

**--write-default-config**
    Write the bundled default configuration to
    ``$XDG_CONFIG_HOME/shoestring-notify/config.toml`` (or
    ``~/.config/shoestring-notify/config.toml``) and exit. Refuses to
    overwrite an existing file unless **--force** is also given.

**--force**
    Permit **--write-default-config** to overwrite an existing config
    file. Only valid together with that flag.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print version and exit.

Configuration
-------------

Read from ``$XDG_CONFIG_HOME/shoestring-notify/config.toml`` (falling back
to ``~/.config``). A missing file is normal — defaults apply. All keys
live under a ``[notify]`` table:

**position** (default ``top-right``)
    Toast corner: ``top-right``, ``top-left``, ``bottom-right``, or
    ``bottom-left``.

**background** (default ``#222222``), **foreground** (default ``#ffffff``)
    Toast background and text colours.

**border** (default ``#557788``)
    Accent stripe, drawn only for ``critical`` urgency.

**default_timeout_ms** (default ``5000``)
    Toast lifetime in milliseconds when the sender passes a timeout of
    ``-1``.

**max_width** (default ``400``)
    Toast width in logical pixels.

**gap** (default ``8``)
    Pixels between toasts and the screen edge.

**padding** (default ``12``)
    Inner padding on all four sides.

**font** (default unset)
    Path to a TTF/OTF font for toast text.

**summary_size** (default ``15.0``), **body_size** (default ``13.0``)
    Font sizes for the title and body.

Colours accept ``#RGB``, ``#RRGGBB``, ``#AARRGGBB`` and ``0x`` forms.

D-Bus interface
---------------

Owns ``org.freedesktop.Notifications`` at
``/org/freedesktop/Notifications`` and implements ``GetCapabilities``
(advertising ``body`` and ``persistence``), ``GetServerInformation``,
``Notify``, and ``CloseNotification``, and emits the
``NotificationClosed`` signal. The ``urgency`` and ``image-path`` hints
are honoured; ``timeout`` of ``-1`` uses ``default_timeout_ms`` and ``0``
never expires.

Environment
-----------

**WAYLAND_DISPLAY**
    The Wayland display to connect to. Required.

**DBUS_SESSION_BUS_ADDRESS**
    The session bus to claim the name on. Required.

**SHOESTRING_NOTIFY_FONT**
    Font path, consulted after the config ``font`` key but before the
    built-in candidates.

**RUST_LOG**
    Tracing filter for diagnostic logging. Default ``info``.

Exit status
-----------

**0**
    Config written (with **--write-default-config**), or the daemon
    exited cleanly on a signal.

**1**
    Invalid arguments, config load/write error, or a bus/startup
    failure. A message is printed to standard error.

See also
--------

**shoestring-wm**\(1)
