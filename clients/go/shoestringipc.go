// Package shoestringipc is a thin, dependency-free client for the shoestring-wm
// IPC socket.
//
// shoestring-wm speaks newline-delimited JSON over a unix socket: one request
// object terminated by '\n' in, one response object out. For the event stream
// the server keeps the connection open and pushes one event object per line.
//
// This package wraps that protocol with socket discovery, the request/response
// round-trip, and an event-stream iterator. Responses are returned as
// map[string]any exactly as they arrive on the wire (including the "type"
// discriminator), so the client need not change when the protocol gains a new
// field — see the stability notes in docs/ipc.rst, the canonical reference for
// every request, response, and event shape.
//
// Example:
//
//	wm, err := shoestringipc.Dial()
//	if err != nil {
//		log.Fatal(err)
//	}
//	defer wm.Close()
//
//	ws, _ := wm.Workspaces()
//	fmt.Println("active:", ws["active"])
package shoestringipc

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
)

// SocketEnv is the environment variable the WM exports pointing at its socket.
const SocketEnv = "SHOESTRING_WM_SOCKET"

// Error is returned when the WM replies with {"type": "error"}.
//
// Branch on the *type* (errors.As) of this error, not on its message string,
// which may be reworded at any time. The one stable prefix is the
// automation-gate refusal, "automation disabled: ...".
type Error struct {
	Message string
}

func (e *Error) Error() string { return e.Message }

// DefaultSocketPath returns the conventional socket path,
// $XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock. The bool is false when
// either environment variable is unset.
func DefaultSocketPath() (string, bool) {
	runtime := os.Getenv("XDG_RUNTIME_DIR")
	display := os.Getenv("WAYLAND_DISPLAY")
	if runtime == "" || display == "" {
		return "", false
	}
	return filepath.Join(runtime, fmt.Sprintf("shoestring-wm-%s.sock", display)), true
}

// ClientSocketPath resolves the socket path a client should use: it prefers
// $SHOESTRING_WM_SOCKET, falling back to DefaultSocketPath. The bool is false
// when neither resolves (no WM is running, or this is not a shoestring-wm
// session).
func ClientSocketPath() (string, bool) {
	if env := os.Getenv(SocketEnv); env != "" {
		return env, true
	}
	return DefaultSocketPath()
}

// Client is a client for the shoestring-wm IPC socket.
//
// It holds no persistent connection: the WM serves one request per connection
// and then hangs up, so each Request dials a fresh short-lived connection
// (cheap — a local unix socket). A Client is therefore reusable for any number
// of calls and safe for concurrent use (each call gets its own connection).
//
// Events and MetricsStream are the exception: the WM keeps those connections
// open and pushes forever, so each returns a *Stream that owns its own
// connection until closed.
type Client struct {
	path string
}

// New discovers the socket via ClientSocketPath and returns a client for it.
// It does not connect — the first connection happens on the first request.
func New() (*Client, error) {
	path, ok := ClientSocketPath()
	if !ok {
		return nil, &Error{Message: "no shoestring-wm socket: set $" + SocketEnv +
			" or run inside a session ($XDG_RUNTIME_DIR + $WAYLAND_DISPLAY)"}
	}
	return &Client{path: path}, nil
}

// NewWithPath returns a client bound to an explicit socket path.
func NewWithPath(path string) *Client { return &Client{path: path} }

// Close is a no-op kept for symmetry: a Client holds no persistent connection.
func (c *Client) Close() error { return nil }

// Request dials a fresh connection, sends one request object, and returns the
// one response object.
//
// req must include a "type" key, e.g. map[string]any{"type": "windows"}. It
// returns an *Error when the server replies with an error.
func (c *Client) Request(req map[string]any) (map[string]any, error) {
	conn, err := net.Dial("unix", c.path)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	if err := writeJSON(conn, req); err != nil {
		return nil, err
	}
	resp, err := readJSON(bufio.NewReader(conn))
	if err != nil {
		return nil, err
	}
	if t, _ := resp["type"].(string); t == "error" {
		msg, _ := resp["message"].(string)
		return nil, &Error{Message: msg}
	}
	return resp, nil
}

