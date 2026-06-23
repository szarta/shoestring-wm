/**
 * Thin TypeScript/Node client for the shoestring-wm IPC socket.
 *
 * shoestring-wm speaks newline-delimited JSON over a unix socket: one request
 * object terminated by `\n` in, one response object out. For the event stream
 * the server keeps the connection open and pushes one event object per line.
 *
 * This module is a dependency-free wrapper (Node's built-in `net` only) around
 * that protocol: socket discovery, the request/response round-trip, and an
 * async-iterable event stream. Responses are returned exactly as they arrive on
 * the wire (including the `type` discriminator), so the client need not change
 * when the protocol gains a new field — see the stability notes in
 * `docs/ipc.rst`, the canonical reference for every request, response, and
 * event shape.
 *
 * @example
 * ```ts
 * import { Client } from "./shoestring-ipc";
 *
 * const wm = new Client();
 * const ws = await wm.workspaces();
 * console.log("active:", ws.active);
 * for await (const event of wm.events()) console.log(event);
 * ```
 */

import { createConnection, type Socket } from "node:net";

/** Environment variable the WM exports pointing at its socket. */
export const SOCKET_ENV = "SHOESTRING_WM_SOCKET";

/** A response or event object as it arrives on the wire. */
export type Message = Record<string, any>;

/**
 * Thrown when the WM replies with `{"type": "error"}`.
 *
 * Branch on `instanceof IpcError`, not on the message string, which may be
 * reworded at any time. The one stable prefix is the automation-gate refusal,
 * `"automation disabled: ..."`.
 */
export class IpcError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "IpcError";
  }
}

/**
 * The conventional socket path,
 * `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`, or `null` when either
 * environment variable is unset.
 */
export function defaultSocketPath(): string | null {
  const runtime = process.env.XDG_RUNTIME_DIR;
  const display = process.env.WAYLAND_DISPLAY;
  if (!runtime || !display) return null;
  return `${runtime}/shoestring-wm-${display}.sock`;
}

/**
 * Resolve the socket path a client should use: prefer `$SHOESTRING_WM_SOCKET`,
 * fall back to {@link defaultSocketPath}. `null` when neither resolves (no WM is
 * running, or this is not a shoestring-wm session).
 */
export function clientSocketPath(): string | null {
  return process.env[SOCKET_ENV] || defaultSocketPath();
}

type Waiter = { resolve: (v: Message) => void; reject: (e: Error) => void };

/** A single short-lived connection: line-buffered JSON over one socket. */
class Conn {
  private buffer = "";
  private readonly lineQueue: Message[] = [];
  private readonly waiters: Waiter[] = [];
  private closedError: Error | null = null;

  private constructor(private readonly sock: Socket) {
    sock.setEncoding("utf8");
    sock.on("data", (chunk: string) => this.onData(chunk));
    sock.on("error", (err: Error) => this.fail(err));
    sock.on("close", () =>
      this.fail(this.closedError ?? new IpcError("shoestring-wm closed the connection")),
    );
  }

  static open(path: string): Promise<Conn> {
    return new Promise((resolve, reject) => {
      const sock = createConnection(path);
      sock.once("connect", () => resolve(new Conn(sock)));
      sock.once("error", (err: Error) => reject(err));
    });
  }

  send(req: Message): void {
    this.sock.write(JSON.stringify(req) + "\n");
  }

  close(): void {
    this.sock.destroy();
  }

  nextLine(): Promise<Message> {
    if (this.lineQueue.length > 0) return Promise.resolve(this.lineQueue.shift()!);
    if (this.closedError) return Promise.reject(this.closedError);
    return new Promise((resolve, reject) => this.waiters.push({ resolve, reject }));
  }

  get closed(): Error | null {
    return this.closedError;
  }

  private onData(chunk: string): void {
    this.buffer += chunk;
    let idx: number;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      let obj: Message;
      try {
        obj = JSON.parse(line);
      } catch (err) {
        this.fail(err as Error);
        return;
      }
      const waiter = this.waiters.shift();
      if (waiter) waiter.resolve(obj);
      else this.lineQueue.push(obj);
    }
  }

  private fail(err: Error): void {
    this.closedError = err;
    const pending = this.waiters.splice(0);
    for (const w of pending) w.reject(err);
  }
}

/**
 * A client for the shoestring-wm IPC socket.
 *
 * Construct with no arguments to auto-discover the socket, or pass an explicit
 * `path`. The client holds no persistent connection: the WM serves one request
 * per connection and then hangs up, so each {@link request} opens a fresh
 * short-lived connection (cheap — a local unix socket). A single {@link Client}
 * is reusable for any number of calls.
 *
 * {@link events} and {@link metricsStream} are the exception: the WM keeps those
 * connections open and pushes forever, so each owns its own connection for the
 * lifetime of the iteration (and closes it when the loop ends or `break`s).
 */
export class Client {
  readonly path: string;

  constructor(path?: string) {
    const p = path ?? clientSocketPath();
    if (!p) {
      throw new IpcError(
        `no shoestring-wm socket: set $${SOCKET_ENV} or run inside a session ` +
          "($XDG_RUNTIME_DIR + $WAYLAND_DISPLAY)",
      );
    }
    this.path = p;
  }

