//! 替客户端连这台电脑配置的服务器（协议 10）。
//!
//! 别的电脑连着这台、打开的工作区里有这台配置的服务器上的仓库时（比如本机连 A，A 的工作区里
//! 有 B 上的仓库），那台电脑不用配置 B：由这里起 ssh 连过去，ssh 的标准输入输出经客户端这条连接
//! 来回转。客户端在上面做的和直接连一样：装 kuaid、启动转接程序、握手。
//!
//! 怎么连照这台电脑的 Kuai 的配置（[`Store::servers`]，跳板机也照它串起来）。要密码、口令时问
//! 发起的那个客户端（askpass 的标记是它的）；这台电脑的 Kuai 存着的密码不用，那是它的用户的。

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context as _, Result};
use fr_core::exec::{Killer, Tree};
use fr_core::hop::{self, Hop};
use fr_core::protocol::Response;
use fr_core::ssh;
use fr_core::store::Store;
use serde_json::json;

use crate::out::Outbox;

/// 起 ssh 的命令：连 `server`（这台电脑配置的服务器的 id），在上面执行 `remote`。
/// `env` 是发起的客户端的（askpass 的标记、它那条连接转发过来的 ssh-agent）；
/// `helper` 是转接跳板机用的程序，就是 kuaid 自己（见 [`fr_core::hop`]）。
pub fn command(
    store: &Store,
    server: &str,
    remote: &str,
    env: &[(String, String)],
    helper: &Path,
) -> Result<Command> {
    let (target, route) = store.route(server)?;
    let askpass_env = fr_core::git::extra_env();
    // 开不了密码转发（见 `Shared::start_askpass`）就只用密钥，要密码直接失败
    let askpass = !askpass_env.is_empty();
    let tag = env
        .iter()
        .find(|(name, _)| name == fr_core::askpass::TAG_ENV)
        .map(|(_, tag)| tag.clone());
    let mut cmd = Command::new(ssh::program());
    // 经这里转接的连接不自动回答存着的口令（客户端那边弹窗问），不用只限公钥
    cmd.args(ssh::login_args(
        target.identity_file.as_deref(),
        target.password_auth,
        false,
        target.forward_agent,
        askpass,
    ));
    cmd.envs(askpass_env.iter().map(|(key, value)| (key, value)));
    cmd.envs(env.iter().map(|(key, value)| (key, value)));
    if route.is_empty() {
        cmd.args(["-o", "ConnectTimeout=10"]);
    } else {
        // 跳板机的密码也问这个客户端：标记和目标服务器的一样
        let hops: Vec<Hop> = route
            .iter()
            .map(|jump| {
                let mut args = ssh::login_args(
                    jump.identity_file.as_deref(),
                    jump.password_auth,
                    false,
                    false,
                    askpass,
                );
                args.extend(ssh::STAGE_OPTIONS.iter().map(|item| item.to_string()));
                Hop {
                    destination: jump.destination.clone(),
                    args,
                    tag: tag.clone(),
                }
            })
            .collect();
        cmd.env(hop::HOPS_ENV, hop::encode(&hops));
        let proxy = hop::proxy_command(helper, hops.len() - 1);
        cmd.args(["-o", &format!("ProxyCommand={proxy}")]);
        // 每登录上一台打一行，客户端拿来报进度、看超时
        cmd.args(ssh::STAGE_OPTIONS);
    }
    cmd.arg("-T");
    cmd.args(ssh::KEEP_OPTIONS);
    cmd.args(["--", target.destination.as_str(), remote]);
    Ok(cmd)
}

/// 往 ssh 的标准输入写的东西。
enum Input {
    Bytes(Vec<u8>),
    /// 写完了，关掉标准输入
    Eof,
}

/// 一条转接。
struct Relay {
    input: mpsc::Sender<Input>,
    killer: Killer,
}

/// 一个客户端开着的转接，按请求的编号记。
#[derive(Default)]
pub struct Relays(Mutex<HashMap<u64, Relay>>);

impl Relays {
    /// 起 `cmd`，它的输出作为请求 `id` 的流发给客户端：标准输出 `chunk: true`，标准错误
    /// `chunk: true, err: true`，结束时最后一条 `result` 是 `{code}`。起不来返回错误。
    pub fn open(self: &Arc<Self>, id: u64, mut cmd: Command, out: &Outbox) -> Result<()> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ssh::no_window(&mut cmd);
        // 结束时转接跳板机的那几层一起结束（见 [`Tree`]）
        let (tree, pipes) = Tree::spawn(&mut cmd).context("无法启动 ssh")?;
        let stdin = pipes.stdin.context("无法获取 ssh 的标准输入")?;
        let stdout = pipes.stdout.context("无法获取 ssh 的标准输出")?;
        let stderr = pipes.stderr.context("无法获取 ssh 的标准错误")?;
        let (input, inputs) = mpsc::channel();
        let relay = Relay {
            input,
            killer: tree.killer(),
        };
        self.0.lock().unwrap().insert(id, relay);