func writeJSON(w io.Writer, v map[string]any) error {
	buf, err := json.Marshal(v)
	if err != nil {
		return err
	}
	_, err = w.Write(append(buf, '\n'))
	return err
}

func readJSON(r *bufio.Reader) (map[string]any, error) {
	line, err := r.ReadBytes('\n')
	if err != nil {
		if err == io.EOF && len(line) == 0 {
			return nil, fmt.Errorf("shoestring-wm closed the connection: %w", io.EOF)
		}
		if err != io.EOF {
			return nil, err
		}
	}
	var msg map[string]any
	if err := json.Unmarshal(line, &msg); err != nil {
		return nil, err
	}
	return msg, nil
}

// --- read-only queries (never gated by automation) ---

// Workspaces lists workspaces and which one is active.
func (c *Client) Workspaces() (map[string]any, error) {
	return c.Request(map[string]any{"type": "workspaces"})
}

// Windows lists every mapped window across all workspaces.
func (c *Client) Windows() (map[string]any, error) {
	return c.Request(map[string]any{"type": "windows"})
}

// Outputs lists every connected output.
func (c *Client) Outputs() (map[string]any, error) {
	return c.Request(map[string]any{"type": "outputs"})
}

// Inputs lists every connected input device (empty under nested winit).
func (c *Client) Inputs() (map[string]any, error) {
	return c.Request(map[string]any{"type": "inputs"})
}

// GetTree snapshots the full window tree (the swaymsg -t get_tree analogue).
func (c *Client) GetTree() (map[string]any, error) {
	return c.Request(map[string]any{"type": "get_tree"})
}

// PointerPosition reads the current pointer location in compositor-space coords.
func (c *Client) PointerPosition() (map[string]any, error) {
	return c.Request(map[string]any{"type": "pointer_position"})
}

// Metrics snapshots the diagnostics registry (answers even when sampling is off).
func (c *Client) Metrics() (map[string]any, error) {
	return c.Request(map[string]any{"type": "metrics"})
}

// FindWindows lists windows whose title/appID match the given regexes. Each is
// an unanchored Rust-regex pattern; pass "" to leave a filter unset (matches
// everything). Reuses the "windows" reply shape.
func (c *Client) FindWindows(title, appID string) (map[string]any, error) {
	req := map[string]any{"type": "find_windows"}
	if title != "" {
		req["title"] = title
	}
	if appID != "" {
		req["app_id"] = appID
	}
	return c.Request(req)
}

// AutomationStatus reads the automation-gate state without changing it.
func (c *Client) AutomationStatus() (map[string]any, error) {
	return c.Request(map[string]any{"type": "automation_status"})
}

// ScreenCaptureStatus reads the screen-capture-gate state without changing it.
func (c *Client) ScreenCaptureStatus() (map[string]any, error) {
	return c.Request(map[string]any{"type": "screen_capture_status"})
}

// MediaStatus reads the last media-privacy snapshot (mute / mic / camera).
func (c *Client) MediaStatus() (map[string]any, error) {
	return c.Request(map[string]any{"type": "media_status"})
}

// --- window management (not gated) ---

// CloseWindow sends xdg_toplevel.close to the window with the given id.
func (c *Client) CloseWindow(id string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "close_window", "id": id})
}

// FocusWindow focuses the window with the given id (raises + activates).
func (c *Client) FocusWindow(id string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "focus_window", "id": id})
}

// RaiseWindow raises the window to the top of the stack (pure restack).
func (c *Client) RaiseWindow(id string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "raise_window", "id": id})
}

// LowerWindow lowers the window to the bottom of the stack.
func (c *Client) LowerWindow(id string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "lower_window", "id": id})
}

// SetWindowSticky sets or clears the sticky (show-on-all-workspaces) flag.
func (c *Client) SetWindowSticky(id string, sticky bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_window_sticky", "id": id, "sticky": sticky})
}

