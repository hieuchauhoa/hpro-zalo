#!/usr/bin/env bash
# Build HPro Zalo from audited source and make a portable bundle (no prebuilt packages, no auto-update).
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$ROOT"

cargo build --release --locked -p olaz-app

OUT="$ROOT/dist/hpro-zalo-portable"
rm -rf "$OUT"
mkdir -p "$OUT"
install -m755 target/release/hpro-zalo "$OUT/hpro-zalo"
install -m644 pkgbuild/hpro-zalo.desktop "$OUT/hpro-zalo.desktop"
install -m644 pkgbuild/hpro-zalo.svg "$OUT/hpro-zalo.svg"

cat > "$OUT/install-user.sh" <<'EOF'
#!/usr/bin/env bash
# Installs into ~/.local (no root).
set -euo pipefail
D="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
P="${PREFIX:-$HOME/.local}"
install -Dm755 "$D/hpro-zalo" "$P/bin/hpro-zalo"
install -Dm644 "$D/hpro-zalo.svg" "$P/share/icons/hicolor/scalable/apps/hpro-zalo.svg"
mkdir -p "$P/share/applications"
sed "s|^Exec=.*|Exec=$P/bin/hpro-zalo %u|" "$D/hpro-zalo.desktop" > "$P/share/applications/hpro-zalo.desktop"
update-desktop-database "$P/share/applications" 2>/dev/null || true
echo "Installed to $P. Run: $P/bin/hpro-zalo"
EOF
chmod +x "$OUT/install-user.sh"
(cd "$ROOT/dist" && tar -czf hpro-zalo-portable.tar.gz hpro-zalo-portable)
echo "Done: $ROOT/dist/hpro-zalo-portable.tar.gz"
