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

```sh
# CPU-only (Mesa llvmpipe). Nothing special required.
podman run --rm -p 127.0.0.1:7355:7355 shoestring-wm-headless

# GPU-accelerated: pass a render node. --group-add keep-groups (podman) lets the
# container user reach the host 'render' group; on docker use --group-add <gid>.
podman run --rm -p 127.0.0.1:7355:7355 \
    --device /dev/dri/renderD128 --group-add keep-groups \
    shoestring-wm-headless
```

Then, from your desktop:

```sh
shoestring-remote-client --connect 127.0.0.1:7355 --label sandbox
```

Publishing to host loopback (`-p 127.0.0.1:7355:7355`) keeps the port off the
network; reach a remote host's container with `ssh -L 7355:127.0.0.1:7355 host`.
The port only opens while the remote gate is on, and the gate couples the
capture + automation gates — nothing is viewable or driveable until then.

## One app per container

The true per-app-isolation pattern: one app, one container. Pass its argv in
`$SHOESTRING_APP`, or bake it into `config.toml`'s `autostart`.

```sh
podman run --rm -p 127.0.0.1:7355:7355 \
    -e SHOESTRING_APP='alacritty' shoestring-wm-headless
```

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `SHOESTRING_WM_HEADLESS_SIZE` | `1920x1080` | Virtual output size `WIDTHxHEIGHT`. |
| `SHOESTRING_WM_RENDER_NODE` | auto | Render node to try; falls back to software if it can't be opened. |
| `SHOESTRING_WM_HEADLESS_SOFTWARE` | unset | `1` forces the llvmpipe software path even with a GPU present. |
| `SHOESTRING_REMOTE_BIND` | `0.0.0.0` | Listener bind address (0.0.0.0 so `-p` reaches it). |
| `SHOESTRING_REMOTE_PORT` | `7355` | Listener port. |
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