// SetWindowAlwaysOnTop sets or clears the always-on-top flag.
func (c *Client) SetWindowAlwaysOnTop(id string, alwaysOnTop bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_window_always_on_top", "id": id, "always_on_top": alwaysOnTop})
}

// SetWindowName overrides the window's display name (empty name clears it).
func (c *Client) SetWindowName(id, name string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_window_name", "id": id, "name": name})
}

// PickWindow enters interactive picker mode; it blocks until the user clicks or
// cancels.
func (c *Client) PickWindow() (map[string]any, error) {
	return c.Request(map[string]any{"type": "pick_window"})
}

// --- input synthesis & capture (gated by the automation gate) ---

// InjectKey synthesizes a keypress to the focused surface. Pass nil modifiers
// for a bare key.
func (c *Client) InjectKey(keysym string, modifiers []string) (map[string]any, error) {
	req := map[string]any{"type": "inject_key", "keysym": keysym}
	if len(modifiers) > 0 {
		req["modifiers"] = modifiers
	}
	return c.Request(req)
}

// InjectText types a literal ASCII string into the focused surface.
func (c *Client) InjectText(text string) (map[string]any, error) {
	return c.Request(map[string]any{"type": "inject_text", "text": text})
}

// InjectClick synthesizes a click. Pass a non-nil x and y to move the pointer
// there first.
func (c *Client) InjectClick(button string, x, y *float64) (map[string]any, error) {
	req := map[string]any{"type": "inject_click", "button": button}
	if x != nil && y != nil {
		req["x"] = *x
		req["y"] = *y
	}
	return c.Request(req)
}

// MoveMouse moves the pointer to compositor-space (x, y) without clicking.
func (c *Client) MoveMouse(x, y float64) (map[string]any, error) {
	return c.Request(map[string]any{"type": "move_mouse", "x": x, "y": y})
}

// Screenshot captures a PNG. Pass output "" for the default; region may be nil
// or a map with x/y/w/h (and then output must be set).
func (c *Client) Screenshot(output string, region map[string]int) (map[string]any, error) {
	req := map[string]any{"type": "screenshot"}
	if output != "" {
		req["output"] = output
	}
	if region != nil {
		req["region"] = region
	}
	return c.Request(req)
}

// RunCommand runs a child process under the WM's environment and captures its
// output. Pass timeoutMs <= 0 to omit the timeout.
func (c *Client) RunCommand(argv []string, timeoutMs int) (map[string]any, error) {
	req := map[string]any{"type": "run_command", "argv": argv}
	if timeoutMs > 0 {
		req["timeout_ms"] = timeoutMs
	}
	return c.Request(req)
}

// DispatchAction runs a bind Action server-side, e.g.
// map[string]any{"type": "focus-workspace", "index": 3}.
func (c *Client) DispatchAction(action map[string]any) (map[string]any, error) {
	return c.Request(map[string]any{"type": "dispatch_action", "action": action})
}

// --- gates ---

// SetAutomation flips the runtime automation gate (also flips screen capture).
func (c *Client) SetAutomation(enabled bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_automation", "enabled": enabled})
}

// SetScreenCapture flips the runtime screen-capture gate.
func (c *Client) SetScreenCapture(enabled bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_screen_capture", "enabled": enabled})
}

// --- session / power (confirm-gated by a dialog, not the automation gate) ---

// Lock locks the session (spawns the configured lock binary).
func (c *Client) Lock() (map[string]any, error) {
	return c.Request(map[string]any{"type": "lock"})
}

// Quit pops the quit-confirmation dialog (exits only on the user's Yes).
func (c *Client) Quit() (map[string]any, error) {
	return c.Request(map[string]any{"type": "quit"})
}

// PowerOff pops the power-off confirmation dialog.
func (c *Client) PowerOff() (map[string]any, error) {
	return c.Request(map[string]any{"type": "power_off"})
}

// Reboot pops the reboot confirmation dialog.
func (c *Client) Reboot() (map[string]any, error) {
	return c.Request(map[string]any{"type": "reboot"})
}

