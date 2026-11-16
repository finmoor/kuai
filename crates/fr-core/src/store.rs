//! 一台电脑保存的仓库列表和工作区（协议 10 起）。
//!
//! 每台电脑（本机、各台服务器）的仓库列表、工作区由它自己的 kuaid 保存在一个文件里（[`path`]），
//! 连上来的电脑看到的都是这一份：哪一边改了，kuaid 把改完的整份推给所有订阅着的客户端。
//! 改动按动作（[`Op`]）发给 kuaid，由它依次执行，几台电脑同时改也不会互相覆盖。
//! 本机的 kuaid 用不了时，本机的 Kuai 直接读写这个文件（同一套 [`Store::apply`]、[`save`]）。
//!
//! 只放各台电脑都要看到同一份的东西：仓库列表、仓库起的名字、工作区（名字和里面有哪些仓库），
//! 和这台电脑上的 Kuai 配置的服务器（不含密码，连上来的电脑经这台连接它们）。开着哪些标签页、
//! 选中哪个、固定的、刚关掉的，各台电脑自己记，不在这里。
//!
//! 文件的格式只加字段：新旧版本的 kuaid 都可能读到它。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::i18n::tr;
use crate::tf;

/// 文件名。放在用户的配置目录里（和 Kuai 的设置文件同一个目录），不放在 `~/.kuai`：
/// 那里只放程序和运行时的文件，按卸载说明删掉它不会删掉工作区。
const FILE: &str = "workspaces.json";
/// 写入前留着的上一份：这一份坏了（写到一半断电）就读它
const BACKUP: &str = "workspaces.json.bak";

/// 一台电脑保存的东西。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// 每改一次加一
    #[serde(default)]
    pub version: u64,
    /// 这台电脑上的仓库（仓库根目录）。别的服务器上的仓库在那台服务器自己的列表里
    #[serde(default)]
    pub repos: Vec<String>,
    /// 仓库起的名字（只改显示），按仓库路径。不在列表里的仓库（子仓库）也可以有
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// 这台电脑上的 Kuai 配置的服务器（不含密码）。连上来的电脑经这台连接它们
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<Server>,
}

/// 一个工作区。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// 在这台电脑上唯一，不随改名变
    pub id: String,
    pub name: String,
    /// 工作区里的仓库，按加进来的先后
    #[serde(default)]
    pub repos: Vec<RepoRef>,
}

/// 工作区里的一个仓库：这台电脑上的（`server` 为空），或者这台电脑配置的一台服务器上的。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// [`Store::servers`] 里的 id；这台电脑上的仓库为空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub path: String,
}

/// 这台电脑配置的一台服务器：连接用的那几项，密码、口令不在这里。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    /// ssh 的目标：`~/.ssh/config` 里的主机别名、`user@host` 或 `ssh://user@host:port`
    pub destination: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub forward_agent: bool,
    /// 用密码登录，不试密钥
    #[serde(default, skip_serializing_if = "is_false")]
    pub password_auth: bool,
    /// 私钥文件（这台电脑上的路径）；空着用 ssh 默认的
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// 跳板机：另一台服务器的 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 一个修改动作。kuaid 收到一串就依次执行；做不了的（工作区已经删了）报错，整串都不算。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// 把仓库加进列表（已经有了不动）
    AddRepo {
        path: String,
    },
    /// 从列表里去掉仓库（仓库本身不动），各个工作区里的也去掉
    RemoveRepo {
        path: String,
    },
    /// 给仓库起名字；`name` 为空是不要起的名字了
    RenameRepo {
        path: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// 新建工作区。`id` 由发起的一方起，和已有的重了就失败
    AddWorkspace {
        id: String,
        name: String,
    },
    RenameWorkspace {
        id: String,
        name: String,
    },
    /// 删工作区（里面的仓库还在列表里）
    RemoveWorkspace {
        id: String,
    },
    /// 把仓库放进工作区（已经在了不动）；这台电脑上的仓库同时加进列表
    AddToWorkspace {
        id: String,
        repo: RepoRef,
    },
    RemoveFromWorkspace {
        id: String,
        repo: RepoRef,
    },
    /// 换掉服务器列表（这台电脑上的 Kuai 改了服务器以后发）。不在了的服务器上的仓库从各个工作区里去掉
    SetServers {
        servers: Vec<Server>,
    },
}

