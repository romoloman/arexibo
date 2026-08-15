#!/bin/bash
# Build arexibo DEB package (headless kiosk deployment, no desktop
# integration -- adapted from a build script shared on GitHub, originally
# aimed at a desktop-user install with a .desktop entry/icons/user-level
# systemd unit; this variant targets a standalone X-server kiosk totem
# instead, matching the README's own "Standalone setup with X server"
# section).
# Usage: ./deb/build-deb.sh <version> [release]
set -euo pipefail

VERSION="${1:?Usage: $0 <version> [release]}"

# Parse version-release (e.g. 0.6.0-2 -> version=0.6.0, release=2)
BASE_VERSION="${VERSION%%-*}"
if [[ -n "${2:-}" ]]; then
  RELEASE="$2"
elif [[ "$VERSION" == *-* ]]; then
  RELEASE="${VERSION#*-}"
else
  RELEASE="1"
fi

# Detect architecture
ARCH=$(dpkg --print-architecture)

echo "Building arexibo ${BASE_VERSION}-${RELEASE} for ${ARCH} (kiosk deployment)"

# Build Rust binary
export CARGO_NET_GIT_FETCH_WITH_CLI=true
cargo build --release

# Create DEB package structure
PKG_DIR="deb-pkg/arexibo"
rm -rf deb-pkg
mkdir -p "${PKG_DIR}/DEBIAN"
mkdir -p "${PKG_DIR}/usr/bin"
mkdir -p "${PKG_DIR}/usr/share/doc/arexibo"
mkdir -p "${PKG_DIR}/usr/lib/systemd/system"

# Install files -- no .desktop entry or icons: this package targets a
# headless kiosk totem with no desktop environment at all, not a regular
# desktop application launched from a menu.
install -m755 target/release/arexibo "${PKG_DIR}/usr/bin/arexibo"
# Strip debug symbols -- meaningfully smaller binary, standard practice
# for a release package (lintian flags an unstripped binary otherwise).
strip --strip-unneeded "${PKG_DIR}/usr/bin/arexibo"
install -m644 arexibo.service "${PKG_DIR}/usr/lib/systemd/system/arexibo.service"
# The actual xinit "client" (see arexibo.service's own comments for why
# this exists instead of just invoking arexibo directly, and why a
# plain ~/.xinitrc doesn't work here).
mkdir -p "${PKG_DIR}/etc/arexibo"
install -m755 deb/arexibo-kiosk-start "${PKG_DIR}/usr/bin/arexibo-kiosk-start"
install -m644 deb/kiosk.conf "${PKG_DIR}/etc/arexibo/kiosk.conf"

# Mark kiosk.conf as a conffile -- dpkg will preserve local edits on
# upgrade (prompting instead of silently overwriting) rather than
# clobbering per-deployment hardware settings.
echo "/etc/arexibo/kiosk.conf" > "${PKG_DIR}/DEBIAN/conffiles"
install -m644 LICENSE "${PKG_DIR}/usr/share/doc/arexibo/"
install -m644 README.md "${PKG_DIR}/usr/share/doc/arexibo/"
install -m644 CHANGELOG.md "${PKG_DIR}/usr/share/doc/arexibo/"
# Minimal, properly-formatted Debian changelog (distinct from the
# project's own CHANGELOG.md above, kept as project documentation) --
# lintian expects this specific structured format, which a renamed
# markdown file doesn't satisfy.
cat > "${PKG_DIR}/usr/share/doc/arexibo/changelog.Debian" << EOF
arexibo (${BASE_VERSION}-${RELEASE}) unstable; urgency=medium

  * See /usr/share/doc/arexibo/CHANGELOG.md for the full upstream
    changelog.

 -- Pau Aliagas <pau@linuxnow.com>  $(date -R)
EOF
gzip -9 -n "${PKG_DIR}/usr/share/doc/arexibo/changelog.Debian"

