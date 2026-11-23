#!/usr/bin/env bash
# 打 Linux 版发布包：dist/kuai-<版本>-linux-x64.tar.gz，解压出来是一个 Kuai 文件夹：
# - kuai：客户端（x86_64）。在哪个系统上编，就要那个版本及以上的 glibc，所以发版时在 ubuntu-22.04 上编；
# - kuaid/<编译目标>/：常驻程序，和 Windows 版的包里的是同一批文件（见下）；
# - 使用说明.txt（tools/package-readme-linux.txt）、许可证、第三方库的许可证清单。
# 同样的东西也放在 dist/kuai-<版本>-linux-x64/Kuai/ 下，本机直接试。
# 带 --deb 时另打 dist/kuai_<版本>_amd64.deb：装在 /opt/kuai，/usr/bin/kuai 链接过去，应用菜单项和
# 图标放在系统目录；Kuai 升级时下新的 .deb 经 pkexec 交给 apt 装（crates/fr-app/src/update.rs）。
# 带 --appimage <appimagetool> <runtime> 时另打 dist/kuai-<版本>-linux-x64.AppImage：AppImage 的
# 程序（appimagetool 1.9.1）和运行时（type2-runtime 20251108）由调用的地方下好、核对过校验值再给。
# 发版时（.github/workflows/release.yml）把它和 Windows 版一起传到 GitHub Releases，
# Kuai 检查更新时找 kuai-*-linux-x64.tar.gz（crates/fr-app/src/update.rs）。
#
# kuaid 要和 Windows 版的包里的一字不差：服务器上的目录名是「版本-所有随带 kuaid 文件的哈希」，
# 常驻进程按它认版本（crates/fr-channel/src/binaries.rs）。两边的文件不一样的话，同一台主机被
# Windows 版和 Linux 版的 Kuai 轮流连上时，常驻进程会被来回换。所以发版时用 --kuaid 拿 Windows
# 那边打好的 kuaid 文件夹（里面有 Linux 版 kuaid 和 Windows 版 kuaid.exe、ConPTY）。
#
# 用法：tools/package-linux.sh --kuaid <Windows 版的包里的 kuaid 文件夹> [--dist <输出目录>]
#         [--deb] [--appimage <appimagetool> <runtime>]
#       tools/package-linux.sh --build-kuaid [--dist <输出目录>] [--deb] [--appimage …]
#         试用：在这里编 Linux 版 kuaid，不带 Windows 版的（连不了 Windows 主机），哈希也和发布的不一样
# 打包的机器要有 Rust、Python 3、strip，GPUI 要的 -dev 包（见 .github/workflows/ci.yml）；
# 用 --build-kuaid 时还要 x86_64-unknown-linux-musl 编译目标。设了 CARGO_TARGET_DIR 的话从那里取编好的程序。
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

dist="$root/dist"
kuaid=""
build_kuaid=0
deb=0
appimagetool=""
runtime=""
while [ $# -gt 0 ]; do
  case "$1" in
    --kuaid) kuaid=$2; shift 2 ;;
    --build-kuaid) build_kuaid=1; shift ;;
    --dist) dist=$2; shift 2 ;;
    --deb) deb=1; shift ;;
    --appimage) appimagetool=$(cd "$(dirname "$2")" && pwd)/$(basename "$2"); runtime=$(cd "$(dirname "$3")" && pwd)/$(basename "$3"); shift 3 ;;
    *) echo "不认识的参数：$1" >&2; exit 2 ;;
  esac
done
if [ -n "$kuaid" ] && [ "$build_kuaid" = 1 ]; then
  echo '--kuaid 和 --build-kuaid 只能选一个' >&2
  exit 2
fi
if [ -z "$kuaid" ] && [ "$build_kuaid" = 0 ]; then
  echo '用 --kuaid <目录> 给出 Windows 版的包里的 kuaid 文件夹，或者用 --build-kuaid 在这里编（试用）' >&2
  exit 2
fi
if [ -n "$kuaid" ]; then
  kuaid=$(cd "$kuaid" && pwd)
  for file in x86_64-unknown-linux-musl/kuaid x86_64-pc-windows-msvc/kuaid.exe; do
    if [ ! -f "$kuaid/$file" ]; then
      echo "$kuaid 里没有 $file" >&2
      exit 2
    fi
  done
fi

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
name="kuai-$version-linux-x64"
out="$dist/$name/Kuai"
target=${CARGO_TARGET_DIR:-target}

cargo build --release -p fr-app
if [ "$build_kuaid" = 1 ]; then
  if ! rustup target list --installed | grep -qx x86_64-unknown-linux-musl; then
    echo '先运行 rustup target add x86_64-unknown-linux-musl' >&2
    exit 1
  fi
  cargo build --release -p kuaid --target x86_64-unknown-linux-musl
fi

rm -rf "${dist:?}/$name"
mkdir -p "$out"
install -m 755 "$target/release/fr-app" "$out/kuai"
# 符号表有二十多 MB。Windows 版的也不带（在 .pdb 里，不进包）
strip --strip-all "$out/kuai"
if [ -n "$kuaid" ]; then
  cp -R "$kuaid" "$out/kuaid"
  # 从 GitHub Actions 的附件拿来的文件没有可执行权限
  find "$out/kuaid" -type d -exec chmod 755 {} +
  find "$out/kuaid" -type f -exec chmod 644 {} +
  find "$out/kuaid" -type f \( -name kuaid -o -name '*.exe' \) -exec chmod 755 {} +