  /** No-op: the client holds no persistent connection. Kept for symmetry. */
  close(): void {}

  /**
   * Open a fresh connection, send one request object, and resolve with the one
   * response object.
   *
   * `req` must include a `type` key, e.g. `{ type: "windows" }`. Rejects with an
   * {@link IpcError} when the server replies with an error.
   */
  async request(req: Message): Promise<Message> {
    const conn = await Conn.open(this.path);
    try {
      conn.send(req);
      const resp = await conn.nextLine();
      if (resp.type === "error") {
        throw new IpcError(typeof resp.message === "string" ? resp.message : "unknown error");
      }
      return resp;
    } finally {
      conn.close();
    }
  }

  // -- read-only queries (never gated by automation) ---

  /** List workspaces and which one is active. */
  workspaces(): Promise<Message> {
    return this.request({ type: "workspaces" });
  }

  /** List every mapped window across all workspaces. */
  windows(): Promise<Message> {
    return this.request({ type: "windows" });
  }

  /** List every connected output. */
  outputs(): Promise<Message> {
    return this.request({ type: "outputs" });
  }

  /** List every connected input device (empty under nested winit). */
  inputs(): Promise<Message> {
    return this.request({ type: "inputs" });
  }

  /** Snapshot the full window tree (the `swaymsg -t get_tree` analogue). */
  getTree(): Promise<Message> {
    return this.request({ type: "get_tree" });
  }

  /** Read the current pointer location in compositor-space coords. */
  pointerPosition(): Promise<Message> {
    return this.request({ type: "pointer_position" });
  }

  /** Snapshot the diagnostics registry (answers even when sampling is off). */
  metrics(): Promise<Message> {
    return this.request({ type: "metrics" });
  }

  /**
   * List windows whose `title`/`appId` match the given regexes. Each is an
   * unanchored Rust-`regex` pattern; omit one to leave that filter unset
   * (matches everything). Reuses the `windows` reply shape.
   */
  findWindows(opts: { title?: string; appId?: string } = {}): Promise<Message> {
    const req: Message = { type: "find_windows" };
    if (opts.title !== undefined) req.title = opts.title;
    if (opts.appId !== undefined) req.app_id = opts.appId;
    return this.request(req);
  }

  /** Read the automation-gate state without changing it. */
  automationStatus(): Promise<Message> {
    return this.request({ type: "automation_status" });
  }

  /** Read the screen-capture-gate state without changing it. */
  screenCaptureStatus(): Promise<Message> {
    return this.request({ type: "screen_capture_status" });
  }

  /** Read the last media-privacy snapshot (mute / mic / camera). */
  mediaStatus(): Promise<Message> {
    return this.request({ type: "media_status" });
  }

  // -- window management (not gated) ---

  /** Send `xdg_toplevel.close` to the window with the given id. */
  closeWindow(id: string): Promise<Message> {
    return this.request({ type: "close_window", id });
  }

  /** Focus the window with the given id (raises + activates, like a click). */
  focusWindow(id: string): Promise<Message> {
    return this.request({ type: "focus_window", id });
  }

  /** Raise the window to the top of the stack (pure restack, no focus change). */
  raiseWindow(id: string): Promise<Message> {
    return this.request({ type: "raise_window", id });
  }

  /** Lower the window to the bottom of the stack (no focus change). */
  lowerWindow(id: string): Promise<Message> {
    return this.request({ type: "lower_window", id });
  }

  /** Set or clear the sticky (show-on-all-workspaces) flag. */
  setWindowSticky(id: string, sticky: boolean): Promise<Message> {
    return this.request({ type: "set_window_sticky", id, sticky });
  }

  /** Set or clear the always-on-top flag. */
  setWindowAlwaysOnTop(id: string, alwaysOnTop: boolean): Promise<Message> {
    return this.request({ type: "set_window_always_on_top", id, always_on_top: alwaysOnTop });
  }

  /** Override the window's display name (empty `name` clears the override). */
  setWindowName(id: string, name: string): Promise<Message> {
    return this.request({ type: "set_window_name", id, name });
  }

  /** Enter interactive picker mode; resolves when the user clicks or cancels. */
  pickWindow(): Promise<Message> {
    return this.request({ type: "pick_window" });
  }

  // -- input synthesis & capture (gated by the automation gate) ---

  /** Synthesize a keypress to the focused surface, optionally with modifiers. */
  injectKey(keysym: string, modifiers?: string[]): Promise<Message> {
    const req: Message = { type: "inject_key", keysym };
    if (modifiers && modifiers.length > 0) req.modifiers = modifiers;
    return this.request(req);
  }

  /** Type a literal ASCII string into the focused surface. */
  injectText(text: string): Promise<Message> {
    return this.request({ type: "inject_text", text });
  }

  /** Synthesize a click; pass both `x` and `y` to move the pointer first. */
  injectClick(button = "left", x?: number, y?: number): Promise<Message> {
    const req: Message = { type: "inject_click", button };
    if (x !== undefined && y !== undefined) {
      req.x = x;
      req.y = y;
    }
    return this.request(req);
  }

