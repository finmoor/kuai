//! 程序自己的路径，启动时记一次。
//!
//! Linux 上程序文件被换掉以后（.deb 升级时 apt 已经装好新版本，这个进程还没重启），
//! `current_exe()` 读的是 `/proc/self/exe`，得到「/opt/kuai/kuai (deleted)」：拿它认装法、找升级
//! 用的目录、当跳板机的 ProxyCommand 都会出错。所以启动时（换掉以前）记下来，以后都用记下的。

use std::path::PathBuf;
use std::sync::OnceLock;

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 在 `main` 的开头调一次。
pub fn remember() {
    let _ = EXE.set(std::env::current_exe().ok());
}

/// 启动时记下的程序路径；没记过（测试、只跑一下就退出的小工具）就现取。
pub fn path() -> std::io::Result<PathBuf> {
    match EXE.get() {
        Some(Some(exe)) => Ok(exe.clone()),
        _ => std::env::current_exe(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn 记下以后用记下的() {
        let before = super::path().unwrap();
        super::remember();
        assert_eq!(super::path().unwrap(), before);
        assert_eq!(super::path().unwrap(), std::env::current_exe().unwrap());
    }
}
