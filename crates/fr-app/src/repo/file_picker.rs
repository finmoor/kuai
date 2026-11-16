//! 选一个已跟踪的文件：看它的历史或逐行追溯。
//!
//! 没有改动的文件不在「本地修改」里，右键菜单够不着；这里列出 `git ls-files` 的全部文件，
//! 输入几个字筛选，上下键选，回车看文件历史，也可以点右边的「逐行追溯」。

use fr_core::i18n::tr;
use fr_core::tf;
use futures::future::BoxFuture;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use fr_channel::GitOutput;

/// 最多显示多少行；再多就让用户多输几个字
const MAX_ROWS: usize = 200;

/// 选中了一个文件。`blame` 为真是要看逐行追溯，否则看文件历史。
pub(super) struct Picked {
    pub path: String,
    pub blame: bool,
}

enum Files {
    Loading,
    Ready(Vec<SharedString>),
    Failed(SharedString),
}

pub(super) struct FilePicker {
    input: Entity<InputState>,
    files: Files,
    selected: usize,
    _load: Task<()>,
    _subscription: Subscription,
}

impl EventEmitter<Picked> for FilePicker {}

impl FilePicker {
    pub(super) fn new(
        list: BoxFuture<'static, anyhow::Result<GitOutput>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("文件名或路径中的关键字")));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.selected = 0;
                cx.notify();
            }
        });
        let load = cx.spawn(async move |this, cx| {
            let files = match list.await.and_then(|out| out.ok()) {
                Ok(bytes) => Files::Ready(parse_files(&bytes)),
                Err(err) => Files::Failed(tf!("无法读取文件列表：{err}", err).into()),
            };
            this.update(cx, |this, cx| {
                this.files = files;
                cx.notify();
            })
            .ok();
        });
        Self {
            input,
            files: Files::Loading,
            selected: 0,
            _load: load,
            _subscription: subscription,
        }
    }

    /// 匹配的文件（最多 `MAX_ROWS` 个）和匹配的总数。
    fn matches(&self, cx: &App) -> (Vec<SharedString>, usize) {
        let Files::Ready(files) = &self.files else {
            return (Vec::new(), 0);
        };
        let needle = self.input.read(cx).value().trim().to_lowercase();
        filter_files(files, &needle)
    }

    fn pick_selected(&mut self, blame: bool, cx: &mut Context<Self>) {
        let (rows, _) = self.matches(cx);
        if let Some(path) = rows.get(self.selected.min(rows.len().saturating_sub(1))) {
            cx.emit(Picked {
                path: path.to_string(),
                blame,
            });
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.matches(cx).0.len();
        if count == 0 {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.selected = (self.selected + 1) % count,
            "up" => self.selected = (self.selected + count - 1) % count,
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

/// `git ls-files -z` 的输出拆成路径。
fn parse_files(bytes: &[u8]) -> Vec<SharedString> {
    let mut files: Vec<SharedString> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| SharedString::from(String::from_utf8_lossy(part).into_owned()))
        .collect();
    // 有冲突的文件在暂存区里有两三个版本，git ls-files 每个版本各列一行（相邻）
    files.dedup();
    files
}

/// 按输入的字筛选（不分大小写）：文件名里有的排在前面，其次是路径里有的。
fn filter_files(files: &[SharedString], needle: &str) -> (Vec<SharedString>, usize) {
    if needle.is_empty() {
        return (files.iter().take(MAX_ROWS).cloned().collect(), files.len());
    }
    let (mut by_name, mut by_path) = (Vec::new(), Vec::new());
    let mut total = 0;
    for file in files {
        let lower = file.to_lowercase();
        if !lower.contains(needle) {
            continue;
        }
        total += 1;
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        if name.contains(needle) {
            if by_name.len() < MAX_ROWS {
                by_name.push(file.clone());
            }
        } else if by_path.len() < MAX_ROWS {
            by_path.push(file.clone());
        }
    }
    by_name.extend(by_path);
    by_name.truncate(MAX_ROWS);
    (by_name, total)
}

impl Render for FilePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_height = (window.viewport_size().height - px(280.)).clamp(px(120.), px(380.));
        let theme = cx.theme();
        let (muted, hover, active, danger) = (
            theme.muted_foreground,
            theme.list_hover,
            theme.list_active,
            theme.danger,
        );
        let (matches, total) = self.matches(cx);
        let selected = self.selected.min(matches.len().saturating_sub(1));
        let shown = matches.len();
        let rows: Vec<AnyElement> = matches
            .into_iter()
            .enumerate()
            .map(|(ix, path)| {
                let (for_history, for_blame) = (path.to_string(), path.to_string());
                h_flex()
                    .id(ix)
                    .px_2()
                    .h(px(26.))
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(ix == selected, |row| row.bg(active))
                    .hover(|s| s.bg(hover))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(Picked {
                            path: for_history.clone(),
                            blame: false,
                        })
                    }))
                    .child(crate::ui::clipped("path", path).flex_1().text_sm())
                    .child(
                        Button::new(("blame", ix))
                            .xsmall()
                            .ghost()
                            .label(tr("逐行追溯"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(Picked {
                                    path: for_blame.clone(),
                                    blame: true,
                                })
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let note: Option<(SharedString, Hsla)> = match &self.files {
            Files::Loading => Some((tr("正在读取文件列表…").into(), muted)),
            Files::Failed(err) => Some((err.clone(), danger)),
            Files::Ready(_) if total == 0 => Some((tr("没有匹配的文件").into(), muted)),
            Files::Ready(_) if total > shown => Some((
                tf!(
                    "共 {total} 个匹配项，仅显示前 {shown} 个，请输入更多字符",
                    total,
                    shown
                )
                .into(),
                muted,
            )),
            Files::Ready(_) => None,
        };
        v_flex()
            .gap_2()
            .capture_key_down(cx.listener(Self::on_key))
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("file-picker-list")
                    .max_h(list_height)
                    .overflow_y_scroll()
                    .child(v_flex().gap_0p5().children(rows)),
            )
            .children(
                note.map(|(text, color)| div().px_2().text_xs().text_color(color).child(text)),
            )
            .child(div().px_2().text_xs().text_color(muted).child(tr(
                "按 Enter 或点击查看文件历史；右侧的「逐行追溯」显示每行的修改者。",
            )))
    }
}

/// 把选文件的对话框弹出来。
pub(super) fn open(picker: Entity<FilePicker>, window: &mut Window, cx: &mut App) {
    let first_build = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        // 对话框打开的那一刻焦点会被它拿走，打开后再放到输入框
        if first_build.replace(false) {
            let input = picker.read(cx).input.clone();
            input.focus_handle(cx).focus(window, cx);
        }
        let on_enter = picker.clone();
        dialog
            .title(tr("选择文件"))
            .w(px(640.))
            .on_ok(move |_, _, cx| {
                on_enter.update(cx, |picker, cx| picker.pick_selected(false, cx));
                false
            })
            .child(picker.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROWS, filter_files, parse_files};
    use gpui_kit::SharedString;

    fn files(names: &[&str]) -> Vec<SharedString> {
        names
            .iter()
            .map(|n| SharedString::from(n.to_string()))
            .collect()
    }

    #[test]
    fn 拆出文件列表() {
        let got = parse_files(b"a.txt\0src/b.rs\0");
        assert_eq!(got, files(&["a.txt", "src/b.rs"]));
    }

    #[test]
    fn 有冲突的文件只列一次() {
        let got = parse_files(b"a.txt\0a.txt\0a.txt\0b.txt\0");
        assert_eq!(got, files(&["a.txt", "b.txt"]));
    }

    #[test]
    fn 文件名里有的排在前面() {
        let all = files(&["main/readme.md", "src/main.rs", "docs/other.md"]);
        let (got, total) = filter_files(&all, "main");
        assert_eq!(total, 2);
        assert_eq!(got, files(&["src/main.rs", "main/readme.md"]));
    }

    #[test]
    fn 不输字时列出全部并报总数() {
        let all: Vec<SharedString> = (0..MAX_ROWS + 5)
            .map(|i| SharedString::from(format!("f{i}")))
            .collect();
        let (got, total) = filter_files(&all, "");
        assert_eq!((got.len(), total), (MAX_ROWS, MAX_ROWS + 5));
    }
}
