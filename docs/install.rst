Install
=======

On Debian/Ubuntu and Fedora, install a prebuilt package (see `Prebuilt
packages`_ below) — it pulls in the runtime libraries for you and wires
shoestring-wm into your login screen. Everywhere else, build from source.

When building from source the runtime needs a small set of native
libraries (DRM, GBM, EGL, udev, libinput, libseat, libdisplay-info,
wayland, xkbcommon, plus PAM for ``shoestring-lock``). Per-distro install
commands for the development headers are below; on most distros the
matching runtime packages are pulled in automatically.

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

Prebuilt packages
-----------------

Tagged releases ship a Debian/Ubuntu ``.deb`` and a Fedora ``.rpm`` on the
`GitHub releases page <https://github.com/szarta/shoestring-wm/releases>`_.
Each bundles every binary (WM plus helpers), the man pages, a session
wrapper, and the Wayland **session file** — so once installed,
shoestring-wm shows up in your display manager's session menu (GDM / SDDM /
LightDM) at the login screen. Pick it there and log in.

Debian / Ubuntu::

    sudo apt install ./shoestring-wm_<version>-1_amd64.deb

Fedora::

    sudo dnf install ./shoestring-wm-<version>-1.x86_64.rpm

The leading ``./`` matters: it tells the package manager to treat the file
as a local package and resolve the runtime library dependencies recorded
inside it. Then generate a starter config::

    shoestring-wm --write-default-config

.. note::

   shoestring-wm is **not** published to crates.io, so
   ``cargo install shoestring-wm`` will not work. Its smithay dependency
   tracks a git revision newer than any crates.io release, which crates.io
   does not allow. On distros without a prebuilt package — notably FreeBSD,
   which has no port yet — build from source as below.

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

To get a source build into your login screen's session menu, install the
binaries to a *system* path (the display manager launches sessions with a
minimal ``$PATH`` that won't include ``~/.local/bin``, and the WM spawns
its helpers by bare name), then add the session file and its wrapper::

    sudo install -Dm755 -t /usr/local/bin/ \
        target/release/shoestring-{wm,bar,menu,notify,ctl,lock,screenshot,region,kill,confirm}
    sudo install -Dm755 resources/shoestring-wm-session \
        /usr/local/bin/shoestring-wm-session
    sudo install -Dm644 resources/shoestring-wm.desktop \
        /usr/share/wayland-sessions/shoestring-wm.desktop

(The ``.deb`` / ``.rpm`` packages do all of this for you.)

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

Optional: XWayland
~~~~~~~~~~~~~~~~~~

XWayland is a **runtime** dependency, and only needed if you run X11 tools
(e.g. GIMP, Inkscape). It is not required to build. The WM spawns the
``Xwayland`` binary on demand, so just install your distro's package:

============== =============================
Distro         Package
============== =============================
Debian/Ubuntu  ``xwayland``
Fedora/RHEL    ``xorg-x11-server-Xwayland``
Arch/Manjaro   ``xorg-xwayland``
Alpine         ``xwayland``
FreeBSD        ``xwayland`` (port: ``x11-servers/xwayland``)
NixOS          ``xwayland``
============== =============================

After installing
----------------

Continue with :doc:`running` to launch the compositor (winit nested in an
existing session, or directly from a TTY), then :doc:`configuration` to
edit keybindings, focus mode, and output scale.
