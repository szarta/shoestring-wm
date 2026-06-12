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

The password prompt is hidden while the screensaver runs and appears as
soon as you begin typing. It hides again — discarding anything typed so
far — when you press **Escape**, or automatically after roughly eight
seconds without a keystroke.

Options
-------

**--pam-service** *NAME*
    PAM service stack to authenticate against. Defaults to
    ``shoestring-lock`` — a dedicated policy shipped with the locker (see
    *Authentication*). Override only to substitute a custom stack.

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

Authentication uses the dedicated ``shoestring-lock`` PAM service rather
than the system ``login`` stack. This is deliberate: on FreeBSD/OpenPAM
the ``login`` service begins with ``auth sufficient pam_self.so``, which
succeeds for the *calling* user with no password — a locker on that
service would unlock on any input. The shipped policy instead performs a
real password check while keeping the binary unprivileged: on Linux via
``pam_unix`` (which calls the setuid ``unix_chkpwd`` from ``libpam-modules``
/ ``pam``); on FreeBSD via ``unix-selfauth`` (the setuid
``unix-selfauth-helper`` from the *security/unix-selfauth-helper* port,
the same mechanism ``swaylock`` uses).

Packaging installs the policy at ``/etc/pam.d/shoestring-lock``
(FreeBSD: ``/usr/local/etc/pam.d/shoestring-lock``). A **source install
must copy the matching file from** ``resources/pam/``; without it PAM
resolves the ``other`` fallback (deny) and every unlock attempt fails
closed.

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
