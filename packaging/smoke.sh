#!/usr/bin/env bash
# 发布物冒烟测试：二进制能起来、窗口能出现、deb 内容与元数据正确
#
#   ./packaging/smoke.sh            # 先打包再冒烟
#   SKIP_BUILD=1 ./packaging/smoke.sh
#
# 无头环境（CI）用：xvfb-run -a ./packaging/smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
DEB="$ROOT/target/package/mclauncher_${VERSION}_amd64.deb"
TARBALL="$ROOT/target/package/mclauncher-${VERSION}-linux-x86_64.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

pass=0
fail=0
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; pass=$((pass+1)); }
bad()  { printf '\033[1;31m  ✗\033[0m %s\n' "$*"; fail=$((fail+1)); }
step() { printf '\n\033[1;34m[%s]\033[0m\n' "$*"; }

step "1/5 构建发布物"
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    ./packaging/build-deb.sh >/dev/null
else
    [[ -f "$DEB" ]] || { echo "缺少 $DEB"; exit 1; }
fi

step "2/5 deb 结构"
if dpkg-deb -c "$DEB" > "$TMP/deb.txt" 2>/dev/null; then
    ok "dpkg-deb 可解析"
else
    bad "dpkg-deb 解析失败"
fi
for path in ./usr/bin/mclauncher ./usr/share/applications/mclauncher.desktop \
            ./usr/share/icons/hicolor/256x256/apps/mclauncher.png; do
    if grep -q "$path" "$TMP/deb.txt"; then ok "包含 $path"; else bad "缺少 $path"; fi
done
pkg_name="$(dpkg-deb -f "$DEB" Package)"
if [[ "$pkg_name" == "mclauncher" ]]; then ok "Package=mclauncher"; else bad "Package 字段异常：$pkg_name"; fi
for field in Version Architecture Depends; do
    value="$(dpkg-deb -f "$DEB" "$field")"
    [[ -n "$value" ]] && ok "$field=$value" || bad "$field 为空"
done

step "3/5 desktop 文件"
if desktop-file-validate packaging/mclauncher.desktop; then
    ok "desktop-file-validate 通过"
else
    bad "desktop 文件校验失败"
fi

step "4/5 启动冒烟（真实窗口）"
# 隔离数据目录：不污染本机配置，也不受已有实例影响
export XDG_DATA_HOME="$TMP/data" XDG_CONFIG_HOME="$TMP/config"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"

# 无头/CI 环境用软件渲染器：默认 GL 路径在无 GPU 的会话里可能拿不到窗口
SLINT_BACKEND=winit-software "$ROOT/target/release/mcl-app" > "$TMP/app.log" 2>&1 &
APP_PID=$!
WIN_ID=""
for _ in $(seq 1 40); do
    sleep 0.5
    if ! kill -0 "$APP_PID" 2>/dev/null; then break; fi
    WIN_ID="$(xwininfo -root -tree 2>/dev/null | grep -m1 "OMCLauncher" | awk '{print $1}' || true)"
    [[ -n "$WIN_ID" ]] && break
done

if [[ -z "$WIN_ID" ]]; then
    bad "5 秒内没有出现主窗口"
    tail -20 "$TMP/app.log" || true
else
    ok "主窗口已创建（$WIN_ID）"
    # 软件渲染器下窗口可能刚 map，稍等再截
    sleep 1
    if import -window "$WIN_ID" "$TMP/shot.png" 2>/dev/null; then
        size="$(identify -format '%wx%h' "$TMP/shot.png" 2>/dev/null || echo '?')"
        ok "窗口可截图（$size）"
    else
        # 软件渲染器的窗口在部分 X server 上无法用 XGetImage 抓取，不算失败
        printf '\033[1;33m  !\033[0m %s\n' "窗口截图跳过（软件渲染器不支持抓取）"
    fi
fi

kill "$APP_PID" 2>/dev/null || true
wait "$APP_PID" 2>/dev/null || true

if grep -q "事件循环就绪" "$TMP/app.log"; then
    ok "启动日志正常"
else
    bad "启动日志缺少事件循环就绪标记"
fi
if grep -qi "panic\|thread 'main' panicked" "$TMP/app.log"; then
    bad "日志出现 panic"
else
    ok "无 panic"
fi

step "5/5 免安装包"
if [[ -f "$TARBALL" ]]; then
    # 先落盘再 grep：`grep -q` 提前退出会让 tar 收到 SIGPIPE，配合 pipefail 会误报
    tar -tzf "$TARBALL" > "$TMP/tar.txt"
    if grep -q "mclauncher$" "$TMP/tar.txt"; then
        ok "tarball 含可执行文件"
    else
        bad "tarball 内容异常"
    fi
    mkdir -p "$TMP/portable"
    tar -xzf "$TARBALL" -C "$TMP/portable"
    port_bin="$(find "$TMP/portable" -maxdepth 2 -name mclauncher -type f | head -1)"
    if [[ -x "$port_bin" ]]; then
        ok "解压后可直接执行"
    else
        bad "解压后不可执行"
    fi
else
    bad "缺少 tarball"
fi

printf '\n结果：%s 通过，%s 失败\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
