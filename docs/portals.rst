XDG Desktop Portals
===================

Desktop portals (``xdg-desktop-portal``, "xdp") are the D-Bus services
that let sandboxed and toolkit apps open file dialogs, read settings,
and — the headline feature — share your screen in a video call. The
portal *frontend* is desktop-agnostic; it delegates each portal
interface to an *implementation backend* chosen by ``$XDG_CURRENT_DESKTOP``.

shoestring-wm sets ``XDG_CURRENT_DESKTOP=shoestring-wm`` (in
``shoestring-wm-session``), which appears in no backend's built-in
``UseIn=`` list. Without a configuration file telling the frontend which
backend to use, screen-capture portals have *no* implementation and
screen sharing silently fails. The two config files shipped under
``resources/`` close that gap.

Backend selection
------------------

``resources/shoestring-wm-portals.conf`` maps portal interfaces to
backends for our desktop:

- screen capture (``ScreenCast``, ``Screenshot``) → **xdg-desktop-portal-wlr**
  (xdpw), the wlroots backend, which speaks the same ``zwlr_screencopy``
  and foreign-toplevel protocols this compositor implements;
- everything else (file chooser, settings, …) → the **GTK** backend.

Install it where the frontend looks for ``<desktop>-portals.conf``::

    # system-wide
    sudo install -Dm644 resources/shoestring-wm-portals.conf \
        /usr/share/xdg-desktop-portal/shoestring-wm-portals.conf

    # or per-user
    install -Dm644 resources/shoestring-wm-portals.conf \
        ~/.config/xdg-desktop-portal/shoestring-wm-portals.conf

Confirm the frontend picked it up (look for ``Using portal configuration
file '…/shoestring-wm-portals.conf'`` and ``Using wlr.portal for …
ScreenCast``)::

    systemctl --user restart xdg-desktop-portal
    /usr/libexec/xdg-desktop-portal -vr   # foreground, prints its choices

xdpw output selection
---------------------

``resources/xdg-desktop-portal-wlr.conf`` is an optional example for
xdpw itself — it controls which output gets shared and the capture frame
rate. With no config xdpw prompts for an output via its built-in chooser;
on a single-monitor or scripted setup you can pin one output instead. See
the comments in that file and ``xdg-desktop-portal-wlr(5)`` for the
placement rules and all keys.

Prerequisites
-------------

- ``xdg-desktop-portal`` and a GTK backend
  (``xdg-desktop-portal-gtk``) — almost always already installed.
- ``xdg-desktop-portal-wlr`` for the screen-capture interfaces.
- ``pipewire`` + ``wireplumber`` running in the session — screencast
  streams are delivered over PipeWire.

The session must also export ``WAYLAND_DISPLAY`` and
``XDG_CURRENT_DESKTOP`` into the D-Bus activation environment so the
dbus-activated portal services inherit them; ``shoestring-wm-session``
plus the compositor's ``systemctl --user import-environment`` already do
this on a systemd session.

.. note::

   **Screen sharing status.** Portal *routing* (this page) is complete,
   and the GTK-backed portals (file chooser, settings, …) work today.
   Full ScreenCast is still gated on a compositor capability: our
   ``zwlr_screencopy`` implementation currently advertises **shm** buffers
   only, while ``xdg-desktop-portal-wlr`` requires a **dmabuf** capture
   format (a hard requirement in the 0.7.x series Ubuntu 24.04 ships).
   Exporting dmabuf buffers from the capture path — or implementing the
   newer ``ext-image-copy-capture-v1`` protocol that xdpw 0.8+ prefers —
   is tracked as follow-up work. Until then, screen sharing through the
   wlroots portal will not produce frames even with the config above in
   place.
