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
BUILD_DEPENDS=	sphinx-build:textproc/py-sphinx

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

USES=		cargo gl localbase:ldflags pkgconfig
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

OPTIONS_DEFINE=		DOCS XWAYLAND
OPTIONS_DEFAULT=	XWAYLAND
XWAYLAND_DESC=		Run X11 applications via Xwayland
XWAYLAND_RUN_DEPENDS=	Xwayland:x11-servers/xwayland

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

.include <bsd.port.mk>
