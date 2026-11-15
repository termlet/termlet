#!/bin/sh
# Linux 的发布包：tar.gz、deb、rpm、AppImage，文件名都是 termlet-<版本>-linux-<架构>.<扩展名>（x64 或 arm64；下载站
# 只放行 /termlet-*）。由 .github/workflows/release.yml 的 Linux job 调用（Debian 12 的容器中构建，x64 在 x64 的运行器、
# arm64 在 arm64 的运行器上各自编译）；本机在 Docker 中验证时用同样的命令。
#
#   sh packaging/linux/package.sh <版本> <程序所在的目录> <输出目录> [x64|arm64]
#     程序所在的目录中要有发布构建的 termlet-desktop 与 termlet（cargo 的 target/release），是这个架构的程序（不给
#     架构时为 x64）；在仓库根目录运行。打包前核对程序的架构，以及用到的 glibc 符号不高于 2.35（nfpm.yaml 的依赖与
#     README.txt 写的要求）。
#
# 工具（release.yml 下载这个架构的、固定的版本并核对 SHA-256，用环境变量指定位置）：
#   NFPM              nfpm（MIT）：按 packaging/linux/nfpm.yaml 打 deb 与 rpm
#   APPIMAGETOOL      appimagetool（MIT）：打 AppImage（它本身也是 AppImage，没有 FUSE 的容器中设
#                     APPIMAGE_EXTRACT_AND_RUN=1）
#   APPIMAGE_RUNTIME  AppImage 的 type2 运行时（MIT，静态链接了 libfuse 等，许可见
#                     packaging/licenses/appimage-runtime.txt，随 AppImage 一起放进去）
#
# 各个包的内容：
#   tar.gz    根部 Termlet、bin/termlet，share/ 下按 XDG 数据目录的结构放菜单项与图标（文件名都是应用的 ID
#             dev.termlet.Termlet，与程序设的 app_id、菜单项的 StartupWMClass 相同；scalable 的图标就是 logo.svg），
#             install.sh 装进当前用户的目录（不需要 root，可以卸载），README.txt 说明安装与依赖
#   deb、rpm  /opt/termlet（程序与许可文本）、/usr/bin/termlet（命令行工具的链接）、/usr/share 下的菜单项与图标，
#             依赖见 nfpm.yaml
#   AppImage  AppRun 按调用的名字运行 Termlet 或命令行工具（见 packaging/linux/AppRun），程序在 usr/lib/termlet
set -eu

if [ $# -ne 3 ] && [ $# -ne 4 ]; then
    echo "用法：sh packaging/linux/package.sh <版本> <程序所在的目录> <输出目录> [x64|arm64]" >&2
    exit 2
fi
version=$1
bin=$2
out=$3
arch=${4:-x64}
# 各种写法的架构：Rust 目标（许可文本按它列出依赖）、nfpm 的（Go 的写法，deb 中写成 arm64、rpm 中写成 aarch64）、
# appimagetool 的、ELF 头中的机器类型（62 是 x86-64，183 是 AArch64）
case $arch in
    x64) rust_target=x86_64-unknown-linux-gnu nfpm_arch=amd64 appimage_arch=x86_64 elf_machine=62 ;;
    arm64) rust_target=aarch64-unknown-linux-gnu nfpm_arch=arm64 appimage_arch=aarch64 elf_machine=183 ;;
    *) echo "架构应为 x64 或 arm64：$arch" >&2; exit 2 ;;
esac
app_id=dev.termlet.Termlet
name="termlet-$version-linux-$arch"
max_glibc=2.35
for f in "$bin/termlet-desktop" "$bin/termlet"; do
    [ -f "$f" ] || { echo "找不到发布构建的 $f" >&2; exit 1; }
    machine=$(od -An -tu2 -j18 -N2 "$f" | tr -d ' ')
    [ "$machine" = "$elf_machine" ] || { echo "$f 不是 $arch 的程序（ELF 机器类型 $machine）" >&2; exit 1; }
