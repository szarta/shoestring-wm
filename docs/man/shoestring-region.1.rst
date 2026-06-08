shoestring-region
=================

Synopsis
--------

| **shoestring-region**

Description
-----------

An interactive rectangle picker for Wayland, equivalent to **slurp**. It
opens a translucent fullscreen layer-shell overlay; the user drags to
select a rectangle and on release the selection is written to standard
output. It is the picker behind ``shoestring-screenshot --region`` but is
usable standalone wherever a region must be chosen.

Takes no arguments. **Escape** (or a degenerate, zero-area drag) cancels.

Output
------

On a successful selection a single line is written to standard output:

::

    OUTPUT_NAME X Y W H

space-separated, where *OUTPUT_NAME* is the ``wl_output`` name the drag
started on and *X Y W H* are integer logical pixels relative to that
output. On cancel nothing is printed.

Environment
-----------

**WAYLAND_DISPLAY**
    The Wayland display to connect to. Required.

**XCURSOR_THEME**
    Cursor theme name for the crosshair. Default ``default``.

**XCURSOR_SIZE**
    Cursor size in pixels. Default ``24``.

Exit status
-----------

**0**
    A rectangle was selected and printed.

**1**
    Cancelled (Escape or empty selection). On a connection error a
    message is printed to standard error.

See also
--------

**shoestring-screenshot**\(1), **shoestring-wm**\(1)
