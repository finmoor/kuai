//! 定时任务要运行的命令：在终端预设的命令后面加上模型、思考深度、「需要确认时」的参数。
//!
//! 各个命令行工具的写法不一样（[`ArgStyle`]），都是在各工具的 `--help` 里核对过的，版本和出处见
//! `docs/design/scheduled-tasks/product-facts.md`。认不出是哪个工具的命令不加参数，提示词等程序
//! 启动以后粘贴进去。
//!
//! 命令里不直接写提示词：kuaid 开终端时把提示词放进环境变量 [`PROMPT_ENV`]，命令里引用它
//! （[`Launch::line`]）。提示词里的引号、换行不会把命令行弄坏，往回翻时命令那一行也不会很长。

use serde::{Deserialize, Serialize};

/// 提示词放在哪个环境变量里。
pub const PROMPT_ENV: &str = "KUAI_TASK_PROMPT";

/// 终端里的程序看这个环境变量知道自己是哪个定时任务运行的。
pub const TASK_ENV: &str = "KUAI_TASK";

/// 终端会话名字里仓库路径和序号之间的分隔符（客户端、kuaid 一样）。
pub const TERM_KEY_SEPARATOR: char = '\u{1f}';

/// 仓库的第 `number` 个终端的会话名字：第 1 个就是仓库路径，之后的在后面加分隔符和序号。
pub fn term_key(repo: &str, number: u32) -> String {
    match number {
        0 | 1 => repo.to_string(),
        n => format!("{repo}{TERM_KEY_SEPARATOR}{n}"),
    }
}

/// 模型、思考深度这些参数是哪个工具的写法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgStyle {
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Antigravity,
}

/// 程序停下来等人确认（改文件、运行命令）时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirm {
    /// 不加参数，按程序自己的设置
    #[default]
    Default,
    /// 只读，不修改文件
    ReadOnly,
    /// 自动接受文件修改
    AcceptEdits,
    /// 自动处理：由程序判断哪些可以直接做
    Auto,
    /// 全部跳过，不经确认执行任何命令
    SkipAll,
}

impl Confirm {
    /// 除了「按程序设置」以外的几种，界面列选项用。
    pub const CHOICES: [Confirm; 4] = [
        Confirm::ReadOnly,
        Confirm::AcceptEdits,
        Confirm::Auto,
        Confirm::SkipAll,
    ];
}

/// 思考深度的几档，从低到高。Claude Code、Antigravity CLI 的 `--effort` 认这五个；Codex 的
/// `model_reasoning_effort`、OpenCode 模型名后面的档位各家模型认的不全一样，原样传过去。
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

impl ArgStyle {
    /// 看命令运行的是哪个程序：跳过前面的 `名字=值`、`env`，取第一个词，去掉目录和 `.exe`
    /// `.cmd` 这类后缀，不分大小写。`CLAUDE_CONFIG_DIR=~/.claude-finn claude` 是 Claude Code。
    pub fn detect(command: &str) -> Option<ArgStyle> {
        let (start, end) = program_span(command)?;
        match program_name(&command[start..end]).as_str() {
            "claude" => Some(ArgStyle::Claude),
            "codex" => Some(ArgStyle::Codex),
            "opencode" => Some(ArgStyle::OpenCode),
            "agy" => Some(ArgStyle::Antigravity),
            _ => None,
        }
    }

