Testing applications
====================

The IPC requests that drive and inspect a session compose into an end-to-end
test for a real application: start a private compositor, launch the app, wait
for its window, drive it with input aimed at that window, check what it did,
keep evidence when a check fails, and tear everything down. This page walks
through that workflow; `A complete example`_ at the end runs it against a GTK
dialog and is the place to start from.

The split of responsibility is simple. The WM makes **presentation and input**
repeatable: a fixed output size, a known background, deterministic window
placement, synchronous waits, and input that lands where it is aimed. The
**application** owns its own determinism: random seeds, clocks, saved state,
network, animations. A game test still seeds its game; the WM guarantees that
the board it draws lands on the same pixels every run.

Every request below is in :doc:`ipc`; the examples use ``shoestring-ctl``,
which maps one subcommand to each request and prints the JSON reply.

=================================  ==========================================
Step                               What to use
=================================  ==========================================
Isolated compositor                ``shoestring-wm --backend headless`` with
                                   its own ``$XDG_RUNTIME_DIR``
Readiness                          the IPC socket appearing, then
                                   ``wait-ready`` (``--xwayland`` for X apps)
Launch                             a child of the test with the WM's
                                   ``$WAYLAND_DISPLAY``; or ``run-command``
Wait for a window                  ``wait-window`` (and ``--unmap``)
Targeted input                     ``click --window``, ``drag --window``,
                                   ``key``, ``type``, ``paste``
Check results                      the app's own output, ``get-pixel``,
                                   ``hash-region``, ``screenshot --window``,
                                   ``tree``
Evidence on failure                ``screenshot``, ``tree``, ``server-info``,
                                   the WM and app logs
Teardown                           ``shutdown``, then the app and the
                                   directory
=================================  ==========================================

An isolated compositor
----------------------

Run the test on its own headless WM, never on the desktop session you are
sitting at: the test needs the automation gate on, takes over keyboard focus,
and must not see your windows.

- **A private runtime directory.** Point ``$XDG_RUNTIME_DIR`` at a fresh
  ``mktemp -d`` directory with mode 0700. The WM's Wayland socket, its IPC
  socket, and the app's own runtime files all land there, so parallel tests
  can't collide and cleanup is one ``rm -rf``. Unix socket paths are capped
  near 108 bytes, so keep the directory short (``/tmp/apptest.XXXXXX``).
- **None of your session's variables.** Unset ``WAYLAND_DISPLAY``,
  ``DISPLAY`` and ``SHOESTRING_WM_SOCKET`` for the WM, or the headless WM may
  be mistaken for the session's and ``shoestring-ctl`` may talk to the wrong
  one.
- **A config of its own.** Pass ``--config`` with a file that sets only what
  the test depends on. ``[general] autostart = []`` keeps the bar and the
  notification daemon out of the picture, and a ``[background] color`` gives
  pixel probes a known value. Everything else keeps its default.
- **Flags for repeatability.** ``--output-size 1280x800`` fixes the
  coordinate space (the default is 1920x1080). ``--renderer pixman`` renders
  on the CPU, so the result doesn't depend on whether the machine has a GPU;
  the renderers blend slightly differently, so keep reference images per
  renderer (see :doc:`running`). ``--enable-automation`` turns the
  automation gate on, which input, pixel probes, screenshots and
  ``run-command`` need. Set ``$SHOESTRING_WM_LOG`` to keep the WM's log as a
  file.

``-C`` is not needed: without it the WM starts nothing.

Readiness
---------

