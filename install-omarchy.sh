#!/bin/bash
# Install OpenUUYC on Omarchy (Arch + Hyprland), including host (被控) support.
#
# This is the manual path for people who build from source or download the
# release tarball. AUR users do not need it: `yay -S openuuyc-bin` already
# installs every file below.
#
# Usage:
#   ./install-omarchy.sh              # install the binary in this tree
#   ./install-omarchy.sh --uninstall  # remove it (keeps user data)
set -euo pipefail

BINARY="${OPENUUYC_BINARY:-target/release/OpenUUYC}"
PREFIX="${PREFIX:-/usr/local}"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }

if [[ ${EUID:-$(id -u)} -eq 0 ]]; then
    SUDO=()
else
    SUDO=(sudo)
fi

uninstall() {
    say "移除 OpenUUYC"
    "${SUDO[@]}" rm -f \
        "$PREFIX/bin/OpenUUYC" \
        "$PREFIX/bin/openuuyc-launch" \
        "$PREFIX/share/applications/openuuyc.desktop" \
        "$PREFIX/share/icons/hicolor/256x256/apps/openuuyc.png" \
        /usr/lib/udev/rules.d/60-openuuyc-uinput.rules
    # Only remove the menu extension if it is the one we wrote.
    menu="$HOME/.config/omarchy/extensions/omarchy-menu.jsonc"
    if [[ -e $menu ]] && grep -q 'install.service.openuuyc' "$menu"; then
        rm -f "$menu"
    fi
    rm -f "$HOME/.config/systemd/user/openuuyc-gui.service"
    systemctl --user daemon-reload 2>/dev/null || true
    say "已移除。用户数据保留在 ~/.local/share/OpenUUYC/"
    exit 0
}

[[ ${1:-} == --uninstall || ${1:-} == -u ]] && uninstall

if [[ ! -x $BINARY ]]; then
    cat >&2 <<EOF
找不到可执行文件：$BINARY

先构建：
    cargo build --release --bin OpenUUYC

或从 Release 下载 tarball 后，在本目录运行：
    OPENUUYC_BINARY=./OpenUUYC ./install-omarchy.sh
EOF
    exit 1
fi

say "安装依赖"
"${SUDO[@]}" pacman -S --needed --noconfirm \
    libva libx11 libxcb libxrandr libxi libxcursor libxfixes \
    libxkbcommon libxkbcommon-x11 wayland libglvnd vulkan-icd-loader \
    dbus alsa-lib openssl fuse3 noto-fonts-cjk

say "确认屏幕共享门户"
if ! pacman -Qq xdg-desktop-portal-hyprland &>/dev/null; then
    warn "未安装 xdg-desktop-portal-hyprland，Wayland 下被控采集需要它"
    warn "安装：sudo pacman -S xdg-desktop-portal-hyprland"
fi

say "安装 uinput 输入权限（被控键鼠必需）"
"${SUDO[@]}" install -Dm644 packaging/linux/60-openuuyc-uinput.rules \
    /usr/lib/udev/rules.d/60-openuuyc-uinput.rules
"${SUDO[@]}" udevadm control --reload-rules
"${SUDO[@]}" modprobe uinput
echo uinput | "${SUDO[@]}" tee /etc/modules-load.d/uinput.conf >/dev/null
# uaccess 授权在规则重载后对当前会话不一定立即生效；补一条 ACL 免得重新登录。
if [[ -e /dev/uinput ]]; then
    "${SUDO[@]}" setfacl -m "u:${USER}:rw" /dev/uinput 2>/dev/null || true
fi

say "安装程序文件"
"${SUDO[@]}" install -Dm755 "$BINARY" "$PREFIX/bin/OpenUUYC"
"${SUDO[@]}" install -Dm755 packaging/linux/openuuyc-launch "$PREFIX/bin/openuuyc-launch"
"${SUDO[@]}" install -Dm644 packaging/linux/openuuyc.desktop \
    "$PREFIX/share/applications/openuuyc.desktop"
"${SUDO[@]}" install -Dm644 assets/icon-256.png \
    "$PREFIX/share/icons/hicolor/256x256/apps/openuuyc.png"
"${SUDO[@]}" install -Dm644 packaging/linux/openuuyc-gui.service \
    /usr/lib/systemd/user/openuuyc-gui.service

# The wrapper execs /usr/bin/OpenUUYC; when installing under /usr/local, point
# it at the binary we just placed so both layouts work.
if [[ $PREFIX != /usr ]]; then
    "${SUDO[@]}" sed -i "s|^exec /usr/bin/OpenUUYC|exec $PREFIX/bin/OpenUUYC|" \
        "$PREFIX/bin/openuuyc-launch"
fi

say "启用图形会话服务"
systemctl --user daemon-reload
systemctl --user enable --now openuuyc-gui.service

# Omarchy only: add an Install → Service row so the app is discoverable there.
if [[ -d $HOME/.config/omarchy ]]; then
    say "添加 Omarchy 菜单项"
    mkdir -p "$HOME/.config/omarchy/extensions"
    target="$HOME/.config/omarchy/extensions/omarchy-menu.jsonc"
    if [[ -e $target ]] && ! grep -q 'install.service.openuuyc' "$target"; then
        warn "$target 已存在，未自动修改"
        warn "请手动加入 packaging/omarchy/omarchy-menu.jsonc 里的条目"
    else
        cp packaging/omarchy/omarchy-menu.jsonc "$target"
        say "菜单项已写入 $target（Quickshell 会自动重新加载）"
    fi
fi

cat <<EOF

完成。

  启动      应用菜单「UU 远程（OpenUUYC）」，或 openuuyc-launch gui
  服务      systemctl --user status openuuyc-gui.service
  日志      tail -f ~/.local/state/openuuyc/gui.log
  菜单      Super + Space → Install → Service → UU Remote (OpenUUYC)

下一步：扫码登录 → 连接设置 → 打开「允许被控」。

首次被控时会在本机屏幕弹出一次屏幕共享授权，点「共享」即可，之后不再询问。
若弹窗被 Hyprland 插件遮挡（例如 overview 常驻表面），可临时提层：

  hyprctl eval 'hl.layer_rule({ match = { namespace = "ch.wysbd.hyprland-preview-share-picker" }, order = 1 })'

卸载：$0 --uninstall
EOF
