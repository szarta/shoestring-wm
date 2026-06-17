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
screen sharing silently fails. The config + descriptor files shipped under
``resources/`` close that gap.

Backend selection
-----------------

``resources/shoestring-wm-portals.conf`` maps portal interfaces to
backends for our desktop:

- ``ScreenCast`` → **xdg-desktop-portal-shoestring**, our own backend
  (below);
- ``Screenshot`` → **xdg-desktop-portal-shoestring**, the same backend;
- everything else (file chooser, settings, …) → the **GTK** backend.

Serving both screen-capture interfaces ourselves means the session needs
**no xdg-desktop-portal-wlr** at all.

The shoestring portal backend
------------------------------

``xdg-desktop-portal-shoestring`` is a small standalone D-Bus service
(``org.freedesktop.impl.portal.desktop.shoestring``) that implements
``org.freedesktop.impl.portal.ScreenCast`` **and**
``org.freedesktop.impl.portal.Screenshot``. For ScreenCast it feeds
PipeWire itself and offers each consumer **both dmabuf and shm**, letting
the consumer pick: fast consumers (browsers, OBS) take zero-copy dmabuf,
while picky ones — notably Zoom, whose bundled Mesa can't import our dmabuf
— take shm. The older ``xdg-desktop-portal-wlr`` path could only offer one
buffer type globally (dmabuf on a GPU box), which is why Zoom's share
dropped.

For Screenshot it captures a frame and writes a PNG, returning its
``file://`` URI to the frontend. A non-interactive request grabs the whole
default output; an ``interactive`` request shells out to
``shoestring-screenshot --region`` so the user can rubber-band a rectangle
with the ``shoestring-region`` picker. ``PickColor`` (the eyedropper) is
not yet implemented and reports failure so the app falls back to its own.

Both interfaces capture frames from the compositor over the same
``zwlr_screencopy_v1`` protocol used by ``shoestring-screenshot``, so the
compositor needs no portal-specific code, and a fault in the portal
(PipeWire/D-Bus) can't take down the desktop. They honor the
**screen-capture gate**: the
``zwlr_screencopy`` manager is only advertised while capture is enabled, so
a cast or screenshot fails cleanly until you turn it on
(``shoestring-ctl screen-capture on`` for the session, or
``general.screen_capture_enabled = true`` in the config; see
:doc:`configuration`).

Installation
------------

Packages install everything below; do it by hand only for a
``cargo``-built tree. **The placement matters** — get the descriptor's
directory wrong and the frontend silently ignores the backend.

The routing config goes where the frontend looks for
``<desktop>-portals.conf`` (``$XDG_CONFIG_HOME`` is honored)::

    sudo install -Dm644 resources/shoestring-wm-portals.conf \
        /usr/share/xdg-desktop-portal/shoestring-wm-portals.conf
    # or per-user: ~/.config/xdg-desktop-portal/shoestring-wm-portals.conf

The backend binary, its ``*.portal`` descriptor, and its D-Bus activation
file::

    sudo install -Dm755 target/release/xdg-desktop-portal-shoestring \
        /usr/libexec/xdg-desktop-portal-shoestring
    sudo install -Dm644 resources/shoestring.portal \
        /usr/share/xdg-desktop-portal/portals/shoestring.portal
    sudo install -Dm644 \
        resources/org.freedesktop.impl.portal.desktop.shoestring.service \
        /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.shoestring.service

.. warning::

   The ``*.portal`` descriptor **must** live in a *system* data directory
   (one of ``$XDG_DATA_DIRS``, e.g. ``/usr/share``). xdg-desktop-portal
   1.18 does **not** scan ``$XDG_DATA_HOME`` (``~/.local/share``) for
   ``*.portal`` files — a per-user copy there is never discovered, the
   frontend can't resolve ``ScreenCast=shoestring``, and it exposes *no*
   ScreenCast interface at all (apps see "no such interface" and screen
   sharing silently does nothing). The routing ``*-portals.conf`` *is* read
   from ``~/.config``; only the descriptor is restricted to system dirs.

The backend is started by **D-Bus activation** (the ``.service`` above)
when the frontend first needs ScreenCast, so it inherits the session's
``WAYLAND_DISPLAY`` from the D-Bus activation environment (which
``shoestring-wm-session`` + the compositor's ``systemctl --user
import-environment`` populate). Alternatively, add
``xdg-desktop-portal-shoestring`` to ``general.autostart`` to launch it
with the session — it inherits the live environment directly and the
frontend finds it already owning the name.

Confirm the frontend resolved the backend (look for
``Using shoestring.portal for … ScreenCast`` and ``… Screenshot``)::

    systemctl --user restart xdg-desktop-portal
    /usr/libexec/xdg-desktop-portal -vr   # foreground, prints its choices

Prerequisites
-------------

- ``xdg-desktop-portal`` (the frontend) and a GTK backend
  (``xdg-desktop-portal-gtk``) — almost always already installed.
- ``pipewire`` running in the session — screencast streams are delivered
  over PipeWire, and the backend links ``libpipewire``.
- For interactive (region) screenshots, ``shoestring-screenshot`` and
  ``shoestring-region`` on ``$PATH`` — both ship in this package.
  ``xdg-desktop-portal-wlr`` is **no longer required**; the Screenshot
  interface is served by ``xdg-desktop-portal-shoestring``.

.. note::

   **Screen sharing status.** Routing + the native ScreenCast and
   Screenshot backends are complete: with the screen-capture gate on,
   browsers and Zoom both share the whole screen, and the Screenshot portal
   captures via the same backend — so the session no longer depends on
   xdg-desktop-portal-wlr. ScreenCast offers each consumer **both dmabuf and
   shm** and lets it choose: dmabuf-capable consumers (browsers, OBS) get
   zero-copy GPU buffers the compositor renders straight into, while pickier
   ones (Zoom) take shm — per-consumer, no global toggle. ``PickColor`` (the
   eyedropper) is not yet implemented.
