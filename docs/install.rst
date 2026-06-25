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
shoestring-wm shows up in your login screen's session menu. Pick it there
and log in — **but only with a Wayland-capable greeter**; see
:ref:`greeter` below, because an X11-based greeter (notably LightDM's
default) will offer the session and then fail to launch it.

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

   Installing **binaries only** — whether via ``cargo install --git``,
   ``cargo install --path``, or copying ``target/release/`` somewhere — does
   **not** lay down the data files under ``resources/`` (the portal routing
   config and ``.portal`` descriptor, the session/``.desktop`` files, action
   scripts). Without the portal files in particular, screen sharing
   (Zoom/browsers) silently hangs. Either install the ``.deb`` / ``.rpm``
   (which place everything), or copy the files from the repo's ``resources/``
   directory by hand — see :doc:`portals` for the two screen-sharing files and
   their exact destinations.

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

A development shell that brings in everything needed for a full
``cargo build --workspace`` (compositor plus every helper, including the
screencast portal):

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
        pipewire           # screencast portal + media monitor (workspace build)
      ];
      # pipewire-sys / libspa-sys generate bindings with bindgen, which needs
      # libclang at build time. Drop both this and `pipewire` only if you build
      # the compositor alone (`cargo build -p shoestring-wm`).
      LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
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

The hardware power / sleep keys
-------------------------------

shoestring-wm can route the physical **power** and **sleep** keys through
its confirm dialog (the same one the bar's *Shut down* / *Sleep* menu rows
use) — but only if you stop the *session manager* from acting on them
first. The WM has no power policy of its own; it just reacts to the key
once nothing else does.

On a systemd system that is ``logind``: by default ``HandlePowerKey`` is
``poweroff`` and ``HandleSuspendKey`` is ``suspend``, so a single press
powers off / suspends **immediately, with no confirmation**, before the
WM ever sees the key. To hand those keys to the WM, edit
``/etc/systemd/logind.conf`` (or drop a file in
``/etc/systemd/logind.conf.d/``)::

    [Login]
    HandlePowerKey=ignore
    HandleSuspendKey=ignore

then ``systemctl restart systemd-logind`` (note: this ends the current
graphical session). Now bind the keys in your shoestring-wm config — the
keysym names are ``XF86PowerOff`` and ``XF86Sleep``::

    [[bindings]]
    key = "XF86PowerOff"
    action = "power-off"   # pops the confirm dialog, then shells out

    [[bindings]]
    key = "XF86Sleep"
    action = "suspend"

The ``power-off`` / ``reboot`` / ``suspend`` actions (and the matching bar
menu rows) shell out to the first available of ``systemctl`` →
``loginctl`` → ``shutdown(8)`` / ``zzz``, so they work on systemd,
elogind, and bare-init / FreeBSD alike. On FreeBSD the power key is wired
through ``devd`` / ``acpiconf`` rather than logind — disable the relevant
``/etc/devd.conf`` (or ``sysctl hw.acpi.power_button_state``) handler the
same way before binding the key here.

.. _greeter:

Logging in: pick a Wayland-capable greeter
------------------------------------------

shoestring-wm is a **Wayland session**, so the greeter that launches it must
hand the seat off to it: the compositor opens the GPU and input devices
through the seat manager (``logind`` on systemd Linux, ``seatd`` elsewhere),
and that only works if the greeter starts the session as the **seat-active**
session. A Wayland-native greeter does this; an **X11 greeter does not**.

The most common pitfall is **LightDM with its default GTK greeter**
(``lightdm-gtk-greeter``), which runs an X server. It *will* list
shoestring-wm in the session menu, but when you pick it the compositor comes
up in the right logind session yet is never told the seat is active, so it
can't take DRM master and you get a **black screen**. This is a LightDM
limitation, not specific to shoestring — the reference compositor
(``weston``) fails to launch under it too. (shoestring fails *recoverably*: a
denied seat leaves a black screen you can escape with ``Ctrl+Alt+F2``, not a
hung machine.)

