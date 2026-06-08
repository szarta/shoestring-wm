shoestring-confirm
==================

Synopsis
--------

| **shoestring-confirm** *PROMPT*

Description
-----------

A modal yes/no confirmation dialog for **shoestring-wm**\(1). Displays
*PROMPT* centred over every output and grabs the keyboard until the user
answers. It is the reusable building block behind the WM's
``confirm_action`` gate (used, for example, to confirm **quit**), but it
is a standalone program that any script can call.

The dialog is dismissed by:

- **Enter** / keypad **Enter** — confirm (exit ``0``);
- **Escape**, any mouse click, or the compositor closing the surface —
  cancel (exit ``1``).

Arguments
---------

*PROMPT*
    The prompt text to display. Required; exactly one argument.

Environment
-----------

**SHOESTRING_CONFIRM_FONT**
    Path to a TrueType/OpenType font for the prompt text. When unset a
    list of common system font paths (DejaVu Sans, Liberation Sans, Noto
    Sans) is searched.

**WAYLAND_DISPLAY**
    The Wayland display to connect to.

Exit status
-----------

The exit code is the answer, and is also echoed as a single decimal
digit to standard output so callers that cannot ``waitpid`` can still
read it.

**0**
    Confirmed (Enter).

**1**
    Cancelled (Escape, click, or surface closed).

**2**
    Error — missing font, no Wayland connection, or a protocol/I/O
    failure. A message is also printed to standard error.

Examples
--------

::

    shoestring-confirm "Log out of this session?" && shoestring-logout

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1)
