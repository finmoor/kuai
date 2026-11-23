//! 诊断（原型见 `docs/design/reliability/` 的「诊断」）。
//!
//! - 仓库页连不上主机时，「重试」旁边的「诊断…」打开 [`Diagnose`]：逐步检查本机的 ssh、各台跳板机、
//!   登录、kuaid、主机上的 git、映射用的 WinFsp，哪一步失败就停在哪，后面写「未检查」。连接走的是
//!   平常那条路（`AppState::connect`）：诊断通过时仓库页也就连上了。哪一步失败靠
//!   [`fr_channel::login_failure`]（登录阶段的失败带着是哪一台）。
//! - 主机设置里的「测试连接」打开同一个对话框（[`Diagnose::open_form`]）：拿表单里填的（还没保存）
//!   只登录一次（[`Connection::probe`]），看系统、主机上的 git、这一版的 kuaid 装了没有；不保存，
//!   也不在主机上装、启动 kuaid。
//! - 设置 →「关于」里的「复制诊断信息…」打开 [`Summary`]：各主机现在的连接情况和最近的错误，不重新连。
//! - 复制的文字默认隐藏主机地址、用户名（[`Redact`]）；密码、口令、令牌本来就不在里面。
//! - 最近的错误（连接失败、git 命令失败）只在内存里留 [`MAX_ERRORS`] 条（[`record_error`]），不落盘。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local};
use fr_channel::{ConnectOptions, Connection, FailedAt, LoginFailure, Probe};
use fr_core::i18n::tr;
use fr_core::tf;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::app_state::{AppState, ConnStatus};
use crate::machines::split_relay;
use crate::settings::{Auth, Server};
use crate::ui;

// ---------- 最近的错误 ----------

/// 最近的错误最多留几条
pub const MAX_ERRORS: usize = 20;
/// 一条错误最多留多少字
const MAX_ERROR_CHARS: usize = 300;

struct ErrorEntry {
    at: DateTime<Local>,
    /// 哪台主机、哪个仓库
    source: String,
    text: String,
}

static ERRORS: Mutex<VecDeque<ErrorEntry>> = Mutex::new(VecDeque::new());

/// 记一条错误（连接失败、git 命令失败）：诊断信息里带上最近的 [`MAX_ERRORS`] 条。只在内存里。
/// 地址里的账号密码由调用的一方去掉（git 的输出记活动时已经去掉了；连接的报错里没有）。
pub fn record_error(source: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    // 太长的截断
    let mut short: String = text.chars().take(MAX_ERROR_CHARS).collect();
    if text.chars().count() > MAX_ERROR_CHARS {
        short.push('…');
    }
    let mut errors = ERRORS.lock().unwrap();
    errors.push_back(ErrorEntry {
        at: Local::now(),
        source: source.to_string(),
        text: short,
    });
    while errors.len() > MAX_ERRORS {
        errors.pop_front();
    }
}

/// 最近的错误，一行一条（新的在后）。
fn errors_text() -> Vec<String> {
    ERRORS
        .lock()
        .unwrap()
        .iter()
        .map(|entry| {
            let text = entry.text.replace(['\r', '\n'], " ");
            format!(
                "  {}  {}  {}",
                entry.at.format("%m-%d %H:%M:%S"),
                entry.source,
                text
            )
        })
        .collect()
}

// ---------- 步骤 ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// 还没轮到、正在检查
    Wait,
    Ok,
    Bad,
    /// 前面失败了，没检查
    Skip,
    /// 不算通过也不算失败（WinFsp 没装：只有映射用得到）
    Note,
}

#[derive(Clone, Debug)]
struct Step {
    name: String,
    state: State,
    text: String,
    /// 灰字补充
    note: String,
    /// ssh 的原话
    raw: String,
    /// 没检查的原因（复制出去时写在括号里）
    why: String,
}

impl Step {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: State::Wait,
            text: tr("正在检查…").into(),
            note: String::new(),
            raw: String::new(),
            why: String::new(),
        }
    }

    fn set(&mut self, state: State, text: impl Into<String>, note: impl Into<String>) {
        self.state = state;
        self.text = text.into();
        self.note = note.into();
    }

    fn skip(&mut self, why: &str) {
        let text = match why.is_empty() {
            true => tr("未检查").to_string(),
            false => tf!("未检查：{why}", why),
        };
        self.set(State::Skip, text, "");
        self.why = why.to_string();
    }
}

/// 诊断哪台主机。
#[derive(Clone, Debug)]
struct Target {
    id: String,
    name: String,
    destination: String,
    /// 依次经过的跳板机：名字和地址
    jumps: Vec<(String, String)>,
    /// 经别的电脑的 kuaid 转接（`machines` 里的转接）：那台的名字
    via: Option<String>,
    auth: Auth,
    identity_file: Option<String>,
}

