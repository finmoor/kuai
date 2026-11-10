//! 设置：服务器列表、仓库列表、上次打开的标签页、主题、窗口位置。
//!
//! 存在 `%APPDATA%\fork-remote\settings.json`，不存密码。环境变量 FR_SETTINGS 可以指定别的文件（测试用）。

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// 上次关闭时开着的仓库标签页
    #[serde(default)]
    pub open_tabs: Vec<RepoEntry>,
    /// 上次选中的标签页：0 是仓库管理器，1 起是 `open_tabs` 里的
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub theme: ThemeChoice,
    /// 上次关闭时窗口的位置和大小
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
}

/// 用哪套主题。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// 跟着 Windows 的深色、浅色设置走
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "跟随系统",
            ThemeChoice::Light => "浅色",
            ThemeChoice::Dark => "深色",
        }
    }
}

/// 窗口的位置和大小（逻辑像素）。最大化时记的是还原后的位置。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    /// ssh 的目标：`~/.ssh/config` 里的主机别名、`user@host` 或 `ssh://user@host:port`
    pub destination: String,
    /// 连接时转发本机的 ssh-agent，服务器上的 git 推拉可以用本机的密钥
    #[serde(default)]
    pub forward_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoEntry {
    /// 所在服务器的 id；本机仓库为空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 仓库根目录；服务器上的是 Linux 路径
    pub path: String,
}

impl RepoEntry {
    /// 目录名。
    pub fn name(&self) -> &str {
        let path = self.path.trim_end_matches(['/', '\\']);
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }
}

pub fn file() -> PathBuf {
    if let Some(path) = std::env::var_os("FR_SETTINGS") {
        return path.into();
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fork-remote")
        .join("settings.json")
}

pub fn load() -> Settings {
    let path = file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    match serde_json::from_str(&text) {
        Ok(settings) => settings,
        Err(err) => {
            // 文件坏了：改名留着，从空设置开始，免得下次保存把它覆盖掉
            let backup = path.with_extension(format!(
                "json.broken-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ));
            let _ = std::fs::rename(&path, &backup);
            log::warn!("设置文件解析失败（{err}），已改名为 {}", backup.display());
            Settings::default()
        }
    }
}

/// 先写临时文件再改名，写到一半退出也不会留下坏文件。
pub fn save(settings: &Settings) -> Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败：{}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)
        .with_context(|| format!("保存设置失败：{}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("保存设置失败：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 仓库名取最后一级目录() {
        let name = |path: &str| {
            RepoEntry {
                server: None,
                path: path.into(),
            }
            .name()
            .to_string()
        };
        assert_eq!(name(r"C:\work\fork-remote"), "fork-remote");
        assert_eq!(name("/data/workspaces/big-repo/"), "big-repo");
        assert_eq!(name("repo"), "repo");
    }
}