The IPC socket is named after the Wayland socket,
``$XDG_RUNTIME_DIR/shoestring-wm-<display>.sock``, and appears after it. Poll
for it (checking the WM hasn't exited meanwhile), export
``SHOESTRING_WM_SOCKET`` to that path, and take the Wayland display name from
it. Then ``shoestring-ctl wait-ready`` returns once the event loop answers.
For an X11 application use ``wait-ready --xwayland``, which waits until
XWayland is up and ``$DISPLAY`` is known, so the app can't start before it.

This poll for the socket is the only place a test needs to sleep; everything
after it can wait on the WM.

Launching the application
-------------------------

Start the app as a child of the test, with ``XDG_RUNTIME_DIR`` and
``WAYLAND_DISPLAY`` set to the WM's (and for GTK, ``GDK_BACKEND=wayland``).
The test then owns the process: it has the pid to wait for or kill, and the
app's stdout and stderr go to files the test reads.

The alternative is ``shoestring-ctl run-command``, which runs a command with
the WM's own environment. Without ``--detach`` it waits for the command and
returns its exit code and output, which suits short helpers (seeding a save
file, querying the app's CLI). With ``--detach`` it returns the pid at once,
but the child runs in a session of its own with its output discarded, and
``shutdown`` doesn't stop it; kill it by pid when you tear down.

Waiting for the window
----------------------

``shoestring-ctl wait-window --app-id '^zenity$' --title '^Rename$'`` blocks
until a matching window is ready, then prints it, id and geometry included.
Ready means the window has its first frame and its place on the output, so
its reported geometry is where it will be drawn and a click inside it lands.
Both filters are unanchored regexes; anchor them to avoid matching a
lookalike. The default timeout is ten seconds, and a timeout exits with
status 2 rather than 1 so a script can tell it from an error.

``wait-window --unmap`` waits for every matching window to close, which is how
to wait for a dialog to be dismissed. For other state changes (a title that
shows progress, focus moving), read ``shoestring-ctl event-stream`` and wait
for the event: ``window_title_changed``, ``window_focused`` and the rest are
in :doc:`ipc`. None of these need a fixed sleep.

Driving input
-------------

Aim input at the window rather than at screen coordinates:

- ``click --window <id> --wx X --wy Y`` clicks at a point relative to the
  window's top-left corner, wherever the window was placed. It focuses the
  window as a real click does. ``drag --window <id>`` drags the same way.
- ``key``, ``type`` and ``paste`` go to the focused surface. ``type``
  handles ASCII letters, digits and space; ``type --via-clipboard`` (or
  ``paste``) enters anything else through the clipboard. ``focus-window
  <id>`` focuses a window without clicking.
- Injected keys skip the WM's own bindings, so ``key super+q`` reaches the
  app instead of closing it. To run a WM binding, use ``dispatch-action``.

Every input request returns once the WM has delivered the input; what the
app does with it happens on the app's time, so check for the effect (next
section) rather than assuming it.

Park the pointer before probing pixels: ``move-mouse`` to the bottom-right
corner of the output, where the arrow points off the screen. Left anywhere
else, the cursor is part of the picture, and a probe or hash under it reads
the cursor.

Checking results
----------------

Prefer the strongest check the app allows, and fall back to pixels:

- **The app's own output.** A dialog that prints its answer, a file the app
  saves, an exit code. These don't depend on rendering at all.
- **The window tree.** ``tree`` and ``windows`` report titles, geometry,
  focus, workspace and stacking: did the dialog open, close, get focus?
- **Pixel probes.** ``get-pixel X Y`` reads one pixel in output-logical
  coordinates: is the indicator lit, is the checkbox ticked?
- **Region hashes.** ``hash-region --region X,Y,W,H`` hashes a rectangle.
  Hash before acting and poll until the hash differs to wait for a repaint
  without a sleep; poll until two hashes in a row agree to wait for it to
  settle. A blinking caret or a spinner never settles, so hash a region that
  leaves it out, or wait only for the change.
- **Reference images.** ``screenshot --window <id> --path FILE`` captures
  the window alone, even when something overlaps it. Compare against a
  reference taken on the same renderer and output size, or keep it as
  evidence.

Evidence on failure
-------------------

When a check fails, save what explains it before teardown removes it: a
``screenshot`` of the whole output, ``tree`` (what was open, where, and what
had focus), ``server-info`` (WM version and every gate's state), the WM log
from ``$SHOESTRING_WM_LOG``, and the app's stdout and stderr. Keep the run
directory instead of deleting it, and print its path.

Teardown
--------

Tear down from an ``EXIT`` trap so it runs however the test ends, failure and
interrupt included:

1. Stop the app if it is still running: it is the test's child, not the
   WM's, so ``shutdown`` doesn't reach it. Kill detached ``run-command``
   children by pid the same way.
2. ``shoestring-ctl shutdown`` stops the WM without a confirmation prompt,
   signals the processes the WM started, and removes its sockets. If the WM
   doesn't answer, kill its pid. Then ``wait`` for it.
3. Remove the run directory, unless the test failed or you asked to keep it.

A complete example
------------------

This script tests ``zenity --entry``: it waits for the dialog, checks the
background with a pixel probe, types a name and waits for the dialog to
repaint, saves a reference image of the window, presses Return, waits for the
dialog to close, and checks what zenity printed. It needs ``shoestring-wm``,
``shoestring-ctl``, ``jq`` and ``zenity``; it prints ``PASS``, or ``FAIL``
with the kept run directory and its ``artifacts/``. The file is
``docs/examples/app-test.sh`` in the repository.

.. literalinclude:: examples/app-test.sh
   :language: sh

The same workflow in Rust drives this project's own end-to-end tests:
``tests/common/mod.rs`` starts an isolated headless WM, maps a test client,
and speaks the IPC socket directly. A test written in another language can do
the same with the newline-delimited JSON described in :doc:`ipc`; see
:ref:`ipc-client-libraries`.