impl Target {
    fn of(app: &AppState, id: &str) -> Option<Target> {
        if let Some((via, server)) = split_relay(id) {
            let via_name = app
                .server(via)
                .map_or_else(|| via.to_string(), |s| s.name.clone());
            return Some(Target {
                id: id.to_string(),
                name: {
                    let name = app.machine_name(id).to_string();
                    if name.is_empty() {
                        server.to_string()
                    } else {
                        name
                    }
                },
                destination: String::new(),
                jumps: Vec::new(),
                via: Some(via_name),
                auth: Auth::Key,
                identity_file: None,
            });
        }
        let server: &Server = app.server(id)?;
        let jumps = app
            .settings
            .route(id)
            .into_iter()
            .map(|jump| (jump.name.clone(), jump.destination.clone()))
            .collect();
        Some(Target {
            id: id.to_string(),
            name: server.name.clone(),
            destination: server.destination.clone(),
            jumps,
            via: None,
            auth: server.auth,
            identity_file: server.identity_file.clone(),
        })
    }

    /// 登录用的是什么（给人看）。
    fn auth_text(&self) -> String {
        match (self.auth, &self.identity_file) {
            (Auth::Password, _) => tr("用的是密码").into(),
            (Auth::Key, Some(file)) => tf!("用的是密钥 {file}", file),
            (Auth::Key, None) => tr("用的是 ssh 默认的密钥（~/.ssh、ssh-agent）").into(),
        }
    }
}

/// 第几步是什么（跳板机有几台，步骤就多几步）。
struct Layout {
    jumps: usize,
}

impl Layout {
    const SSH: usize = 0;
    fn jump(&self, index: usize) -> usize {
        1 + index
    }
    fn login(&self) -> usize {
        1 + self.jumps
    }
    fn kuaid(&self) -> usize {
        2 + self.jumps
    }
    fn git(&self) -> usize {
        3 + self.jumps
    }
    fn winfsp(&self) -> usize {
        4 + self.jumps
    }
}

fn initial_steps(target: &Target) -> Vec<Step> {
    let mut steps = vec![Step::new(tr("本机的 ssh"))];
    for (name, _) in &target.jumps {
        steps.push(Step::new(tf!("跳板机 {name}", name)));
    }
    steps.push(Step::new(match &target.via {
        Some(via) => tf!("经 {via} 连接 {name}", via, name = &target.name),
        None => tf!("登录 {name}", name = &target.name),
    }));
    steps.push(Step::new("kuaid"));
    steps.push(Step::new(tr("主机上的 git")));
    steps.push(Step::new(match cfg!(windows) {
        true => tr("映射（WinFsp）"),
        false => tr("映射（fuse3）"),
    }));
    steps
}

/// 本机的 ssh：版本（`ssh -V` 写在标准错误里）和路径。
fn ssh_version() -> Result<(String, String), String> {
    let program = fr_core::ssh::program();
    let mut cmd = std::process::Command::new(&program);
    cmd.arg("-V");
    fr_core::ssh::no_window(&mut cmd);
    let out = cmd.output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => fr_core::ssh::missing().to_string(),
        _ => tf!("无法运行 ssh：{err}", err),
    })?;
    let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let text = match text.is_empty() {
        true => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        false => text,
    };
    let version = text.lines().next().unwrap_or_default().trim().to_string();
    Ok((version, program.to_string_lossy().to_string()))
}

/// 连接报错里去掉末尾的「（ssh：…）」：ssh 的原话另外显示。
fn without_raw(message: &str, raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return message.to_string();
    }
    match message.find(raw) {
        Some(at) => {
            let head = &message[..at];
            let cut = head.rfind(['（', '(']).unwrap_or(head.len());
            message[..cut].trim_end().to_string()
        }
        None => message.to_string(),
    }
}

/// 连接失败时按 `failure`（登录阶段在哪一步失败；None 是登录上以后出的错）填各步。
fn fill_failure(
    steps: &mut [Step],
    layout: &Layout,
    target: &Target,
    message: &str,
    failure: Option<&LoginFailure>,
) {
    let ok_jump = |steps: &mut [Step], upto: usize| {
        for index in 0..upto.min(layout.jumps) {
            steps[layout.jump(index)].set(State::Ok, tr("已连接"), "");
        }
    };
    let raw = failure.map(|f| f.ssh.clone()).unwrap_or_default();
    // 登录阶段的失败只写原因（是哪一台看这一行的名字就知道）；登录上以后的写整句
    let text = match failure.map(|f| f.reason.trim()).filter(|r| !r.is_empty()) {
        Some(reason) => reason.to_string(),
        None => without_raw(message, &raw),
    };
    let bad_at = match failure.map(|f| f.at) {
        Some(FailedAt::Jump(index)) => {
            ok_jump(steps, index);
            layout.jump(index.min(layout.jumps.saturating_sub(1)))
        }
        Some(FailedAt::Forward(index)) => {
            ok_jump(steps, index + 1);
            match index + 1 < layout.jumps {
                true => layout.jump(index + 1),
                false => layout.login(),
            }
        }
        Some(FailedAt::Login) => {
            ok_jump(steps, layout.jumps);
            layout.login()
        }
        // 登录上以后出的错：启动不了 kuaid、握手不成……
        None => {
            ok_jump(steps, layout.jumps);
            if target.via.is_none() {
                steps[layout.login()].set(State::Ok, tr("已登录"), target.auth_text());
            }
            match target.via {
                Some(_) => layout.login(),
                None => layout.kuaid(),
            }
        }
    };
    // 换密钥、改密码只对登录被拒绝有用；连不上、超时时还没走到认证，不提
    let denied = failure.is_some_and(LoginFailure::denied);
    let note = match (bad_at == layout.login(), denied, target.auth) {
        (true, true, Auth::Password) => {
            format!(
                "{}{}",
                target.auth_text(),
                tr("。可以在主机设置里重新填写密码。")
            )
        }
        (true, true, Auth::Key) => format!(
            "{}{}",
            target.auth_text(),
            tr("。可以在主机设置里换密钥，或改用密码登录。")
        ),
        _ => String::new(),
    };
    steps[bad_at].set(State::Bad, text, note);
    steps[bad_at].raw = raw;
    let why = match bad_at {
        at if at == layout.kuaid() => tr("kuaid 没有启动"),
        at if at >= layout.login() => tr("登录没有成功"),
        _ => tr("跳板机没有连上"),
    };
    for step in steps.iter_mut().take(layout.git() + 1).skip(bad_at + 1) {
        step.skip(why);
    }
}

