//! 找要上传到服务器的 kuaid 程序（Linux 版和 Windows 版），算出它在服务器上的目录名。
//!
//! 目录名是 `<版本>-<程序内容的哈希>`：开发时版本号不变、程序在变，
//! 靠哈希区分，服务器上的旧程序会被换掉。
//!
//! Windows 版 kuaid 旁边还带着新版 ConPTY（`conpty.dll`、`OpenConsole.exe`，见 `vendor/conpty`），
//! 和它一起装、一起传，也算进哈希。

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

/// 随 Windows 版 kuaid 带的新版 ConPTY：kuaid 只认它自己目录里的这两个文件。
const CONPTY_FILES: [&str; 2] = ["conpty.dll", "OpenConsole.exe"];

struct Found {
    target: &'static Target,
    path: PathBuf,
    /// 和程序放在一起的文件（名字、在哪）：Windows 版带的 ConPTY，没找到是空的
    extras: Vec<(&'static str, PathBuf)>,
}

pub(crate) struct Binaries {
    found: Vec<Found>,
    build: u64,
}

impl Binaries {
    pub fn find() -> Result<Binaries> {
        let mut found = Vec::new();
        let mut build = FNV_OFFSET;
        for target in TARGETS {
            if let Some(path) = candidates(target).into_iter().find(|p| p.is_file()) {
                let extras = extras_for(target, &path);
                for file in std::iter::once(&path).chain(extras.iter().map(|(_, file)| file)) {
                    let bytes = std::fs::read(file)
                        .with_context(|| tf!("无法读取 {path}", path = file.display()))?;
                    build = fnv1a(build, &bytes);
                }
                found.push(Found {
                    target,
                    path,
                    extras,
                });
            }
        }
        if found.is_empty() {
            bail!(tr(
                "未找到 kuaid。开发环境请先运行 `cargo build-kuaid`，或用环境变量 FR_KUAID_DIR 指定目录"
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

    /// 带没带给这种系统、这种 CPU 架构用的那一份（测试连接时先说清楚，不等到连接时才报）。
    pub fn has(&self, os: Os, arch: &str) -> bool {
        self.found
            .iter()
            .any(|found| found.target.os == os && found.target.arch.eq_ignore_ascii_case(arch))
    }

    /// 这一版带了这种系统的哪几种 CPU 架构（`x86_64`、`AMD64`……）。
    pub fn arches(&self, os: Os) -> Vec<&'static str> {
        self.found
            .iter()
            .filter(|found| found.target.os == os)
            .map(|found| found.target.arch)
            .collect()
    }

    /// 给某种系统、某种 CPU 架构用的那一份。没有的话说清是这一版不支持这种架构、支持哪些：
    /// 原来只说「暂无对应的 kuaid」，看不出是 ARM 的主机用不了。
    fn found_for(&self, os: Os, arch: &str) -> Result<&Found> {
        let found = self
            .found
            .iter()
            .find(|found| found.target.os == os && found.target.arch.eq_ignore_ascii_case(arch));
        let Some(found) = found else {
            let system = match os {
                Os::Linux => "Linux",
                Os::Windows => "Windows",
            };
            let list = self.arches(os).join(tr("、"));
            bail!(
                "{}",
                tf!(
                    "该主机为 {system}（{arch}），当前版本不支持此 CPU 架构。支持的架构：{list}。",
                    system,
                    arch,
                    list
                )
            );
        };
        Ok(found)
    }

    /// 把这台电脑用的那一份装到 `<home>/.kuai/<版本>-<哈希>/` 下，返回装好的程序和这次是不是新装的。
    ///
    /// 和别的电脑经 ssh 装上来的是同一个位置，版本一样就是同一份。不直接跑 Kuai 带的那份：kuaid
    /// 靠所在的目录名认自己是哪一版（决定常驻进程换不换），而且它常驻，会占着 Kuai 安装目录里的
    /// 文件，Kuai 就更新不了了。
    pub fn install_local(&self, home: &Path) -> Result<(PathBuf, bool)> {
        let Some(found) = self.found.iter().find(|found| is_host(found.target)) else {
            bail!(tr("未找到本机可用的 kuaid"));
        };
        let dir = home.join(".kuai").join(self.dir_name());
        // 带的文件先放：程序一放好就可能被启动
        for (name, source) in &found.extras {
            place(source, &dir, name)?;
        }
        let installed = dir.join(found.target.file);
        let fresh = place(&found.path, &dir, found.target.file)?;
        Ok((installed, fresh))
    }

    /// 读出给某种系统、某种 CPU 架构用的程序。
    pub fn binary_for(&self, os: Os, arch: &str) -> Result<Vec<u8>> {
        let path = &self.found_for(os, arch)?.path;
        std::fs::read(path).with_context(|| tf!("无法读取 {path}", path = path.display()))
    }

    /// 读出要传上去的全部文件（名字、内容）：程序在最前，后面是带的文件。
    pub fn files_for(&self, os: Os, arch: &str) -> Result<Vec<(&'static str, Vec<u8>)>> {
        let found = self.found_for(os, arch)?;
        std::iter::once((found.target.file, &found.path))
            .chain(found.extras.iter().map(|(name, path)| (*name, path)))
            .map(|(name, path)| {
                let bytes = std::fs::read(path)
                    .with_context(|| tf!("无法读取 {path}", path = path.display()))?;
                Ok((name, bytes))
            })
            .collect()
    }
}

/// 把 `source` 放到 `dir` 下名为 `name` 的文件；那里已经有一样大的就不动。返回这次放了没有。
fn place(source: &Path, dir: &Path, name: &str) -> Result<bool> {
    let target = dir.join(name);
    let same = || match (target.metadata(), source.metadata()) {
        (Ok(a), Ok(b)) => a.is_file() && a.len() == b.len(),
        _ => false,
    };
    if same() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir).with_context(|| tf!("无法创建 {path}", path = dir.display()))?;
    // 先写到旁边再改名：写到一半的程序不会被别人启动
    let partial = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let copied = std::fs::copy(source, &partial).and_then(|_| std::fs::rename(&partial, &target));
    if let Err(err) = copied {
        let _ = std::fs::remove_file(&partial);
        // 另一个 Kuai 抢先装好了（正在运行的程序换不掉）：用它的
        if !same() {
            return Err(err).with_context(|| tf!("无法写入 {path}", path = target.display()));
        }
        return Ok(false);
    }
    Ok(true)
}

/// Windows 版 kuaid 带的 ConPTY：先找程序旁边（打包后、`FR_KUAID_DIR` 里），开发时再找仓库里的
/// `vendor/conpty/<架构>`。两个文件要齐，不齐就不带（kuaid 用系统自带的 ConPTY）。
fn extras_for(target: &Target, kuaid: &Path) -> Vec<(&'static str, PathBuf)> {
    if target.os != Os::Windows {
        return Vec::new();
    }
    let dirs = kuaid
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .chain(dev_conpty_dir(target));
    for dir in dirs {
        let files: Vec<_> = CONPTY_FILES
            .iter()
            .map(|name| (*name, dir.join(name)))
            .collect();
        if files.iter().all(|(_, path)| path.is_file()) {
            return files;
        }
    }
    Vec::new()
}

/// 开发时（cargo 编的，没打包）仓库里放 ConPTY 的目录。发布版不往这里找：编译机器上的路径
/// 在用户电脑上没有意义。
#[cfg(debug_assertions)]
fn dev_conpty_dir(target: &Target) -> Option<PathBuf> {
    let arch = match target.arch {
        "AMD64" => "x64",
        "ARM64" => "arm64",
        _ => return None,
    };
    Some(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/conpty")
            .join(arch),
    )
}

#[cfg(not(debug_assertions))]
fn dev_conpty_dir(_: &Target) -> Option<PathBuf> {
    None
}

/// 依次在这些地方找：环境变量 FR_KUAID_DIR；程序旁边的 kuaid 目录（打包后）；
/// 开发时 cargo 的 target 目录（`target/release/fr-app.exe` 对应 `target/<目标>/release/kuaid`）。
/// 本机这种系统的那一份，开发时就编在客户端程序旁边（`target/release/kuaid.exe`）。
fn candidates(target: &Target) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(dir) = std::env::var_os("FR_KUAID_DIR") {
        list.push(PathBuf::from(dir).join(target.triple).join(target.file));
    }
    if let Ok(exe) = fr_core::exe::path()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn target(os: Os, arch: &str) -> &'static Target {
        TARGETS
            .iter()
            .find(|t| t.os == os && t.arch == arch)
            .unwrap()
    }