    /// 「需要确认时」选这一种要加的参数；这个工具没有对应的写法是 None（界面上灰着）。
    pub fn confirm_args(self, confirm: Confirm) -> Option<&'static str> {
        use Confirm::*;
        match (self, confirm) {
            (_, Default) => Some(""),
            (ArgStyle::Claude, ReadOnly) => Some("--permission-mode plan"),
            (ArgStyle::Claude, AcceptEdits) => Some("--permission-mode acceptEdits"),
            (ArgStyle::Claude, Auto) => Some("--permission-mode auto"),
            (ArgStyle::Claude, SkipAll) => Some("--dangerously-skip-permissions"),
            (ArgStyle::Codex, ReadOnly) => Some("-s read-only"),
            // 沙箱限在仓库里，里面的事不问
            (ArgStyle::Codex, AcceptEdits) => Some("-s workspace-write -a never"),
            (ArgStyle::Codex, Auto) => Some("--approve-for-me"),
            (ArgStyle::Codex, SkipAll) => Some("--dangerously-bypass-approvals-and-sandbox"),
            (ArgStyle::OpenCode, Auto) => Some("--auto"),
            (ArgStyle::OpenCode, _) => None,
            (ArgStyle::Antigravity, ReadOnly) => Some("--mode plan"),
            (ArgStyle::Antigravity, AcceptEdits) => Some("--mode accept-edits"),
            (ArgStyle::Antigravity, Auto) => Some("--mode auto"),
            (ArgStyle::Antigravity, SkipAll) => Some("--dangerously-skip-permissions"),
        }
    }

    /// 这一种在这台主机上能不能用：这个工具有对应的写法，而且不是 root 上 Claude Code 的「全部跳过」
    /// （Claude Code 不许 root 用 `--dangerously-skip-permissions`，一启动就报错退出，2.1.332 实测）。
    pub fn confirm_usable(self, confirm: Confirm, root: bool) -> bool {
        let refused = root && self == ArgStyle::Claude && confirm == Confirm::SkipAll;
        self.confirm_args(confirm).is_some() && !refused
    }

    /// 思考深度能不能单独指定。OpenCode 的写在模型名后面，没选模型就指定不了。
    pub fn effort_needs_model(self) -> bool {
        self == ArgStyle::OpenCode
    }
}

/// 命令里程序那个词从哪到哪（字节位置）：跳过前面的环境变量赋值和 `env`。带引号的路径连引号一起算。
fn program_span(command: &str) -> Option<(usize, usize)> {
    let mut at = 0;
    loop {
        let rest = &command[at..];
        let start = at + (rest.len() - rest.trim_start().len());
        let word = &command[start..];
        if word.is_empty() {
            return None;
        }
        let end = start + word_len(word);
        let text = &command[start..end];
        let assignment = text.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if assignment || text == "env" {
            at = end;
            continue;
        }
        return Some((start, end));
    }
}

/// 开头这个词有多长：到下一个空白为止，引号里的空白不算。
fn word_len(text: &str) -> usize {
    let mut quote: Option<char> = None;
    for (index, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, c) if c.is_whitespace() => return index,
            _ => {}
        }
    }
    text.len()
}

/// 程序的名字：去掉引号、目录、`.exe` `.cmd` `.bat` `.ps1`，小写。
fn program_name(word: &str) -> String {
    let word = word.trim_matches(['"', '\'']);
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = name.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stem) = lower.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    lower
}

/// 提示词怎么交给程序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptVia {
    /// 作为命令的参数（引用 [`PROMPT_ENV`]）
    Arg,
    /// 等程序启动以后粘贴进去、回车
    Paste,
}

/// 拼好的命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    /// 不含提示词的那一段，界面上「将运行」显示它
    pub command: String,
    pub prompt: PromptVia,
    /// 提示词前面要跟的参数：Antigravity CLI 是 `-i`，OpenCode 的交互界面是 `--prompt`，
    /// 别的是空的（提示词直接跟在最后）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_flag: String,
    /// 运行结束程序自己退出（OpenCode 指定了模型时），不留在会话里
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exits: bool,
}

/// 命令在哪种 shell 里输入：引用环境变量的写法不一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// bash、zsh、sh、fish
    Posix,
    PowerShell,
    /// `cmd.exe`：提示词里有双引号、换行时参数会坏，尽量不用
    Cmd,
}

impl Shell {
    /// 引用提示词的那一段。
    pub fn prompt_ref(self) -> String {
        match self {
            Shell::Posix => format!("\"${PROMPT_ENV}\""),
            Shell::PowerShell => format!("$env:{PROMPT_ENV}"),
            Shell::Cmd => format!("\"%{PROMPT_ENV}%\""),
        }
    }
}

