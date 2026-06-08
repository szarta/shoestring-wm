shoestring-lock
===============

Synopsis
--------

| **shoestring-lock** [**--pam-service** *NAME*] [**--font** *PATH*] [**--font-size** *PX*]

Description
-----------

The screen locker for **shoestring-wm**\(1). It uses the
``ext-session-lock-v1`` protocol so the compositor blanks every output
and routes all input to the lock surface until authentication succeeds.
An animated maze runs as a screensaver behind the password prompt.

It is normally launched by the WM's **lock** action (keybind or
``shoestring-ctl lock``) rather than invoked directly. The locked session
is released only when PAM authenticates the current user; the lock cannot
be dismissed by other means (VT switching aside, which the compositor
governs).

Options
-------

**--pam-service** *NAME*
    PAM service stack to authenticate against. When omitted, the first
    of ``system-auth``, ``login``, ``passwd`` that exists under
    ``/etc/pam.d`` is used, falling back to ``login``.

**--font** *PATH*
    TrueType/OpenType font for the prompt text. Falls back to
    ``$SHOESTRING_LOCK_FONT`` and then a list of common system font
    paths.

**--font-size** *PX*
    Prompt text size in pixels. Default ``28``.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print version and exit.

Authentication
--------------

The user to authenticate is taken from ``$USER``. A minimal PAM
conversation answers password (echo-off) prompts with the typed input;
echo-on prompts (e.g. interactive 2FA) are not handled. The session
unlocks only when both ``authenticate`` and ``acct_mgmt`` succeed.

Environment
-----------

**USER**
    The account to authenticate. Required.

**WAYLAND_DISPLAY**
    The Wayland display to connect to. Required.

**SHOESTRING_LOCK_FONT**
    Font path, consulted after ``--font`` but before the built-in
    candidates.

**SHOESTRING_LOCK_LOG**
    Tracing filter for diagnostic logging (e.g. ``debug``). Default
    ``info``.

Exit status
-----------

**0**
    Locked, authenticated, and unlocked cleanly.

**1**
    Initialisation or PAM failure (no Wayland connection, missing
    protocol, font load error, ...). A message is printed to standard
    error.

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1)
