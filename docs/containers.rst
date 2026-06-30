Containers and compositor isolation
===================================

The :doc:`headless backend <running>` plus :doc:`remote-desktop serve mode
<bindings>` add up to something more than the sum of their parts: a complete,
self-contained desktop session — its own compositor, its own apps, its own
clipboard and seat — that has **no** dependency on the host's display, login
session, or input devices, and that is fully **observable and driveable over a
socket**. That is exactly the shape of a thing you put in a container.

This page explains why the headless backend is container-friendly, the
serve-mode topology that lets you view and drive a containerised session, and
the "desktop per container" pattern it enables. A ready-to-run ``Dockerfile``
ships in `packaging/docker/
<https://github.com/barrendo/shoestring-wm/tree/main/packaging/docker>`_; see
its ``README.md`` for the full build/run reference.

.. note::

   **Status.** Shipped: the headless serve-mode stack is verified end to end (a
   served headless session driven from a separate machine over ``ssh -L``), the
   ``Dockerfile`` in ``packaging/docker/`` builds and runs, and rendering is
   **GPU-optional** — with a render node it uses the GPU, without one it falls
   back to Mesa ``llvmpipe`` (so a plain ``docker run`` with no ``--device``
   works). See *GPU and CPU-only rendering* below.

Why headless fits a container
-----------------------------

A normal compositor is hostile to containers: it wants DRM *master* (KMS
modesetting), a ``logind`` / ``libseat`` session, and ``/dev/input`` devices.
The headless backend needs none of these. It opens a single unprivileged DRM
**render node** (``/dev/dri/renderD128``), builds a surfaceless GL renderer on
it, and takes all of its input over IPC. Concretely:

- **No seat, no VT, no display manager.** A render node is unprivileged — no
  DRM master — so the container needs no ``--privileged``, no ``CAP_SYS_ADMIN``,
  and no host session.
- **No input devices.** Input arrives via ``inject_input`` / the remote
  client's captured input (see :doc:`ipc`), so there is no ``/dev/input`` to map
  in and no seat to acquire.
- **No host display.** Nothing scans the frame out; it is read through the
  capture path and streamed by the serve-mode server.
- **Small image.** The compositor is a low-dependency Rust binary; the runtime
  needs Mesa (for the GL renderer) and, optionally, ``xwayland`` for X11 apps.

The one host dependency is the render node itself — see *GPU and CPU-only
rendering* below.

The serve-mode topology
-----------------------

The container is the **served** side; you view and drive it from a real desktop
(the **viewer**). This is the same topology the machine axis uses (see
:doc:`bindings`), only the served box is a container instead of another machine:

.. code-block:: text

   ┌─ container ───────────────────────────────┐
   │  shoestring-wm --backend headless         │
   │     ├─ apps (Wayland / XWayland clients)   │
   │     └─ IPC socket  (automation + observe)  │
   │  shoestring-remote-server  ─ TCP :7355 ────┼──► ssh -L / published port
   └────────────────────────────────────────────┘
                                                     │
   ┌─ your desktop ──────────────────────────────────▼─┐
   │  shoestring-remote-client --connect host:7355      │
   │     → appears on the machine axis (Super+J/K)       │
   └─────────────────────────────────────────────────────┘

Inside the container the entrypoint (``packaging/docker/entrypoint.sh``) starts
the compositor, waits for its IPC socket, lets the autostarted
``shoestring-remote-server`` register, and opens the remote gate with
``shoestring-ctl remote on`` (the server's listener stays closed until the gate
is on — the same explicit, opt-in sharing gate described in :doc:`bindings`).
From your desktop, ``shoestring-remote-client`` connects to the published port
(directly or over an ``ssh -L`` tunnel), and the session joins your machine
axis: ``Super+J`` to view and drive it, ``Super+Escape`` to break back out.
Clipboard moves between you and the container with ``Super+Shift+C`` /
``Super+Shift+V`` exactly as between two machines.

Build the image from the repository root, then run it (no GPU required)::

    podman build -t shoestring-wm-headless -f packaging/docker/Dockerfile .

    podman run --rm -p 127.0.0.1:7355:7355 shoestring-wm-headless

    # then, on your desktop:
    shoestring-remote-client --connect 127.0.0.1:7355 --label sandbox

Publishing to host loopback (``-p 127.0.0.1:7355:7355``) keeps the port off the
network — reach a remote host's container with
``ssh -L 7355:127.0.0.1:7355 host``. To use the host GPU, add
``--device /dev/dri/renderD128 --group-add keep-groups``.

GPU and CPU-only rendering
--------------------------

A GPU is **optional**. When a render node is passed through
(``--device /dev/dri/renderD128``) the headless renderer builds on it via GBM,
which is faster and additionally enables GPU dmabuf import/export for accelerated
clients. Point ``$SHOESTRING_WM_RENDER_NODE`` at a different node if the default
is not the right one.

With **no GPU at all** — a CI runner or a CPU-only cloud host, or simply a
``docker run`` without ``--device`` — the backend falls back automatically to a
surfaceless EGL platform on Mesa's ``llvmpipe`` software rasteriser (no GBM, no
DRM node). It logs a warning and renders real frames on the CPU; everything the
capture and serve paths need works identically, only slower. Force this path on
a GPU box with ``-e SHOESTRING_WM_HEADLESS_SOFTWARE=1`` (add
``-e LIBGL_ALWAYS_SOFTWARE=1`` to guarantee llvmpipe). The container image must
carry the Mesa software stack (``libgl1-mesa-dri`` / ``mesa-dri-drivers`` —
which ships ``swrast``/``llvmpipe``) for the fallback to work.

A GPU-less run is therefore just::

    podman run --rm -p 127.0.0.1:7355:7355 shoestring-wm-headless

The image carries the Mesa software stack, so this works out of the box.

What it buys you
----------------

Because the compositor *is* the automation and observation surface (see
:doc:`ipc`), a containerised session is not just a remote desktop — it is a
**scriptable, inspectable sandbox**:

- **Disposable, reproducible desktops.** Spin one up, drive an app, tear it
  down. The whole UI state is in the container.
- **Parallel agent sandboxes.** Run many at once, each with its own clipboard,
  seat, and ``event-stream``, each driven and inspected independently over IPC —
  a natural fit for the project's automation goal of letting an agent drive both
  the WM and the apps inside it.
- **Structured observation, not screen-scraping.** ``windows``, ``outputs``,
  ``metrics``, and the event stream describe the session directly, instead of
  inferring state from pixels.

For comparison, the classic "desktop in a container" rigs bolt VNC and
``xdotool`` onto ``Xvfb`` + a minimal WM (or ``sway --headless`` + ``wayvnc``).
The difference here is that automation and observation are first-class in the
compositor, and the serve protocol is a native, damage-tracked tile stream
rather than generic VNC.

Isolation boundaries
--------------------

The isolation unit is the **container**: each is a separate session with its own
compositor and apps. Within a single container the apps still share one seat and
one clipboard (now gated, but shared) — this is session isolation, not a
security sandbox *between* the apps in it.

For true per-app isolation, run **one app per container** — the compositor
hosting a single client. That is often the more useful pattern anyway: each app
gets its own disposable, observable, individually-viewable desktop. Pass the
app's argv in ``$SHOESTRING_APP`` (or add it to the image's ``config.toml``
``autostart``)::

    podman run --rm -p 127.0.0.1:7355:7355 \
        -e SHOESTRING_APP='alacritty' shoestring-wm-headless