**Use a Wayland-capable greeter instead.** Any of these hand off correctly:

- **greetd** — minimal and the best fit here: it sits directly on the same
  seat manager shoestring already uses (``logind`` on Linux, ``seatd`` on the
  BSDs), so it's consistent across platforms and pulls in almost nothing.
- **GDM** or **SDDM** (with its Wayland greeter) — heavier, but work out of
  the box.

greetd on Debian/Ubuntu
~~~~~~~~~~~~~~~~~~~~~~~~

Install greetd (its built-in ``agreety`` greeter needs no extra package),
point it at shoestring-wm, and switch the active display manager:

.. code-block:: console

    # sudo apt install greetd

Write ``/etc/greetd/config.toml``:

.. code-block:: toml

    [terminal]
    # Use a VT that has no getty. vt 1 collides with getty@tty1 on Debian/
    # Ubuntu (two readers on one tty = garbled keyboard); 7 is free (it's
    # where the old X display manager lived) and is the package default.
    vt = 7

    [default_session]
    # agreety prompts for login on the VT, then execs the session as a fresh
    # seat-active session — the clean handoff LightDM doesn't do. Point --cmd
    # at the *session wrapper* (shoestring-wm-session), NOT the bare
    # `shoestring-wm` binary: the wrapper exports XDG_CURRENT_DESKTOP and seeds
    # the systemd/D-Bus environment that desktop portals need. Launch the bare
    # compositor here and screenshots / screen sharing break — see the note below.
    command = "agreety --cmd shoestring-wm-session"
    # The greeter's own unprivileged user. On Debian/Ubuntu the greetd
    # package creates `_greetd`; some distros use `greeter`. Check with
    # `getent passwd _greetd greeter`.
    user = "_greetd"

Then make greetd the display manager and reboot:

.. code-block:: console

    # sudo systemctl disable lightdm.service
    # sudo systemctl enable greetd.service
    # sudo reboot

At greetd's ``login:`` prompt (agreety looks like a plain text login), log in
normally and shoestring launches. For a nicer login UI, install
``greetd-tuigreet`` and use ``command = "tuigreet --cmd shoestring-wm-session"``
instead.

.. important::

   **Point the greeter at the session wrapper, not the bare compositor.** A
   greeter's ``--cmd`` runs exactly what you give it, bypassing the
   ``shoestring-wm.desktop`` file a graphical DM (GDM/SDDM) would use — so it
   must name ``shoestring-wm-session``, never ``shoestring-wm``. The wrapper
   exports ``XDG_CURRENT_DESKTOP=shoestring-wm`` and seeds the systemd / D-Bus
   activation environment *before* the compositor starts. Skip it and
   ``xdg-desktop-portal`` cannot select the shoestring backend (see
   :doc:`portals`): the **Screenshot** and **ScreenCast** portal interfaces
   are then served by no backend, so screen sharing silently fails and X11
   apps under XWayland — GIMP's *File ▸ Create ▸ Screenshot*, for instance —
   fall back to grabbing the empty X11 root window and produce a **solid black
   image**. (Note this is a *different* black screen from the seat-handoff one
   above: that one is a black display at login; this one is a working desktop
   whose screenshots/recordings come out black.) To recover an
   already-running session without logging out::

       systemctl --user set-environment XDG_CURRENT_DESKTOP=shoestring-wm
       dbus-update-activation-environment XDG_CURRENT_DESKTOP=shoestring-wm
       systemctl --user restart xdg-desktop-portal.service

No greeter at all
~~~~~~~~~~~~~~~~~

On a single-user machine you can skip the greeter entirely: log in on a text
VT and launch the compositor directly (or from your shell profile / a getty
autologin). A bare TTY login is already a seat-active session, so the handoff
is never in question. See :doc:`running`.

After installing
----------------

Continue with :doc:`running` to launch the compositor (winit nested in an
existing session, or directly from a TTY), then :doc:`configuration` to
edit keybindings, focus mode, and output scale.
