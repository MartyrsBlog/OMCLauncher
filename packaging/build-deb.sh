#!/usr/bin/env bash
# 构建发布物：Debian 软件包 + 免安装二进制 tarball
#
#   ./packaging/build-deb.sh             # release 构建 + 打包
#   SKIP_BUILD=1 ./packaging/build-deb.sh # 复用已有 target/release/mcl-app
#
# 产物（target/package/）：
#   mclauncher_<version>_amd64.deb
#   mclauncher-<version>-linux-x86_64.tar.gz   （解压即用，含 README 与 desktop 文件）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
ARCH="${ARCH:-amd64}"
NAME="mclauncher"
OUT="$ROOT/target/package"
STAGE="$OUT/${NAME}_${VERSION}_${ARCH}"
PORTABLE="$OUT/${NAME}-${VERSION}-linux-x86_64"

info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    info "构建 release 二进制"
    cargo build --release -p mcl-app
fi

BIN="$ROOT/target/release/mcl-app"
[[ -x "$BIN" ]] || { echo "缺少 $BIN，先执行 cargo build --release -p mcl-app" >&2; exit 1; }

info "准备 .deb 目录结构（$VERSION / $ARCH）"
rm -rf "$STAGE" "$PORTABLE"
mkdir -p "$STAGE/DEBIAN" \
         "$STAGE/usr/bin" \
         "$STAGE/usr/share/applications" \
         "$STAGE/usr/share/icons/hicolor/256x256/apps" \
         "$STAGE/usr/share/doc/$NAME"

install -m 755 "$BIN" "$STAGE/usr/bin/$NAME"
install -m 644 packaging/mclauncher.desktop "$STAGE/usr/share/applications/$NAME.desktop"
install -m 644 packaging/mclauncher.png "$STAGE/usr/share/icons/hicolor/256x256/apps/$NAME.png"
install -m 644 packaging/copyright "$STAGE/usr/share/doc/$NAME/copyright"

SIZE_KB=$(du -sk "$STAGE/usr" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $NAME
Version: $VERSION
Section: games
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE_KB
Depends: libc6 (>= 2.31), libx11-6, libxkbcommon0, libfontconfig1, libgl1
Recommends: xdg-utils
Maintainer: blmay <blmay@localhost>
Homepage: https://github.com/HC-zhang/mc-launcher
Description: Native Minecraft launcher
 A lightweight native Minecraft launcher built with Rust and Slint.
 Manages instances, installs vanilla/Fabric/Quilt/Forge/NeoForge, and
 downloads game files with mirror failover. Java runtimes are downloaded
 on demand, so no system Java is required.
EOF

info "构建 deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT/${NAME}_${VERSION}_${ARCH}.deb" >/dev/null

info "制作免安装 tarball"
mkdir -p "$PORTABLE"
install -m 755 "$BIN" "$PORTABLE/$NAME"
# 便携标记：解压即用版把数据放在二进制同级 mclauncher-data/
: > "$PORTABLE/$NAME.portable"
install -m 644 packaging/mclauncher.desktop "$PORTABLE/$NAME.desktop"
install -m 644 packaging/mclauncher.png "$PORTABLE/$NAME.png"
install -m 644 packaging/README-portable.md "$PORTABLE/README.md"
tar -C "$OUT" -czf "$OUT/${NAME}-${VERSION}-linux-x86_64.tar.gz" "$(basename "$PORTABLE")"
rm -rf "$PORTABLE" "$STAGE"

info "完成"
ls -lh "$OUT"/*.deb "$OUT"/*.tar.gz
