//! 设置：服务器列表、仓库列表、工作区和各自开着的标签页、语言、主题、托盘和窗口位置。
//!
//! 存在 `%APPDATA%\fork-remote\settings.json`，不存密码（服务器的密码、私钥口令在系统的凭据管理器里，
//! 见 `secrets`）。环境变量 FR_SETTINGS 可以指定别的文件（测试用）。

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

fn default_auto_fetch() -> u32 {
    10
}

/// 可以选的自动获取间隔（分钟）；0 是不自动获取。
pub const AUTO_FETCH_CHOICES: [u32; 6] = [0, 5, 10, 15, 30, 60];

/// 各块的大小（逻辑像素），拖分隔条改的。没拖过的是空，按默认值或窗口大小算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// 仓库页侧栏的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<f32>,
    /// 本地修改里文件列表的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_list: Option<f32>,
    /// 提交历史下面详情区的高度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_height: Option<f32>,
    /// 详情区左边一栏（说明、改动的文件）的宽度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_width: Option<f32>,
}

/// 一个仓库自己记的东西：起的名字、隐藏了哪些分支、最近用过的提交说明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMemo {
    #[serde(flatten)]
    pub repo: RepoEntry,
    /// 自己起的名字（只改显示，目录不动）；没有就显示目录名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 提交列表里隐藏的引用，完整的名字（`refs/heads/…`、`refs/remotes/…`、`refs/tags/…`）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_refs: Vec<String>,
    /// 最近用过的提交说明，新的在前
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_messages: Vec<String>,
}

impl RepoMemo {
    /// 最多记几条提交说明
    pub const MAX_MESSAGES: usize = 10;

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.hidden_refs.is_empty() && self.recent_messages.is_empty()
    }

    /// 记下一条刚用过的提交说明：放到最前面，重复的去掉，多了去掉最旧的。
    pub fn remember_message(&mut self, message: &str) {
        let message = message.trim();
        if message.is_empty() {
            return;
        }
        self.recent_messages.retain(|m| m != message);
        self.recent_messages.insert(0, message.to_string());
        self.recent_messages.truncate(Self::MAX_MESSAGES);
    }
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
    /// 每隔几分钟自动获取一次（只对正在看的那个仓库）；0 是不自动获取
    #[serde(default = "default_auto_fetch")]
    pub auto_fetch_minutes: u32,
    /// 改动并排着看（左边旧的、右边新的），不勾就是上下排在一栏里
    #[serde(default)]
    pub diff_split: bool,
    /// 工作区有改动时切换分支、拉取：先贮藏，做完再恢复
    #[serde(default)]
    pub stash_before_switch: bool,
    #[serde(default)]
    pub layout: Layout,
    /// 各个仓库自己记的东西
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_memos: Vec<RepoMemo>,
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
            auto_fetch_minutes: default_auto_fetch(),
            diff_split: false,
            stash_before_switch: false,
            layout: Layout::default(),
            repo_memos: Vec::new(),
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
        // 手改设置文件写了别的数：收到最近的一档
        if !AUTO_FETCH_CHOICES.contains(&self.auto_fetch_minutes) {
            self.auto_fetch_minutes = AUTO_FETCH_CHOICES
                .into_iter()
                .min_by_key(|choice| choice.abs_diff(self.auto_fetch_minutes))
                .unwrap_or(0);
        }
        // 仓库已经不在列表里、也没开着了，它记的东西就不留
        let (repos, groups) = (&self.repos, &self.workspaces);
        self.repo_memos.retain(|memo| {
            !memo.is_empty()
                && (repos.contains(&memo.repo)
                    || groups.iter().any(|group| group.tabs.contains(&memo.repo)))
        });
    }

    /// 这个仓库记的东西。
    pub fn memo(&self, repo: &RepoEntry) -> Option<&RepoMemo> {
        self.repo_memos.iter().find(|memo| memo.repo == *repo)
    }

    /// 改这个仓库记的东西；原来没有就新建一份，改完是空的就不留。
    pub fn edit_memo(&mut self, repo: &RepoEntry, edit: impl FnOnce(&mut RepoMemo)) {
        let ix = match self.repo_memos.iter().position(|memo| memo.repo == *repo) {
            Some(ix) => ix,
            None => {
                self.repo_memos.push(RepoMemo {
                    repo: repo.clone(),
                    name: None,
                    hidden_refs: Vec::new(),
                    recent_messages: Vec::new(),
                });
                self.repo_memos.len() - 1
            }
        };
        edit(&mut self.repo_memos[ix]);
        if self.repo_memos[ix].is_empty() {
            self.repo_memos.remove(ix);
        }
    }

    /// 仓库显示的名字：起过名字就用它，没有就是目录名。
    pub fn repo_name<'a>(&'a self, repo: &'a RepoEntry) -> &'a str {
        self.memo(repo)
            .and_then(|memo| memo.name.as_deref())
            .unwrap_or_else(|| repo.name())
    }

    /// 给仓库起名字。空的、或者和目录名一样，就是不要自己起的名字了。
    pub fn set_repo_name(&mut self, repo: &RepoEntry, name: &str) {
        let name = name.trim();
        let name = (!name.is_empty() && name != repo.name()).then(|| name.to_string());
        self.edit_memo(repo, |memo| memo.name = name);
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

/// 登录服务器的方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// 密钥：指定的私钥文件，或者 ssh 默认的那些（`~/.ssh` 里的、ssh-agent、`~/.ssh/config`）
    #[default]
    Key,
    /// 密码，不试密钥
    Password,
}

