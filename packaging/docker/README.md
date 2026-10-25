# shoestring-wm — headless serve-mode container

A disposable, reproducible, IPC-observable, remote-viewable desktop: the
[headless backend](../../docs/running.rst) (no seat, master, or input devices)
plus `shoestring-remote-server`, behind the runtime remote gate. Build once,
run to get a sandboxed desktop you view from a real machine with
`shoestring-remote-client`.

This is the "compositor isolation" story — see
[`docs/containers.rst`](../../docs/containers.rst) for the full framing.

## Build

From the repository root (the build context must be the repo so the workspace
sources are available):

```sh
podman build -t shoestring-wm-headless -f packaging/docker/Dockerfile .
# or: docker build -t shoestring-wm-headless -f packaging/docker/Dockerfile .
```

The build is multi-stage: a `rust:1-bookworm` builder compiles just the three
binaries the container needs — `shoestring-wm` (headless only, no winit/tty
pull-ins), `shoestring-remote-server`, `shoestring-ctl` — into a
`debian:bookworm-slim` runtime carrying the Mesa software stack.

## Run

A GPU is **optional**.

The remote server listens on a Unix socket in `/run/shoestring/remote`.
Bind-mount a private host directory there, and map your host user to the
container user (uid 1000) so the server's uid check accepts you:

```sh
mkdir -m 700 -p "$XDG_RUNTIME_DIR/sandbox"

# CPU-only (Mesa llvmpipe). Nothing special required.
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    -v "$XDG_RUNTIME_DIR/sandbox:/run/shoestring/remote:Z" \
    shoestring-wm-headless

# GPU-accelerated: also pass a render node. --group-add keep-groups (podman)
# lets the container user reach the host 'render' group.
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    -v "$XDG_RUNTIME_DIR/sandbox:/run/shoestring/remote:Z" \
    --device /dev/dri/renderD128 --group-add keep-groups \
    shoestring-wm-headless
```

Then, from your desktop:

```sh
shoestring-remote-client --connect "$XDG_RUNTIME_DIR/sandbox/shoestring-remote.sock" --label sandbox
```

`:Z` relabels the directory for SELinux; leave it off where SELinux isn't in
use. With docker (no user namespace by default), the same mount works when
your host uid is 1000; otherwise the server refuses you. Reach a container on
another host by forwarding its socket over ssh:
`ssh -N -L "$XDG_RUNTIME_DIR/box.sock:/run/user/<uid>/sandbox/shoestring-remote.sock" host`.

The socket only exists while the remote gate is on, and the gate couples the
capture + automation gates — nothing is viewable or driveable until then.

To use TCP instead, set `SHOESTRING_REMOTE_TCP` and publish the port, for
example `-e SHOESTRING_REMOTE_TCP=0.0.0.0:7355 -e SHOESTRING_REMOTE_TCP_ANY_HOST=1
-p 127.0.0.1:7355:7355`. The server binds `0.0.0.0` inside the container
because the published port can't reach the container's own loopback, and it
refuses any address other than loopback without the second setting. That
setting is safe here only because `-p 127.0.0.1:...` keeps the port on the
host's loopback; never pair it with a bare `-p 7355:7355`. Nothing checks who
connects over TCP: every user on the host can reach a loopback-published port
and get the whole session.

## One app per container

The true per-app-isolation pattern: one app, one container. Pass its argv in
`$SHOESTRING_APP`, or bake it into `config.toml`'s `autostart`.

```sh
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    -v "$XDG_RUNTIME_DIR/sandbox:/run/shoestring/remote:Z" \
    -e SHOESTRING_APP='alacritty' shoestring-wm-headless
```

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `SHOESTRING_WM_HEADLESS_SIZE` | `1920x1080` | Virtual output size `WIDTHxHEIGHT`. |
| `SHOESTRING_WM_RENDER_NODE` | auto | Render node to try; falls back to software if it can't be opened. |
| `SHOESTRING_WM_HEADLESS_SOFTWARE` | unset | `1` forces the llvmpipe software path even with a GPU present. |
| `SHOESTRING_REMOTE_SOCKET` | `/run/shoestring/remote/shoestring-remote.sock` | The remote server's Unix socket. |
| `SHOESTRING_REMOTE_TCP` | unset | `ADDR:PORT` to listen on TCP instead, on loopback unless `SHOESTRING_REMOTE_TCP_ANY_HOST=1`. Unchecked: anyone who can reach it gets the session. |
| `SHOESTRING_REMOTE_TCP_ANY_HOST` | `0` | `1` allows `SHOESTRING_REMOTE_TCP` on an address other than loopback, such as `0.0.0.0`. Required for TCP in a container; see above. |
| `SHOESTRING_REMOTE_ENABLE` | `1` | `0` starts gated-off; flip later with `podman exec … shoestring-ctl remote on`. |
| `SHOESTRING_APP` | unset | Optional app argv to launch into the session. |
| `SHOESTRING_WM_CONFIG` | `/etc/shoestring-wm/config.toml` | WM config path. |

## Observe / drive a running container

The compositor *is* the automation surface. Drive it over IPC from inside the
container:

```sh
podman exec -it <ctr> sh -lc \
  'SHOESTRING_WM_SOCKET=$(ls $XDG_RUNTIME_DIR/shoestring-wm-*.sock) \
   shoestring-ctl windows'
```