  /** Move the pointer to compositor-space `(x, y)` without clicking. */
  moveMouse(x: number, y: number): Promise<Message> {
    return this.request({ type: "move_mouse", x, y });
  }

  /** Capture a PNG; `region` is `{x,y,w,h}` and requires `output`. */
  screenshot(
    opts: { output?: string; region?: { x: number; y: number; w: number; h: number } } = {},
  ): Promise<Message> {
    const req: Message = { type: "screenshot" };
    if (opts.output !== undefined) req.output = opts.output;
    if (opts.region !== undefined) req.region = opts.region;
    return this.request(req);
  }

  /** Run a child process under the WM's environment and capture its output. */
  runCommand(argv: string[], timeoutMs?: number): Promise<Message> {
    const req: Message = { type: "run_command", argv };
    if (timeoutMs !== undefined) req.timeout_ms = timeoutMs;
    return this.request(req);
  }

  /** Run a bind Action server-side, e.g. `{ type: "focus-workspace", index: 3 }`. */
  dispatchAction(action: Message): Promise<Message> {
    return this.request({ type: "dispatch_action", action });
  }

  // -- gates ---

  /** Flip the runtime automation gate (also flips screen capture). */
  setAutomation(enabled: boolean): Promise<Message> {
    return this.request({ type: "set_automation", enabled });
  }

  /** Flip the runtime screen-capture gate. */
  setScreenCapture(enabled: boolean): Promise<Message> {
    return this.request({ type: "set_screen_capture", enabled });
  }

  // -- session / power (confirm-gated by a dialog, not the automation gate) ---

  /** Lock the session (spawns the configured lock binary). */
  lock(): Promise<Message> {
    return this.request({ type: "lock" });
  }

  /** Pop the quit-confirmation dialog (exits only on the user's *Yes*). */
  quit(): Promise<Message> {
    return this.request({ type: "quit" });
  }

  /** Pop the power-off confirmation dialog. */
  powerOff(): Promise<Message> {
    return this.request({ type: "power_off" });
  }

  /** Pop the reboot confirmation dialog. */
  reboot(): Promise<Message> {
    return this.request({ type: "reboot" });
  }

  /** Pop the suspend confirmation dialog. */
  suspend(): Promise<Message> {
    return this.request({ type: "suspend" });
  }

  // -- display / media ---

  /** Set a per-output gamma ramp from a color temperature (Kelvin, KMS only). */
  setGamma(
    temperature: number,
    opts: { output?: string; brightness?: number; gamma?: number } = {},
  ): Promise<Message> {
    const req: Message = { type: "set_gamma", temperature };
    if (opts.output !== undefined) req.output = opts.output;
    if (opts.brightness !== undefined) req.brightness = opts.brightness;
    if (opts.gamma !== undefined) req.gamma = opts.gamma;
    return this.request(req);
  }

  /** Clear the WM's own gamma override and restore the original ramp. */
  resetGamma(output?: string): Promise<Message> {
    const req: Message = { type: "reset_gamma" };
    if (output !== undefined) req.output = output;
    return this.request(req);
  }

  /** Toggle the default audio sink mute (via shoestring-mediad). */
  setAudioMute(enabled: boolean): Promise<Message> {
    return this.request({ type: "set_audio_mute", enabled });
  }

  /** Toggle the default audio source (microphone) mute. */
  setMicMute(enabled: boolean): Promise<Message> {
    return this.request({ type: "set_mic_mute", enabled });
  }

  // -- config ---

  /** Re-read the WM's TOML config and recompile the binding table. */
  reloadConfig(): Promise<Message> {
    return this.request({ type: "reload_config" });
  }

  // -- streams (each owns its own connection until it ends) ---

  /**
   * Subscribe to the event stream, yielding one event object per line. Opens
   * its own connection (the WM keeps stream connections open and pushes
   * forever) and closes it when the loop ends or `break`s. The iterator also
   * ends when the WM closes the connection.
   */
  events(): AsyncGenerator<Message> {
    return this.stream({ type: "event_stream" });
  }

  /**
   * Subscribe to metrics samples (requires `[diagnostics].enabled`).
   * `intervalMs` is clamped up to the WM's sample interval. Owns its own
   * connection like {@link events}.
   */
  metricsStream(intervalMs?: number): AsyncGenerator<Message> {
    const req: Message = { type: "metrics_stream" };
    if (intervalMs !== undefined) req.interval_ms = intervalMs;
    return this.stream(req);
  }

  private async *stream(req: Message): AsyncGenerator<Message> {
    const conn = await Conn.open(this.path);
    try {
      conn.send(req);
      const ack = await conn.nextLine();
      if (ack.type === "error") {
        throw new IpcError(typeof ack.message === "string" ? ack.message : "unknown error");
      }
      for (;;) {
        try {
          yield await conn.nextLine();
        } catch (err) {
          if (conn.closed) return;
          throw err;
        }
      }
    } finally {
      conn.close();
    }
  }
}