impl Op {
    /// Windows 上的写法：这台电脑上的仓库的路径换成反斜杠（别的电脑经 kuaid 发来的是正斜杠，
    /// 见 kuaid 的 `dirs::shown`），存下来和这台电脑上的 Kuai 自己加的是同一种写法。
    /// 别的服务器上的仓库原样不动。
    fn native(&self) -> Op {
        let fix = |path: &str| path.replace('/', "\\");
        let fix_ref = |repo: &RepoRef| match repo.server {
            None => RepoRef::local(fix(&repo.path)),
            Some(_) => repo.clone(),
        };
        match self {
            Op::AddRepo { path } => Op::AddRepo { path: fix(path) },
            Op::RemoveRepo { path } => Op::RemoveRepo { path: fix(path) },
            Op::RenameRepo { path, name } => Op::RenameRepo {
                path: fix(path),
                name: name.clone(),
            },
            Op::AddToWorkspace { id, repo } => Op::AddToWorkspace {
                id: id.clone(),
                repo: fix_ref(repo),
            },
            Op::RemoveFromWorkspace { id, repo } => Op::RemoveFromWorkspace {
                id: id.clone(),
                repo: fix_ref(repo),
            },
            other => other.clone(),
        }
    }
}

/// 同一个仓库的几种写法认作一个：Windows 上不分大小写、两种斜杠都认，末尾的斜杠不算。
fn same_path(a: &str, b: &str, windows: bool) -> bool {
    let trim = |path: &str| path.trim_end_matches(['/', '\\']).to_string();
    match windows {
        true => crate::dir::windows_key(&trim(a)) == crate::dir::windows_key(&trim(b)),
        false => trim(a) == trim(b),
    }
}

impl RepoRef {
    pub fn local(path: impl Into<String>) -> RepoRef {
        RepoRef {
            server: None,
            path: path.into(),
        }
    }

    fn same(&self, other: &RepoRef, windows: bool) -> bool {
        self.server == other.server
            && match self.server {
                // 这台电脑上的按这台的规矩比；别的服务器上的不知道是什么系统，原样比
                None => same_path(&self.path, &other.path, windows),
                Some(_) => self.path == other.path,
            }
    }
}

impl Store {
    /// 依次执行这串动作，返回有没有改动（有就把版本号加一）。做不了的动作报错，
    /// 这时 `self` 不动。`windows`：这台电脑是不是 Windows（路径怎么比）。
    pub fn apply(&mut self, ops: &[Op], windows: bool) -> Result<bool> {
        let mut next = self.clone();
        for op in ops {
            let op = match windows {
                true => op.native(),
                false => op.clone(),
            };
            next.apply_one(&op, windows)?;
        }
        next.prune(windows);
        let changed = next != *self;
        if changed {
            next.version = self.version + 1;
            *self = next;
        }
        Ok(changed)
    }