/// 连上了：填 kuaid、git 两步。
fn fill_connected(
    steps: &mut [Step],
    layout: &Layout,
    target: &Target,
    conn: &Connection,
    terms: Option<usize>,
) {
    for index in 0..layout.jumps {
        steps[layout.jump(index)].set(State::Ok, tr("已连接"), "");
    }
    let login_note = match target.via {
        Some(_) => String::new(),
        None => target.auth_text(),
    };
    steps[layout.login()].set(State::Ok, tr("已登录"), login_note);
    let hello = &conn.hello;
    let mut note = Vec::new();
    if hello.daemon {
        note.push(tr("常驻进程").to_string());
    }
    if let Some(terms) = terms {
        note.push(tf!("{n} 个终端", n = terms));
    }
    if conn.installed {
        note.push(tr("这次连接时刚装上").to_string());
    }
    steps[layout.kuaid()].set(
        State::Ok,
        tf!(
            "{version}，协议 {protocol}",
            version = &hello.version,
            protocol = hello.protocol
        ),
        note.join(" · "),
    );
    fill_git(&mut steps[layout.git()], &hello.git);
}

/// 主机上的 git：`raw` 是 `git --version` 的输出，空的是没找到 git。
fn fill_git(step: &mut Step, raw: &str) {
    let (min_major, min_minor, _) = fr_core::git::MIN_VERSION;
    let minimum = tf!("最低要求 {v}", v = format!("{min_major}.{min_minor}"));
    match fr_core::git::parse_version(raw) {
        Some(version) => match fr_core::git::too_old(Some(version)) {
            Some(old) => step.set(State::Bad, tf!("{old}，版本过低", old), minimum),
            None => {
                let (major, minor, patch) = version;
                step.set(State::Ok, format!("{major}.{minor}.{patch}"), minimum)
            }
        },
        None => step.set(State::Bad, tr("主机上没有找到 git"), ""),
    }
}

/// 表单里的「测试连接」登录上了：系统、这一版的 kuaid 装了没有、主机上的 git。没装 kuaid 不算失败：
/// 保存后第一次连接时自动装。
fn fill_probe(steps: &mut [Step], layout: &Layout, target: &Target, probe: &Probe) {
    for index in 0..layout.jumps {
        steps[layout.jump(index)].set(State::Ok, tr("已连接"), "");
    }
    steps[layout.login()].set(State::Ok, tr("已登录"), target.auth_text());
    let system = format!("{} {}", probe.os, probe.arch);
    let kuaid = &mut steps[layout.kuaid()];
    if !probe.supported {
        kuaid.set(
            State::Bad,
            tf!(
                "目前仅支持 Linux 和 Windows 主机，该主机为 {os}",
                os = &probe.os
            ),
            "",
        );
        steps[layout.git()].skip(tr("该主机上无法运行 kuaid"));
        return;
    }
    match probe.kuaid {
        Some(true) => kuaid.set(
            State::Ok,
            tf!("已安装 {version}", version = env!("CARGO_PKG_VERSION")),
            tf!("{system} · 测试连接不启动 kuaid", system),
        ),
        Some(false) => kuaid.set(
            State::Note,
            tr("尚未安装"),
            tf!("{system} · 保存后首次连接时自动安装", system),
        ),
        None => kuaid.set(State::Note, tr("未检查"), system),
    }
    fill_git(
        &mut steps[layout.git()],
        probe.git.as_deref().unwrap_or_default(),
    );
}

fn fill_winfsp(step: &mut Step) {
    match fr_mount::installed() {
        true => step.set(State::Ok, tr("本机已安装"), tr("只有映射用得到")),
        false => step.set(State::Note, tr("本机未安装"), tr("只有映射用得到")),
    }
}

// ---------- 复制的文字 ----------

/// 复制前把主机地址、用户名换掉：主机写成「主机 1」，用户目录写成 `C:\Users\<用户>`，IPv4 地址写成
/// `<地址>`。
struct Redact {
    /// 要换的字和换成什么，长的在前
    pairs: Vec<(String, String)>,
}