else
  install -D -m 755 "$target/x86_64-unknown-linux-musl/release/kuaid" \
    "$out/kuaid/x86_64-unknown-linux-musl/kuaid"
fi
# 文本用 LF（在 Windows 的工作副本里拷过来的可能是 CRLF）
tr -d '\r' < tools/package-readme-linux.txt > "$out/使用说明.txt"
for file in LICENSE-MIT LICENSE-APACHE; do
  tr -d '\r' < "$file" > "$out/$file"
done
python3 tools/third-party-licenses.py "$out/THIRD-PARTY-LICENSES.txt" linux
chmod 644 "$out"/*.txt "$out"/LICENSE-*

archive="$dist/$name.tar.gz"
rm -f "$archive"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$dist/$name" -czf "$archive" Kuai

echo "打好了：$out"
(cd "$out" && find . -type f | sort | while read -r file; do
  printf '  %-60s %8s\n' "${file#./}" "$(du -h "$file" | cut -f1)"
done)
printf '  %-60s %8s\n' "$(basename "$archive")" "$(du -h "$archive" | cut -f1)"

# 应用菜单项：.deb 写 /opt/kuai/kuai，AppImage 写 kuai（AppRun 旁边那个）
desktop_entry() {
  cat <<EOF
[Desktop Entry]
Type=Application
Name=Kuai
Comment=Remote Git client and terminal
Comment[zh_CN]=远程 Git 客户端和终端
Exec=$1
Icon=kuai
Terminal=false
Categories=Development;RevisionControl;
StartupWMClass=kuai
StartupNotify=true
EOF
}

if [ "$deb" = 1 ]; then
  pkg="$dist/deb-root"
  rm -rf "${dist:?}/deb-root"
  mkdir -p "$pkg/DEBIAN" "$pkg/opt" "$pkg/usr/bin" "$pkg/usr/share/applications" \
    "$pkg/usr/share/icons/hicolor/256x256/apps" "$pkg/usr/share/doc/kuai"
  cp -R "$out" "$pkg/opt/kuai"
  # 使用说明是给解压版看的
  rm -f "$pkg/opt/kuai/使用说明.txt"
  ln -s /opt/kuai/kuai "$pkg/usr/bin/kuai"
  desktop_entry /opt/kuai/kuai > "$pkg/usr/share/applications/kuai.desktop"
  install -m 644 crates/fr-app/assets/icon.png "$pkg/usr/share/icons/hicolor/256x256/apps/kuai.png"
  tr -d '\r' < LICENSE-MIT > "$pkg/usr/share/doc/kuai/copyright"
  find "$pkg" -type d -exec chmod 755 {} +
  # 依赖：程序直接链接的库（objdump -p 的 NEEDED：libc、libgcc_s、libxcb、xkbcommon）所在的包，
  # 加上 GPUI 运行时才加载的 Vulkan、Wayland。glibc 和发版用的 ubuntu-22.04 一样
  installed_size=$(du -sk "$pkg/opt" "$pkg/usr" | awk '{s+=$1} END {print s}')
  cat > "$pkg/DEBIAN/control" <<EOF
Package: kuai
Version: $version
Architecture: amd64
Maintainer: Kuai <https://github.com/finmoor/kuai/issues>
Installed-Size: $installed_size
Depends: libc6 (>= 2.35), libgcc-s1, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libvulkan1
Recommends: git, openssh-client, fuse3, mesa-vulkan-drivers
Section: devel
Priority: optional
Homepage: https://github.com/finmoor/kuai
Description: Remote Git client and terminal
 Kuai manages Git, terminals and files of local and remote repositories in
 one window. Remote hosts are reached over SSH; a small helper (kuaid) is
 installed on each host automatically and keeps terminals running when the
 connection drops.
EOF
  deb_file="$dist/kuai_${version}_amd64.deb"
  rm -f "$deb_file"
  # xz：老的 dpkg（Debian 11 以前）不认 zstd
  dpkg-deb --root-owner-group -Zxz --build "$pkg" "$deb_file" >/dev/null
  rm -rf "${dist:?}/deb-root"
  printf '  %-60s %8s\n' "$(basename "$deb_file")" "$(du -h "$deb_file" | cut -f1)"
fi

if [ -n "$appimagetool" ]; then
  appdir="$dist/AppDir"
  rm -rf "${dist:?}/AppDir"
  cp -R "$out" "$appdir"
  rm -f "$appdir/使用说明.txt"
  ln -s kuai "$appdir/AppRun"
  desktop_entry kuai > "$appdir/kuai.desktop"
  install -m 644 crates/fr-app/assets/icon.png "$appdir/kuai.png"
  ln -s kuai.png "$appdir/.DirIcon"
  appimage="$dist/kuai-$version-linux-x64.AppImage"
  rm -f "$appimage"
  # appimagetool 自己也是 AppImage：解开了跑，打包的机器不用有 FUSE
  ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" --no-appstream \
    --runtime-file "$runtime" "$appdir" "$appimage" >/dev/null
  rm -rf "${dist:?}/AppDir"
  printf '  %-60s %8s\n' "$(basename "$appimage")" "$(du -h "$appimage" | cut -f1)"
fi