    fn apply_one(&mut self, op: &Op, windows: bool) -> Result<()> {
        match op {
            Op::AddRepo { path } => self.add_repo(path, windows),
            Op::RemoveRepo { path } => {
                self.repos.retain(|p| !same_path(p, path, windows));
                let gone = RepoRef::local(path.clone());
                for workspace in &mut self.workspaces {
                    workspace.repos.retain(|r| !r.same(&gone, windows));
                }
            }
            Op::RenameRepo { path, name } => {
                let key = self.name_key(path, windows);
                match name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    Some(name) => {
                        self.names.insert(key, name.to_string());
                    }
                    None => {
                        self.names.remove(&key);
                    }
                }
            }
            Op::AddWorkspace { id, name } => {
                if id.trim().is_empty() {
                    bail!(tr("工作区的 id 不能为空"));
                }
                if self.workspaces.iter().any(|w| w.id == *id) {
                    bail!("{}", tf!("已有 id 为 {id} 的工作区", id));
                }
                self.workspaces.push(Workspace {
                    id: id.clone(),
                    name: name.trim().to_string(),
                    repos: Vec::new(),
                });
            }
            Op::RenameWorkspace { id, name } => {
                self.workspace_mut(id)?.name = name.trim().to_string();
            }
            Op::RemoveWorkspace { id } => {
                self.workspace_mut(id)?;
                self.workspaces.retain(|w| w.id != *id);
            }
            Op::AddToWorkspace { id, repo } => {
                if repo.server.is_none() {
                    self.add_repo(&repo.path, windows);
                }
                let workspace = self.workspace_mut(id)?;
                if !workspace.repos.iter().any(|r| r.same(repo, windows)) {
                    workspace.repos.push(repo.clone());
                }
            }
            Op::RemoveFromWorkspace { id, repo } => {
                self.workspace_mut(id)?
                    .repos
                    .retain(|r| !r.same(repo, windows));
            }
            Op::SetServers { servers } => {
                self.servers = servers.clone();
            }
        }
        Ok(())
    }

    fn add_repo(&mut self, path: &str, windows: bool) {
        if !path.trim().is_empty() && !self.repos.iter().any(|p| same_path(p, path, windows)) {
            self.repos.push(path.to_string());
        }
    }

    /// 起的名字记在哪个键下：已经有同一个仓库的另一种写法就用它。
    fn name_key(&self, path: &str, windows: bool) -> String {
        self.names
            .keys()
            .find(|key| same_path(key, path, windows))
            .cloned()
            .unwrap_or_else(|| path.to_string())
    }

    fn workspace_mut(&mut self, id: &str) -> Result<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .with_context(|| tf!("工作区 {id} 不存在", id))
    }

    /// 仓库起的名字；没起是 None。
    pub fn name_of(&self, path: &str, windows: bool) -> Option<&str> {
        self.names
            .iter()
            .find(|(key, _)| same_path(key, path, windows))
            .map(|(_, name)| name.as_str())
    }

    /// 这台电脑的仓库列表里有没有这个仓库。
    pub fn has_repo(&self, path: &str, windows: bool) -> bool {
        self.repos.iter().any(|p| same_path(p, path, windows))
    }

    pub fn server(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// 连这台电脑配置的服务器 `id`：它的配置，和要依次经过的跳板机（从直接连的那台起，见
    /// [`Server::jump`]）。没有这台服务器、跳板机不在了或者绕成了圈，报错。
    pub fn route(&self, id: &str) -> Result<(&Server, Vec<&Server>)> {
        let target = self
            .server(id)
            .with_context(|| tf!("未找到服务器 {id}", id))?;
        let mut route: Vec<&Server> = Vec::new();
        let mut at = target;
        while let Some(jump) = at.jump.as_deref() {
            let next = self
                .server(jump)
                .with_context(|| tf!("未找到服务器 {id}", id = jump))?;
            if next.id == target.id || route.iter().any(|seen| seen.id == next.id) {
                bail!(
                    "{}",
                    tf!("服务器 {name} 的跳板机设置有循环", name = target.name)
                );
            }
            route.push(next);
            at = next;
        }
        route.reverse();
        Ok((target, route))
    }

    /// 理一遍：工作区里指向已经不在的服务器的仓库去掉；起的名字只留还用得着的
    /// （列表里的仓库、它们目录下面的子仓库、工作区里的）。
    fn prune(&mut self, windows: bool) {
        let servers = &self.servers;
        for workspace in &mut self.workspaces {
            workspace.repos.retain(|r| match &r.server {
                None => true,
                Some(id) => servers.iter().any(|s| s.id == *id),
            });
        }
        let repos = &self.repos;
        let workspaces = &self.workspaces;
        let inside = |path: &str| {
            repos.iter().any(|repo| {
                let root = repo.trim_end_matches(['/', '\\']);
                let path = path.replace('\\', "/");
                let root = root.replace('\\', "/");
                let (path, root) = match windows {
                    true => (path.to_lowercase(), root.to_lowercase()),
                    false => (path, root),
                };
                path == root
                    || path
                        .strip_prefix(&root)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        let in_workspace = |path: &str| {
            workspaces
                .iter()
                .flat_map(|w| &w.repos)
                .any(|r| r.server.is_none() && same_path(&r.path, path, windows))
        };
        self.names
            .retain(|path, name| !name.trim().is_empty() && (inside(path) || in_workspace(path)));
    }
}

/// 状态文件放在哪：用户的配置目录（和 Kuai 的设置文件同一个目录）。
/// Windows 是 `%APPDATA%\kuai\`；设了 `FR_KUAID_HOME`（测试时不碰在用的那一套）就放在它下面。
/// Linux 是 `$XDG_CONFIG_HOME/kuai/`，没设就是 `~/.config/kuai/`。
pub fn path() -> PathBuf {
    dir().join(FILE)
}

fn dir() -> PathBuf {
    let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(home) = env("FR_KUAID_HOME") {
        return PathBuf::from(home).join("kuai");
    }
    if cfg!(windows) {
        return match env("APPDATA") {
            Some(appdata) => PathBuf::from(appdata).join("kuai"),
            None => PathBuf::from(crate::dir::home())
                .join("AppData")
                .join("Roaming")
                .join("kuai"),
        };
    }
    match env("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config).join("kuai"),
        None => PathBuf::from(crate::dir::home())
            .join(".config")
            .join("kuai"),
    }
}

/// 读状态文件。没有就是空的；坏了（写到一半断电）读上一份，也坏了就从空的开始，
/// 坏的那份改名留着，不被下次写入盖掉。
pub fn load(path: &Path) -> Store {
    match read(path) {
        Ok(Some(store)) => return store,
        Ok(None) => {}
        Err(err) => {
            eprintln!("{err:#}");
            let kept = path.with_extension("json.broken");
            let _ = std::fs::rename(path, kept);
        }
    }
    let backup = path.with_file_name(BACKUP);
    read(&backup).ok().flatten().unwrap_or_default()
}

fn read(path: &Path) -> Result<Option<Store>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| tf!("无法读取 {path}", path = path.display()));
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| tf!("{path} 格式错误", path = path.display()))
}

