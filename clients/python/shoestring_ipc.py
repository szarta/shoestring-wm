"""Thin Python client for the shoestring-wm IPC socket.

shoestring-wm speaks newline-delimited JSON over a unix socket: one request
object terminated by ``\\n`` in, one response object out. For the event stream
the server keeps the connection open and pushes one event object per line.

This module is a dependency-free (stdlib-only) wrapper around that protocol. It
handles socket discovery, the request/response round-trip, and the event-stream
loop. Responses are returned as plain ``dict`` objects exactly as they arrive on
the wire (including the ``"type"`` discriminator), so the client never has to be
updated when the protocol gains a new field — see the stability notes in
``docs/ipc.rst``. The canonical reference for every request, response, and event
shape is that same document.

Example::

    import shoestring_ipc

    with shoestring_ipc.Client() as wm:
        print(wm.workspaces()["active"])
        for win in wm.windows()["windows"]:
            print(win["id"], win["title"])
        for event in wm.events():
            print(event)
"""

from __future__ import annotations

import contextlib
import json
import os
import socket
from pathlib import Path
from typing import Any, Iterator, Optional

#: Environment variable the WM exports pointing at its socket.
SOCKET_ENV = "SHOESTRING_WM_SOCKET"

__all__ = [
    "SOCKET_ENV",
    "IpcError",
    "default_socket_path",
    "client_socket_path",
    "Client",
]


class IpcError(RuntimeError):
    """Raised when the WM replies with ``{"type": "error"}``.

    The human-readable text is in :attr:`message`. Branch on the *presence* of
    this exception, not on the message string, which may be reworded at any
    time (the one stable prefix is the automation-gate refusal,
    ``"automation disabled: ..."``).
    """

    def __init__(self, message: str) -> None:
        super().__init__(message)
        self.message = message


def default_socket_path() -> Optional[Path]:
    """The conventional socket path,
    ``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``.

    Returns ``None`` when either environment variable is unset.
    """
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    display = os.environ.get("WAYLAND_DISPLAY")
    if not runtime or not display:
        return None
    return Path(runtime) / f"shoestring-wm-{display}.sock"


def client_socket_path() -> Optional[Path]:
    """Resolve the socket path a client should use.

    Prefers ``$SHOESTRING_WM_SOCKET`` when set, falling back to
    :func:`default_socket_path`. Returns ``None`` when neither resolves (no WM
    is running, or this is not a shoestring-wm session).
    """
    env = os.environ.get(SOCKET_ENV)
    if env:
        return Path(env)
    return default_socket_path()