impl Auth {
    fn is_key(&self) -> bool {
        *self == Auth::Key
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    /// ssh 的目标：`~/.ssh/config` 里的主机别名、`user@host` 或 `ssh://user@host:port`。
    /// 界面上分成主机、用户名、端口三个框填，见 [`Target`]
    pub destination: String,
    /// 连接时转发本机的 ssh-agent，服务器上的 git 推拉可以用本机的密钥
    #[serde(default)]
    pub forward_agent: bool,
    /// 登录方式。密码、私钥口令不在这里，存在系统的凭据管理器里（见 `secrets`）
    #[serde(default, skip_serializing_if = "Auth::is_key")]
    pub auth: Auth,
    /// 私钥文件（只在用密钥登录时有用）；空着用 ssh 默认的密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

impl Server {
    /// 连接时用的私钥文件：用密码登录的不带。
    pub fn key_file(&self) -> Option<&str> {
        match self.auth {
            Auth::Key => self.identity_file.as_deref(),
            Auth::Password => None,
        }
    }
}

/// ssh 目标拆开的三段；没有的那段是空串。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// 主机名、IP，或者 `~/.ssh/config` 里的主机别名
    pub host: String,
    pub user: String,
    pub port: String,
}

impl Target {
    /// 拆开 `host`、`user@host`、`ssh://user@host:port`；`host:端口`、`[IPv6]:端口` 也认。
    pub fn parse(destination: &str) -> Target {
        let text = destination.trim();
        let rest = text
            .strip_prefix("ssh://")
            .map_or(text, |rest| rest.trim_end_matches('/'));
        // 用户名里可以有 @（比如域账号），以最后一个为界
        let (user, rest) = rest.rsplit_once('@').unwrap_or(("", rest));
        let (host, port) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((host, tail)) => (host, tail.strip_prefix(':').unwrap_or("")),
            None => match rest.rsplit_once(':') {
                // 只有一个冒号、后面全是数字才是端口；冒号多的是不带方括号的 IPv6 地址
                Some((host, port))
                    if !host.contains(':')
                        && !port.is_empty()
                        && port.chars().all(|c| c.is_ascii_digit()) =>
                {
                    (host, port)
                }
                _ => (rest, ""),
            },
        };
        Target {
            host: host.to_string(),
            user: user.to_string(),
            port: port.to_string(),
        }
    }

    /// `user@host:port`，没有的段不写。VS Code 的 Remote-SSH 认这种写法。
    pub fn authority(&self) -> String {
        let mut text = String::new();
        if !self.user.is_empty() {
            text.push_str(&self.user);
            text.push('@');
        }
        if self.port.is_empty() {
            text.push_str(&self.host);
        } else if self.host.contains(':') {
            text.push_str(&format!("[{}]:{}", self.host, self.port));
        } else {
            text.push_str(&format!("{}:{}", self.host, self.port));
        }
        text
    }

    /// 拼成交给 ssh 的目标：有端口的写成 `ssh://…`（ssh 只在这种写法里认端口）。
    pub fn compose(&self) -> String {
        if self.port.is_empty() {
            self.authority()
        } else {
            format!("ssh://{}", self.authority())
        }
    }

    /// 端口是不是 1～65535 的数字。
    pub fn port_ok(&self) -> bool {
        self.port.parse::<u16>().is_ok_and(|port| port > 0)
    }
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
    fn 仓库记的东西跟着仓库走() {
        let repo = |path: &str| RepoEntry {
            server: None,
            path: path.into(),
        };
        let mut settings = Settings {
            repos: vec![repo("C:/a"), repo("C:/b")],
            ..Settings::default()
        };
        settings.edit_memo(&repo("C:/a"), |memo| {
            memo.hidden_refs.push("refs/heads/wip".into());
            for n in 0..12 {
                memo.remember_message(&format!("说明 {n}"));
            }
            memo.remember_message("说明 5");
            memo.remember_message("  ");
        });
        let memo = settings.memo(&repo("C:/a")).unwrap();
        assert_eq!(memo.recent_messages.len(), RepoMemo::MAX_MESSAGES);
        assert_eq!(memo.recent_messages[0], "说明 5");
        assert_eq!(memo.recent_messages[1], "说明 11");
        assert_eq!(
            memo.recent_messages
                .iter()
                .filter(|m| *m == "说明 5")
                .count(),
            1
        );
        // 存一遍再读回来还在
        let text = serde_json::to_string(&settings).unwrap();
        let again = super::parse(&text).unwrap();
        assert_eq!(again.repo_memos, settings.repo_memos);
        // 改完是空的就不留；仓库从列表里去掉以后也不留
        settings.edit_memo(&repo("C:/b"), |_| {});
        assert!(settings.memo(&repo("C:/b")).is_none());
        settings.repos.remove(0);
        settings.normalize();
        assert!(settings.repo_memos.is_empty());
    }