    /// 开发时才去仓库里找（见 [`dev_conpty_dir`]），按发布版编的测试里不找。
    #[cfg(debug_assertions)]
    #[test]
    fn windows_版带上_conpty_开发时取仓库里的() {
        let nowhere = Path::new("no-such-dir").join("kuaid.exe");
        let extras = extras_for(target(Os::Windows, "AMD64"), &nowhere);
        let names: Vec<&str> = extras.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, CONPTY_FILES);
        assert!(extras.iter().all(|(_, path)| path.is_file()), "{extras:?}");
    }

    #[test]
    fn linux_版不带别的文件() {
        let nowhere = Path::new("no-such-dir").join("kuaid");
        assert!(extras_for(target(Os::Linux, "x86_64"), &nowhere).is_empty());
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn 装到本机时带的文件一起放好() {
        let base = std::env::temp_dir().join(format!("fr-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let kuaid = base.join("kuaid.exe");
        std::fs::write(&kuaid, b"not really kuaid").unwrap();
        // 打包后 ConPTY 就在 kuaid 旁边
        for name in CONPTY_FILES {
            std::fs::write(base.join(name), name).unwrap();
        }
        let windows = target(Os::Windows, "AMD64");
        let binaries = Binaries {
            found: vec![Found {
                target: windows,
                extras: extras_for(windows, &kuaid),
                path: kuaid,
            }],
            build: 0x1234,
        };
        let home = base.join("home");
        let (exe, fresh) = binaries.install_local(&home).unwrap();
        assert!(fresh);
        let dir = exe.parent().unwrap();
        assert_eq!(dir, home.join(".kuai").join(binaries.dir_name()));
        for name in ["kuaid.exe", "conpty.dll", "OpenConsole.exe"] {
            assert!(dir.join(name).is_file(), "{name}");
        }
        // 再装一次：都在，不重放
        assert!(!binaries.install_local(&home).unwrap().1);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
