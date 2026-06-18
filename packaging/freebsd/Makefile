PORTNAME=	shoestring-wm
DISTVERSIONPREFIX=	v
DISTVERSION=	0.3.0
CATEGORIES=	x11-wm wayland

MAINTAINER=	barrendo@gmail.com
COMMENT=	Lightweight Rust Wayland window manager built on Smithay
WWW=		https://github.com/szarta/shoestring-wm

LICENSE=	MIT
LICENSE_FILE=	${WRKSRC}/LICENSE

# sphinx-build turns the reStructuredText under docs/ into the man pages
# (they are generated, not shipped in the source tarball).
BUILD_DEPENDS=	sphinx-build:textproc/py-sphinx@${PY_FLAVOR}

# The compositor's native libraries. This list is identical to
# x11-wm/cosmic-comp's (the other Smithay compositor in the tree); both
# drive DRM/KMS through the same userland stack on FreeBSD.
LIB_DEPENDS=	libudev.so:devel/libudev-devd \
		libdisplay-info.so:sysutils/libdisplay-info \
		libseat.so:sysutils/seatd \
		libinput.so:x11/libinput \
		libxkbcommon.so:x11/libxkbcommon
# mesa-dri: the GLES driver the TTY backend renders through at runtime.
# unix-selfauth-helper: the setuid helper shoestring-lock's PAM policy
# delegates to so the locker can authenticate unprivileged (see the
# Authentication section of shoestring-lock(1)).
RUN_DEPENDS=	mesa-dri>0:graphics/mesa-dri \
		unix-selfauth-helper>0:security/unix-selfauth-helper

USES=		cargo gl localbase:ldflags pkgconfig python:build
USE_GITHUB=	yes
GH_ACCOUNT=	szarta
USE_GL=		egl gbm

# Build all ten workspace binaries: the compositor (root crate) plus the
# nine helper crates. The remaining members (config, ipc) are libraries
# and produce no binary, so they are not listed here.
CARGO_INSTALL_PATH=	. \
			crates/shoestring-bar \
			crates/shoestring-confirm \
			crates/shoestring-ctl \
			crates/shoestring-kill \
			crates/shoestring-lock \
			crates/shoestring-menu \
			crates/shoestring-notify \
			crates/shoestring-region \
			crates/shoestring-screenshot

OPTIONS_DEFINE=		DOCS XWAYLAND SCREENCAST
OPTIONS_DEFAULT=	XWAYLAND SCREENCAST
OPTIONS_SUB=		yes
XWAYLAND_DESC=		Run X11 applications via Xwayland
XWAYLAND_RUN_DEPENDS=	Xwayland:x11-servers/xwayland

# Screen sharing: builds xdg-desktop-portal-shoestring (our ScreenCast +
# Screenshot portal backend) and pulls in the PipeWire stack + portal frontend.
# Both portal interfaces are served natively, so xdg-desktop-portal-wlr is not
# required. Disable it for a leaner dependency set with no screen-capture support.
SCREENCAST_DESC=	Screen sharing + screenshots via xdg-desktop-portal + PipeWire
SCREENCAST_LIB_DEPENDS=	libpipewire-0.3.so:multimedia/pipewire
SCREENCAST_RUN_DEPENDS=	xdg-desktop-portal>0:deskutils/xdg-desktop-portal \
			wireplumber>0:multimedia/wireplumber
# Append the PipeWire-linked crates to what cargo installs: the portal backend
# (lands in bin; moved to libexec in post-install-SCREENCAST-on) and
# shoestring-mediad, the media-privacy monitor that feeds the bar's MUTE/MIC/CAM
# indicators (stays in bin; the WM autostarts it by bare name). Both link
# pipewire, so they're gated behind this option; pipewire dev headers come with
# the LIB_DEPENDS above and clang/bindgen is in the FreeBSD base. wireplumber
# (run-dep) supplies `wpctl`, which mediad prefers for authoritative mute control.
SCREENCAST_VARS=	CARGO_INSTALL_PATH+=crates/xdg-desktop-portal-shoestring \
			CARGO_INSTALL_PATH+=crates/shoestring-mediad