        // 写标准输入放在自己的线程里：ssh 收得慢时不能卡住读这个客户端的下一个请求
        // （其中可能就有 ssh 在等的密码回答）
        std::thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(Input::Bytes(bytes)) = inputs.recv() {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            // 收到 Eof、或者转接结束了：丢掉 stdin 就是关掉 ssh 的标准输入
        });
        let err_out = out.clone();
        let errors = std::thread::spawn(move || pump(stderr, id, true, &err_out));
        let relays = self.clone();
        let out = out.clone();
        std::thread::spawn(move || {
            pump(stdout, id, false, &out);
            let _ = errors.join();
            // 输出都读完了才等：ssh 这时已经结束或者正在结束
            let code = tree.wait().ok().and_then(|status| status.code());
            relays.0.lock().unwrap().remove(&id);
            let done = Response {
                id,
                result: Some(json!({ "code": code })),
                ..Default::default()
            };
            out.push(&done, &[]);
        });
        Ok(())
    }

    /// 客户端发来的字节写进 ssh 的标准输入；`eof` 为真时写完关掉它。转接已经结束就丢掉。
    pub fn input(&self, relay: u64, bytes: Vec<u8>, eof: bool) {
        let relays = self.0.lock().unwrap();
        let Some(relay) = relays.get(&relay) else {
            return;
        };
        if !bytes.is_empty() {
            let _ = relay.input.send(Input::Bytes(bytes));
        }
        if eof {
            let _ = relay.input.send(Input::Eof);
        }
    }

    /// 结束一条转接（客户端取消了）；不是转接的编号就什么也不做。
    pub fn kill(&self, id: u64) {
        let killer = self
            .0
            .lock()
            .unwrap()
            .get(&id)
            .map(|relay| relay.killer.clone());
        // 不会卡住读下一个请求：Windows 上要启动 taskkill，kill 放在别的线程里做
        if let Some(killer) = killer {
            killer.kill();
        }
    }

    /// 结束所有转接（客户端断开了）。
    pub fn kill_all(&self) {
        let killers: Vec<Killer> = self
            .0
            .lock()
            .unwrap()
            .values()
            .map(|relay| relay.killer.clone())
            .collect();
        for killer in killers {
            killer.kill();
        }
    }
}

/// 把一根管子里读到的原样发给客户端，读到头为止。客户端收得慢时 [`Outbox::send`] 会等，
/// ssh 跟着慢下来。
fn pump(mut from: impl Read, id: u64, err: bool, out: &Outbox) {
    let mut buf = vec![0u8; fr_core::exec::CHUNK_SIZE];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let header = Response {
                    id,
                    chunk: true,
                    err,
                    bytes: n as u64,
                    ..Default::default()
                };
                out.send(&header, &buf[..n]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::store::Server;

    fn store() -> Store {
        let server = |id: &str, jump: Option<&str>| Server {
            id: id.into(),
            name: id.into(),
            destination: format!("ssh://root@{id}:2222"),
            jump: jump.map(str::to_string),
            ..Server::default()
        };
        let mut store = Store {
            servers: vec![server("s91", Some("s49")), server("s49", None)],
            ..Store::default()
        };
        store.servers[0].password_auth = true;
        store.servers[1].identity_file = Some(r"C:\keys\id_49".into());
        store
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(cmd: &Command, name: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn 照这台的配置连_跳板机的密码也问发起的客户端() {
        let env = vec![(fr_core::askpass::TAG_ENV.to_string(), "c7".to_string())];
        let helper = Path::new(r"C:\Users\me\.kuai\1.14.0-abc\kuaid.exe");
        let cmd = command(&store(), "s91", "true", &env, helper).unwrap();
        let list = args(&cmd);
        // 目标服务器用密码登录，命令在最后
        assert!(list.contains(&"PreferredAuthentications=keyboard-interactive,password".into()));
        assert_eq!(
            list[list.len() - 3..],
            ["--", "ssh://root@s91:2222", "true"]
        );
        // 经跳板机：不设超时，转接交给 kuaid 自己
        assert!(!list.iter().any(|arg| arg.starts_with("ConnectTimeout")));
        let proxy = list
            .iter()
            .find(|arg| arg.starts_with("ProxyCommand="))
            .unwrap();
        assert!(proxy.contains("kuaid.exe\" --ssh-hop 0 %h %p"), "{proxy}");
        assert_eq!(
            env_of(&cmd, fr_core::askpass::TAG_ENV).as_deref(),
            Some("c7")
        );
        let hops = env_of(&cmd, hop::HOPS_ENV).unwrap();
        assert!(
            hops.contains("ssh://root@s49:2222") && hops.contains("id_49"),
            "{hops}"
        );
        assert!(hops.contains("\"tag\":\"c7\""), "{hops}");

        // 直接连的：10 秒连不上就放弃
        let direct = args(&command(&store(), "s49", "true", &env, helper).unwrap());
        assert!(direct.contains(&"ConnectTimeout=10".into()));
        assert!(direct.contains(&"IdentitiesOnly=yes".into()));
        assert!(command(&store(), "没有", "true", &env, helper).is_err());
    }

    #[test]
    fn 转接的输出原样回来_标准输入写进去_结束时回退出码() {
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Outbox::new(writer);
        let relays = Arc::new(Relays::default());
        // 拿 git 当被连的那一头：hash-object 读完标准输入才输出
        let mut cmd = Command::new("git");
        cmd.args(["hash-object", "--stdin"]);
        relays.open(5, cmd, &out).unwrap();
        relays.input(5, b"hel".to_vec(), false);
        relays.input(5, b"lo\n".to_vec(), true);
        let mut reader = std::io::BufReader::new(reader);
        let mut stdout = Vec::new();
        loop {
            let (response, payload) = fr_core::protocol::read_message::<_, Response>(&mut reader)
                .unwrap()
                .unwrap();
            assert_eq!(response.id, 5);
            if response.chunk {
                if !response.err {
                    stdout.extend_from_slice(&payload);
                }
                continue;
            }
            assert_eq!(response.result.unwrap()["code"], 0);
            break;
        }
        // `echo hello | git hash-object --stdin` 的结果
        assert_eq!(
            String::from_utf8_lossy(&stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        // 结束了的转接不再记着
        assert!(relays.0.lock().unwrap().is_empty());
        out.close();
    }
}
