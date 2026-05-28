Install
=======

shoestring-wm is built from source with cargo. The runtime needs a small
set of native libraries (DRM, GBM, EGL, udev, libinput, libseat,
libdisplay-info, wayland, xkbcommon, plus PAM for ``shoestring-lock``).
Per-distro install commands for the development headers are below; on
most distros the matching runtime packages are pulled in automatically.

The TTY backend (the daily-driver path) needs all of the libraries below.
If you only want the winit dev backend you can build with
``--no-default-features --features winit`` and skip the DRM/GBM/EGL/udev/
libinput/libseat/libdisplay-info dev packages; you still need wayland and
xkbcommon.

You also need:

- Rust 1.76+ (stable). ``rustup`` is the recommended installer.
- A working DRM/KMS-capable GPU driver if you want to run on a TTY
  (open-source ``amdgpu`` / ``i915`` / ``nouveau`` work out of the box;
  proprietary nvidia is untested and not a v1 target).

Build
-----

::

    git clone https://github.com/szarta/shoestring-wm
    cd shoestring-wm
    # --workspace builds the WM plus every helper and sibling
    # (bar, menu, notify, lock, ctl, screenshot, region, ...).
    cargo build --release --workspace
    # Or, winit-only dev build of just the WM (skip the DRM stack):
    cargo build --release --no-default-features --features winit -p shoestring-wm

The compiled binaries land under ``target/release/``. Drop the ones
you want on ``$PATH``:

::

    install -Dm755 -t ~/.local/bin/ \
        target/release/shoestring-{wm,bar,menu,notify,ctl,lock,screenshot,region,kill,confirm}
    shoestring-wm --write-default-config

Distro packages
---------------

Debian / Ubuntu
~~~~~~~~~~~~~~~

::

    sudo apt install build-essential pkg-config \
        libwayland-dev libxkbcommon-dev \
        libdrm-dev libgbm-dev libegl-dev \
        libudev-dev libinput-dev \
        libseat-dev libdisplay-info-dev \
        libpam0g-dev

If your distro's ``libdisplay-info-dev`` is older than 0.1 you may need
to pull it from backports (Debian) or build it from upstream.

Fedora / RHEL
~~~~~~~~~~~~~

::

    sudo dnf install gcc pkgconf-pkg-config \
        wayland-devel libxkbcommon-devel \
        libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
        systemd-devel libinput-devel \
        libseat-devel libdisplay-info-devel \
        pam-devel

(``systemd-devel`` provides ``libudev.pc`` on Fedora.)

Arch / Manjaro
~~~~~~~~~~~~~~

::

    sudo pacman -S --needed base-devel pkgconf \
        wayland libxkbcommon \
        libdrm libglvnd mesa \
        systemd libinput \
        seatd libdisplay-info \
        pam

Alpine
~~~~~~

::

    sudo apk add build-base pkgconfig \
        wayland-dev libxkbcommon-dev \
        libdrm-dev mesa-dev \
        eudev-dev libinput-dev \
        seatd-dev libdisplay-info-dev \
        linux-pam-dev

FreeBSD
~~~~~~~

::

    pkg install rust pkgconf \
        wayland libxkbcommon \
        drm-kmod mesa-libs \
        libinput seatd libdisplay-info

(PAM is provided by the base system.)

DRM-KMS support on FreeBSD depends on the ``drm-kmod`` port matching
your kernel; verify ``/dev/dri/card0`` exists after a reboot. The
``udev`` userland shim is provided automatically when ``libinput`` is
installed.

NixOS
~~~~~

A development shell that brings in everything needed:

.. code-block:: nix

    # shell.nix
    { pkgs ? import <nixpkgs> {} }:
    pkgs.mkShell {
      nativeBuildInputs = with pkgs; [ pkg-config rustc cargo ];
      buildInputs = with pkgs; [
        wayland libxkbcommon
        libdrm mesa libGL
        udev libinput
        seatd libdisplay-info
        pam
      ];
    }

The packaging story for shoestring-wm itself (a Nix derivation under
``nixpkgs``) is not yet upstream.

After installing
----------------

Continue with :doc:`running` to launch the compositor (winit nested in an
existing session, or directly from a TTY), then :doc:`configuration` to
edit keybindings, focus mode, and output scale.
