Default Bindings
================

The bindings below are what ``shoestring-wm --write-default-config``
writes — every key is user-rebindable in the config file
(see :doc:`configuration`).

Window management
-----------------

================================  ============================================
Binding                            Action
================================  ============================================
``Super+E``                        Tile left half
``Super+W``                        Tile right half
``Super+M``                        Maximize (toggle)
``Super+G``                        Arrange workspace in a grid (one-shot)
``Super+Shift+G``                  Arrange workspace in a spiral (one-shot)
``Super+Ctrl+G``                   Arrange workspace as BSP/dwindle (one-shot)
``Super+D``                        Minimize
``Super+Shift+D``                  Restore most-recently-minimized window
``Super+X``                        Close the focused window
``Super+Up``                        Raise focused window to the top
``Super+Shift+Up``                  Lower focused window to the bottom
``Super+S``                         Toggle sticky (show on all workspaces)
``Super+A``                         Toggle always-on-top
``Super+Left-drag``                Move the window under the cursor
``Super+Right-drag``               Resize the window under the cursor
================================  ============================================

The three **arrange** binds are *one-shot*: they tile every window on the
active workspace once (each output independently, in reading order), then leave
the windows floating — there's no persistent tiling mode, so opening or closing
a window later does not re-flow. Press an arrange bind again to re-tile.
Minimized and fullscreen windows are left alone.

Workspaces
----------

=====================================  ========================================
Binding                                Action
=====================================  ========================================
``Super+H``                            Focus previous workspace
``Super+L``                            Focus next workspace
``Super+Ctrl+H``                       Move focused window to previous workspace
``Super+Ctrl+L``                       Move focused window to next workspace
``Super+1`` … ``Super+9``              Focus workspace 1..9
``Super+Shift+1`` … ``Super+Shift+9``  Move focused window to workspace 1..9
=====================================  ========================================

Workspaces 10..16 exist (when ``[workspaces].count`` allows them) but
have no default keybind; bind them with ``focus-workspace`` /
``move-window-to-workspace`` actions as needed.

Scrolling the mouse wheel over the bare desktop — with no window or
panel under the pointer — also switches workspaces: wheel up moves to the
next workspace, wheel down to the previous, clamping at the first and
last. This is a built-in pointer behavior, not a rebindable key.

Machine axis (remote desktop)
-----------------------------

The vertical counterpart to the workspace keys: you are always at
``(machine, workspace)``, and these switch *which machine* you drive. Index 0
is the local machine; each ``shoestring-remote-client`` connected to a remote
box is the next index. While a remote is the active view the WM captures **all**
input and forwards it raw to that machine (its own keymap and keybinds apply) —
only these keys stay local.

=====================================  ========================================
Binding                                Action
=====================================  ========================================
``Super+J``                            View next machine (toward remotes)
``Super+K``                            View previous machine (toward local)
``Super+Escape``                       Break out to the local machine (index 0)
=====================================  ========================================

The axis saturates at 0 (local) and the connected-machine count — no wrap. With
no remote connected these are no-ops (you stay local). See :doc:`ipc` for the
``register_remote_client`` / ``set_view`` / ``view_changed`` surface a client
uses to join the axis.

A remote joins the axis by running **shoestring-remote-client** on the local
machine. It connects out to a remote box's ``shoestring-remote-server`` (reached
over an ``ssh -L`` tunnel or plain loopback), presents that desktop fullscreen,
and registers on the axis so ``Super+J``/``K`` reach it::

    # On the served box: enable serve mode (see the remote-server) so its
    # listener opens, reached locally as 127.0.0.1:7355 via:
    ssh -L 7355:127.0.0.1:7355 user@served-box

    # On the local machine:
    shoestring-remote-client --connect 127.0.0.1:7355 --label served-box

While that client is the active view its surface is shown and all local input is
forwarded to the served box; ``Super+Escape`` returns to local and hides it. The
remote cursor is drawn with the **local** xcursor theme (``$XCURSOR_THEME`` /
``$XCURSOR_SIZE``). ``$SHOESTRING_REMOTE_CLIENT_LOG`` redirects its log to a file.

Launchers and shell
-------------------

================================  ============================================
Binding                            Action
================================  ============================================
``Super+Return``                   Spawn ``alacritty``
``Super+P``                        Spawn ``shoestring-menu`` (commands)
``Super+B``                        Spawn ``shoestring-menu --mode bookmarks``
``Super+Tab``                      Spawn ``shoestring-menu --mode windows`` (jump to window)
``Super+Shift+L``                  Lock session (spawn ``shoestring-lock``)
``Super+Space``                    Cycle keyboard layout (``cycle-layout``)
``Super+Shift+Q``                  Quit shoestring-wm (confirm dialog)
================================  ============================================

The ``quit`` action raises a yes/no modal rendered by
``shoestring-confirm``; pressing Enter exits cleanly, Escape stays
running. The ``power-off``, ``reboot``, and ``suspend`` actions use the
same modal and then shell out to ``systemctl`` / ``loginctl`` /
``shutdown(8)`` (the bar's control menu offers the same three). They are
unbound by default; to route the hardware power / sleep keys through them
you must first stop ``logind`` from handling those keys — see
:doc:`install`.

Media and brightness
--------------------

The default config also binds the standard XF86 keys to action scripts
under ``scripts/actions/`` in the repository. The bindings spawn the
script by name (no path), so installing the scripts on ``$PATH`` lights
them up; if the scripts are missing the spawn fails silently with a
warning and the bind still resolves.

================================  ============================================
Binding                            Spawns
================================  ============================================
``XF86AudioRaiseVolume``           ``shoestring-volume-up``
``XF86AudioLowerVolume``           ``shoestring-volume-down``
``XF86AudioMute``                  ``shoestring-volume-mute``
``XF86AudioMicMute``               ``shoestring-mic-mute``
``XF86MonBrightnessUp``            ``shoestring-brightness-up``
``XF86MonBrightnessDown``          ``shoestring-brightness-down``
================================  ============================================

If ``shoestring-menu`` is not on ``$PATH``, the ``Super+P`` / ``Super+B`` /
``Super+Tab`` spawn silently fails (a warning is logged). The bind is still
defined so installing the menu later "just works".

Virtual terminals
-----------------

``Ctrl+Alt+F1`` … ``Ctrl+Alt+F12`` switch the active Linux VT. Only
takes effect on the TTY backend; ignored (with a warning) under winit.