impl Redact {
    fn new() -> Self {
        let mut redact = Redact { pairs: Vec::new() };
        let (var, sep) = match cfg!(windows) {
            true => ("USERPROFILE", '\\'),
            false => ("HOME", '/'),
        };
        if let Some(home) = std::env::var_os(var) {
            let home = home.to_string_lossy().trim_end_matches(sep).to_string();
            if let Some((base, _)) = home.rsplit_once(sep) {
                let hidden = format!("{base}{sep}<{}>", tr("用户"));
                if cfg!(windows) {
                    redact.add(&home.replace('\\', "/"), &hidden.replace('\\', "/"));
                }
                redact.add(&home, &hidden);
            }
        }
        redact
    }

    fn add(&mut self, from: &str, to: &str) {
        if from.trim().is_empty() || self.pairs.iter().any(|(f, _)| f == from) {
            return;
        }
        self.pairs.push((from.to_string(), to.to_string()));
        self.pairs
            .sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    }

    /// 一台主机的地址：整个地址、`用户@`、主机名都换成 `label`（用户名换成 `<用户>`）。
    fn host(&mut self, destination: &str, label: &str) {
        let user_label = format!("<{}>", tr("用户"));
        self.add(destination, label);
        let bare = destination
            .trim_start_matches("ssh://")
            .trim_end_matches('/')
            .to_string();
        let (user, host) = match bare.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_string()), host.to_string()),
            None => (None, bare.clone()),
        };
        // 去掉端口：`host:22`、`[::1]:22`
        let host = match host.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or(rest).to_string(),
            None => host.rsplit_once(':').map_or(host.clone(), |(h, port)| {
                match port.chars().all(|c| c.is_ascii_digit()) {
                    true => h.to_string(),
                    false => host.clone(),
                }
            }),
        };
        self.add(&bare, label);
        if host.len() >= 2 {
            self.add(&host, label);
        }
        if let Some(user) = user {
            self.add(&format!("{user}@"), &format!("{user_label}@"));
        }
    }

    fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (from, to) in &self.pairs {
            out = out.replace(from, to);
        }
        hide_ipv4(&out)
    }
}

/// 文字里的 IPv4 地址换成 `<地址>`。
fn hide_ipv4(text: &str) -> String {
    let label = format!("<{}>", tr("地址"));
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit() && (i == 0 || !is_addr_char(chars[i - 1]));
        if starts && let Some(len) = ipv4_len(&chars[i..]) {
            out.push_str(&label);
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_addr_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.'
}

/// 从开头起是不是一个 IPv4 地址（四段 0～255 的数），是的话有多长。
fn ipv4_len(chars: &[char]) -> Option<usize> {
    let mut at = 0;
    for part in 0..4 {
        let start = at;
        while at < chars.len() && chars[at].is_ascii_digit() && at - start < 3 {
            at += 1;
        }
        if at == start {
            return None;
        }
        let number: String = chars[start..at].iter().collect();
        if number.parse::<u32>().ok()? > 255 {
            return None;
        }
        if part < 3 {
            if chars.get(at) != Some(&'.') {
                return None;
            }
            at += 1;
        }
    }
    // 后面紧跟着字母数字的不算（主机名里的数），点后面还有字母数字的也不算（版本号 1.2.3.4.5）；
    // 句末的点不影响
    let more = |c: Option<&char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    match chars.get(at) {
        Some('.') if more(chars.get(at + 1)) => None,
        c if more(c) => None,
        _ => Some(at),
    }
}

/// 复制出去的开头：Kuai 版本、系统、时间。
fn header(at: DateTime<Local>) -> String {
    format!(
        "Kuai {}（{}）\n{}\n",
        env!("CARGO_PKG_VERSION"),
        os_text(),
        tf!(
            "诊断时间 {time}",
            time = at.format("%Y-%m-%d %H:%M:%S %:z").to_string()
        )
    )
}

/// 系统：Windows 11 25H2（26200）x86_64。
fn os_text() -> String {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_LOCAL_MACHINE;
        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        if let Ok(key) = key {
            let build: String = key.get_value("CurrentBuild").unwrap_or_default();
            let display: String = key.get_value("DisplayVersion").unwrap_or_default();
            // 注册表里的产品名在 Windows 11 上还写着 10，按版本号认
            let name = match build.parse::<u32>().unwrap_or(0) >= 22000 {
                true => "Windows 11",
                false => "Windows 10",
            };
            return format!("{name} {display}（{build}）{}", std::env::consts::ARCH);
        }
    }
    // Linux：发行版的名字（Ubuntu 24.04.5 LTS）
    #[cfg(not(windows))]
    if let Ok(text) = std::fs::read_to_string("/etc/os-release")
        && let Some(name) = text
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
    {
        let name = name.trim().trim_matches('"');
        return format!("{name} {}", std::env::consts::ARCH);
    }
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn state_text(state: State) -> &'static str {
    match state {
        State::Wait => tr("检查中"),
        State::Ok => tr("通过"),
        State::Bad => tr("失败"),
        State::Skip => tr("未检查"),
        State::Note => tr("提示"),
    }
}