// Suspend pops the suspend confirmation dialog.
func (c *Client) Suspend() (map[string]any, error) {
	return c.Request(map[string]any{"type": "suspend"})
}

// --- display / media ---

// SetGamma sets a per-output gamma ramp from a color temperature (Kelvin, KMS
// only). Pass output "" for every capable output; brightness/gamma <= 0 omit
// those fields (the WM defaults them to 1.0).
func (c *Client) SetGamma(temperature int, output string, brightness, gamma float64) (map[string]any, error) {
	req := map[string]any{"type": "set_gamma", "temperature": temperature}
	if output != "" {
		req["output"] = output
	}
	if brightness > 0 {
		req["brightness"] = brightness
	}
	if gamma > 0 {
		req["gamma"] = gamma
	}
	return c.Request(req)
}

// ResetGamma clears the WM's own gamma override and restores the original ramp.
// Pass output "" to clear every output the WM set.
func (c *Client) ResetGamma(output string) (map[string]any, error) {
	req := map[string]any{"type": "reset_gamma"}
	if output != "" {
		req["output"] = output
	}
	return c.Request(req)
}

// SetAudioMute toggles the default audio sink mute (via shoestring-mediad).
func (c *Client) SetAudioMute(enabled bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_audio_mute", "enabled": enabled})
}

// SetMicMute toggles the default audio source (microphone) mute.
func (c *Client) SetMicMute(enabled bool) (map[string]any, error) {
	return c.Request(map[string]any{"type": "set_mic_mute", "enabled": enabled})
}

// --- config ---

// ReloadConfig re-reads the WM's TOML config and recompiles the binding table.
func (c *Client) ReloadConfig() (map[string]any, error) {
	return c.Request(map[string]any{"type": "reload_config"})
}

// --- streams ---

// Stream is a live subscription to a server-pushed event stream. Call Next
// repeatedly; it blocks until the next message arrives and returns io.EOF when
// the WM closes the connection. Call Close to stop early. A Stream owns its own
// connection, so requests on the parent Client are unaffected.
type Stream struct {
	conn net.Conn
	r    *bufio.Reader
}

// Next reads the next message from the stream. It returns io.EOF at end.
func (s *Stream) Next() (map[string]any, error) {
	line, err := s.r.ReadBytes('\n')
	if err != nil {
		if err == io.EOF && len(line) == 0 {
			return nil, io.EOF
		}
		if err != io.EOF {
			return nil, err
		}
	}
	var msg map[string]any
	if err := json.Unmarshal(line, &msg); err != nil {
		return nil, err
	}
	return msg, nil
}

// Close stops the subscription and releases its connection.
func (s *Stream) Close() error { return s.conn.Close() }

// Events subscribes to the event stream. The returned Stream owns its own
// connection until Close (or the WM hangs up).
func (c *Client) Events() (*Stream, error) {
	return c.subscribe(map[string]any{"type": "event_stream"})
}

// MetricsStream subscribes to metrics samples (requires [diagnostics].enabled).
// intervalMs <= 0 pushes on every sample tick; otherwise it is clamped up to
// the WM's sample interval. The returned Stream owns its own connection.
func (c *Client) MetricsStream(intervalMs int) (*Stream, error) {
	req := map[string]any{"type": "metrics_stream"}
	if intervalMs > 0 {
		req["interval_ms"] = intervalMs
	}
	return c.subscribe(req)
}

func (c *Client) subscribe(req map[string]any) (*Stream, error) {
	conn, err := net.Dial("unix", c.path)
	if err != nil {
		return nil, err
	}
	r := bufio.NewReader(conn)
	if err := writeJSON(conn, req); err != nil {
		conn.Close()
		return nil, err
	}
	ack, err := readJSON(r)
	if err != nil {
		conn.Close()
		return nil, err
	}
	if t, _ := ack["type"].(string); t == "error" {
		conn.Close()
		msg, _ := ack["message"].(string)
		return nil, &Error{Message: msg}
	}
	return &Stream{conn: conn, r: r}, nil
}