done
# 用到的 glibc 符号的最高版本（deb、rpm 的依赖与 README.txt 都写 2.35 以上；构建环境换了以后在这里发现）
glibc=$(objdump -T "$bin/termlet-desktop" "$bin/termlet" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -u -V | tail -n 1)
if [ -z "$glibc" ] || [ "$(printf '%s\n%s\n' "$glibc" "$max_glibc" | sort -V | tail -n 1)" != "$max_glibc" ]; then
    echo "程序用到的 glibc 符号最高是 ${glibc:-（读不出）}，超过了 $max_glibc：改构建环境，或一起改 nfpm.yaml 的依赖与 README" >&2
    exit 1
fi
echo "程序用到的 glibc 符号最高是 $glibc"
for v in NFPM APPIMAGETOOL APPIMAGE_RUNTIME; do
    eval "value=\${$v:-}"
    if [ -z "$value" ] || [ ! -f "$value" ]; then
        echo "环境变量 $v 要指向这个工具（见脚本开头）" >&2
        exit 1
    fi
done
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# tar.gz：解压出的目录就是暂存目录，deb、rpm、AppImage 也从它取文件
stage="$out/$name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/share/applications"
install -m 755 "$bin/termlet-desktop" "$stage/Termlet"
install -m 755 "$bin/termlet" "$stage/bin/termlet"
install -m 755 packaging/linux/install.sh "$stage/install.sh"
install -m 644 packaging/linux/README.txt "$stage/README.txt"
install -m 644 "packaging/linux/$app_id.desktop" "$stage/share/applications/"
cp -R packaging/linux/icons "$stage/share/icons"
install -D -m 644 docs/brand/logo.svg "$stage/share/icons/hicolor/scalable/apps/$app_id.svg"
node scripts/release.mjs notices --target "$rust_target" --out "$stage"
tar -C "$out" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"

# deb、rpm：nfpm 在工作目录中运行（配置中的源文件相对于它：stage/ 是上面的暂存目录）。菜单项的 Exec 写安装位置的
# 完整路径（/usr/bin/termlet 是命令行工具，Termlet 不在 PATH 中）
repo=$(pwd)
ln -s "$stage" "$work/stage"
sed "s|^Exec=.*|Exec=/opt/termlet/Termlet|" "packaging/linux/$app_id.desktop" >"$work/$app_id.desktop"
for packager in deb rpm; do
    (cd "$work" && VERSION=$version PACKAGE_ARCH=$nfpm_arch "$NFPM" package \
        --config "$repo/packaging/linux/nfpm.yaml" --packager "$packager" --target "$out/$name.$packager")
done

# AppImage：根部是 AppRun、菜单项与图标（.DirIcon），程序与许可文本在 usr/lib/termlet，菜单项与图标另按 XDG 的结构
# 放在 usr/share（桌面集成工具从这里取）
appdir="$work/AppDir"
lib="$appdir/usr/lib/termlet"
mkdir -p "$lib/bin" "$appdir/usr/share/applications"
install -m 755 packaging/linux/AppRun "$appdir/AppRun"
install -m 755 "$stage/Termlet" "$lib/Termlet"
install -m 755 "$stage/bin/termlet" "$lib/bin/termlet"
cp "$stage"/license_*.txt "$lib/"
cp -R "$stage/licenses" "$lib/licenses"
install -m 644 packaging/licenses/appimage-runtime.txt "$lib/licenses/appimage-runtime.txt"
install -m 644 "packaging/linux/$app_id.desktop" "$appdir/$app_id.desktop"
install -m 644 "packaging/linux/$app_id.desktop" "$appdir/usr/share/applications/$app_id.desktop"
cp -R "$stage/share/icons" "$appdir/usr/share/icons"
install -m 644 "$stage/share/icons/hicolor/256x256/apps/$app_id.png" "$appdir/$app_id.png"
ln -s "$app_id.png" "$appdir/.DirIcon"
rm -f "$out/$name.AppImage"
ARCH=$appimage_arch "$APPIMAGETOOL" --no-appstream --runtime-file "$APPIMAGE_RUNTIME" "$appdir" "$out/$name.AppImage"
chmod 755 "$out/$name.AppImage"

ls -l "$out"