/// 一台主机的诊断结果写成文字。
fn steps_text(target: &Target, steps: &[Step]) -> String {
    let mut place = target.destination.clone();
    if let Some(via) = &target.via {
        place = tf!("经 {via} 转接", via);
    }
    if !target.jumps.is_empty() {
        let jumps: Vec<&str> = target.jumps.iter().map(|(_, d)| d.as_str()).collect();
        place = tf!("{place}，经 {jumps}", place, jumps = jumps.join(" → "));
    }
    let mut out = tf!("主机 {name}（{place}）", name = &target.name, place);
    out.push('\n');
    for step in steps {
        let state = state_text(step.state);
        let line = match step.state {
            State::Skip if step.why.is_empty() => format!("  [{state}] {}\n", step.name),
            State::Skip => format!("  [{state}] {}（{}）\n", step.name, step.why),
            _ => format!("  [{state}] {}：{}\n", step.name, step.text),
        };
        out.push_str(&line);
        if !step.note.is_empty() {
            out.push_str(&format!("        {}\n", step.note));
        }
        for line in step.raw.lines().filter(|line| !line.trim().is_empty()) {
            out.push_str(&format!("        ssh：{}\n", line.trim()));
        }
    }
    out
}

fn errors_section() -> String {
    let errors = errors_text();
    let mut out = tf!("最近的错误（最多 {n} 条）", n = MAX_ERRORS);
    out.push('\n');
    match errors.is_empty() {
        true => out.push_str(&format!("  {}\n", tr("无"))),
        false => {
            for line in errors {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    out
}

// ---------- 诊断对话框 ----------

/// 怎么连。
enum Source {
    /// 已保存的主机：走平常那条路（`AppState::connect`），诊断通过时也就连上了
    Saved,
    /// 主机设置里的「测试连接」：拿表单里填的只登录一次（[`Connection::probe`]）
    Form(ConnectOptions),
}

/// 连出来的结果。
enum Outcome {
    /// 本机没有可用的 ssh，没连
    NoSsh,
    Connected(Arc<Connection>, Option<usize>),
    Probed(Probe),
    /// 报错和登录阶段在哪一步失败的（已保存的主机从 `AppState` 读）
    Failed(String, Option<LoginFailure>),
}

/// 诊断一台主机的对话框。
pub struct Diagnose {
    app: Entity<AppState>,
    target: Target,
    source: Source,
    steps: Vec<Step>,
    running: bool,
    at: DateTime<Local>,
    /// 复制时隐藏主机地址和用户名
    hide: bool,
    /// 展开看要复制的内容
    preview: bool,
    task: Option<Task<()>>,
    /// 连上了告诉打开它的页面（仓库页借这个连接重新读取，不用再点「重试」）
    on_connected: Option<Connected>,
}

/// 诊断连上了主机时调用的，见 [`Diagnose::open`]。
pub type Connected = std::rc::Rc<dyn Fn(&mut App)>;

impl Diagnose {
    /// 打开诊断 `id` 这台主机的对话框，马上开始检查。连上了（全部通过）时调用 `on_connected`。
    pub fn open(
        app: Entity<AppState>,
        id: &str,
        on_connected: Option<Connected>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(target) = Target::of(app.read(cx), id) else {
            return;
        };
        let title = tf!("诊断 {name}", name = &target.name);
        Self::show(app, target, Source::Saved, on_connected, title, window, cx);
    }

    /// 主机设置里的「测试连接」：`server` 是表单里填的（还没保存），`jumps` 是依次经过的跳板机的
    /// 名字和地址，`options` 是照表单拼好的连接选项（带着这次填的密码或口令）。只登录一次，不保存，
    /// 也不在主机上装、启动 kuaid。
    pub fn open_form(
        app: Entity<AppState>,
        server: &Server,
        jumps: Vec<(String, String)>,
        options: ConnectOptions,
        window: &mut Window,
        cx: &mut App,
    ) {
        let target = Target {
            id: server.id.clone(),
            name: server.name.clone(),
            destination: server.destination.clone(),
            jumps,
            via: None,
            auth: server.auth,
            identity_file: server.identity_file.clone(),
        };
        let title = tf!("测试连接 {name}", name = &target.name);
        Self::show(app, target, Source::Form(options), None, title, window, cx);
    }

    fn show(
        app: Entity<AppState>,
        target: Target,
        source: Source,
        on_connected: Option<Connected>,
        title: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title: SharedString = title.into();
        let view = cx.new(|cx| {
            let mut this = Diagnose {
                app,
                steps: initial_steps(&target),
                target,
                source,
                running: false,
                at: Local::now(),
                hide: true,
                preview: false,
                task: None,
                on_connected,
            };
            this.run(cx);
            this
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title(title.clone()).w(px(680.)).child(view.clone())
        });
    }

    fn layout(&self) -> Layout {
        Layout {
            jumps: self.target.jumps.len(),
        }
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        self.steps = initial_steps(&self.target);
        let winfsp = self.layout().winfsp();
        fill_winfsp(&mut self.steps[winfsp]);
        self.running = true;
        self.at = Local::now();
        let id = self.target.id.clone();
        let ssh = cx.background_executor().spawn(async { ssh_version() });
        // 表单里的「测试连接」：和找 ssh 一起开始（没有 ssh 时它很快失败，结果不用）
        let probing = match &self.source {
            Source::Saved => None,
            Source::Form(options) => {
                let (destination, options) = (self.target.destination.clone(), options.clone());
                Some(
                    cx.background_executor()
                        .spawn(async move { Connection::probe(&destination, &options) }),
                )
            }
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let ssh = ssh.await;
            let ssh_ok = ssh.is_ok();
            let probing = probing.filter(|_| ssh_ok);
            let connecting = this.update(cx, |this, cx| {
                match &ssh {
                    Ok((version, path)) => {
                        this.steps[Layout::SSH].set(State::Ok, version.clone(), path.clone())
                    }
                    Err(err) => this.steps[Layout::SSH].set(State::Bad, err.clone(), ""),
                }
                cx.notify();
                // 本机的 ssh 都没有：不用再连（经别的电脑转接的不用本机的 ssh 连目标，但要连那台）
                let saved = matches!(this.source, Source::Saved);
                (ssh_ok && saved).then(|| this.app.update(cx, |app, cx| app.connect(&id, cx)))
            });
            let Ok(connecting) = connecting else {
                return;
            };
            let outcome = match (connecting, probing) {
                (Some(connecting), _) => match connecting.await {
                    Ok(conn) => {
                        // 连上了：问 kuaid 现在有几个终端
                        let terms = conn.term_list().await.ok().map(|list| list.len());
                        Outcome::Connected(conn, terms)
                    }
                    Err(message) => Outcome::Failed(message.to_string(), None),
                },
                (None, Some(probing)) => match probing.await {
                    Ok(probe) => Outcome::Probed(probe),
                    Err(err) => Outcome::Failed(
                        format!("{err:#}"),
                        fr_channel::login_failure(&err).cloned(),
                    ),
                },
                (None, None) => Outcome::NoSsh,
            };
            this.update(cx, |this, cx| {
                let layout = this.layout();
                match &outcome {
                    Outcome::NoSsh => {
                        for step in this.steps.iter_mut().take(layout.git() + 1).skip(1) {
                            step.skip(tr("本机没有可用的 ssh"));
                        }
                    }
                    Outcome::Connected(conn, terms) => {
                        fill_connected(&mut this.steps, &layout, &this.target, conn, *terms);
                        if let Some(on_connected) = this.on_connected.clone() {
                            on_connected(cx);
                        }
                    }
                    Outcome::Probed(probe) => {
                        fill_probe(&mut this.steps, &layout, &this.target, probe)
                    }
                    Outcome::Failed(message, failure) => {
                        let failure = match this.source {
                            Source::Saved => this.app.read(cx).last_login_failure(&this.target.id),
                            Source::Form(_) => failure.clone(),
                        };
                        fill_failure(
                            &mut this.steps,
                            &layout,
                            &this.target,
                            message,
                            failure.as_ref(),
                        );
                    }
                }
                this.running = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// 复制出去的文字。
    fn report(&self) -> String {
        let text = format!(
            "{}\n{}\n{}",
            header(self.at),
            steps_text(&self.target, &self.steps),
            errors_section()
        );
        match self.hide {
            true => self.redact().apply(&text),
            false => text,
        }
    }

    fn redact(&self) -> Redact {
        let mut redact = Redact::new();
        redact.host(
            &self.target.destination,
            &format!("<{}>", tf!("主机 {n}", n = 1)),
        );
        for (ix, (_, destination)) in self.target.jumps.iter().enumerate() {
            let label = format!("<{}>", tf!("跳板机 {n}", n = ix + 1));
            redact.host(destination, &label);
        }
        redact
    }
}

impl Render for Diagnose {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let mut intro = match &self.target.via {
            Some(via) => tf!("经 {via} 转接", via),
            None => self.target.destination.clone(),
        };
        if !self.target.jumps.is_empty() {
            let names: Vec<&str> = self.target.jumps.iter().map(|(n, _)| n.as_str()).collect();
            intro = tf!(
                "{intro} · 经跳板机 {names}",
                intro,
                names = names.join(" → ")
            );
        }
        let intro = match self.source {
            Source::Saved => tf!("{intro}。逐步检查，哪一步失败就停在哪。", intro),
            Source::Form(_) => tf!(
                "{intro}。按表单中的设置逐步检查；不保存设置，也不在主机上安装或启动 kuaid。",
                intro
            ),
        };
        // 看要复制的内容时，它换掉检查结果（两样都放的话对话框比窗口还高）
        let body = match self.preview {
            true => report_box(self.report(), cx),
            false => div()
                .id("diagnose-steps")
                .max_h(px(440.))
                .overflow_y_scroll()
                .child(steps_box(&self.steps, cx))
                .into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(intro),
            )
            .child(body)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("diagnose-hide")
                            .label(tr("复制时隐藏主机地址和用户名"))
                            .checked(self.hide)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.hide = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("密码、口令和令牌不会复制。")),
                    ),
            )
            .child(
                h_flex().child(
                    Button::new("diagnose-preview")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(if self.preview {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        }))
                        .label(if self.preview {
                            tr("返回检查结果")
                        } else {
                            tr("查看要复制的内容")
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.preview = !this.preview;
                            cx.notify();
                        })),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("diagnose-again")
                            .small()
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(tr("重新诊断"))
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("diagnose-close")
                            .small()
                            .label(tr("关闭"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("diagnose-copy")
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::Copy))
                            .label(tr("复制诊断信息"))
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let text = this.report();
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                window.close_dialog(cx);
                                window.push_notification(
                                    Notification::success(tr("诊断信息已复制")),
                                    cx,
                                );
                            })),
                    ),
            )
    }
}