    #[test]
    fn ssh目标拆开再拼回去() {
        let target = |host: &str, user: &str, port: &str| Target {
            host: host.into(),
            user: user.into(),
            port: port.into(),
        };
        for (text, parts) in [
            ("dev", target("dev", "", "")),
            ("root@192.0.2.8", target("192.0.2.8", "root", "")),
            (
                "ssh://root@192.0.2.8:2222",
                target("192.0.2.8", "root", "2222"),
            ),
            (
                "ssh://git.example.com:2222",
                target("git.example.com", "", "2222"),
            ),
            (
                "u@corp.example.com@bastion",
                target("bastion", "u@corp.example.com", ""),
            ),
            (
                "ssh://u@[2001:db8::1]:2222",
                target("2001:db8::1", "u", "2222"),
            ),
            ("u@2001:db8::1", target("2001:db8::1", "u", "")),
        ] {
            assert_eq!(Target::parse(text), parts, "{text}");
            assert_eq!(parts.compose(), text, "{text}");
        }
        // 这几种写法也认，拼回去是规范的写法
        assert_eq!(Target::parse(" ssh://dev/ "), target("dev", "", ""));
        assert_eq!(Target::parse("dev:2200"), target("dev", "", "2200"));
        assert_eq!(target("dev", "", "2200").compose(), "ssh://dev:2200");
        // Remote-SSH 用的写法不带 ssh://
        assert_eq!(target("dev", "root", "2200").authority(), "root@dev:2200");

        assert!(target("dev", "", "22").port_ok());
        assert!(target("dev", "", "65535").port_ok());
        for bad in ["", "0", "65536", "22a", "-1"] {
            assert!(!target("dev", "", bad).port_ok(), "{bad}");
        }
    }

    #[test]
    fn 旧设置里的服务器按密钥登录() {
        let old = r#"{"servers":[{"id":"s1","name":"dev","destination":"root@dev"}]}"#;
        let settings = super::parse(old).unwrap();
        assert_eq!(settings.servers[0].auth, Auth::Key);
        assert_eq!(settings.servers[0].identity_file, None);
        // 默认值不写进文件
        let text = serde_json::to_string(&settings.servers[0]).unwrap();
        assert!(
            !text.contains("auth") && !text.contains("identity_file"),
            "{text}"
        );

        let server = Server {
            auth: Auth::Password,
            identity_file: Some("C:/keys/id".into()),
            ..settings.servers[0].clone()
        };
        let text = serde_json::to_string(&server).unwrap();
        assert!(text.contains(r#""auth":"password""#), "{text}");
        // 用密码登录的，连接时不带私钥文件
        assert_eq!(server.key_file(), None);
        let again: Server = serde_json::from_str(&text).unwrap();
        assert_eq!(again, server);
    }

    #[test]
    fn 仓库可以另起名字() {
        let repo = RepoEntry {
            server: None,
            path: r"C:\work\chrome-mcp".into(),
        };
        let mut settings = Settings {
            repos: vec![repo.clone()],
            ..Settings::default()
        };
        assert_eq!(settings.repo_name(&repo), "chrome-mcp");
        settings.set_repo_name(&repo, "  咸鱼插件 ");
        assert_eq!(settings.repo_name(&repo), "咸鱼插件");
        // 存一遍再读回来还在
        let again = super::parse(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(again.repo_name(&repo), "咸鱼插件");
        // 清空、或者改回目录名：不留自己起的名字
        settings.set_repo_name(&repo, "chrome-mcp");
        assert!(settings.memo(&repo).is_none());
        settings.set_repo_name(&repo, "x");
        settings.set_repo_name(&repo, "");
        assert_eq!(settings.repo_name(&repo), "chrome-mcp");
        assert!(settings.repo_memos.is_empty());
    }

    #[test]
    fn 自动获取的间隔收到最近的一档() {
        let read = |text: &str| super::parse(text).unwrap().auto_fetch_minutes;
        assert_eq!(read("{}"), 10);
        assert_eq!(read(r#"{"auto_fetch_minutes":0}"#), 0);
        assert_eq!(read(r#"{"auto_fetch_minutes":7}"#), 5);
        assert_eq!(read(r#"{"auto_fetch_minutes":999}"#), 60);
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
