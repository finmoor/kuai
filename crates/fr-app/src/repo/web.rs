//! 托管平台的网页：推送后去创建合并请求，在网页上看提交、分支、仓库。
//!
//! 只做一件事：打开对的网页（本机的浏览器）。不登录、不调平台的接口、不显示合并请求的状态。
//! 网页地址从远程地址推出来（[`fr_core::hosting`]）；github.com 这几个公开的站点直接认得是哪种
//! 平台，别的主机第一次用到时问一次，按主机记在设置里（设置 → Git 里能改）。主机在内网、本机到
//! 不了托管平台的情况不处理。

use fr_core::hosting::{self, Flavor, Web};
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::*;

use super::RepoView;
use super::ops::args;
use crate::ui;

/// 要打开的网页。
#[derive(Clone, Debug)]
pub(super) enum Page {
    /// 仓库首页
    Repo,
    Commit(String),
    Branch(String),
    /// 从这个分支创建合并请求
    NewRequest(String),
}

/// `git config --get-regexp ^remote\..*\.url$` 的输出：（远程名，地址）。远程名里可以有点。
pub(super) fn parse_remote_urls(out: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|line| {
            let (key, url) = line.split_once(' ')?;
            let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
            (!name.is_empty() && !url.trim().is_empty())
                .then(|| (name.to_string(), url.trim().to_string()))
        })
        .collect()
}

pub(super) fn remote_urls_args() -> Vec<String> {
    args(&["config", "--get-regexp", r"^remote\..*\.url$"])
}

impl RepoView {
    /// 一个远程的网页；地址不是托管平台的（本机路径）就没有。
    pub(super) fn web_of(&self, remote: &str) -> Option<Web> {
        self.remote_urls
            .iter()
            .find(|(name, _)| name == remote)
            .and_then(|(_, url)| hosting::web(url))
    }

    /// 这个主机是哪种平台：公开的站点直接认，别的看设置里记的；没记过是 None。
    pub(super) fn flavor_of(&self, host: &str, cx: &App) -> Option<Flavor> {
        Flavor::known(host).or_else(|| self.app.read(cx).settings.hosting.get(host).copied())
    }

    /// 一个远程分支的名字（`origin/feature/x`）拆成（远程，分支）。远程的名字里可以有 `/`：取配置里
    /// 最长的那个前缀；配置里没有的按第一个 `/` 拆。
    pub(super) fn split_remote_ref(&self, name: &str) -> Option<(String, String)> {
        self.remote_names
            .iter()
            .filter_map(|remote| {
                let rest = name.strip_prefix(remote.as_str())?.strip_prefix('/')?;
                Some((remote.clone(), rest.to_string()))
            })
            .max_by_key(|(remote, _)| remote.len())
            .or_else(|| {
                name.split_once('/')
                    .map(|(remote, branch)| (remote.to_string(), branch.to_string()))
            })
    }

    /// 看提交、建合并请求时用哪个远程：当前分支上游所在的，没有就是默认的那个。
    pub(super) fn web_remote(&self) -> String {
        self.upstream_remote()
            .unwrap_or_else(|| self.default_remote())
    }

    /// 有网页可开的远程：（远程名，网页）。
    pub(super) fn web_remotes(&self) -> Vec<(String, Web)> {
        self.remote_urls
            .iter()
            .filter_map(|(name, url)| Some((name.clone(), hosting::web(url)?)))
            .collect()
    }