/// 写状态文件：先写临时文件再改名，原来那份留作上一份。
pub fn save(path: &Path, store: &Store) -> Result<()> {
    let dir = path.parent().context(tr("状态文件的路径无效"))?;
    std::fs::create_dir_all(dir).with_context(|| tf!("无法创建 {path}", path = dir.display()))?;
    let text = serde_json::to_string_pretty(store)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).with_context(|| tf!("无法写入 {path}", path = temp.display()))?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_file_name(BACKUP));
    }
    std::fs::rename(&temp, path).with_context(|| tf!("无法写入 {path}", path = path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_workspace(id: &str, name: &str) -> Op {
        Op::AddWorkspace {
            id: id.into(),
            name: name.into(),
        }
    }

    fn put(id: &str, repo: RepoRef) -> Op {
        Op::AddToWorkspace {
            id: id.into(),
            repo,
        }
    }

    #[test]
    fn 改动依次执行_有改动才加版本号() {
        let mut store = Store::default();
        let ops = [
            add_workspace("w1", " 交付 "),
            put("w1", RepoRef::local("/data/wms")),
            Op::AddRepo {
                path: "/data/web".into(),
            },
        ];
        assert!(store.apply(&ops, false).unwrap());
        assert_eq!(store.version, 1);
        assert_eq!(store.workspaces[0].name, "交付");
        // 放进工作区的本机仓库也进了列表
        assert_eq!(store.repos, ["/data/wms", "/data/web"]);
        // 再做一遍：什么都没变，版本号不动
        assert!(!store.apply(&ops[1..], false).unwrap());
        assert_eq!(store.version, 1);
    }

    #[test]
    fn 有一个做不了_整串都不算() {
        let mut store = Store::default();
        store.apply(&[add_workspace("w1", "a")], false).unwrap();
        let before = store.clone();
        let ops = [
            Op::AddRepo {
                path: "/data/x".into(),
            },
            put("没有这个", RepoRef::local("/data/x")),
        ];
        assert!(store.apply(&ops, false).is_err());
        assert_eq!(store, before);
        // 工作区的 id 重了也不行
        assert!(store.apply(&[add_workspace("w1", "b")], false).is_err());
    }

    #[test]
    fn windows_上同一个仓库的几种写法认作一个() {
        let mut store = Store::default();
        store
            .apply(
                &[
                    add_workspace("w", "a"),
                    put("w", RepoRef::local(r"C:\Users\Feng\Repo")),
                    put("w", RepoRef::local("c:/users/feng/repo/")),
                    Op::RenameRepo {
                        path: "C:/Users/Feng/Repo".into(),
                        name: Some("仓库".into()),
                    },
                ],
                true,
            )
            .unwrap();
        assert_eq!(store.repos, [r"C:\Users\Feng\Repo"]);
        assert_eq!(store.workspaces[0].repos.len(), 1);
        assert_eq!(store.name_of(r"C:\users\feng\repo", true), Some("仓库"));
        // 别的电脑经 kuaid 加的是正斜杠：存成反斜杠，和这台自己加的一样
        let ops = [put("w", RepoRef::local("D:/work/x"))];
        store.apply(&ops, true).unwrap();
        assert_eq!(store.repos[1], r"D:\work\x");
        assert_eq!(store.workspaces[0].repos[1].path, r"D:\work\x");
        // Linux 上大小写不同就是两个仓库
        let mut linux = Store::default();
        let ops = [
            Op::AddRepo { path: "/a".into() },
            Op::AddRepo { path: "/A".into() },
        ];
        linux.apply(&ops, false).unwrap();
        assert_eq!(linux.repos.len(), 2);
    }

    #[test]
    fn 去掉仓库_各个工作区里的也去掉_起的名字也不留() {
        let mut store = Store::default();
        let wms = RepoRef::local("/data/wms");
        store
            .apply(
                &[
                    add_workspace("w1", "a"),
                    add_workspace("w2", "b"),
                    put("w1", wms.clone()),
                    put("w2", wms.clone()),
                    Op::RenameRepo {
                        path: "/data/wms".into(),
                        name: Some("WMS".into()),
                    },
                    // 子仓库起的名字跟着列表里的仓库留着
                    Op::RenameRepo {
                        path: "/data/wms/plugins/a".into(),
                        name: Some("插件 A".into()),
                    },
                ],
                false,
            )
            .unwrap();
        assert_eq!(store.names.len(), 2);
        store
            .apply(
                &[Op::RemoveRepo {
                    path: "/data/wms".into(),
                }],
                false,
            )
            .unwrap();
        assert!(store.repos.is_empty());
        assert!(store.workspaces.iter().all(|w| w.repos.is_empty()));
        assert!(store.names.is_empty());
        // 起的名字清空就是不要了
        store
            .apply(
                &[
                    Op::AddRepo { path: "/r".into() },
                    Op::RenameRepo {
                        path: "/r".into(),
                        name: Some("R".into()),
                    },
                    Op::RenameRepo {
                        path: "/r".into(),
                        name: Some("  ".into()),
                    },
                ],
                false,
            )
            .unwrap();
        assert!(store.names.is_empty());
    }

    #[test]
    fn 服务器不在了_工作区里它上面的仓库跟着去掉() {
        let server = |id: &str| Server {
            id: id.into(),
            name: id.into(),
            destination: format!("root@{id}"),
            ..Server::default()
        };
        let on = |id: &str| RepoRef {
            server: Some(id.into()),
            path: "/data/x".into(),
        };
        let mut store = Store::default();
        store
            .apply(
                &[
                    Op::SetServers {
                        servers: vec![server("s91"), server("s15")],
                    },
                    add_workspace("w", "a"),
                    put("w", on("s91")),
                    put("w", on("s15")),
                ],
                false,
            )
            .unwrap();
        // 别的服务器上的仓库不进这台电脑的列表
        assert!(store.repos.is_empty());
        store
            .apply(
                &[Op::SetServers {
                    servers: vec![server("s15")],
                }],
                false,
            )
            .unwrap();
        assert_eq!(store.workspaces[0].repos, [on("s15")]);
    }

    #[test]
    fn 跳板机按配置串起来_从直接连的那台起() {
        let server = |id: &str, jump: Option<&str>| Server {
            id: id.into(),
            name: id.into(),
            destination: format!("root@{id}"),
            jump: jump.map(str::to_string),
            ..Server::default()
        };
        let mut store = Store {
            servers: vec![
                server("s91", Some("s15")),
                server("s15", Some("s49")),
                server("s49", None),
            ],
            ..Store::default()
        };
        let (target, route) = store.route("s91").unwrap();
        assert_eq!(target.id, "s91");
        let ids: Vec<&str> = route.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s49", "s15"]);
        assert!(store.route("s49").unwrap().1.is_empty());
        assert!(store.route("没有").is_err());
        // 绕成圈、跳板机不在了：报错，不死循环
        store.servers[2].jump = Some("s91".into());
        assert!(store.route("s91").is_err());
        store.servers[2].jump = Some("不在了".into());
        assert!(store.route("s91").is_err());
    }

    #[test]
    fn 写进文件再读回来_坏了读上一份() {
        let dir = std::env::temp_dir().join(format!("fr-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(FILE);
        assert_eq!(load(&path), Store::default(), "没有文件就是空的");
        let mut store = Store::default();
        store
            .apply(
                &[
                    add_workspace("w", "工作"),
                    Op::AddRepo { path: "/a".into() },
                ],
                false,
            )
            .unwrap();
        save(&path, &store).unwrap();
        assert_eq!(load(&path), store);
        let first = store.clone();
        store
            .apply(&[Op::AddRepo { path: "/b".into() }], false)
            .unwrap();
        save(&path, &store).unwrap();
        // 写坏了：读上一份，坏的那份改名留着
        std::fs::write(&path, "{ 写到一半").unwrap();
        assert_eq!(load(&path), first);
        assert!(path.with_extension("json.broken").exists());
        // 旧版本不认得的字段不影响读
        std::fs::write(&path, r#"{"version":3,"repos":["/x"],"以后加的":1}"#).unwrap();
        assert_eq!(load(&path).repos, ["/x"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
