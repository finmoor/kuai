//! 设置：服务器列表、仓库列表、工作区和各自开着的标签页、语言、主题、托盘和窗口位置。
//!
//! 存在 `%APPDATA%\fork-remote\settings.json`，不存密码。环境变量 FR_SETTINGS 可以指定别的文件（测试用）。

use fr_core::tf;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use fr_core::i18n::{Language, tr};
use serde::{Deserialize, Serialize};

fn default_light_theme() -> String {
    "light".into()
}

fn default_dark_theme() -> String {
    "dark".into()
}

/// 一个工作区：一组仓库标签页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroup {
    pub name: String,
    #[serde(default)]
    pub tabs: Vec<RepoEntry>,
    /// 选中的标签页：0 是仓库管理器，1 起是 `tabs` 里的
    #[serde(default)]
    pub active: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// 旧版本（1.0.x）存标签页的地方，读进来以后挪到第一个工作区里，不再写回去
    #[serde(default, skip_serializing)]
    open_tabs: Vec<RepoEntry>,
    #[serde(default, skip_serializing)]
    active_tab: usize,
    /// 工作区：各自记着开着哪些仓库标签页。至少有一个
    #[serde(default)]
    pub workspaces: Vec<TabGroup>,
    /// 现在用的是第几个工作区
    #[serde(default)]
    pub active_workspace: usize,
    #[serde(default)]
    pub language: Language,
    /// 深色还是浅色：跟随系统，或者固定一种
    #[serde(default)]
    pub theme: ThemeChoice,
    /// 浅色时用哪套配色（`appearance::PALETTES` 里的 id）
    #[serde(default = "default_light_theme")]
    pub light_theme: String,
    /// 深色时用哪套配色
    #[serde(default = "default_dark_theme")]
    pub dark_theme: String,
    /// 最小化时收到托盘，任务栏上不留按钮
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// 点窗口的关闭按钮时不退出，收到托盘
    #[serde(default)]
    pub close_to_tray: bool,
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
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => tr("跟随系统"),
            ThemeChoice::Light => tr("浅色"),
            ThemeChoice::Dark => tr("深色"),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let mut settings = Self {
            servers: Vec::new(),
            repos: Vec::new(),
            open_tabs: Vec::new(),
            active_tab: 0,
            workspaces: Vec::new(),
            active_workspace: 0,
            language: Language::default(),
            theme: ThemeChoice::default(),
            light_theme: default_light_theme(),
            dark_theme: default_dark_theme(),
            minimize_to_tray: false,
            close_to_tray: false,
            window: None,
        };
        settings.normalize();
        settings
    }
}

impl Settings {
    /// 读进来以后理一遍：旧版本的标签页挪进第一个工作区；保证至少有一个工作区、下标不越界。
    pub fn normalize(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces.push(TabGroup {
                name: tr("默认").to_string(),
                tabs: std::mem::take(&mut self.open_tabs),
                active: self.active_tab,
            });
        }
        self.open_tabs.clear();
        self.active_tab = 0;
        self.active_workspace = self.active_workspace.min(self.workspaces.len() - 1);
        for group in &mut self.workspaces {
            group.active = group.active.min(group.tabs.len());
        }
    }

    /// 现在用的工作区。
    pub fn workspace(&self) -> &TabGroup {
        &self.workspaces[self.active_workspace.min(self.workspaces.len() - 1)]
    }

    pub fn workspace_mut(&mut self) -> &mut TabGroup {
        let ix = self.active_workspace.min(self.workspaces.len() - 1);
        &mut self.workspaces[ix]
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
    match parse(&text) {
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

/// 解析设置文件的内容。
///
/// 1.1 以前的设置文件里没有语言这一项：那时界面只有中文，升级上来的保持中文，不因为系统的
/// 显示语言是英文就突然换成英文界面。没有设置文件（新装）才跟随系统。
fn parse(text: &str) -> serde_json::Result<Settings> {
    let mut settings: Settings = serde_json::from_str(text)?;
    let has_language = serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|value| value.get("language").is_some());
    if !has_language {
        settings.language = Language::Chinese;
    }
    settings.normalize();
    Ok(settings)
}

/// 先写临时文件再改名，写到一半退出也不会留下坏文件。
pub fn save(settings: &Settings) -> Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| tf!("创建目录失败：{dir}", dir = dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)
        .with_context(|| tf!("保存设置失败：{tmp}", tmp = tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| tf!("保存设置失败：{path}", path = path.display()))
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

    #[test]
    fn 旧版本的标签页挪进第一个工作区() {
        let old = r#"{"repos":[{"path":"C:/a"}],"open_tabs":[{"path":"C:/a"}],"active_tab":1,"theme":"dark"}"#;
        let mut settings: Settings = serde_json::from_str(old).unwrap();
        settings.normalize();
        assert_eq!(settings.workspaces.len(), 1);
        assert_eq!(settings.workspace().tabs.len(), 1);
        assert_eq!(settings.workspace().active, 1);
        assert_eq!(settings.theme, ThemeChoice::Dark);
        assert_eq!(settings.light_theme, "light");
        // 写回去以后没有旧字段，再读一遍结果一样
        let text = serde_json::to_string(&settings).unwrap();
        assert!(!text.contains("open_tabs"));
        let mut again: Settings = serde_json::from_str(&text).unwrap();
        again.normalize();
        assert_eq!(again.workspaces, settings.workspaces);
    }

    #[test]
    fn 工作区的下标越界时收回来() {
        let text = r#"{"workspaces":[{"name":"a","tabs":[],"active":5}],"active_workspace":9}"#;
        let mut settings: Settings = serde_json::from_str(text).unwrap();
        settings.normalize();
        assert_eq!(settings.active_workspace, 0);
        assert_eq!(settings.workspace().active, 0);
    }

    #[test]
    fn 旧设置文件没有语言这一项时保持中文() {
        use fr_core::i18n::Language;
        // 1.1 以前存的：没有 language
        let old = super::parse(r#"{"servers":[],"repos":[]}"#).unwrap();
        assert_eq!(old.language, Language::Chinese);
        // 存过语言的照它来
        let system = super::parse(r#"{"language":"system"}"#).unwrap();
        assert_eq!(system.language, Language::System);
        let english = super::parse(r#"{"language":"en"}"#).unwrap();
        assert_eq!(english.language, Language::English);
        // 新装的（没有设置文件）跟随系统
        assert_eq!(super::Settings::default().language, Language::System);
    }
}
