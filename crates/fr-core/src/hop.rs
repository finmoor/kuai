//! 经跳板机连接：目标服务器的 ssh 不直接连过去，而是用 `ProxyCommand` 让上一台跳板机转接。
//!
//! `ProxyCommand` 填的是当前程序自己（客户端或 fr-probe）加 `--ssh-hop <第几台> %h %p`：ssh 启动它，
//! 它再启动一个 `ssh -W 主机:端口 跳板机`，标准输入输出原样交给那个 ssh。中间隔这一层，是因为
//! 每台跳板机各有各的登录方式（私钥、密码）和存着的密码，这些放不进一行 `ProxyCommand` 里：
//! 各台的 ssh 选项放在环境变量 [`HOPS_ENV`] 里（一路继承下来），密码不在里面，只有 askpass 的标记，
//! 这一层按第几台取出来用。几台跳板机就这样一层套一层。
//!
//! 所以用到跳板机的程序，`main` 的开头要调用 [`run_helper`]：客户端、fr-probe，还有替客户端连它
//! 配置的服务器的 kuaid（协议 10）。

use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

/// 各台跳板机的 ssh 选项（JSON），由最外面那个 ssh 的环境变量一路继承给每一层
pub const HOPS_ENV: &str = "FR_SSH_HOPS";
const FLAG: &str = "--ssh-hop";
/// 某一台跳板机的 ssh 以失败结束时，往标准错误打这一行，后面跟是第几台。
/// 最先打出来的那一台是出问题的那一台：它断了，经它连接的后面几台才跟着断
pub const FAILED: &str = "KUAI-HOP-FAILED";

/// 一台跳板机怎么连。
pub struct Hop {
    /// ssh 的目标
    pub destination: String,
    /// 登录用的 ssh 选项（私钥、密码登录、要密码时怎么问）
    pub args: Vec<String>,
    /// askpass 的标记：这台跳板机的 ssh 要密码时，主程序靠它认出是哪一台
    pub tag: Option<String>,
}

pub fn encode(hops: &[Hop]) -> String {
    let list: Vec<Value> = hops
        .iter()
        .map(|hop| json!({ "destination": hop.destination, "args": hop.args, "tag": hop.tag }))
        .collect();
    Value::Array(list).to_string()
}

