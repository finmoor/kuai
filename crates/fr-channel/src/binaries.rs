//! 找要上传到服务器的 kuaid 程序（Linux 版和 Windows 版），算出它在服务器上的目录名。
//!
//! 目录名是 `<版本>-<程序内容的哈希>`：开发时版本号不变、程序在变，
//! 靠哈希区分，服务器上的旧程序会被换掉。

use fr_core::i18n::tr;
use fr_core::tf;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// 服务器是哪种系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Linux,
    Windows,
}

struct Target {
    os: Os,
    /// 服务器报的 CPU 架构：Linux 是 `uname -m`，Windows 是 `%PROCESSOR_ARCHITECTURE%`
    arch: &'static str,
    /// 编译目标
    triple: &'static str,
    file: &'static str,
}

const TARGETS: &[Target] = &[
    Target {
        os: Os::Linux,
        arch: "x86_64",
        triple: "x86_64-unknown-linux-musl",
        file: "kuaid",
    },
    Target {
        os: Os::Linux,
        arch: "aarch64",
        triple: "aarch64-unknown-linux-musl",
        file: "kuaid",
    },
    Target {
        os: Os::Windows,
        arch: "AMD64",
        triple: "x86_64-pc-windows-msvc",
        file: "kuaid.exe",
    },
    Target {
        os: Os::Windows,
        arch: "ARM64",
        triple: "aarch64-pc-windows-msvc",
        file: "kuaid.exe",
    },
];

pub(crate) struct Binaries {
    found: Vec<(&'static Target, PathBuf)>,
    build: u64,
}

impl Binaries {
    pub fn find() -> Result<Binaries> {
        let mut found = Vec::new();
        let mut build = FNV_OFFSET;
        for target in TARGETS {
            if let Some(path) = candidates(target).into_iter().find(|p| p.is_file()) {
                let bytes = std::fs::read(&path)
                    .with_context(|| tf!("无法读取 {path}", path = path.display()))?;
                build = fnv1a(build, &bytes);
                found.push((target, path));
            }
        }
        if found.is_empty() {
            bail!(tr(
                "未找到 kuaid 程序。开发环境请先运行 `cargo build-kuaid`，或通过环境变量 FR_KUAID_DIR 指定目录"
            ));
        }
        Ok(Binaries { found, build })
    }

    /// 服务器上 `~/.kuai/` 下的目录名。
    pub fn dir_name(&self) -> String {
        format!(
            "{}-{:012x}",
            env!("CARGO_PKG_VERSION"),
            self.build & 0xffff_ffff_ffff
        )
    }

    /// 给某种系统、某种 CPU 架构用的程序放在哪。
    pub fn path_for(&self, os: Os, arch: &str) -> Result<&PathBuf> {
        let found = self
            .found
            .iter()
            .find(|(target, _)| target.os == os && target.arch.eq_ignore_ascii_case(arch));
        let Some((_, path)) = found else {
            let system = match os {
                Os::Linux => "Linux",
                Os::Windows => "Windows",
            };
            bail!(
                "{}",
                tf!(
                    "服务器为 {system}（{arch}），暂无对应的 kuaid",
                    system,
                    arch
                )
            );
        };
        Ok(path)
    }

    /// 把这台电脑用的那一份装到 `<home>/.kuai/<版本>-<哈希>/` 下，返回装好的程序和这次是不是新装的。
    ///
    /// 和别的电脑经 ssh 装上来的是同一个位置，版本一样就是同一份。不直接跑 Kuai 带的那份：kuaid
    /// 靠所在的目录名认自己是哪一版（决定常驻进程换不换），而且它常驻，会占着 Kuai 安装目录里的
    /// 文件，Kuai 就更新不了了。
    pub fn install_local(&self, home: &Path) -> Result<(PathBuf, bool)> {
        let found = self.found.iter().find(|(target, _)| is_host(target));
        let Some((target, source)) = found else {
            bail!(tr("未找到本机可用的 kuaid"));
        };
        let dir = home.join(".kuai").join(self.dir_name());
        let installed = dir.join(target.file);
        let same = |a: &Path, b: &Path| match (a.metadata(), b.metadata()) {
            (Ok(a), Ok(b)) => a.is_file() && a.len() == b.len(),
            _ => false,
        };
        if same(&installed, source) {
            return Ok((installed, false));
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| tf!("无法创建 {path}", path = dir.display()))?;
        // 先写到旁边再改名：写到一半的程序不会被别人启动
        let partial = dir.join(format!("{}.{}.tmp", target.file, std::process::id()));
        let copied =
            std::fs::copy(source, &partial).and_then(|_| std::fs::rename(&partial, &installed));
        if let Err(err) = copied {
            let _ = std::fs::remove_file(&partial);
            // 另一个 Kuai 抢先装好了（正在运行的程序换不掉）：用它的
            if !same(&installed, source) {
                return Err(err)
                    .with_context(|| tf!("无法写入 {path}", path = installed.display()));
            }
            return Ok((installed, false));
        }
        Ok((installed, true))
    }

    /// 读出给某种系统、某种 CPU 架构用的程序。
    pub fn binary_for(&self, os: Os, arch: &str) -> Result<Vec<u8>> {
        let path = self.path_for(os, arch)?;
        std::fs::read(path).with_context(|| tf!("无法读取 {path}", path = path.display()))
    }
}

/// 依次在这些地方找：环境变量 FR_KUAID_DIR；程序旁边的 kuaid 目录（打包后）；
/// 开发时 cargo 的 target 目录（`target/release/fr-app.exe` 对应 `target/<目标>/release/kuaid`）。
/// 本机这种系统的那一份，开发时就编在客户端程序旁边（`target/release/kuaid.exe`）。
fn candidates(target: &Target) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(dir) = std::env::var_os("FR_KUAID_DIR") {
        list.push(PathBuf::from(dir).join(target.triple).join(target.file));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        list.push(dir.join("kuaid").join(target.triple).join(target.file));
        if let Some(root) = dir.parent() {
            list.push(root.join(target.triple).join("release").join(target.file));
        }
        if is_host(target) {
            list.push(dir.join(target.file));
        }
    }
    list
}

/// 这个编译目标是不是正在运行的这台电脑的。
fn is_host(target: &Target) -> bool {
    let arch = match target.arch {
        "AMD64" | "x86_64" => "x86_64",
        "ARM64" | "aarch64" => "aarch64",
        other => other,
    };
    let os = match target.os {
        Os::Linux => "linux",
        Os::Windows => "windows",
    };
    std::env::consts::OS == os && std::env::consts::ARCH == arch
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a：只用来区分程序版本，不需要抗碰撞。
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
