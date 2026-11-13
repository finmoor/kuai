//! 找要上传到服务器的 Linux 版 kuaid 程序，算出它在服务器上的目录名。
//!
//! 目录名是 `<版本>-<程序内容的哈希>`：开发时版本号不变、程序在变，
//! 靠哈希区分，服务器上的旧程序会被换掉。

use fr_core::i18n::tr;
use fr_core::tf;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

/// 服务器 `uname -m` 的结果和对应的编译目标
const TARGETS: &[(&str, &str)] = &[
    ("x86_64", "x86_64-unknown-linux-musl"),
    ("aarch64", "aarch64-unknown-linux-musl"),
];

pub(crate) struct Binaries {
    found: Vec<(&'static str, PathBuf)>,
    build: u64,
}

impl Binaries {
    pub fn find() -> Result<Binaries> {
        let mut found = Vec::new();
        let mut build = FNV_OFFSET;
        for (arch, triple) in TARGETS {
            if let Some(path) = candidates(triple).into_iter().find(|p| p.is_file()) {
                let bytes = std::fs::read(&path)
                    .with_context(|| tf!("无法读取 {path}", path = path.display()))?;
                build = fnv1a(build, &bytes);
                found.push((*arch, path));
            }
        }
        if found.is_empty() {
            bail!(tr(
                "未找到 Linux 版 kuaid。开发环境请先运行 `cargo build-kuaid`，或通过环境变量 FR_KUAID_DIR 指定目录"
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

    /// 读出给某种 CPU 架构用的程序。
    pub fn binary_for(&self, arch: &str) -> Result<Vec<u8>> {
        let Some((_, path)) = self.found.iter().find(|(a, _)| *a == arch) else {
            bail!(
                "{}",
                tf!(
                    "服务器的 CPU 架构为 {arch}，暂无该架构的 kuaid",
                    arch
                )
            );
        };
        std::fs::read(path).with_context(|| tf!("无法读取 {path}", path = path.display()))
    }
}

/// 依次在这些地方找：环境变量 FR_KUAID_DIR；程序旁边的 kuaid 目录（打包后）；
/// 开发时 cargo 的 target 目录（`target/release/fr-app.exe` 对应 `target/<目标>/release/kuaid`）。
fn candidates(triple: &str) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(dir) = std::env::var_os("FR_KUAID_DIR") {
        list.push(PathBuf::from(dir).join(triple).join("kuaid"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        list.push(dir.join("kuaid").join(triple).join("kuaid"));
        if let Some(target) = dir.parent() {
            list.push(target.join(triple).join("release").join("kuaid"));
        }
    }
    list
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
