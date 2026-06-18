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
   does not allow. On distros without a prebuilt package, build from source
   as below. FreeBSD additionally carries an in-repo ports skeleton
   (``packaging/freebsd``) that builds a real ``pkg`` locally — though it is
   not yet in the official ports tree, so ``pkg install shoestring-wm`` does
   not resolve it. See `FreeBSD`_.

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

For desktop-portal integration (file dialogs, screen sharing), also drop
in the portal backend-selection config so the portal frontend knows which
backend to use for our session::

    sudo install -Dm644 resources/shoestring-wm-portals.conf \
        /usr/share/xdg-desktop-portal/shoestring-wm-portals.conf

See :doc:`portals` for the prerequisites (``xdg-desktop-portal``,
PipeWire) and the native ScreenCast + Screenshot backend, plus the
descriptor/``.service`` files the packages install.

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
        libpam0g-dev \
        libpipewire-0.3-dev libclang-dev

If your distro's ``libdisplay-info-dev`` is older than 0.1 you may need
to pull it from backports (Debian) or build it from upstream.

The last line (``libpipewire-0.3-dev`` — which carries the ``libspa-0.2``
headers — and ``libclang-dev``) is the build dependency of the native
screencast portal, whose ``pipewire``/``libspa`` ``-sys`` crates generate
bindings with ``bindgen``. ``cargo build --workspace`` always builds the
portal, so these are required for a workspace build.

Fedora / RHEL
~~~~~~~~~~~~~

::

    sudo dnf install gcc pkgconf-pkg-config \
        wayland-devel libxkbcommon-devel \
        libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
        systemd-devel libinput-devel \
        libseat-devel libdisplay-info-devel \
        pam-devel \
        pipewire-devel clang

(``systemd-devel`` provides ``libudev.pc`` on Fedora; ``pipewire-devel`` +
``clang`` build the screencast portal's bindgen bindings.)

Arch / Manjaro
~~~~~~~~~~~~~~

::

    sudo pacman -S --needed base-devel pkgconf \
        wayland libxkbcommon \
        libdrm libglvnd mesa \
        systemd libinput \
        seatd libdisplay-info \
        pam \
        pipewire clang

Alpine
~~~~~~

::

    sudo apk add build-base pkgconfig \
        wayland-dev libxkbcommon-dev \
        libdrm-dev mesa-dev \
        eudev-dev libinput-dev \
        seatd-dev libdisplay-info-dev \
        linux-pam-dev \
        pipewire-dev clang-dev

FreeBSD
~~~~~~~

**The FreeBSD port.** The repository carries a ``USES=cargo`` ports
skeleton at ``packaging/freebsd`` — the FreeBSD analogue of the ``.deb`` /
``.rpm``. It is **not yet in the official FreeBSD ports tree** (a planned
post-1.0 step), so ``pkg install shoestring-wm`` will not find it. To build
and install it yourself, drop the skeleton into a ports tree and use the
normal ports workflow:

.. code-block:: console

    # with a FreeBSD ports tree checked out at /usr/ports
    # cp -R packaging/freebsd /usr/ports/x11-wm/shoestring-wm
    # cd /usr/ports/x11-wm/shoestring-wm
    # make install        # or `make package` to produce a .pkg

The port pulls in every dependency (including the screen locker's
``unix-selfauth-helper``) and installs the binaries, man pages, the Wayland
session file, and the locker's PAM policy under ``/usr/local`` — so
shoestring-wm appears in your display manager's session menu with no manual
wiring, and the locker steps further down are handled for you. See
``packaging/freebsd/README.md`` for how the dependency list and checksums
are regenerated.

To **build from source** instead, install the toolchain and libraries::

    pkg install rust pkgconf \
        wayland libxkbcommon \
        drm-kmod mesa-libs \
        libinput seatd libdisplay-info \
        pipewire

(``pipewire`` provides ``libpipewire-0.3``/``libspa-0.2`` for the screencast
portal; ``clang``/``libclang`` for its bindgen build come from the base
system. Omit it only if you ``--exclude`` the portal crate from the build.)

For just the winit dev backend (which is the verified-working
configuration on FreeBSD), only the first line is needed::

    pkg install rust pkgconf wayland libxkbcommon

**Linker path.** ``pkg`` installs libraries under ``/usr/local/lib``,
which the base ``ld`` does not search by default, so the link step fails
with ``unable to find library -lxkbcommon``. Point the linker at it via
``RUSTFLAGS`` (or a ``.cargo/config.toml``)::

    RUSTFLAGS="-L/usr/local/lib" \
        cargo build --release --no-default-features --features winit -p shoestring-wm

    # …or persist it for the checkout:
    mkdir -p .cargo
    printf '[build]\nrustflags = ["-L", "/usr/local/lib"]\n' > .cargo/config.toml

DRM-KMS support on FreeBSD depends on the ``drm-kmod`` port matching
your kernel; verify ``/dev/dri/card0`` exists after a reboot. The
``udev`` userland shim is provided automatically when ``libinput`` is
installed.

**The screen locker on FreeBSD.** ``shoestring-lock`` builds and runs on
FreeBSD (via a vendored, OpenPAM-portable ``pam-client2`` under
``third_party/``). It authenticates through a dedicated ``shoestring-lock``
PAM policy that, on FreeBSD, delegates to the setuid
``unix-selfauth-helper`` — so the locker binary itself stays unprivileged.
A **from-source** install (the port does this for you) must therefore
install both:

.. code-block:: console

    # pkg install unix-selfauth-helper
    # install -m 644 resources/pam/shoestring-lock.freebsd \
        /usr/local/etc/pam.d/shoestring-lock

Without that policy file PAM denies every unlock (fails closed). Do **not**
point the locker at the system ``login`` service on FreeBSD: it begins with
``auth sufficient pam_self.so``, which unlocks for the calling user with no
password.

**Known gaps on FreeBSD.** The fd-leak and RSS metrics gauges read
``/proc`` and are silently omitted when no procfs is mounted (the default);
leak *detection* is therefore Linux-only. This does not affect the core
compositor or the winit dev backend.

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

Optional: wl-clipboard
~~~~~~~~~~~~~~~~~~~~~~

``wl-clipboard`` provides the ``wl-copy`` binary used by
``shoestring-screenshot --clipboard`` to copy a capture directly to the
Wayland clipboard. Only needed if you use that flag.

============== ================
Distro         Package
============== ================
Debian/Ubuntu  ``wl-clipboard``
Fedora/RHEL    ``wl-clipboard``
Arch/Manjaro   ``wl-clipboard``
Alpine         ``wl-clipboard``
FreeBSD        ``wl-clipboard`` (port: ``x11/wl-clipboard``)
NixOS          ``wl-clipboard``
============== ================

After installing
----------------

Continue with :doc:`running` to launch the compositor (winit nested in an
existing session, or directly from a TTY), then :doc:`configuration` to
edit keybindings, focus mode, and output scale.
