Hardware compatibility
======================

The automated tests run shoestring-wm headless, on a software renderer, with
no display, seat, or input devices. They cover the protocols, rendering, and
IPC well, and real hardware not at all: mode setting on a GPU driver, libinput
devices, the seat, suspend and resume, and monitors coming and going. This page is
the manual test matrix for those paths and the record of what has been run on
real machines. For how individual applications behave, see
:doc:`compatibility`.

Record hardware by class and driver: the kind of machine, the GPU vendor and
kernel driver, the kernel, Mesa, and distribution versions, and the
shoestring-wm version. Leave out host names, serial numbers, network details,
and anything else that identifies a machine or where it sits.

The checklist
-------------

Run these in a TTY session (the ``tty`` backend), the way the WM runs as a
daily desktop; the nested ``winit`` backend uses the host session's display
and input, so it tests none of this. Before you start, note the versions
(``shoestring-ctl server-info`` gives the WM's) and the GPU driver
(``lspci -k`` on Linux, ``pciconf -lv`` on FreeBSD). ``shoestring-ctl
outputs`` and ``shoestring-ctl inputs`` show what the WM sees at each step.

.. list-table::
   :header-rows: 1
   :widths: 18 82

   * - Check
     - Steps, and what passing looks like
   * - Startup
     - Log in on a TTY and start the WM. Every connected output lights at
       its preferred mode and scale, ``outputs`` lists each one, and
       ``inputs`` lists the keyboard, pointer, and touchpad. A window receives
       typing, clicks, scrolling, and touchpad gestures.
   * - Suspend and resume
     - With windows open, suspend (lid, the ``suspend`` action, or
       ``systemctl suspend``), wait a minute, resume. Every output comes back
       at its mode, the windows are where they were, and input works without
       replugging anything. Lock the session (``shoestring-ctl lock``) before
       a second cycle: it resumes still locked, the prompt takes the
       password, and the desktop returns. A second cycle can fail where the
       first did not.
   * - Monitor hotplug
     - With windows open, connect an external monitor, then disconnect it,
       then connect it again. The new output lights at its preferred mode
       and joins the layout (or takes its ``[output.<name>].position``), and
       ``outputs`` follows each change. On unplug, the windows that were on
       it move to a remaining output, where you can focus and use them. Repeat with the lid
       closed if the machine is a laptop.
   * - Mixed output scales
     - Outputs at different scales, such as an internal panel at 1.5 or 2
       and an external monitor at 1 (``[output.<name>].scale``). A native
       Wayland window is sharp on each output and keeps its size when moved
       between them; the cursor has the right size on both. XWayland windows
       are expected to look soft on the fractional output (see
       `Known limitations`_).
   * - GPU driver
     - On each GPU vendor and driver available: the checks above, plus a
       window playing video or a game for a few minutes without tearing,
       stalls or corruption, a ``shoestring-ctl screenshot`` that matches
       the screen, and screen sharing through the portal (see
       :doc:`portals`). On a machine with two GPUs, note which one drove the
       outputs.
   * - Input method
     - Start ``fcitx5`` or ``ibus`` from ``[general].autostart``. Composing
       text in a GTK and a Qt application shows the candidate popup next to
       the text cursor and commits the chosen text, and a ``Super`` binding
       still works while the IME is active (see :doc:`running`).
   * - XWayland
     - Start an X11 application (``xterm``, GIMP, a Wine program). It maps
       next to Wayland windows, takes focus and input, copies, and pastes
       both ways with a Wayland application, and closes cleanly. A game
       that grabs the pointer keeps it while focused.

Tested configurations
---------------------

Each row is one machine class, with the result of each check. *Not yet
recorded* means nobody has run that check on that configuration; it does not
mean the check fails.

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - Configuration
     - Results
   * - **Laptop, Intel UHD integrated graphics (Comet Lake, i915)**, one
       internal 2560×1440 panel at scale 1. Ubuntu 24.04, kernel 7.0,
       Mesa 25.2. shoestring-wm 0.9.0, ``tty`` backend.
     - **Startup:** passes; this is the machine's everyday session.
       **XWayland:** the server starts and the session exports ``DISPLAY``;
       application checks not yet recorded.
       **Suspend and resume, monitor hotplug, mixed output scales, input
       method:** not yet recorded.
   * - **FreeBSD 15.1.** shoestring-wm from ``main`` after 0.9.1.
     - The nested ``winit`` backend runs (see :doc:`install`), and the
       ``headless`` backend passes the end-to-end tests on its software
       renderer, which exercises no hardware. **Every check on this page,
       with the ``tty`` backend and drm-kmod:** not yet recorded.

To add a result, run the checks that apply, then add or update a row here in a
pull request, or open an issue with the same details (see
:ref:`reporting-compat`). Include the versions listed above, and for a
failure, the WM log (``SHOESTRING_WM_LOG``) and what you did just before it.

Known limitations
-----------------

- **Proprietary NVIDIA drivers** are untested and not a target for this
  release; the open-source ``nouveau`` driver is the supported path on
  NVIDIA hardware (see :doc:`install`).
- **XWayland and fractional scaling.** X11 applications render at one
  global scale, so they look blurry or mis-sized on a fractional-scale or
  mixed-DPI setup (see :doc:`compatibility`).
- **Power and sleep keys** go to the WM only if ``logind`` is told to ignore
  them; by default ``logind`` powers off or suspends at once (see
  :doc:`install`).
- **No lock before sleep.** Suspending the machine leaves the session as it
  was, unlocked if it was unlocked. ``[general].idle_lock_secs`` locks after
  inactivity, and a system sleep hook can run ``shoestring-ctl lock``.
- **Gamma control** (``set_gamma``, ``zwlr_gamma_control_v1``) works only on
  the ``tty`` backend, where the WM sets the outputs' gamma ramps itself.
- **Nested sessions** (``winit``) list none of the input devices and use the
  host session's outputs, so a nested run does not count for any check on
  this page.
