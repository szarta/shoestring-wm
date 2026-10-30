shoestring-screenshot
=====================

Synopsis
--------

| **shoestring-screenshot** [**-o** *NAME*] [**-f** *PATH*] [**-c**] [**-r** | **--region-rect** *X,Y,W,H*] [**--ocr** [**--ocr-scale** *N*] [**--ocr-psm** *MODE*] [**--ocr-threshold**]]

Description
-----------

Capture a Wayland output, or a rectangle within one, to a PNG via the
compositor's ``wlr-screencopy`` server. The image can be written to a
file, copied to the clipboard, or both.

With no options it captures the first output the compositor advertises
and saves it to the default path.

With **--ocr** the capture is turned into text instead of a file: it is
converted to grayscale, upscaled, and run through **tesseract**\(1), whose
recognized text is printed to standard output. Pair it with **--region**
to lift a coworker's message or a terminal snippet as text, or drive it
over the WM's ``run_command`` IPC with **--region-rect** to read a
widget's text without a screenshot-and-vision round-trip.

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

**--ocr**
    OCR the capture and print the recognized text to standard output
    instead of saving a PNG. Runs **tesseract**\(1) on a grayscale,
    upscaled copy of the capture. **--file** and **--clipboard** still
    save or copy the original image when given; their path is logged to
    standard error so standard output carries the text alone.

**--ocr-scale** *N*
    Integer factor (1-8, default 4) to upscale the capture by before OCR.
    Small on-screen fonts recognize far better enlarged. Only meaningful
    with **--ocr**.

**--ocr-psm** *MODE*
    Tesseract page-segmentation mode (its ``--psm``; 0-13, default 7).
    7 treats the image as a single text line - right for a status bar or
    one-line label; 6 for a block of text, 3 for a full page. Only
    meaningful with **--ocr**.

**--ocr-threshold**
    Binarize the capture with an Otsu auto-threshold before OCR. Off by
    default (tesseract binarizes internally and the grayscale path reads
    most text). Turn it on to normalize polarity - light text on a dark
    field comes out dark-on-light - and to strip anti-aliasing and faint
    divider or grid lines to pure black and white, which cleans up labels
    on crisp UI fonts at the cost of detail on soft or low-contrast text.
    Only meaningful with **--ocr**.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print version and exit.

Output
------

When a file is written (the default, or whenever **--file** is given) its
path is printed to standard output. With **--clipboard** alone nothing is
printed. With **--ocr** the recognized text is printed instead, and any
saved file's path goes to standard error.

Environment
-----------

**WAYLAND_DISPLAY**
    The Wayland display to connect to. Required.

**XDG_PICTURES_DIR**, **HOME**
    Used to build the default save path.

**SHOESTRING_REGION_BIN**
    Path to the region picker invoked by **--region**. Defaults to
    ``shoestring-region`` on ``$PATH``.

**SHOESTRING_TESSERACT_BIN**
    Path to the OCR engine invoked by **--ocr**. Defaults to
    ``tesseract`` on ``$PATH``.

Exit status
-----------

**0**
    The capture was saved and/or copied successfully.

Non-zero
    A failure - no such output, capture error, Wayland error,
    **wl-copy** failed, or (with **--ocr**) **tesseract** was not found
    or exited non-zero. A message is printed to standard error.

See also
--------

**shoestring-region**\(1), **shoestring-ctl**\(1), **shoestring-wm**\(1),
**wl-copy**\(1), **tesseract**\(1)