    /// 「合并请求」在默认远程的平台上的叫法（还不知道是哪种平台时叫合并请求）。
    pub(super) fn request_name(&self, cx: &App) -> &'static str {
        self.web_of(&self.web_remote())
            .and_then(|web| self.flavor_of(&web.host, cx))
            .unwrap_or(Flavor::Other)
            .request()
    }

    /// 打开 `remote` 上的一个网页。不知道这个主机是哪种平台时先问（仓库首页不用问）。
    pub(super) fn open_web(
        &mut self,
        remote: String,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(web) = self.web_of(&remote) else {
            let text = tf!(
                "远程 {remote} 的地址不是托管平台的地址，无法打开网页。",
                remote
            );
            window.push_notification(Notification::warning(text), cx);
            return;
        };
        if matches!(page, Page::Repo) {
            return open(&web.base, window, cx);
        }
        match self.flavor_of(&web.host, cx) {
            Some(flavor) => self.open_web_as(remote, web, flavor, page, window, cx),
            None => self.ask_flavor(remote, web, page, window, cx),
        }
    }

    /// 问这个主机是哪种托管平台，记下来，接着打开刚才要开的网页。
    fn ask_flavor(
        &mut self,
        remote: String,
        web: Web,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choose = super::dialogs::Choose {
            title: tf!("{host} 使用哪种托管平台？", host = web.host.as_str()).into(),
            description: tr(
                "各平台网页的地址写法不同。选择后按主机记住，可在「设置 → Git」中更改。选「其他」时只能打开仓库首页。",
            )
            .into(),
            options: Flavor::ALL
                .iter()
                .map(|flavor| (flavor.label().into(), SharedString::default()))
                .collect(),
            selected: 1,
            checks: Vec::new(),
            ok: tr("确定"),
        };
        self.choose(choose, window, cx, move |this, picked, _, window, cx| {
            let flavor = Flavor::ALL.get(picked).copied().unwrap_or(Flavor::Other);
            let host = web.host.clone();
            this.app.update(cx, |app, cx| {
                app.update_settings(cx, |s| {
                    s.hosting.insert(host, flavor);
                })
            });
            this.open_web_as(
                remote.clone(),
                web.clone(),
                flavor,
                page.clone(),
                window,
                cx,
            );
        });
    }

    fn open_web_as(
        &mut self,
        remote: String,
        web: Web,
        flavor: Flavor,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let url = match &page {
            Page::Repo => Some(web.base.clone()),
            Page::Commit(id) => web.commit(flavor, id),
            Page::Branch(branch) => web.branch(flavor, branch),
            Page::NewRequest(branch) => {
                return self.open_new_request(remote, web, flavor, branch.clone(), window, cx);
            }
        };
        match url {
            Some(url) => open(&url, window, cx),
            // 认不出的平台：只能开首页
            None => open(&web.base, window, cx),
        }
    }

    /// 创建合并请求的页面：先问远程的默认分支是哪个（合并到它），再拼地址。
    fn open_new_request(
        &mut self,
        remote: String,
        web: Web,
        flavor: Flavor,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.default_branch_of(&remote, cx);
        cx.spawn_in(window, async move |_, cx| {
            let target = target.await.filter(|target| *target != branch);
            cx.update(
                |window, cx| match web.new_request(flavor, &branch, target.as_deref()) {
                    Some(url) => open(&url, window, cx),
                    None => {
                        open(&web.base, window, cx);
                        let text = tr("无法拼出创建合并请求的地址，已打开仓库首页。");
                        window.push_notification(Notification::info(text), cx);
                    }
                },
            )
            .ok();
        })
        .detach();
    }

    /// 远程的默认分支：`refs/remotes/<远程>/HEAD` 指着的；没设时按有没有 main、master 猜。
    fn default_branch_of(&self, remote: &str, cx: &mut Context<Self>) -> Task<Option<String>> {
        let guess = ["main", "master", "develop"]
            .into_iter()
            .find(|name| {
                let full = format!("{remote}/{name}");
                self.refs.remotes.iter().any(|branch| branch.name == full)
            })
            .map(str::to_string);
        let Some(channel) = self.channel.clone() else {
            return Task::ready(guess);
        };
        let head = format!("refs/remotes/{remote}/HEAD");
        let read = channel.git(
            &self.entry.path,
            args(&["symbolic-ref", "-q", "--short", &head]),
        );
        let prefix = format!("{remote}/");
        cx.background_spawn(async move {
            read.await
                .ok()
                .filter(|out| out.success())
                .and_then(|out| {
                    String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .strip_prefix(&prefix)
                        .map(str::to_string)
                })
                .or(guess)
        })
    }

    /// 推送成功的通知：带「创建合并请求」（推的是远程的默认分支时不带）；远程说了话时另有
    /// 「查看输出」。远程在输出里给了链接就用它，没有就按平台拼。
    pub(super) fn notify_pushed(
        &mut self,
        text: String,
        branch: String,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 已经推出去的不再提供撤销
        window.remove_notification::<super::undo::UndoNote>(cx);
        let said = self.last_output_text();
        let link = said.as_deref().and_then(hosting::request_link);
        let web = self.web_of(&remote);
        let request = web
            .as_ref()
            .and_then(|web| self.flavor_of(&web.host, cx))
            .unwrap_or(Flavor::Other)
            .request();
        let target = self.default_branch_of(&remote, cx);
        let open_output = self.output_opener_if_said(cx);
        let view = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            // 推的就是默认分支：没有合并请求可建
            let is_default = target.await.as_deref() == Some(branch.as_str());
            cx.update(|window, cx| {
                let can_request = !is_default && (link.is_some() || web.is_some());
                if !can_request && open_output.is_none() {
                    window.push_notification(Notification::success(text), cx);
                    return;
                }
                let note =
                    ui::actions_note(NotificationType::Success, None, text, move |_, _, cx| {
                        let me = cx.entity().downgrade();
                        let mut buttons = Vec::new();
                        if can_request {
                            let (view, me) = (view.clone(), me.clone());
                            let (link, branch, remote) =
                                (link.clone(), branch.clone(), remote.clone());
                            buttons.push(
                                Button::new("push-request")
                                    .primary()
                                    .label(tf!("创建{request}", request))
                                    .on_click(move |_, window, cx| {
                                        me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                        if let Some(link) = &link {
                                            return open(link, window, cx);
                                        }
                                        let (branch, remote) = (branch.clone(), remote.clone());
                                        view.update(cx, |this, cx| {
                                            this.open_web(
                                                remote,
                                                Page::NewRequest(branch),
                                                window,
                                                cx,
                                            )
                                        })
                                        .ok();
                                    }),
                            );
                        }
                        if let Some(open_output) = open_output.clone() {
                            buttons.push(
                                Button::new("view-output").label(tr("查看输出")).on_click(
                                    move |_, window, cx| {
                                        open_output(window, cx);
                                        me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    },
                                ),
                            );
                        }
                        buttons
                    })
                    // 不点也会自己收起：和别的成功通知一样
                    .autohide(!can_request);
                window.push_notification(note, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 「打开 ⌄」菜单里的一组：各远程的网页、创建合并请求。没有能开网页的远程时不加。
    pub(super) fn web_menu(&self, cx: &mut Context<Self>) -> Option<super::menus::MenuFn> {
        let remotes = self.web_remotes();
        if remotes.is_empty() {
            return None;
        }
        let view = cx.entity().downgrade();
        let branch = self.status.as_ref().and_then(|s| s.branch.clone());
        let remote = self.web_remote();
        let request = self.request_name(cx);
        Some(Box::new(move |mut menu, _, _| {
            menu = menu.separator().label(tr("在浏览器中打开"));
            for (name, web) in &remotes {
                let (view, remote) = (view.clone(), name.clone());
                let shown = web
                    .base
                    .split_once("://")
                    .map_or(web.base.as_str(), |(_, rest)| rest);
                menu = menu.item(PopupMenuItem::new(format!("{name}  ·  {shown}")).on_click(
                    move |_, window, cx| {
                        let remote = remote.clone();
                        view.update(cx, |this, cx| this.open_web(remote, Page::Repo, window, cx))
                            .ok();
                    },
                ));
            }
            if let Some(branch) = &branch {
                let (view, branch, remote) = (view.clone(), branch.clone(), remote.clone());
                menu = menu.item(PopupMenuItem::new(tf!("创建{request}…", request)).on_click(
                    move |_, window, cx| {
                        let (branch, remote) = (branch.clone(), remote.clone());
                        view.update(cx, |this, cx| {
                            this.open_web(remote, Page::NewRequest(branch), window, cx)
                        })
                        .ok();
                    },
                ));
            }
            menu
        }))
    }
}

/// 在本机的浏览器里打开。
fn open(url: &str, window: &mut Window, cx: &mut App) {
    crate::shell::report(crate::shell::open_link(url), window, cx);
}

/// 提交右键菜单里的两项：在网页上查看、复制链接。`web` 是（远程，网页，是哪种平台）；提交还没
/// 推送时两项灰着，写明原因。
pub(super) fn commit_items(
    menu: PopupMenu,
    view: &WeakEntity<RepoView>,
    remote: &str,
    web: &Web,
    flavor: Option<Flavor>,
    id: &str,
    unpushed: bool,
) -> PopupMenu {
    let link = flavor.and_then(|flavor| web.commit(flavor, id));
    let (view, remote, commit) = (view.clone(), remote.to_string(), id.to_string());
    let open = PopupMenuItem::new(match unpushed {
        true => tr("在网页上查看此提交（尚未推送）"),
        false => tr("在网页上查看此提交"),
    })
    .disabled(unpushed)
    .on_click(move |_, window, cx| {
        let (remote, commit) = (remote.clone(), commit.clone());
        view.update(cx, |this, cx| {
            this.open_web(remote, Page::Commit(commit), window, cx)
        })
        .ok();
    });
    let menu = menu.item(open);
    // 还不知道是哪种平台时拼不出链接：先点一次「在网页上查看」选了平台才有
    match link {
        Some(link) => menu.item(
            PopupMenuItem::new(tr("复制提交链接"))
                .disabled(unpushed)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                }),
        ),
        None => menu,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_remote_urls;

    #[test]
    fn 读出各远程的地址() {
        let out = "remote.origin.url git@github.com:o/r.git\nremote.up.stream.url https://h/o/r\nremote.x.pushurl nope\nremote..url bad\n";
        assert_eq!(
            parse_remote_urls(out.as_bytes()),
            [
                ("origin".to_string(), "git@github.com:o/r.git".to_string()),
                ("up.stream".to_string(), "https://h/o/r".to_string()),
            ]
        );
        assert!(parse_remote_urls(b"").is_empty());
    }
}