/// 各步一行：图标、名字、结果（下面灰字补充、ssh 的原话）。
fn steps_box(steps: &[Step], cx: &App) -> Div {
    let theme = cx.theme();
    let (muted, border, success, danger, fg, secondary) = (
        theme.muted_foreground,
        theme.border,
        theme.success,
        theme.danger,
        theme.foreground,
        theme.secondary,
    );
    v_flex()
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .children(steps.iter().enumerate().map(|(ix, step)| {
            let icon: AnyElement = match step.state {
                State::Wait => Spinner::new()
                    .icon(Icon::new(IconName::LoaderCircle))
                    .with_size(px(14.))
                    .into_any_element(),
                State::Ok => ui::icon(IconName::CircleCheck)
                    .text_color(success)
                    .into_any_element(),
                State::Bad => ui::icon(IconName::CircleX)
                    .text_color(danger)
                    .into_any_element(),
                State::Skip => ui::icon(IconName::CircleMinus)
                    .text_color(muted)
                    .into_any_element(),
                State::Note => ui::icon(IconName::Info)
                    .text_color(muted)
                    .into_any_element(),
            };
            let dim = matches!(step.state, State::Skip | State::Wait);
            h_flex()
                .items_start()
                .gap_3()
                .px_3()
                .py(px(9.))
                .when(ix > 0, |row| row.border_t_1().border_color(border))
                .child(div().pt(px(2.)).flex_none().child(icon))
                .child(
                    div()
                        .w(px(150.))
                        .flex_none()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if dim { muted } else { fg })
                        .child(step.name.clone()),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .text_color(match step.state {
                                    State::Bad => danger,
                                    State::Skip | State::Wait => muted,
                                    _ => fg,
                                })
                                .child(step.text.clone()),
                        )
                        .when(!step.note.is_empty(), |col| {
                            col.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .whitespace_normal()
                                    .child(step.note.clone()),
                            )
                        })
                        .when(!step.raw.is_empty(), |col| {
                            col.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.))
                                    .bg(secondary)
                                    .text_xs()
                                    .font_family(crate::fonts::mono())
                                    .whitespace_normal()
                                    .child(step.raw.clone()),
                            )
                        }),
                )
        }))
}

