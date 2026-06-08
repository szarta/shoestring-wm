shoestring-screenshot
=====================

Synopsis
--------

| **shoestring-screenshot** [**-o** *NAME*] [**-f** *PATH*] [**-c**] [**-r** | **--region-rect** *X,Y,W,H*]

Description
-----------

Capture a Wayland output, or a rectangle within one, to a PNG via the
compositor's ``wlr-screencopy`` server. The image can be written to a
file, copied to the clipboard, or both.

With no options it captures the first output the compositor advertises
and saves it to the default path.

Options
-------

**-o**, **--output** *NAME*
    Capture the output with this ``wl_output`` name (e.g. ``eDP-1``,
    ``HDMI-A-1``). Defaults to the first advertised output. Ignored when
    **--region** is used (the picker chooses the output).

**-f**, **--file** *PATH*
    Destination PNG path. Defaults to
    ``$XDG_PICTURES_DIR/Screenshot-YYYYMMDD-HHMMSS.png`` (or
    ``~/Pictures`` when ``XDG_PICTURES_DIR`` is unset).

**-r**, **--region**
    Run **shoestring-region**\(1) first to drag-select a rectangle, then
    capture only that region. Mutually exclusive with **--region-rect**.

**--region-rect** *X,Y,W,H*
    Capture an explicit rectangle (logical pixels in the named output's
    coordinate space) without spawning the picker. Requires **--output**.
    Intended for IPC-driven captures where the coordinates are already
    known. Mutually exclusive with **--region**.

**-c**, **--clipboard**
    Copy the captured PNG to the clipboard via **wl-copy**. Combined with
    **--file** the image is both saved and copied; used alone, no file is
    written and nothing is printed to standard output.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print version and exit.

Output
------

When a file is written (the default, or whenever **--file** is given) its
path is printed to standard output. With **--clipboard** alone nothing is
printed.

Environment
-----------

**WAYLAND_DISPLAY**
    The Wayland display to connect to. Required.

**XDG_PICTURES_DIR**, **HOME**
    Used to build the default save path.

**SHOESTRING_REGION_BIN**
    Path to the region picker invoked by **--region**. Defaults to
    ``shoestring-region`` on ``$PATH``.

Exit status
-----------

**0**
    The capture was saved and/or copied successfully.

Non-zero
    A failure — no such output, capture error, Wayland error, or
    **wl-copy** failed. A message is printed to standard error.

See also
--------

**shoestring-region**\(1), **shoestring-ctl**\(1), **shoestring-wm**\(1),
**wl-copy**\(1)
