# syntax=docker/dockerfile:1
#
# shoestring-wm — headless serve-mode compositor in a container.
#
# A disposable, reproducible, IPC-observable, remote-viewable desktop: the
# headless backend (no seat/master/input) plus shoestring-remote-server, behind
# the runtime remote gate. Build once, `podman run` / `docker run` to get a
# sandboxed desktop you view from a real machine with shoestring-remote-client.
#
# A GPU is OPTIONAL. Pass a render node (`--device /dev/dri/renderD128`) for the
# hardware GBM path; with no node the headless backend renders on the CPU with
# pixman (real frames, no EGL needed). See docs/containers.rst.
#
#   podman build -t shoestring-wm-headless -f packaging/docker/Dockerfile .
#   mkdir -m 700 -p "$XDG_RUNTIME_DIR/sandbox"
#   podman run --rm --userns=keep-id:uid=1000,gid=1000 \
#       -v "$XDG_RUNTIME_DIR/sandbox:/run/shoestring/remote:Z" \
#       shoestring-wm-headless                    # add --device ... for a GPU
#   shoestring-remote-client --connect "$XDG_RUNTIME_DIR/sandbox/shoestring-remote.sock"

# ---- build stage -------------------------------------------------------------
FROM docker.io/library/rust:1-bookworm AS build

# Build deps for the *headless* WM only: wayland + xkbcommon + drm/gbm/egl +
# pixman (the default renderer without a GPU). We
# deliberately build no winit/tty backend, so the udev / libinput / libseat /
# pipewire / pam dev packages the full workspace needs are absent here.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libwayland-dev libxkbcommon-dev \
        libdrm-dev libgbm-dev libegl-dev libpixman-1-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Four binaries: the headless WM (no default features → no winit/tty pull-ins),
# the remote-desktop bridge, the IPC CLI the entrypoint drives, and the
# screenshot helper (so `shoestring-ctl screenshot` works inside the container —
# a self-observable sandbox). BuildKit cache mounts keep the cargo registry +
# target dir warm across rebuilds; copy the artefacts out of the (non-layer)
# cache mount in the same RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p shoestring-wm --no-default-features --features headless \
 && cargo build --release --locked \
        -p shoestring-remote-server -p shoestring-ctl -p shoestring-screenshot \
 && mkdir -p /out \
 && cp target/release/shoestring-wm \
       target/release/shoestring-remote-server \
       target/release/shoestring-ctl \
       target/release/shoestring-screenshot /out/

# ---- runtime stage -----------------------------------------------------------
FROM docker.io/library/debian:bookworm-slim AS runtime

# Runtime libs + Mesa. With no GPU passthrough the headless backend renders
# with pixman (libpixman-1-0), a CPU compositor that needs no EGL. With
# --device /dev/dri/renderD128 it takes the hardware path through Mesa instead,
# and the Mesa software stack (libgl1-mesa-dri ships llvmpipe; libegl-mesa0 is
# the EGL driver) serves `--renderer llvmpipe` — the image is GPU-optional.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libwayland-server0 libxkbcommon0 \
        libdrm2 libgbm1 libegl1 libgles2 libglapi-mesa libegl-mesa0 \
        libgl1-mesa-dri libpixman-1-0 \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/shoestring-wm \
                  /out/shoestring-remote-server \
                  /out/shoestring-ctl \
                  /out/shoestring-screenshot \
                  /usr/local/bin/
COPY packaging/docker/config.toml /etc/shoestring-wm/config.toml
COPY packaging/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Unprivileged user. A short, user-owned XDG_RUNTIME_DIR keeps the wayland +
# IPC socket paths under the unix-socket length limit (~108 bytes). Render-node
# access (when a GPU is passed through) needs the container user in the host's
# 'render' group — use `--group-add keep-groups` (podman) or `--group-add <gid>`.
#
# The remote server listens on a Unix socket in /run/shoestring/remote, a
# directory of its own so a host bind mount exposes only that socket, not the
# WM's IPC socket. It accepts only the container user's uid (1000), so map your
# host user to it: `--userns=keep-id:uid=1000,gid=1000` (podman).
RUN useradd -m -u 1000 -U shoestring \
 && mkdir -p /run/shoestring/remote \
 && chown -R shoestring:shoestring /run/shoestring \
 && chmod 700 /run/shoestring /run/shoestring/remote
USER shoestring

ENV XDG_RUNTIME_DIR=/run/shoestring \
    SHOESTRING_WM_HEADLESS_SIZE=1920x1080 \
    SHOESTRING_REMOTE_SOCKET=/run/shoestring/remote/shoestring-remote.sock
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
