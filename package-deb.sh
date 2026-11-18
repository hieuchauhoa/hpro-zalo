#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$ROOT_DIR/target/deb"
PKG_NAME="hpro-zalo"
PKG_VER="0.2.0"
PKG_REL="1"
ARCH="amd64"
DEB_DIR="$DIST_DIR/${PKG_NAME}_${PKG_VER}-${PKG_REL}_${ARCH}"

rm -rf "$DEB_DIR"
mkdir -p "$DEB_DIR/DEBIAN"
mkdir -p "$DEB_DIR/usr/bin"
mkdir -p "$DEB_DIR/usr/share/applications"
mkdir -p "$DEB_DIR/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$DEB_DIR/usr/share/icons/hicolor/256x256/apps"
mkdir -p "$DEB_DIR/usr/share/doc/hpro-zalo"

# Binaries
if [ -f "$ROOT_DIR/target/release/hpro-zalo" ]; then
    install -Dm755 "$ROOT_DIR/target/release/hpro-zalo" "$DEB_DIR/usr/bin/hpro-zalo"
    ln -sf hpro-zalo "$DEB_DIR/usr/bin/olaz"
fi
if [ -f "$ROOT_DIR/target/release/hpro-zalo-web" ]; then
    install -Dm755 "$ROOT_DIR/target/release/hpro-zalo-web" "$DEB_DIR/usr/bin/hpro-zalo-web"
fi

# Desktop & Icons
install -Dm644 "$ROOT_DIR/pkgbuild/hpro-zalo.desktop" "$DEB_DIR/usr/share/applications/hpro-zalo.desktop"
install -Dm644 "$ROOT_DIR/pkgbuild/hpro-zalo.svg" "$DEB_DIR/usr/share/icons/hicolor/scalable/apps/hpro-zalo.svg"

# Render 256x256 PNG icon
if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w 256 -h 256 "$ROOT_DIR/pkgbuild/hpro-zalo.svg" -o "$DEB_DIR/usr/share/icons/hicolor/256x256/apps/hpro-zalo.png"
elif [ -f "/mnt/data/Package Center/spk/apps/hprozalo/PACKAGE_ICON_256.PNG" ]; then
    cp "/mnt/data/Package Center/spk/apps/hprozalo/PACKAGE_ICON_256.PNG" "$DEB_DIR/usr/share/icons/hicolor/256x256/apps/hpro-zalo.png"
fi

# License & Docs
if [ -f "$ROOT_DIR/LICENSE" ]; then
    install -Dm644 "$ROOT_DIR/LICENSE" "$DEB_DIR/usr/share/doc/hpro-zalo/copyright"
fi

# Control file
cat > "$DEB_DIR/DEBIAN/control" <<EOF
Package: hpro-zalo
Version: ${PKG_VER}-${PKG_REL}
Section: net
Priority: optional
Architecture: ${ARCH}
Maintainer: HPro <noreply@users.noreply.github.com>
Installed-Size: $(du -ks "$DEB_DIR/usr" | cut -f1)
Depends: libc6, libwebkit2gtk-4.1-0 | libwebkit2gtk-4.0-37, libgtk-3-0, libglib2.0-0, libsoup-3.0-0 | libsoup2.4-1, libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, ffmpeg, ca-certificates
Provides: olaz, hprozalo
Conflicts: olaz
Replaces: olaz
Homepage: https://github.com/hieuchauhoa/hpro-zalo
Description: HPro Zalo — Native Linux client for Zalo (Zero-Persistence & VoIP)
 Native Linux desktop client and zero-persistence web relay for Zalo.
 Features: Multi-Account isolation, Anti-Recall message vault,
 client-side AES-256-GCM Vault PIN lock, SOCKS5 proxy, VoIP calls,
 and zero server storage footprint.
EOF

# Postinst & Postrm
cat > "$DEB_DIR/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
exit 0
EOF
chmod 755 "$DEB_DIR/DEBIAN/postinst"

cat > "$DEB_DIR/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
    fi
fi
exit 0
EOF
chmod 755 "$DEB_DIR/DEBIAN/postrm"

# Build deb
dpkg-deb --build --root-owner-group "$DEB_DIR" "$DIST_DIR/${PKG_NAME}_${PKG_VER}-${PKG_REL}_${ARCH}.deb"
cp -f "$DIST_DIR/${PKG_NAME}_${PKG_VER}-${PKG_REL}_${ARCH}.deb" "$DIST_DIR/hprozalo_${PKG_VER}-${PKG_REL}_${ARCH}.deb"
cp -f "$DIST_DIR/${PKG_NAME}_${PKG_VER}-${PKG_REL}_${ARCH}.deb" "$DIST_DIR/hprozalo.deb"

echo "Built: $DIST_DIR/${PKG_NAME}_${PKG_VER}-${PKG_REL}_${ARCH}.deb"
