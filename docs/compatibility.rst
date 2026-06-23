Application compatibility
=========================

shoestring-wm aims to run ordinary desktop applications without
special-casing. This page collects what is known to work, the handful of
app-specific quirks worth knowing about, and how to work around a
misbehaving client. It is a **living document** — if you hit an
application-specific problem, please report it (see
:ref:`reporting-compat` below) so it can be captured here.

For *what protocols* the compositor implements (the capability list behind
this compatibility), see :doc:`overview`. For tuning an individual app's
placement, see the window-rules section of :doc:`configuration`.

Wayland-native applications
---------------------------

GTK and Qt applications run natively and are the best-supported case:
floating placement, the four window actions, fractional scaling
(``wp_viewporter`` + ``wp_fractional_scale_manager_v1``), the clipboard and
primary selection, drag-and-drop, and input methods all work as expected.

A few toolkits need a nudge to use Wayland instead of falling back to
XWayland:

- **Electron / Chromium-based apps** (VS Code, Discord, Slack, Chromium,
  Chrome, …) often default to XWayland. Launch them with
  ``--ozone-platform-hint=auto`` (or ``--ozone-platform=wayland``) to run
  natively — e.g. ``code --ozone-platform-hint=auto``. Native Wayland gives
  correct scaling and avoids the XWayland caveats below.
- **Firefox** runs on Wayland by default on current releases; on older
  builds set ``MOZ_ENABLE_WAYLAND=1`` in the environment.

Window decorations
~~~~~~~~~~~~~~~~~~~

The compositor advertises ``xdg-decoration`` and forces **server-side**
mode, and by default draws no titlebar (an optional border is configurable
via ``[decorations].border_width``). The practical effect:

- Apps that honor the protocol (most Qt apps, many GTK apps) render
  **undecorated** — no client titlebar, no double decorations. Move and
  resize them by holding ``Super`` and dragging.
- Apps that ignore it and always draw client-side decorations (GNOME/GTK
  apps using ``libdecor``) keep their own titlebar and shadow. That is
  cosmetic, not a malfunction.

This matches the project's "no decoration polish" non-goal — see
:doc:`overview`.

XWayland (X11) applications
---------------------------

X11-only applications (GIMP, Inkscape, ``feh``, older Java/Tk tools, many
games) run through **XWayland**, which the WM spawns on demand. Install your
distribution's ``xwayland`` package first (see :doc:`install`); the ``.deb``
and ``.rpm`` already recommend it. Once the compositor is running,
``$DISPLAY`` is exported, so you can launch X11 apps straight from a
terminal.

What works:

- X11 toplevels map alongside Wayland windows and obey the same floating
  placement and window actions.
- Clipboard **and** primary-selection (middle-click paste) are forwarded
  bidirectionally between X11 and Wayland clients.
- Window rules match X11 apps too: the rule matcher reads ``WM_CLASS`` as
  the ``app_id`` equivalent. Use ``xprop WM_CLASS`` to find the value.

Known XWayland limitations (these are inherent to XWayland, not specific to
shoestring-wm):

- **Fractional / mixed-DPI scaling** is coarse. XWayland apps render at a
  single global scale rather than per-output fractional scale, so they can
  look blurry or mis-sized on a fractional-scaled or mixed-DPI multi-monitor
  setup. Native Wayland is sharp; prefer it where the app offers it.
- A few apps assume a root window they can draw on (some screensavers,
  ``xwallpaper``); the WM does not provide one.

Games, Steam and Proton
-----------------------

.. note::

   End-to-end gaming verification (Steam, Proton, native Linux titles, anti-
   cheat) is **in progress and not yet certified**. This section documents
   the compositor-side pieces that are in place and the known gaps; treat
   specific titles as untested until listed as verified. Reports from a real
   gaming machine are very welcome.

The protocol building blocks games rely on are implemented:

- **Pointer lock + relative motion** (``zwp_pointer_constraints_v1`` /
  ``zwp_relative_pointer_v1``) for FPS mouse-look — the cursor locks while
  over the game surface and the game receives raw relative deltas.
- **Keyboard-shortcuts inhibit** (``zwp_keyboard_shortcuts_inhibit_v1``) so
  a focused full-screen game receives ``Super``-combos instead of the WM
  eating them.
- **Fullscreen** as a first-class window state, bypassing layer-shell
  surfaces (the bar) so the game owns the whole output.
- **XWayland** for Windows games via Proton/Wine and for X11-only native
  titles; **dmabuf** import for GPU clients on the udev/DRM backend.

Known gaps to be aware of when testing:

- **No tearing / immediate presentation yet.** ``tearing_control_v1`` is not
  implemented, so presentation is always vsync'd. Games cannot opt into
  tearing for lower latency.
- Steam's own UI is Chromium-based and runs under XWayland; the
  fractional-scaling caveat above applies to the client and overlay.
- Proton/anti-cheat behavior is untested on this compositor.

Screen sharing and conferencing
--------------------------------

Screen capture for browsers, OBS and conferencing apps goes through the
native desktop portal (``xdg-desktop-portal-shoestring``), which serves the
**ScreenCast** and **Screenshot** interfaces. It must be installed and
routed — see :doc:`portals` for setup, which the packaged ``.deb`` / ``.rpm``
handle automatically.

App-specific notes:

- **Browsers and OBS** take zero-copy ``dmabuf`` frames and work directly.
- **Zoom** cannot import the compositor's ``dmabuf`` buffers (its bundled
  Mesa is too old), so the portal hands it **shm** frames per-consumer
  instead — no global switch needed. This is handled automatically; see
  :doc:`portals`.
- Screen sharing is **opt-in**: the capture capability is off until enabled,
  so the first share in a session may require flipping the screen-capture
  gate. See :doc:`portals` and :doc:`ipc`.

Input methods
-------------

CJK and other composed input via **fcitx5** or **ibus** (with its Wayland
front-end) works out of the box — the three cooperating protocols are always
advertised, and ``Super`` keybinds keep working while an IME is active.
On-screen keyboards (``squeekboard``, ``wvkbd``) are supported the same way.
Full setup, including launching the IME daemon from ``[general].autostart``,
is in :doc:`running`.

Working around a misbehaving application
----------------------------------------

Most app-specific placement problems are solvable with a **window rule**
(see :doc:`configuration`) — for example, pinning a chat client to a
workspace, forcing a tool window to a fixed size and position, or matching
an X11 app by its ``WM_CLASS``. If an app opens on the wrong workspace, at
the wrong size, or steals focus, a rule is usually the fix before anything
in the compositor needs to change.

.. _reporting-compat:

Reporting a compatibility issue
-------------------------------

When an application misbehaves, the most useful report includes:

- the app, its version, and whether it ran **native Wayland or XWayland**
  (X11 apps show up with a ``WM_CLASS``; check ``shoestring-ctl windows``);
- what you expected versus what happened;
- the compositor log for the session — set
  ``SHOESTRING_WM_LOG=/tmp/swm.log`` before launching (see :doc:`running`);
- the output of ``shoestring-ctl -p windows`` while the app is open.

File it on the project's issue tracker; verified quirks and their
workarounds get folded back into this page.
