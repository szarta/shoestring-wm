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
``Super+D``                        Minimize
``Super+Shift+D``                  Restore most-recently-minimized window
``Super+X``                        Close the focused window
``Super+Left-drag``                Move the window under the cursor
``Super+Right-drag``               Resize the window under the cursor
================================  ============================================

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

Workspaces 10..16 exist but have no default keybind; bind them with
``focus-workspace`` / ``move-window-to-workspace`` actions as needed.

Launchers and shell
-------------------

================================  ============================================
Binding                            Action
================================  ============================================
``Super+Return``                   Spawn ``alacritty``
``Super+P``                        Spawn ``shoestring-menu`` (commands)
``Super+B``                        Spawn ``shoestring-menu --mode bookmarks``
``Super+Shift+Q``                  Quit shoestring-wm
================================  ============================================

If ``shoestring-menu`` is not on ``$PATH``, the ``Super+P`` / ``Super+B``
spawn silently fails (a warning is logged). The bind is still defined so
installing the menu later "just works".

Virtual terminals
-----------------

``Ctrl+Alt+F1`` … ``Ctrl+Alt+F12`` switch the active Linux VT. Only
takes effect on the TTY backend; ignored (with a warning) under winit.