impl Launch {
    /// 在 shell 里输入的整行。提示词是粘贴的就只有命令。
    pub fn line(&self, shell: Shell) -> String {
        match self.prompt {
            PromptVia::Paste => self.command.clone(),
            PromptVia::Arg if self.prompt_flag.is_empty() => {
                format!("{} {}", self.command, shell.prompt_ref())
            }
            PromptVia::Arg => format!(
                "{} {} {}",
                self.command,
                self.prompt_flag,
                shell.prompt_ref()
            ),
        }
    }
}

/// 在预设的命令 `base` 后面加上模型、思考深度、「需要确认时」的参数。
///
/// `style` 是 None（认不出是哪个工具）时什么都不加，提示词用粘贴的。`model`、`effort` 空着是不指定。
/// 这个工具没有对应写法的 `confirm`（[`ArgStyle::confirm_args`] 是 None）当成「按程序设置」。
pub fn compose(
    base: &str,
    style: Option<ArgStyle>,
    model: &str,
    effort: &str,
    confirm: Confirm,
) -> Launch {
    let base = base.trim();
    let (model, effort) = (model.trim(), effort.trim());
    let Some(style) = style else {
        return Launch {
            command: base.to_string(),
            prompt: PromptVia::Paste,
            prompt_flag: String::new(),
            exits: false,
        };
    };
    let mut command = base.to_string();
    let mut prompt_flag = "";
    let mut exits = false;
    fn add(command: &mut String, text: &str) {
        if !text.is_empty() {
            command.push(' ');
            command.push_str(text);
        }
    }
    match style {
        ArgStyle::Claude => {
            if !model.is_empty() {
                add(&mut command, &format!("--model {model}"));
            }
            if !effort.is_empty() {
                add(&mut command, &format!("--effort {effort}"));
            }
        }
        ArgStyle::Codex => {
            if !model.is_empty() {
                add(&mut command, &format!("-m {model}"));
            }
            if !effort.is_empty() {
                add(&mut command, &format!("-c model_reasoning_effort={effort}"));
            }
        }
        ArgStyle::Antigravity => {
            if !model.is_empty() {
                add(&mut command, &format!("--model {model}"));
            }
            if !effort.is_empty() {
                add(&mut command, &format!("--effort {effort}"));
            }
            // 交互运行第一条消息、之后留在会话里
            prompt_flag = "-i";
        }
        // 交互界面没有模型参数：指定了模型只能用 `run`（运行结束即退出），思考深度写在模型名后面。
        // 预设的命令里程序名后面的参数是给交互界面的，`run` 不一定认，不带过去
        ArgStyle::OpenCode if !model.is_empty() => {
            let end = program_span(base).map_or(base.len(), |(_, end)| end);
            command = base[..end].to_string();
            let variant = match effort.is_empty() {
                true => String::new(),
                false => format!("#{effort}"),
            };
            add(&mut command, &format!("run -m {model}{variant}"));
            exits = true;
        }
        ArgStyle::OpenCode => prompt_flag = "--prompt",
    }
    add(&mut command, style.confirm_args(confirm).unwrap_or(""));
    Launch {
        command,
        prompt: PromptVia::Arg,
        prompt_flag: prompt_flag.to_string(),
        exits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(base: &str, model: &str, effort: &str, confirm: Confirm) -> String {
        compose(base, ArgStyle::detect(base), model, effort, confirm).line(Shell::Posix)
    }

    #[test]
    fn 认出是哪个工具() {
        assert_eq!(ArgStyle::detect("claude"), Some(ArgStyle::Claude));
        assert_eq!(ArgStyle::detect("claude -c"), Some(ArgStyle::Claude));
        assert_eq!(
            ArgStyle::detect("codex --no-alt-screen"),
            Some(ArgStyle::Codex)
        );
        assert_eq!(ArgStyle::detect("opencode"), Some(ArgStyle::OpenCode));
        assert_eq!(ArgStyle::detect("agy"), Some(ArgStyle::Antigravity));
        // 前面的环境变量、env 跳过
        assert_eq!(
            ArgStyle::detect("CLAUDE_CONFIG_DIR=~/.claude-finn claude"),
            Some(ArgStyle::Claude)
        );
        assert_eq!(
            ArgStyle::detect("env A=1 B=\"x y\" codex"),
            Some(ArgStyle::Codex)
        );
        // 带目录、带后缀、带引号、大小写
        assert_eq!(
            ArgStyle::detect("~/.local/bin/claude"),
            Some(ArgStyle::Claude)
        );
        assert_eq!(
            ArgStyle::detect(r#""C:\Program Files\nodejs\Codex.CMD" --no-alt-screen"#),
            Some(ArgStyle::Codex)
        );
        assert_eq!(ArgStyle::detect("claude.exe"), Some(ArgStyle::Claude));
        // 认不出的
        assert_eq!(ArgStyle::detect("npm run dev"), None);
        assert_eq!(ArgStyle::detect("devbot chat"), None);
        assert_eq!(ArgStyle::detect("claude-wrapper"), None);
        assert_eq!(ArgStyle::detect(""), None);
        assert_eq!(ArgStyle::detect("A=1"), None);
        // 参数里出现的不算
        assert_eq!(ArgStyle::detect("echo claude"), None);
    }

    #[test]
    fn 各个工具的写法() {
        // 和原型里「将运行」那一行一样
        assert_eq!(
            line("claude", "fable", "high", Confirm::ReadOnly),
            r#"claude --model fable --effort high --permission-mode plan "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            line(
                "codex --no-alt-screen",
                "gpt-6.2-sol",
                "xhigh",
                Confirm::Default
            ),
            r#"codex --no-alt-screen -m gpt-6.2-sol -c model_reasoning_effort=xhigh "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            line("agy", "", "", Confirm::AcceptEdits),
            r#"agy --mode accept-edits -i "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            line("agy", "gemini-4-hydrogen-high", "max", Confirm::Auto),
            r#"agy --model gemini-4-hydrogen-high --effort max --mode auto -i "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            line(
                "CLAUDE_CONFIG_DIR=~/.claude-finn claude",
                "opus",
                "medium",
                Confirm::AcceptEdits
            ),
            r#"CLAUDE_CONFIG_DIR=~/.claude-finn claude --model opus --effort medium --permission-mode acceptEdits "$KUAI_TASK_PROMPT""#
        );
        // 什么都不指定：只多一个提示词
        assert_eq!(
            line("claude", "", "", Confirm::Default),
            r#"claude "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            line("codex --no-alt-screen", " ", "", Confirm::SkipAll),
            r#"codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox "$KUAI_TASK_PROMPT""#
        );
    }

    #[test]
    fn opencode_选了模型用_run() {
        let launch = compose(
            "opencode",
            Some(ArgStyle::OpenCode),
            "openai/gpt-6-astra",
            "high",
            Confirm::Auto,
        );
        assert_eq!(
            launch.command,
            "opencode run -m openai/gpt-6-astra#high --auto"
        );
        assert!(launch.exits);
        assert_eq!(
            launch.line(Shell::Posix),
            r#"opencode run -m openai/gpt-6-astra#high --auto "$KUAI_TASK_PROMPT""#
        );
        // 没选思考深度：模型名后面不带档位
        assert_eq!(
            line("opencode", "openai/gpt-6-astra", "", Confirm::Default),
            r#"opencode run -m openai/gpt-6-astra "$KUAI_TASK_PROMPT""#
        );
        // 没选模型：交互界面，提示词用 --prompt；思考深度指定不了，不写
        let launch = compose(
            "opencode",
            Some(ArgStyle::OpenCode),
            "",
            "high",
            Confirm::Auto,
        );
        assert_eq!(launch.command, "opencode --auto");
        assert!(!launch.exits);
        assert_eq!(
            launch.line(Shell::Posix),
            r#"opencode --auto --prompt "$KUAI_TASK_PROMPT""#
        );
        // 预设里给交互界面的参数不带到 run 上
        assert_eq!(
            line(
                "XDG_CONFIG_HOME=~/alt opencode --standalone",
                "openai/gpt-6.2-sol",
                "xhigh",
                Confirm::Default
            ),
            r#"XDG_CONFIG_HOME=~/alt opencode run -m openai/gpt-6.2-sol#xhigh "$KUAI_TASK_PROMPT""#
        );
        assert!(ArgStyle::OpenCode.effort_needs_model());
        assert!(!ArgStyle::Claude.effort_needs_model());
    }

    #[test]
    fn 认不出的工具不加参数_提示词粘贴() {
        let launch = compose("devbot chat", None, "some-model", "high", Confirm::SkipAll);
        assert_eq!(launch.command, "devbot chat");
        assert_eq!(launch.prompt, PromptVia::Paste);
        assert_eq!(launch.line(Shell::Posix), "devbot chat");
        assert_eq!(launch.line(Shell::PowerShell), "devbot chat");
    }

    #[test]
    fn 这个工具没有的确认方式当成按程序设置() {
        assert_eq!(ArgStyle::OpenCode.confirm_args(Confirm::ReadOnly), None);
        assert_eq!(ArgStyle::OpenCode.confirm_args(Confirm::SkipAll), None);
        assert_eq!(ArgStyle::OpenCode.confirm_args(Confirm::Default), Some(""));
        assert_eq!(
            line("opencode", "", "", Confirm::ReadOnly),
            r#"opencode --prompt "$KUAI_TASK_PROMPT""#
        );
        // 别的三个工具四种都有
        for style in [ArgStyle::Claude, ArgStyle::Codex, ArgStyle::Antigravity] {
            for confirm in Confirm::CHOICES {
                assert!(
                    style
                        .confirm_args(confirm)
                        .is_some_and(|args| !args.is_empty()),
                    "{style:?} {confirm:?}"
                );
                assert!(style.confirm_usable(confirm, false));
            }
        }
        // root 上 Claude Code 不能「全部跳过」，别的照常
        assert!(!ArgStyle::Claude.confirm_usable(Confirm::SkipAll, true));
        assert!(ArgStyle::Claude.confirm_usable(Confirm::AcceptEdits, true));
        assert!(ArgStyle::Codex.confirm_usable(Confirm::SkipAll, true));
        assert!(!ArgStyle::OpenCode.confirm_usable(Confirm::ReadOnly, false));
    }

    #[test]
    fn 各种_shell_里引用提示词() {
        let launch = compose(
            "claude",
            Some(ArgStyle::Claude),
            "fable",
            "",
            Confirm::Default,
        );
        assert_eq!(
            launch.line(Shell::Posix),
            r#"claude --model fable "$KUAI_TASK_PROMPT""#
        );
        assert_eq!(
            launch.line(Shell::PowerShell),
            "claude --model fable $env:KUAI_TASK_PROMPT"
        );
        assert_eq!(
            launch.line(Shell::Cmd),
            r#"claude --model fable "%KUAI_TASK_PROMPT%""#
        );
        let launch = compose("agy", Some(ArgStyle::Antigravity), "", "", Confirm::Default);
        assert_eq!(
            launch.line(Shell::PowerShell),
            "agy -i $env:KUAI_TASK_PROMPT"
        );
    }

    #[test]
    fn 存成的样子() {
        let launch = compose(
            "claude",
            Some(ArgStyle::Claude),
            "fable",
            "",
            Confirm::Default,
        );
        assert_eq!(
            serde_json::to_string(&launch).unwrap(),
            r#"{"command":"claude --model fable","prompt":"arg"}"#
        );
        let launch = compose(
            "opencode",
            Some(ArgStyle::OpenCode),
            "openai/gpt-6-astra",
            "",
            Confirm::Default,
        );
        let json = serde_json::to_string(&launch).unwrap();
        assert_eq!(
            json,
            r#"{"command":"opencode run -m openai/gpt-6-astra","prompt":"arg","exits":true}"#
        );
        assert_eq!(serde_json::from_str::<Launch>(&json).unwrap(), launch);
        assert_eq!(
            serde_json::to_string(&(ArgStyle::OpenCode, Confirm::AcceptEdits)).unwrap(),
            r#"["opencode","accept_edits"]"#
        );
    }
}