fn decode(text: &str) -> Option<Vec<Hop>> {
    let list: Vec<Value> = serde_json::from_str(text).ok()?;
    list.iter()
        .map(|hop| {
            Some(Hop {
                destination: hop.get("destination")?.as_str()?.to_string(),
                args: hop
                    .get("args")?
                    .as_array()?
                    .iter()
                    .filter_map(|arg| arg.as_str().map(str::to_string))
                    .collect(),
                tag: hop.get("tag").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

/// 经第 `index` 台跳板机转接的 `ProxyCommand`。`helper` 是会在开头调用 [`run_helper`] 的程序。
pub fn proxy_command(helper: &Path, index: usize) -> String {
    // % 在 ProxyCommand 里是占位符的开头，路径里的要写两遍
    let helper = helper.to_string_lossy().replace('%', "%%");
    format!("\"{helper}\" {FLAG} {index} %h %p")
}

/// 第 `index` 台跳板机的 ssh 的参数：转接到 `host:port`。
fn hop_args(hops: &[Hop], index: usize, host: &str, port: &str, helper: &Path) -> Vec<String> {
    let hop = &hops[index];
    let mut args = hop.args.clone();
    let mut add = |items: &[&str]| args.extend(items.iter().map(|item| item.to_string()));
    match index {
        // 第一台从本机直接连：连不上 10 秒就放弃
        0 => add(&["-o", "ConnectTimeout=10"]),
        // 后面的经前一台转接。不设超时：前一台可能正等着用户输密码，等多久由主程序看着办
        _ => add(&[
            "-o",
            &format!("ProxyCommand={}", proxy_command(helper, index - 1)),
        ]),
    }
    add(crate::ssh::KEEP_OPTIONS);
    // IPv6 地址要加方括号，不然分不清哪个冒号后面是端口
    let to = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    add(&["-W", &to, "--", &hop.destination]);
    args
}

/// 程序启动时最先调用（和 `fr_core::askpass::run_helper` 一起）：如果是被 ssh 当作 `ProxyCommand`
/// 启动的，就把连接转到那台跳板机上，一直到连接结束，返回退出码；不是的话返回 None，照常启动。
pub fn run_helper() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next()? != FLAG {
        return None;
    }
    let index: usize = args.next()?.parse().ok()?;
    let (host, port) = (args.next()?, args.next()?);
    let code = match run(index, &host, &port) {
        Ok(0) => 0,
        Ok(code) => {
            eprintln!("{FAILED} {index}");
            code
        }
        Err(err) => {
            eprintln!("{err:#}");
            eprintln!("{FAILED} {index}");
            255
        }
    };
    Some(code)
}

fn run(index: usize, host: &str, port: &str) -> Result<i32> {
    let hops = std::env::var(HOPS_ENV)
        .ok()
        .and_then(|text| decode(&text))
        .context("FR_SSH_HOPS")?;
    let hop = hops.get(index).context("FR_SSH_HOPS")?;
    let helper = std::env::current_exe()?;
    let mut cmd = Command::new(crate::ssh::program());
    cmd.args(hop_args(&hops, index, host, port, &helper));
    // 继承下来的标记是目标服务器那个 ssh 的，换成这一台的
    match &hop.tag {
        Some(tag) => cmd.env(crate::askpass::TAG_ENV, tag),
        None => cmd.env_remove(crate::askpass::TAG_ENV),
    };
    crate::ssh::no_window(&mut cmd);
    // 标准输入输出原样交给它：外面那个 ssh 的数据就从这里过
    let status = cmd.status().context("ssh")?;
    Ok(status.code().unwrap_or(255))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hops() -> Vec<Hop> {
        vec![
            Hop {
                destination: "ops@bastion".into(),
                args: vec!["-i".into(), r"C:\keys\my key".into()],
                tag: Some("t0".into()),
            },
            Hop {
                destination: "ssh://me@10.0.0.5:2222".into(),
                args: vec![],
                tag: None,
            },
        ]
    }

    #[test]
    fn 各台的选项原样传给下一层() {
        let back = decode(&encode(&hops())).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].destination, "ops@bastion");
        assert_eq!(back[0].args, ["-i", r"C:\keys\my key"]);
        assert_eq!(back[0].tag.as_deref(), Some("t0"));
        assert_eq!(back[1].tag, None);
        assert!(decode("不是 JSON").is_none());
    }

    #[test]
    fn 第一台直接连_后面的经前一台转接() {
        let helper = Path::new(r"C:\Program Files\Kuai 100%\kuai.exe");
        let first = hop_args(&hops(), 0, "10.0.0.5", "2222", helper);
        assert!(first.contains(&"ConnectTimeout=10".to_string()));
        assert!(!first.iter().any(|arg| arg.starts_with("ProxyCommand")));
        assert_eq!(
            first[first.len() - 4..],
            ["-W", "10.0.0.5:2222", "--", "ops@bastion"]
        );
        // 私钥这些登录选项在最前面
        assert_eq!(first[..2], ["-i", r"C:\keys\my key"]);

        let second = hop_args(&hops(), 1, "fd00::7", "22", helper);
        // 程序路径里的空格靠引号，% 写两遍；%h %p 留给 ssh 填
        let proxy = r#"ProxyCommand="C:\Program Files\Kuai 100%%\kuai.exe" --ssh-hop 0 %h %p"#;
        assert!(second.contains(&proxy.to_string()), "{second:?}");
        assert!(!second.iter().any(|arg| arg.starts_with("ConnectTimeout")));
        assert_eq!(
            second[second.len() - 4..],
            ["-W", "[fd00::7]:22", "--", "ssh://me@10.0.0.5:2222"]
        );
    }
}
