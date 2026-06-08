shoestring-wm
=============

Synopsis
--------

| **shoestring-wm** [**-c** *FILE*] [**-b** winit|tty] [**-C** *CMD*] [**--enable-automation**]
| **shoestring-wm** **--write-default-config** [**--force**] [**-c** *FILE*]
| **shoestring-wm** **-V** | **--version**
| **shoestring-wm** **-h** | **--help**

Description
-----------

**shoestring-wm** is a floating Wayland window manager. It runs either
as a nested winit window inside an existing X11/Wayland session (for
development) or natively from a Linux virtual terminal using DRM/KMS,
libinput, and libseat.

The compositor reads its configuration from a TOML file (see
**shoestring-wm**\(5)) and exports a unix-socket JSON IPC for status
bars and launcher utilities. The default keymap mirrors an Openbox
flow: Super+E/W for half-tiling, Super+M to maximize, Super+D to
minimize, Super+1..9 to switch workspaces, Super+drag for move/resize.
Scrolling the mouse wheel over the bare desktop switches workspaces
(wheel up for the next, wheel down for the previous).

Options
-------

**-c**, **--config** *FILE*
    Path to a TOML config file. Defaults to
    ``$XDG_CONFIG_HOME/shoestring-wm/config.toml``.

**-b**, **--backend** winit|tty
    Force a backend. Auto-detected from the environment when omitted:
    *tty* when neither ``$WAYLAND_DISPLAY`` nor ``$DISPLAY`` is set,
    *winit* otherwise.

**-C**, **--command** *CMD*
    Spawn *CMD* once the compositor is ready. Defaults to
    ``weston-terminal``. Split on whitespace; no shell quoting.

**--write-default-config**
    Write the bundled default config to the target path (resolved as
    for **-c**) and exit. Refuses to overwrite an existing file unless
    **--force** is also given.

**--force**
    Allow **--write-default-config** to overwrite an existing file.

**--enable-automation**
    Force the runtime automation gate ON at startup, overriding
    ``[general].automation_enabled`` in the config. The gate is off
    by default; remote-automation IPC methods (``inject_key`` /
    ``inject_text`` / ``inject_click`` / ``move_mouse`` /
    ``screenshot`` / ``run_command`` / ``dispatch_action``) refuse to fire while it is
    off. The runtime IPC ``set_automation`` request can still flip the
    gate; the config file remains the source of truth at next start.

**-V**, **--version**
    Print version information.

**-h**, **--help**
    Print short usage.

Environment
-----------

``XDG_CONFIG_HOME``, ``HOME``
    Resolve the default config path.

``RUST_LOG``
    ``tracing_subscriber`` filter; default ``info``.

``SHOESTRING_WM_LOG``
    If set, tracing output is appended to this file instead of stderr.
    Useful when running on a TTY.

``WAYLAND_DISPLAY``
    Set by **shoestring-wm** for child processes before any client is
    spawned.

``SHOESTRING_WM_SOCKET``
    Set by **shoestring-wm** to the path of its IPC socket so
    **shoestring-ctl**\(1) and **shoestring-bar**\(1) can find it.

Files
-----

``$XDG_CONFIG_HOME/shoestring-wm/config.toml``
    User configuration file. See **shoestring-wm**\(5).

``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``
    IPC socket. The exact path is also exported as
    ``$SHOESTRING_WM_SOCKET``.

Exit status
-----------

**0**
    Normal exit (via the ``quit`` action, ``--write-default-config``, or
    a clean shutdown).

Non-zero
    Startup error (missing config file when explicitly named, backend
    unavailable, etc.). See stderr or ``$SHOESTRING_WM_LOG``.

Helpers
-------

The WM ships and (in some cases) spawns these companion binaries:

**shoestring-ctl**\(1)
    Reference CLI client for the IPC socket.

**shoestring-bar**\(1)
    Status bar; spawned by default via ``[general].autostart``.

**shoestring-menu**\(1)
    dmenu-style launcher; bound to ``Super+P`` / ``Super+B`` by default.

**shoestring-lock**
    Session locker; spawned by the ``lock`` action and the ``lock``
    IPC request. Configurable via ``[general].lock_command``.

**shoestring-screenshot**
    PNG capture via wlr-screencopy; invoked by the ``screenshot`` IPC
    request. Region selection delegates to **shoestring-region**.

**shoestring-region**
    Slurp-equivalent rectangle picker; reads coordinates back over its
    stdout.

**shoestring-kill**
    xkill-equivalent. Sends ``pick_window`` then ``close_window`` on
    success.

**shoestring-confirm**
    Modal yes/no dialog. Used by the ``quit`` action; available for
    custom destructive flows.

See also
--------

**shoestring-wm**\(5), **shoestring-ctl**\(1), **shoestring-bar**\(1),
**shoestring-menu**\(1)
