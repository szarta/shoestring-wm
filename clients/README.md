# shoestring-wm IPC client libraries

Thin, dependency-free client libraries for the shoestring-wm IPC socket, so you
can script the window manager from Python, Go, or TypeScript without shelling
out to `shoestring-ctl` or reimplementing the wire protocol.

The protocol itself — newline-delimited JSON over a unix socket — is documented
in [`docs/ipc.rst`](../docs/ipc.rst). That document is the canonical reference
for every request, response, and event shape; these libraries are a transport
convenience on top of it, not a second source of truth.

## Design

All three libraries follow the same deliberately-thin shape:

- **Socket discovery.** They resolve the socket exactly like the reference
  client: prefer `$SHOESTRING_WM_SOCKET`, else
  `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`.
- **One connection per request.** The WM serves a single request per connection
  and then hangs up, so each call opens a fresh short-lived connection (a local
  unix socket — cheap). A client object is reusable and holds no persistent
  state.
- **Streams own their connection.** `event_stream` and `metrics_stream` are the
  exception: the WM keeps those connections open and pushes forever, so the
  event iterator owns its own connection for the life of the loop.
- **Untyped responses.** Responses come back as the language's native JSON value
  (`dict` / `map[string]any` / object), exactly as they arrive on the wire,
  including the `type` discriminator. This keeps the libraries forward-compatible
  by construction: the protocol is append-only and adds optional fields, so a
  client built today keeps working against a newer WM without edits (see the
  stability rules in `docs/ipc.rst`). Convenience methods (`workspaces()`,
  `windows()`, `find_windows(...)`, `inject_key(...)`, …) just build the request
  object for you.
- **Errors.** A `{"type": "error"}` reply is raised/returned as a typed error
  (`IpcError` / `*Error`). Branch on the error *type*, not its message text — the
  one stable prefix is the automation-gate refusal, `"automation disabled: ..."`.

## Python

Stdlib only; Python 3.8+. Single module, no install needed — drop
[`python/shoestring_ipc.py`](python/) next to your script or put it on
`PYTHONPATH`.

```python
import shoestring_ipc

wm = shoestring_ipc.Client()  # auto-discovers the socket
print(wm.workspaces()["active"])
for win in wm.windows()["windows"]:
    print(win["id"], win["app_id"], win["title"])

# Stream events:
for event in wm.events():
    print(event)
```

Run the demo: `python3 python/example.py`.

## Go

Stdlib only; Go 1.21+. Import the module:

```go
import shoestringipc "github.com/szarta/shoestring-wm/clients/go"

wm, err := shoestringipc.New() // auto-discovers the socket
if err != nil {
    log.Fatal(err)
}
ws, _ := wm.Workspaces()
fmt.Println("active:", ws["active"])

stream, err := wm.Events()
if err != nil {
    log.Fatal(err)
}
defer stream.Close()
for {
    ev, err := stream.Next()
    if err == io.EOF {
        break
    }
    fmt.Println(ev)
}
```

Run the demo: `cd go && go run ./example`.

## TypeScript / Node

Node's built-in `net` only — zero runtime dependencies. Build with `tsc`
(TypeScript 5+, `@types/node` 18+):

```ts
import { Client } from "./shoestring-ipc";

const wm = new Client(); // auto-discovers the socket
const ws = await wm.workspaces();
console.log("active:", ws.active);

for await (const event of wm.events()) {
  console.log(event);
}
```

Build and run the demo: `cd typescript && npm install && npm run example`.

## Versioning

The wire format is stabilizing but only contractually frozen at shoestring-wm
1.0 (see the stability section of `docs/ipc.rst`). These libraries carry no
hard-coded schema, so they ride additive protocol changes for free; a request
or field that a *too-old WM* doesn't recognize comes back as a normal `error`.
Key off `shoestring-ctl --version` when you need to gate on a specific feature.
