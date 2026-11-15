#!/bin/sh
# Termlet 的安装脚本（Linux）：把这个目录中的 Termlet 加进当前用户的应用菜单（菜单项与图标），并把命令行工具
# termlet 链接到 ~/.local/bin。不需要 root，不复制程序：程序留在这个目录中，移动目录后重新运行一次。
#
#   ./install.sh               安装（重复运行即更新菜单项、图标与链接）
#   ./install.sh --uninstall   卸载：删除菜单项、图标与链接（不删除这个目录，也不删除 ~/.config/termlet 中的数据）
#
# Adds Termlet in this directory to the current user's application menu (desktop entry and icons) and links the
# termlet command into ~/.local/bin. No root needed; the program stays here, so run this again after moving the
# directory. --uninstall removes the menu entry, icons and link (not this directory or your data).
set -eu

app_id=dev.termlet.Termlet
here=$(cd "$(dirname "$0")" && pwd -P)
data=${XDG_DATA_HOME:-$HOME/.local/share}
desktop=$data/applications/$app_id.desktop
icons=$data/icons/hicolor
link=$HOME/.local/bin/termlet

# 刷新菜单与图标的缓存；没有这些工具时跳过（多数桌面环境自己会发现变化）
refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$data/applications" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
    fi
}

install_app() {
    for f in Termlet bin/termlet "share/applications/$app_id.desktop" share/icons/hicolor; do
        if [ ! -e "$here/$f" ]; then
            echo "找不到 $here/$f：请在解压出的目录中运行 / $f not found: run this from the extracted directory" >&2
            exit 1
        fi
    done
    # 菜单项的 Exec 中这些字符要转义，这里不处理，换个目录即可
    case $here in
        *[\"\`\$\\%\|\&]*)
            echo "目录的路径中不能有 \" \` \$ \\ % | & 这些字符 / unsupported characters in the path: $here" >&2
            exit 1
            ;;
    esac

    mkdir -p "$data/applications" "$icons"
    sed "s|^Exec=.*|Exec=\"$here/Termlet\"|" "$here/share/applications/$app_id.desktop" >"$desktop.tmp"
    mv "$desktop.tmp" "$desktop"
    cp -R "$here/share/icons/hicolor/." "$icons/"

    mkdir -p "$(dirname "$link")"
    if [ -e "$link" ] && [ ! -L "$link" ]; then
        echo "跳过：$link 已存在且不是链接 / skipped: $link exists and is not a symlink" >&2
    else
        ln -sfn "$here/bin/termlet" "$link"
    fi
    refresh

    echo "已安装 / installed: $desktop"
    echo "命令行工具 / command: $link -> $here/bin/termlet"
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "注意：$HOME/.local/bin 不在 PATH 中，重新登录（多数发行版会自动加入）或把它加进 PATH 后才能直接运行 termlet" \
            "/ note: $HOME/.local/bin is not in PATH; log in again or add it to PATH to run termlet" ;;
    esac
}

uninstall() {
    rm -f "$desktop" "$icons"/*/apps/"$app_id".png "$icons"/*/apps/"$app_id".svg
    # 只删除指向这个目录的链接
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$here/bin/termlet" ]; then
        rm -f "$link"
    fi
    refresh
    echo "已卸载菜单项、图标与链接；这个目录与 ~/.config/termlet 中的数据没有删除" \
        "/ removed the menu entry, icons and link; this directory and your data in ~/.config/termlet are kept"
}

case ${1:-} in
    '') install_app ;;
    --uninstall) uninstall ;;
    *)
        echo "用法 / usage: $0 [--uninstall]" >&2
        exit 2
        ;;
esac
