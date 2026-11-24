# AppImage 运行时的许可证

AppImage 版的 Kuai（`kuai-<版本>-linux-x64.AppImage`）开头是 AppImage 的运行时：它把包里的文件经
FUSE 挂出来，再运行里面的 `kuai`。运行时不在这个仓库里：发版时 `.github/workflows/release.yml`
从 GitHub 下官方编好的 `runtime-x86_64`（核对 SHA-256），交给 `tools/package-linux.sh` 的
`--appimage`，由 appimagetool 拼到包的开头。这里只放它和它静态链接的库的许可证全文，
`tools/third-party-licenses.py` 把它们写进 Linux 版的 `THIRD-PARTY-LICENSES.txt`。

- 运行时：[AppImage/type2-runtime](https://github.com/AppImage/type2-runtime) 的 `20251108` 版
  （MIT，`LICENSE`）。在 Alpine Linux 3.21 里静态链接编出来，带着这几个库的代码：libfuse、squashfuse
  的版本写死在它的 `scripts/common/install-dependencies.sh` 里，另外三个用的是编的时候 Alpine 3.21
  软件源里的（`scripts/chroot/chroot_build.sh`），具体是哪个小版本没有记下来：

| 库 | 版本 | 许可证 | 文件 |
|---|---|---|---|
| musl libc | Alpine 3.21 的（1.2.5） | MIT | `musl/COPYRIGHT` |
| libfuse | 3.15.0（打了 type2-runtime 的 `patches/libfuse/mount.c.diff`） | LGPL-2.1 | `libfuse/LICENSE` |
| squashfuse | 0.5.2 | BSD-2-Clause | `squashfuse/LICENSE` |
| zstd | Alpine 3.21 的（1.5.6） | BSD-3-Clause | `zstd/LICENSE` |
| zlib | Alpine 3.21 的 | Zlib | `zlib/LICENSE` |

许可证全文取自 musl 1.2.5、libfuse 3.15.0、squashfuse 0.5.2、zstd 1.5.6、zlib 1.3.1 的源码。libfuse
是 LGPL 的，静态链接进了运行时：要换掉它重新链接的话，运行时和这几个库的源码、构建办法都是公开的，
见 `NOTICE`。

**更新**：换运行时的版本时，改 `release.yml` 里的下载地址和 SHA-256、`tools/package-linux.sh` 开头
写的版本，再按新版的构建脚本核对这几个库的版本和许可证，换掉这里的文件，改这张表、`NOTICE` 和
`tools/third-party-licenses.py` 里 `VENDORED` 的版本。
