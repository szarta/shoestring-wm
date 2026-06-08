shoestring-kill
===============

Synopsis
--------

| **shoestring-kill**

Description
-----------

An **xkill**-equivalent window picker for **shoestring-wm**\(1). Asks the
WM to enter pick mode, waits for the user to click a toplevel window, and
then closes it (sending ``xdg_toplevel.close``, so the client may still
surface a save-prompt rather than exit immediately).

It is a thin wrapper over the WM's IPC ``pick-window`` / ``close-window``
requests — see **shoestring-ctl**\(1) for the underlying surface.

Takes no arguments.

Usage:

- **Left-click** a window — close it (exit ``0``);
- **Escape** or **right-click** — cancel (exit ``1``).

Environment
-----------

**SHOESTRING_WM_SOCKET**
    Path to the WM control socket. Must be set (the WM normally exports
    it into the session environment).

Exit status
-----------

**0**
    A window was selected and closed.

**1**
    Cancelled (Escape or right-click).

**2**
    Error — could not resolve or connect to the socket, or the WM
    returned an unexpected response. A message is printed to standard
    error.

Output
------

Progress and the outcome are reported on standard error, e.g.
``shoestring-kill: closed Firefox (firefox)``. Nothing is written to
standard output.

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1)
