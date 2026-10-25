shoestring-remote-server
========================

Synopsis
--------

| **shoestring-remote-server** [**--socket** *PATH* \| **--tcp** *ADDR:PORT* [**--tcp-any-host**]] [**--output** *NAME*]

Description
-----------

The serve side of shoestring-wm's remote desktop. It runs on the machine
being viewed, usually from the WM's ``autostart``, and registers with the
local **shoestring-wm**\(1) over its IPC socket.

While the WM's remote gate is off the server listens on nothing. When it is
turned on (``shoestring-ctl remote on``, or the bar's remote chip) the server
opens its listener; each viewer that connects sees the served output and
drives it, and the bar shows that the session is being viewed. Turning the
gate off, or the server exiting, ends every session and closes the listener.

By default the listener is a Unix socket of mode 0600, and a connection is
accepted only from a process running as the same user. A viewer on another
machine reaches it through OpenSSH's socket forwarding, so the ssh login is
the authentication::

    ssh -N -L "$XDG_RUNTIME_DIR/box.sock:<socket path>" box
    shoestring-remote-client --connect "$XDG_RUNTIME_DIR/box.sock"

``shoestring-ctl remote status`` reports where the server listens, in
``endpoint``.

Options
-------

**--socket** *PATH*
    Listen on this Unix socket. The default is
    ``$XDG_RUNTIME_DIR/shoestring-remote-$WAYLAND_DISPLAY.sock``.

**--tcp** *ADDR:PORT*
    Listen on TCP instead. Nothing checks who connects: on a loopback
    address any local user can see and control the session. A host name is
    resolved once at startup and checked by the address it resolves to.
    Only a loopback address is accepted unless **--tcp-any-host** is also
    given. The server logs a warning at startup.

**--tcp-any-host**
    Allow **--tcp** on an address other than loopback, ``0.0.0.0``
    included. Every host that can reach that address and port can then see
    and control the session. Refused without this.

**--output** *NAME*
    The WM output to serve. The default is the first output.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print the version and exit.

**--socket** and **--tcp** are mutually exclusive.

Environment
-----------

``SHOESTRING_WM_SOCKET``
    The WM's IPC socket. Required to find the WM.

``SHOESTRING_REMOTE_SOCKET``, ``SHOESTRING_REMOTE_TCP``, ``SHOESTRING_REMOTE_OUTPUT``
    The same as **--socket**, **--tcp** and **--output**. A flag wins over
    its variable.

``SHOESTRING_REMOTE_TCP_ANY_HOST``
    ``1`` is the same as **--tcp-any-host**; ``0`` or unset is not. Any other
    value is an error.

``SHOESTRING_REMOTE_SERVER_LOG``
    Append the server's log to this file.

``RUST_LOG``
    The log filter.

The old ``SHOESTRING_REMOTE_BIND`` and ``SHOESTRING_REMOTE_PORT`` settings
are refused with an error rather than ignored.

Exit status
-----------

**0**
    The WM went away, or **--help** or **--version** was given.

**1**
    Bad arguments, a refused TCP address, or a failure to reach the WM or
    open the listener; see stderr.

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1), **ssh**\(1)