/// 要复制的内容：等宽字，太长的滚动。
fn report_box(text: String, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .id("diagnose-report")
        .max_h(px(440.))
        .overflow_y_scroll()
        .p_2()
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_xs()
        .font_family(crate::fonts::mono())
        .whitespace_normal()
        .children(
            text.lines()
                .map(|line| div().child(line.to_string()))
                .collect::<Vec<_>>(),
        )
        .into_any_element()
}

// ---------- 设置 → 关于：复制诊断信息 ----------

/// 「复制诊断信息…」：各主机现在的连接情况、最近的错误。不重新连。
pub struct Summary {
    app: Entity<AppState>,
    at: DateTime<Local>,
    hide: bool,
}

impl Summary {
    pub fn open(app: Entity<AppState>, window: &mut Window, cx: &mut App) {
        let view = cx.new(|_| Summary {
            app,
            at: Local::now(),
            hide: true,
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(tr("复制诊断信息"))
                .w(px(680.))
                .child(view.clone())
        });
    }

    fn report(&self, cx: &App) -> String {
        let app = self.app.read(cx);
        let mut redact = Redact::new();
        let mut hosts = String::new();
        for (ix, server) in app.settings.servers.iter().enumerate() {
            let state = match app.status(&server.id) {
                ConnStatus::Idle => tr("未连接").to_string(),
                ConnStatus::Connecting => tr("正在连接").to_string(),
                ConnStatus::Ready(conn) => tf!(
                    "已连接 · kuaid {version}，协议 {protocol} · {git}",
                    version = &conn.hello.version,
                    protocol = conn.hello.protocol,
                    git = conn.hello.git.trim()
                ),
                ConnStatus::Failed(err) => tf!("连接失败：{err}", err = err.to_string()),
            };
            hosts.push_str(&format!(
                "  {}（{}）：{}\n",
                server.name, server.destination, state
            ));
            redact.host(
                &server.destination,
                &format!("<{}>", tf!("主机 {n}", n = ix + 1)),
            );
        }
        if hosts.is_empty() {
            hosts = format!("  {}\n", tr("无"));
        }
        let text = format!(
            "{}\n{}\n{}{}\n{}",
            header(self.at),
            tr("各主机最近一次的连接结果"),
            hosts,
            String::new(),
            errors_section()
        );
        match self.hide {
            true => redact.apply(&text),
            false => text,
        }
    }
}

