//! 自定义命令：新建终端的预设（`presets.rs`）除了出现在新建终端的菜单里，还可以出现在提交、分支、
//! 文件的右键菜单里，命令里的 `$sha`、`$branch`、`$file`、`$repo`、`$remote` 换成点的那个对象。
//!
//! 最用得上的是把对象交给终端里的 AI：「让 Claude Code 评审此提交」就是 `claude "评审提交 $sha"`
//! 在新终端里跑。菜单里只写名称和图标，不写命令。默认开一个终端运行；勾了「在后台运行」的不开
//! 终端，借 git 的 shell 别名在仓库目录里执行，输出显示在底部的「输出」里。

use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::RepoView;
use super::ops::Step;
use crate::presets::{self, Place, Preset, Vars};

/// 右键点的是什么：没有的那几样空着。
#[derive(Clone, Default)]
pub(super) struct At {
    pub sha: Option<String>,
    pub branch: Option<String>,
    pub file: Option<String>,
}

impl At {
    pub(super) fn commit(id: &str) -> Self {
        Self {
            sha: Some(id.to_string()),
            ..Self::default()
        }
    }

    pub(super) fn branch(name: &str) -> Self {
        Self {
            branch: Some(name.to_string()),
            ..Self::default()
        }
    }

    pub(super) fn file(path: &str) -> Self {
        Self {
            file: Some(path.to_string()),
            ..Self::default()
        }
    }
}

/// 把这种对象的自定义命令加到右键菜单的最下面（没有就不加）。在构建菜单的回调里调。
pub(super) fn items(
    menu: PopupMenu,
    view: &WeakEntity<RepoView>,
    place: Place,
    at: At,
    cx: &App,
) -> PopupMenu {
    let commands = presets::for_place(place, cx);
    if commands.is_empty() {
        return menu;
    }
    let mut menu = menu.separator();
    for preset in commands {
        let icon =
            presets::icon(&preset.icon).text_color(presets::color(&preset.icon, preset.hue, cx));
        let (view, at) = (view.clone(), at.clone());
        menu = menu.item(PopupMenuItem::new(preset.name.clone()).icon(icon).on_click(
            move |_, window, cx| {
                let (preset, at) = (preset.clone(), at.clone());
                view.update(cx, |this, cx| this.run_custom(preset, at, window, cx))
                    .ok();
            },
        ));
    }
    menu
}

impl RepoView {
    /// 命令里的变量换成什么：没点着的那几样用当前的（当前分支、HEAD）。
    pub(super) fn custom_vars(&self, at: &At, cx: &App) -> Vars {
        let status = self.status.as_ref();
        let branch = at
            .branch
            .clone()
            .or_else(|| status.and_then(|s| s.branch.clone()))
            .unwrap_or_default();
        let sha = at
            .sha
            .clone()
            .or_else(|| {
                // 点的是分支：它指着的提交
                at.branch.as_ref().and_then(|name| {
                    self.refs
                        .branches
                        .iter()
                        .chain(&self.refs.remotes)
                        .find(|b| b.name == *name)
                        .map(|b| b.target.clone())
                })
            })
            .or_else(|| status.and_then(|s| s.head.clone()))
            .unwrap_or_default();
        Vars {
            sha,
            branch,
            file: at.file.clone().unwrap_or_default(),
            repo: self.app.read(cx).repo_name(&self.entry).to_string(),
            remote: self.web_remote(),
        }
    }

    /// 运行一条自定义命令：变量换好以后，在新终端里输入，或者在后台执行。
    pub(super) fn run_custom(
        &mut self,
        preset: Preset,
        at: At,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = presets::expand(&preset.command, &self.custom_vars(&at, cx));
        if !preset.background {
            let start = presets::Start {
                command,
                ..presets::Start::of(&preset)
            };
            self.new_terminal_with(String::new(), Some(start), window, cx);
            return;
        }
        // 借 git 的 shell 别名在仓库根目录里执行（本机和主机上都是 sh）
        let step = Step::local(vec![
            "-c".into(),
            format!("alias.fr-run=!{command}"),
            "fr-run".into(),
        ])
        .shown(command)
        .live();
        let name = preset.name.clone();
        self.run_steps(
            preset.name,
            vec![step],
            window,
            cx,
            Box::new(move |this, result, window, cx| match result {
                Ok(_) => {
                    let text = tf!("「{name}」已完成", name = name.as_str());
                    // 后台运行的命令只有这里看得到输出：通知留着，等人点「查看输出」或关掉
                    // （和推送成功带「创建合并请求」的那条一样）
                    let note = match this.output_opener(cx) {
                        Some(open) => crate::ui::action_note(
                            gpui_kit::component::notification::NotificationType::Success,
                            text,
                            move |_, _, cx| {
                                let (open, me) = (open.clone(), cx.entity().downgrade());
                                gpui_kit::component::button::Button::new("custom-output")
                                    .label(tr("查看输出"))
                                    .on_click(move |_, window, cx| {
                                        open(window, cx);
                                        me.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                                    })
                            },
                        ),
                        None => Notification::success(text),
                    };
                    window.push_notification(note, cx);
                }
                Err(text) => this.failure(&name, text, window, cx),
            }),
        );
    }

    /// 给预设设的快捷键、新建终端菜单里选了预设：命令里要是用了变量，按当前的分支、HEAD 换。
    pub(super) fn expand_start(&self, start: presets::Start, cx: &App) -> presets::Start {
        if !start.command.contains('$') {
            return start;
        }
        presets::Start {
            command: presets::expand(&start.command, &self.custom_vars(&At::default(), cx)),
            ..start
        }
    }
}