class Client:
    """A client for the shoestring-wm IPC socket.

    Construct with no arguments to auto-discover the socket, or pass an explicit
    ``path``. The client holds **no** persistent connection: the WM serves one
    request per connection and then hangs up, so each :meth:`request` opens a
    fresh short-lived connection (cheap — a local unix socket). A single
    :class:`Client` is therefore reusable for any number of calls.

    :meth:`events` and :meth:`metrics_stream` are the exception: the WM keeps
    those connections open and pushes forever, so each generator owns its own
    connection for the lifetime of the iteration.
    """

    def __init__(self, path: Optional[os.PathLike[str] | str] = None) -> None:
        if path is None:
            resolved = client_socket_path()
            if resolved is None:
                raise IpcError(
                    "no shoestring-wm socket: set $SHOESTRING_WM_SOCKET or run "
                    "inside a session ($XDG_RUNTIME_DIR + $WAYLAND_DISPLAY)"
                )
            path = resolved
        self.path = Path(path)

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_exc: object) -> None:
        # No persistent resources to release; present for `with` ergonomics.
        pass

    def close(self) -> None:
        """No-op: the client holds no persistent connection. Kept for symmetry."""

    # -- core transport ------------------------------------------------------

    @contextlib.contextmanager
    def _connect(self) -> Iterator[Any]:
        """Open a fresh connection, yielding a read/write binary file over it."""
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(str(self.path))
            file = sock.makefile("rwb")
            try:
                yield file
            finally:
                file.close()
        finally:
            sock.close()

    def request(self, req: dict[str, Any]) -> dict[str, Any]:
        """Send one request object over a fresh connection, return the response.

        ``req`` must include a ``"type"`` key (e.g. ``{"type": "windows"}``).
        Raises :class:`IpcError` if the server replies with an error, or
        :class:`ConnectionError` if the connection drops before a reply.
        """
        with self._connect() as file:
            file.write(json.dumps(req).encode("utf-8") + b"\n")
            file.flush()
            line = file.readline()
        if not line:
            raise ConnectionError(
                "shoestring-wm closed the connection without replying"
            )
        resp: dict[str, Any] = json.loads(line)
        if resp.get("type") == "error":
            raise IpcError(resp.get("message", "unknown error"))
        return resp

    # -- read-only queries (never gated by automation) -----------------------

    def workspaces(self) -> dict[str, Any]:
        """List workspaces and which one is active."""
        return self.request({"type": "workspaces"})

    def windows(self) -> dict[str, Any]:
        """List every mapped window across all workspaces."""
        return self.request({"type": "windows"})

    def outputs(self) -> dict[str, Any]:
        """List every connected output."""
        return self.request({"type": "outputs"})

    def inputs(self) -> dict[str, Any]:
        """List every connected input device (empty under nested winit)."""
        return self.request({"type": "inputs"})

    def get_tree(self) -> dict[str, Any]:
        """Snapshot the full window tree (the ``swaymsg -t get_tree`` analogue)."""
        return self.request({"type": "get_tree"})

    def pointer_position(self) -> dict[str, Any]:
        """Read the current pointer location in compositor-space coords."""
        return self.request({"type": "pointer_position"})

    def metrics(self) -> dict[str, Any]:
        """Snapshot the diagnostics registry (answers even when sampling is off)."""
        return self.request({"type": "metrics"})

    def find_windows(
        self,
        title: Optional[str] = None,
        app_id: Optional[str] = None,
    ) -> dict[str, Any]:
        """List windows whose ``title``/``app_id`` match the given regexes.

        Each filter is an unanchored Rust-``regex`` pattern; both are AND-ed and
        an unset filter matches everything. Reuses the ``windows`` reply shape.
        """
        req: dict[str, Any] = {"type": "find_windows"}
        if title is not None:
            req["title"] = title
        if app_id is not None:
            req["app_id"] = app_id
        return self.request(req)

    def automation_status(self) -> dict[str, Any]:
        """Read the automation-gate state without changing it."""
        return self.request({"type": "automation_status"})

    def screen_capture_status(self) -> dict[str, Any]:
        """Read the screen-capture-gate state without changing it."""
        return self.request({"type": "screen_capture_status"})

    def media_status(self) -> dict[str, Any]:
        """Read the last media-privacy snapshot (mute / mic / camera)."""
        return self.request({"type": "media_status"})

    # -- window management (not gated) ---------------------------------------

    def close_window(self, window_id: str) -> dict[str, Any]:
        """Send ``xdg_toplevel.close`` to the window with the given id."""
        return self.request({"type": "close_window", "id": window_id})

    def focus_window(self, window_id: str) -> dict[str, Any]:
        """Focus the window with the given id (raises + activates, like a click)."""
        return self.request({"type": "focus_window", "id": window_id})

    def raise_window(self, window_id: str) -> dict[str, Any]:
        """Raise the window to the top of the stack (pure restack, no focus change)."""
        return self.request({"type": "raise_window", "id": window_id})

    def lower_window(self, window_id: str) -> dict[str, Any]:
        """Lower the window to the bottom of the stack (no focus change)."""
        return self.request({"type": "lower_window", "id": window_id})

    def set_window_sticky(self, window_id: str, sticky: bool) -> dict[str, Any]:
        """Set or clear the sticky (show-on-all-workspaces) flag."""
        return self.request(
            {"type": "set_window_sticky", "id": window_id, "sticky": sticky}
        )

    def set_window_always_on_top(
        self, window_id: str, always_on_top: bool
    ) -> dict[str, Any]:
        """Set or clear the always-on-top flag."""
        return self.request(
            {
                "type": "set_window_always_on_top",
                "id": window_id,
                "always_on_top": always_on_top,
            }
        )

    def set_window_name(self, window_id: str, name: str) -> dict[str, Any]:
        """Override the window's display name (empty ``name`` clears the override)."""
        return self.request({"type": "set_window_name", "id": window_id, "name": name})

    def pick_window(self) -> dict[str, Any]:
        """Enter interactive picker mode; blocks until the user clicks or cancels."""
        return self.request({"type": "pick_window"})

    # -- input synthesis & capture (gated by the automation gate) ------------

    def inject_key(
        self, keysym: str, modifiers: Optional[list[str]] = None
    ) -> dict[str, Any]:
        """Synthesize a keypress to the focused surface, optionally with modifiers."""
        req: dict[str, Any] = {"type": "inject_key", "keysym": keysym}
        if modifiers:
            req["modifiers"] = list(modifiers)
        return self.request(req)

    def inject_text(self, text: str) -> dict[str, Any]:
        """Type a literal ASCII string into the focused surface."""
        return self.request({"type": "inject_text", "text": text})

    def inject_click(
        self,
        button: str = "left",
        x: Optional[float] = None,
        y: Optional[float] = None,
    ) -> dict[str, Any]:
        """Synthesize a click; pass both ``x`` and ``y`` to move the pointer first."""
        req: dict[str, Any] = {"type": "inject_click", "button": button}
        if x is not None and y is not None:
            req["x"] = x
            req["y"] = y
        return self.request(req)

    def move_mouse(self, x: float, y: float) -> dict[str, Any]:
        """Move the pointer to compositor-space ``(x, y)`` without clicking."""
        return self.request({"type": "move_mouse", "x": x, "y": y})

    def screenshot(
        self,
        output: Optional[str] = None,
        region: Optional[dict[str, int]] = None,
    ) -> dict[str, Any]:
        """Capture a PNG; ``region`` is ``{"x","y","w","h"}`` and needs ``output``."""
        req: dict[str, Any] = {"type": "screenshot"}
        if output is not None:
            req["output"] = output
        if region is not None:
            req["region"] = region
        return self.request(req)

    def run_command(
        self, argv: list[str], timeout_ms: Optional[int] = None
    ) -> dict[str, Any]:
        """Run a child process under the WM's environment and capture its output."""
        req: dict[str, Any] = {"type": "run_command", "argv": list(argv)}
        if timeout_ms is not None:
            req["timeout_ms"] = timeout_ms
        return self.request(req)

    def dispatch_action(self, action: dict[str, Any]) -> dict[str, Any]:
        """Run a bind ``Action`` server-side, e.g. ``{"type": "focus-workspace", "index": 3}``."""
        return self.request({"type": "dispatch_action", "action": action})

    # -- gates ----------------------------------------------------------------

    def set_automation(self, enabled: bool) -> dict[str, Any]:
        """Flip the runtime automation gate (also flips screen capture)."""
        return self.request({"type": "set_automation", "enabled": enabled})

    def set_screen_capture(self, enabled: bool) -> dict[str, Any]:
        """Flip the runtime screen-capture gate."""
        return self.request({"type": "set_screen_capture", "enabled": enabled})

    # -- session / power (confirm-gated by a dialog, not the automation gate) -

    def lock(self) -> dict[str, Any]:
        """Lock the session (spawns the configured lock binary)."""
        return self.request({"type": "lock"})

    def quit(self) -> dict[str, Any]:
        """Pop the quit-confirmation dialog (exits only on the user's *Yes*)."""
        return self.request({"type": "quit"})

    def power_off(self) -> dict[str, Any]:
        """Pop the power-off confirmation dialog."""
        return self.request({"type": "power_off"})

    def reboot(self) -> dict[str, Any]:
        """Pop the reboot confirmation dialog."""
        return self.request({"type": "reboot"})

    def suspend(self) -> dict[str, Any]:
        """Pop the suspend confirmation dialog."""
        return self.request({"type": "suspend"})

    # -- display / media ------------------------------------------------------

    def set_gamma(
        self,
        temperature: int,
        output: Optional[str] = None,
        brightness: Optional[float] = None,
        gamma: Optional[float] = None,
    ) -> dict[str, Any]:
        """Set a per-output gamma ramp from a color temperature (Kelvin, KMS only)."""
        req: dict[str, Any] = {"type": "set_gamma", "temperature": temperature}
        if output is not None:
            req["output"] = output
        if brightness is not None:
            req["brightness"] = brightness
        if gamma is not None:
            req["gamma"] = gamma
        return self.request(req)

    def reset_gamma(self, output: Optional[str] = None) -> dict[str, Any]:
        """Clear the WM's own gamma override and restore the original ramp."""
        req: dict[str, Any] = {"type": "reset_gamma"}
        if output is not None:
            req["output"] = output
        return self.request(req)

    def set_audio_mute(self, enabled: bool) -> dict[str, Any]:
        """Toggle the default audio sink mute (via shoestring-mediad)."""
        return self.request({"type": "set_audio_mute", "enabled": enabled})

    def set_mic_mute(self, enabled: bool) -> dict[str, Any]:
        """Toggle the default audio source (microphone) mute."""
        return self.request({"type": "set_mic_mute", "enabled": enabled})

    # -- config ---------------------------------------------------------------

    def reload_config(self) -> dict[str, Any]:
        """Re-read the WM's TOML config and recompile the binding table."""
        return self.request({"type": "reload_config"})

    # -- streams (each owns the connection until exhausted) ------------------

    def events(self) -> Iterator[dict[str, Any]]:
        """Subscribe to the event stream, yielding one event ``dict`` per line.

        Opens its own connection (the WM keeps stream connections open and
        pushes forever) and owns it until the generator is closed or the WM
        hangs up. Use a separate call to :meth:`request` for queries meanwhile.
        """
        yield from self._stream({"type": "event_stream"})

    def metrics_stream(
        self, interval_ms: Optional[int] = None
    ) -> Iterator[dict[str, Any]]:
        """Subscribe to metrics samples (requires ``[diagnostics].enabled``).

        ``interval_ms`` is clamped up to the WM's sample interval. Like
        :meth:`events`, this owns its own connection until exhausted.
        """
        req: dict[str, Any] = {"type": "metrics_stream"}
        if interval_ms is not None:
            req["interval_ms"] = interval_ms
        yield from self._stream(req)

    def _stream(self, req: dict[str, Any]) -> Iterator[dict[str, Any]]:
        with self._connect() as file:
            file.write(json.dumps(req).encode("utf-8") + b"\n")
            file.flush()
            first = file.readline()
            if not first:
                raise ConnectionError(
                    "shoestring-wm closed the connection without replying"
                )
            ack: dict[str, Any] = json.loads(first)
            if ack.get("type") == "error":
                raise IpcError(ack.get("message", "unknown error"))
            for line in file:
                line = line.strip()
                if line:
                    yield json.loads(line)