# Debian-format copyright file (distinct from the upstream LICENSE file
# already installed above -- lintian expects this specific format/name).
cat > "${PKG_DIR}/usr/share/doc/arexibo/copyright" << 'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: arexibo
Source: https://github.com/birkenfeld/arexibo

Files: *
Copyright: 2026 Georg Brandl and contributors
License: AGPL-3.0

License: AGPL-3.0
 See /usr/share/doc/arexibo/LICENSE for the full license text
 (GNU Affero General Public License, version 3).
EOF

# Detect the binary's real shared-library dependencies automatically
# (dpkg-shlibdeps) instead of a hand-curated list -- catches everything
# actually needed (e.g. libc6, which a manual list had missed) rather
# than only what was remembered/guessed at write time.
mkdir -p "${PKG_DIR}/debian"
touch "${PKG_DIR}/debian/control"
(cd "${PKG_DIR}" && dpkg-shlibdeps -O usr/bin/arexibo 2>/dev/null \
    | sed 's/^shlibs:Depends=//') > /tmp/arexibo-shlibdeps.txt
SHLIBS_DEPENDS=$(cat /tmp/arexibo-shlibdeps.txt)
rm -rf "${PKG_DIR}/debian" /tmp/arexibo-shlibdeps.txt

# Create control file
cat > "${PKG_DIR}/DEBIAN/control" << EOF
Package: arexibo
Version: ${BASE_VERSION}-${RELEASE}
Section: misc
Priority: optional
Architecture: ${ARCH}
Depends: ${SHLIBS_DEPENDS}, xinit, adduser, x11-xserver-utils, xinput, pulseaudio-utils, pulseaudio, dbus-daemon
Maintainer: Pau Aliagas <pau@linuxnow.com>
Description: Rust-based digital signage player for Xibo CMS (kiosk deployment)
 Arexibo is a Rust-based digital signage player compatible with Xibo CMS.
 It provides a lightweight alternative to the official Xibo player,
 designed for kiosk and digital signage deployments on Linux.
 .
 This package installs arexibo as a standalone X-server kiosk service
 (via xinit, no desktop environment required) -- see
 /usr/share/doc/arexibo/README.md's "Standalone setup with X server"
 section, and the post-install message, for the one-time manual display
 registration step needed before starting the service.
Homepage: https://github.com/birkenfeld/arexibo
EOF

# Postinst: create the dedicated system user + persistent envdir the
# systemd unit expects (/var/lib/arexibo), matching common Debian
# packaging convention for a service's own state directory (FHS ->
# /var/lib/<package>). Does NOT enable or start the service -- it can't
# work yet until the display has been registered once manually (see the
# comment inside arexibo.service itself, and the message below).
cat > "${PKG_DIR}/DEBIAN/postinst" << 'EOF'
#!/bin/sh
set -e

if ! getent passwd arexibo >/dev/null; then
    adduser --system --group --home /var/lib/arexibo \
        --shell /usr/sbin/nologin arexibo
fi
mkdir -p /var/lib/arexibo
chown arexibo:arexibo /var/lib/arexibo

echo ""
echo "arexibo installed. Before starting the systemd service, register"
echo "this display with your CMS once, manually, as the arexibo user:"
echo ""
echo "  sudo -u arexibo arexibo --host <CMS URL> --key <CMS key> /var/lib/arexibo"
echo ""
echo "(--display-id is optional -- if omitted, arexibo derives a stable"
echo "id automatically from /etc/machine-id; only pass it explicitly if"
echo "you need to match an existing display record with a specific"
echo "hardware key already assigned in the CMS)"
echo ""
echo "Then enable and start the service:"
echo ""
echo "  systemctl enable --now arexibo.service"
echo ""

exit 0
EOF
chmod 755 "${PKG_DIR}/DEBIAN/postinst"

# Build DEB
mkdir -p dist
dpkg-deb --build "${PKG_DIR}" "dist/arexibo_${BASE_VERSION}-${RELEASE}_${ARCH}.deb"

echo "Built DEBs:"
ls -lh dist/*.deb

# Clean up
rm -rf deb-pkg