impl Render for Summary {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let text = self.report(cx);
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(tr(
                        "版本、系统、各主机最近一次的连接结果、最近的错误。复制前可以先看内容。",
                    )),
            )
            .child(report_box(text, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("summary-hide")
                            .label(tr("复制时隐藏主机地址和用户名"))
                            .checked(self.hide)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.hide = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr("密码、口令和令牌不会复制。")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("summary-close")
                            .small()
                            .label(tr("关闭"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("summary-copy")
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::Copy))
                            .label(tr("复制诊断信息"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let text = this.report(cx);
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                window.close_dialog(cx);
                                window.push_notification(
                                    Notification::success(tr("诊断信息已复制")),
                                    cx,
                                );
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, Redact, State, Target, fill_probe, hide_ipv4, initial_steps, without_raw};
    use crate::settings::Auth;
    use fr_channel::Probe;

    fn target(jumps: usize) -> Target {
        Target {
            id: "s1".into(),
            name: "dev".into(),
            destination: "dev@host".into(),
            jumps: (0..jumps)
                .map(|n| (format!("j{n}"), format!("ops@j{n}")))
                .collect(),
            via: None,
            auth: Auth::Key,
            identity_file: None,
        }
    }

    fn probe(os: &str, kuaid: Option<bool>, git: Option<&str>) -> Probe {
        Probe {
            os: os.into(),
            arch: "x86_64".into(),
            supported: os == "Linux" || os == "Windows",
            kuaid,
            git: git.map(String::from),
        }
    }

    /// 测试连接登录上了：kuaid 没装不算失败（保存后连接时装），git 照版本判断。
    #[test]
    fn 测试连接登录上了_kuaid_没装不算失败() {
        let target = target(1);
        let layout = Layout { jumps: 1 };
        let mut steps = initial_steps(&target);
        fill_probe(
            &mut steps,
            &layout,
            &target,
            &probe("Linux", Some(false), Some("git version 2.43.0")),
        );
        assert_eq!(steps[layout.jump(0)].state, State::Ok);
        assert_eq!(steps[layout.login()].state, State::Ok);
        assert_eq!(steps[layout.kuaid()].state, State::Note);
        assert!(steps[layout.kuaid()].note.starts_with("Linux x86_64"));
        assert_eq!(steps[layout.git()].state, State::Ok);
        assert_eq!(steps[layout.git()].text, "2.43.0");

        let mut steps = initial_steps(&target);
        fill_probe(
            &mut steps,
            &layout,
            &target,
            &probe("Linux", Some(true), Some("git version 2.25.1")),
        );
        assert_eq!(steps[layout.kuaid()].state, State::Ok);
        assert_eq!(steps[layout.git()].state, State::Bad);

        let mut steps = initial_steps(&target);
        fill_probe(&mut steps, &layout, &target, &probe("Linux", None, None));
        assert_eq!(steps[layout.kuaid()].state, State::Note);
        assert_eq!(steps[layout.git()].state, State::Bad);
    }

    #[test]
    fn 测试连接遇到不支持的系统_kuaid_那一步失败_git_不查() {
        let target = target(0);
        let layout = Layout { jumps: 0 };
        let mut steps = initial_steps(&target);
        fill_probe(
            &mut steps,
            &layout,
            &target,
            &probe("Darwin", None, Some("git version 2.43.0")),
        );
        assert_eq!(steps[layout.login()].state, State::Ok);
        assert_eq!(steps[layout.kuaid()].state, State::Bad);
        assert!(steps[layout.kuaid()].text.contains("Darwin"));
        assert_eq!(steps[layout.git()].state, State::Skip);
    }

    #[test]
    fn 隐藏地址和用户名() {
        let mut redact = Redact { pairs: Vec::new() };
        redact.host("dev@10.0.3.17", "<主机 1>");
        redact.host("ops@bastion.example.com:2222", "<跳板机 1>");
        let text = "dev@10.0.3.17: Permission denied (publickey).\n\
                    经 ops@bastion.example.com:2222 · bastion.example.com 拒绝\n\
                    Connection to 203.0.113.8 port 22 timed out · git 2.43.0 · 1.2.3.4.5";
        let out = redact.apply(text);
        assert_eq!(
            out,
            "<主机 1>: Permission denied (publickey).\n\
             经 <跳板机 1> · <跳板机 1> 拒绝\n\
             Connection to <地址> port 22 timed out · git 2.43.0 · 1.2.3.4.5"
        );
        assert_eq!(hide_ipv4("at 10.0.0.1."), "at <地址>.");
        assert_eq!(hide_ipv4("v300.1.1.1"), "v300.1.1.1");
        assert_eq!(hide_ipv4("999.1.1.1"), "999.1.1.1");
    }

    #[test]
    fn 报错去掉末尾的_ssh_原话() {
        let raw = "dev@h: Permission denied (publickey).";
        let message = format!("连接 dev@h：登录被拒绝（ssh：{raw}）");
        assert_eq!(without_raw(&message, raw), "连接 dev@h：登录被拒绝");
        assert_eq!(without_raw("连接失败", ""), "连接失败");
    }
}