PORTDOCS=	README.md

SPHINXBUILD=	${LOCALBASE}/bin/sphinx-build

# The login-session wrapper is otherwise-POSIX shell, but ships with a
# bash shebang and one bash-ism (`set -o pipefail`). Rewrite both so the
# installed copy runs on a stock FreeBSD with no bash. The systemd block
# inside already self-guards via `command -v systemctl`, so it no-ops here.
post-patch:
	@${REINPLACE_CMD} -e '1s,.*,#!/bin/sh,' \
		-e 's,set -euo pipefail,set -eu,' \
		${WRKSRC}/resources/shoestring-wm-session

# The shipped D-Bus activation file hard-codes the Linux /usr/libexec path;
# rewrite it to this prefix's libexec.
post-patch-SCREENCAST-on:
	@${REINPLACE_CMD} -e 's,/usr/libexec/,${PREFIX}/libexec/,' \
		${WRKSRC}/resources/org.freedesktop.impl.portal.desktop.shoestring.service

# Generate the man pages the same way the deb/rpm release CI does.
post-build:
	@cd ${WRKSRC}/docs && ${SPHINXBUILD} -b man . _build/man

post-install:
	${INSTALL_SCRIPT} ${WRKSRC}/resources/shoestring-wm-session \
		${STAGEDIR}${PREFIX}/bin/shoestring-wm-session
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/wayland-sessions
	${INSTALL_DATA} ${WRKSRC}/resources/shoestring-wm.desktop \
		${STAGEDIR}${PREFIX}/share/wayland-sessions/
# Dedicated PAM policy for the locker, installed as a .sample: pkg copies
# it to the live name on first install and removes it on deinstall if
# unedited, so a user's local policy survives upgrades. Without it
# shoestring-lock fails closed (PAM `other` = deny).
	@${MKDIR} ${STAGEDIR}${PREFIX}/etc/pam.d
	${INSTALL_DATA} ${WRKSRC}/resources/pam/shoestring-lock.freebsd \
		${STAGEDIR}${PREFIX}/etc/pam.d/shoestring-lock.sample
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/man/man1 \
		${STAGEDIR}${PREFIX}/share/man/man5
	${INSTALL_MAN} ${WRKSRC}/docs/_build/man/*.1 \
		${STAGEDIR}${PREFIX}/share/man/man1/
	${INSTALL_MAN} ${WRKSRC}/docs/_build/man/*.5 \
		${STAGEDIR}${PREFIX}/share/man/man5/

post-install-DOCS-on:
	@${MKDIR} ${STAGEDIR}${DOCSDIR}
	${INSTALL_DATA} ${WRKSRC}/README.md ${STAGEDIR}${DOCSDIR}/

# Screencast portal backend. cargo installed the helper into bin/; relocate it
# to libexec (it's a backend, not a user command). The *.portal descriptor MUST
# live in a system data dir — xdg-desktop-portal scans XDG_DATA_DIRS, never a
# per-user dir — so it goes under ${PREFIX}/share. The .service is D-Bus
# activation; the *-portals.conf routes the ScreenCast interface to us.
post-install-SCREENCAST-on:
	@${MKDIR} ${STAGEDIR}${PREFIX}/libexec
	@${MV} ${STAGEDIR}${PREFIX}/bin/xdg-desktop-portal-shoestring \
		${STAGEDIR}${PREFIX}/libexec/xdg-desktop-portal-shoestring
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/xdg-desktop-portal/portals
	${INSTALL_DATA} ${WRKSRC}/resources/shoestring.portal \
		${STAGEDIR}${PREFIX}/share/xdg-desktop-portal/portals/
	${INSTALL_DATA} ${WRKSRC}/resources/shoestring-wm-portals.conf \
		${STAGEDIR}${PREFIX}/share/xdg-desktop-portal/
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/dbus-1/services
	${INSTALL_DATA} ${WRKSRC}/resources/org.freedesktop.impl.portal.desktop.shoestring.service \
		${STAGEDIR}${PREFIX}/share/dbus-1/services/

.include <bsd.port.mk>
