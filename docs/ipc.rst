IPC
===

shoestring-wm exposes a small unix-socket IPC for querying state and
subscribing to events. The protocol is newline-delimited JSON: one
``Request`` object terminated by ``\n`` in, one ``Response`` object out;
for the event-stream request, the server keeps the connection open and
pushes ``Event`` objects forever (one per line) until the client
disconnects.

Socket location
---------------

The WM listens on
``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`` and exports the
path as ``$SHOESTRING_WM_SOCKET`` to every child process.

Clients should:

1. Prefer ``$SHOESTRING_WM_SOCKET`` when set.
2. Fall back to the conventional path above.

The reference client (``shoestring-ctl``) and the bar both use the
``shoestring-ipc`` crate's ``client_socket_path()`` helper which
implements this fallback.

Requests
--------

Each request is a JSON object with a ``type`` discriminator:

.. list-table::
   :header-rows: 1
   :widths: 22 78

   * - Request
     - Meaning
   * - ``{"type": "workspaces"}``
     - List workspaces and which one is currently active.
   * - ``{"type": "windows"}``
     - List every mapped window (across all workspaces).
   * - ``{"type": "outputs"}``
     - List every connected output.
   * - ``{"type": "event_stream"}``
     - Switch into streaming mode. Server replies once with ``Ok`` then
       pushes events.
   * - ``{"type": "inject_key", "keysym": "Return"}``
     - Synthesize a single keypress (press + release) targeting the
       focused surface. ``keysym`` is any X keysym name understood by
       ``xkb_keysym_from_name`` (e.g. ``"Return"``, ``"F5"``,
       ``"BackSpace"``, ``"q"``).
   * - ``{"type": "inject_text", "text": "hello"}``
     - Type a literal string into the focused surface. v1 supports ASCII
       letters, digits, and space; other codepoints return an ``error``
       so the caller knows to break the string up.
   * - ``{"type": "inject_click", "button": "left"}``
     - Synthesize a single mouse click. ``button`` is one of ``"left"`` /
       ``"right"`` / ``"middle"``, or a numeric Linux ``BTN_*`` code as a
       string. Pass ``"x"`` and ``"y"`` (both, as numbers) to move the
       pointer to those compositor-space coordinates first.
   * - ``{"type": "pick_window"}``
     - Enter interactive picker mode: the WM waits for the user's next
       click and replies with ``picked_window``. Pointer/keyboard input
       is intercepted while the picker is up — left-click resolves to a
       toplevel, right-click and Escape cancel, other keys are
       swallowed. Only one picker may be active at a time. If the
       client disconnects mid-pick the picker is cancelled.
   * - ``{"type": "close_window", "id": "..."}``
     - Send ``xdg_toplevel.close`` to the window with the given
       ``ext-foreign-toplevel-list-v1`` identifier. The client may
       still surface a save-prompt rather than exiting immediately.
       Returns ``error`` if no window matches.
   * - ``{"type": "focus_window", "id": "..."}``
     - Focus the window with the given identifier. Unminimizes it if
       needed and switches to its workspace if it lives elsewhere,
       then raises + activates it the same way a click does. Returns
       ``error`` if no window matches.

Injected key and click events bypass the WM's binding table — a scripted
``Super+q`` will NOT trigger the ``Quit`` binding. Use the relevant typed
request (or, for now, the WM's existing keybind config) when you want a
WM-level action.

Responses
---------

The server replies with a single JSON object tagged by ``type``:

``ok``
    Generic success acknowledgement. Sent in reply to ``event_stream``
    before the event stream starts.

``workspaces``
    ``{"type": "workspaces", "active": <1..16>, "count": <int>}``

``windows``
    ``{"type": "windows", "windows": [WindowSummary, ...]}``

``outputs``
    ``{"type": "outputs", "outputs": [OutputSummary, ...]}``

``picked_window``
    ``{"type": "picked_window", "window": WindowSummary | null}``.
    Returned in reply to ``pick_window`` once the user resolves the
    picker. ``null`` on cancel or a click that didn't land on a
    toplevel.

``error``
    ``{"type": "error", "message": "..."}``. The client should print the
    message and exit non-zero.

``WindowSummary``::

    {
      "id":        "stable-string-id",
      "title":     "Window title",
      "app_id":    "alacritty",
      "workspace": 3,
      "focused":   true
    }

``id`` matches the ``identifier`` event from ``ext-foreign-toplevel-list-v1``,
so a bar can cross-reference between protocols.

``OutputSummary``::

    {
      "name":   "DP-1",
      "width":  3840,
      "height": 2160,
      "scale":  1.5
    }

Events
------

Pushed continuously on an ``event_stream`` connection, one per line.
Each event is tagged by ``type``.

``workspace_changed``
    ``{"type": "workspace_changed", "active": <1..16>}``

``window_opened``
    ``{"type": "window_opened", "id": "...", "title": "...", "app_id": "...", "workspace": 1}``

``window_closed``
    ``{"type": "window_closed", "id": "..."}``

``window_focused``
    ``{"type": "window_focused", "id": "..." | null}``. ``null`` when no
    window holds keyboard focus.

``window_title_changed``
    ``{"type": "window_title_changed", "id": "...", "title": "...", "app_id": "..."}``

``output_added``
    Same shape as ``OutputSummary``.

``output_removed``
    ``{"type": "output_removed", "name": "DP-1"}``

Reference client
----------------

``shoestring-ctl`` is a thin CLI wrapper around the protocol. Each
subcommand maps to one request:

.. code-block:: console

    $ shoestring-ctl workspaces
    {"type":"workspaces","active":3,"count":16}

    $ shoestring-ctl --pretty windows
    {
      "type": "windows",
      "windows": [
        { "id": "...", "title": "tmux", "app_id": "alacritty",
          "workspace": 1, "focused": true }
      ]
    }

    $ shoestring-ctl event-stream
    {"type":"workspace_changed","active":4}
    {"type":"window_focused","id":"abcd-1234"}
    ...

    $ shoestring-ctl key Return         # synthesize a single Enter press
    $ shoestring-ctl type "hello 123"   # type ASCII into focused surface
    $ shoestring-ctl click left         # click at the current pointer
    $ shoestring-ctl click left --x 200 --y 400

    $ shoestring-ctl pick-window        # blocks until user clicks
    {"type":"picked_window","window":{"id":"...","title":"...", ...}}
    $ shoestring-ctl close-window <id>  # ask that toplevel to close

A higher-level binary, ``shoestring-kill`` (xkill equivalent), chains
the two: it sends ``pick_window``, then forwards the resulting ``id``
to ``close_window`` on success. Bind it via the WM config or invoke it
from a menu.

Flags:

``-s, --socket PATH``
    Override the socket path (otherwise ``$SHOESTRING_WM_SOCKET`` or the
    default).

``-p, --pretty``
    Pretty-print JSON output instead of one-line-per-object.

Writing your own client
-----------------------

A minimal Python client::

    import os, json, socket

    s = socket.socket(socket.AF_UNIX)
    s.connect(os.environ["SHOESTRING_WM_SOCKET"])
    s.sendall(b'{"type":"event_stream"}\n')

    f = s.makefile("r")
    # First line is the Ok ack:
    print("ack:", f.readline().strip())
    for line in f:
        print(json.loads(line))

The protocol is one-shot per connection except for ``event_stream``;
open a new connection for each ad-hoc query.
