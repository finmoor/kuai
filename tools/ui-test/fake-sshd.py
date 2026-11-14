"""测试用的 SSH 服务：只听 127.0.0.1，客户端要执行的命令一律不执行，回一行 KUAID-START 以后
把通道接到 `<kuaid 程序> --stdio` 上。客户端就能一路走到「已连接」，目录浏览、打开仓库、git 操作
看到的都是那个 kuaid 那边的东西。见 README.md「登录服务器的各种情况」。

用法：python fake-sshd.py <主目录> <kuaid 程序> <端口> [--password-env 变量名] [--kbd] [--forward]
设置文件里服务器的地址写 ssh://tester@127.0.0.1:<端口>。要装 paramiko。

- 不带选项：谁登录都放行（none、publickey）。
- `--password-env 变量名`：要密码，密码放在这个环境变量里（不出现在命令行上）；默认用 password
  方式问，加 `--kbd` 改用 keyboard-interactive（很多 Linux 服务器是这样）。登录结果记在输出里。
- `--forward`：当跳板机用：允许转接到 127.0.0.1 上的端口（`ssh -W` 要的 direct-tcpip）。
  两个实例一个当跳板机、一个当目标服务器，就能测经跳板机连接。
- `<kuaid 程序>` 一般是本机编译的 `target/debug/kuaid.exe`（「服务器」就是 <主目录>）；也可以是
  一个 .cmd，在里面 ssh 到真的服务器、跑放在临时目录里的 kuaid（`HOME` 也指过去），界面就能测
  真的 Linux 服务器，又不把开发版装进它的 `~/.kuai`（那会顶掉在用的常驻进程）。

测完要收拾的：真的 ssh.exe 会把这个服务记进 ~/.ssh/known_hosts（`[127.0.0.1]:<端口>` 那一行），删掉。
"""
import argparse
import os
import socket
import subprocess
import sys
import threading

import paramiko

# 输出重定向到文件时也用 UTF-8，不跟着系统的代码页
sys.stdout.reconfigure(encoding='utf-8')
parser = argparse.ArgumentParser()
parser.add_argument('home')
parser.add_argument('kuaid')
parser.add_argument('port', type=int)
parser.add_argument('--password-env')
parser.add_argument('--kbd', action='store_true')
parser.add_argument('--forward', action='store_true')
ARGS = parser.parse_args()
HOME, KUAID, PORT = os.path.abspath(ARGS.home), os.path.abspath(ARGS.kuaid), ARGS.port
PASSWORD = os.environ.get(ARGS.password_env) if ARGS.password_env else None
if ARGS.password_env and not PASSWORD:
    sys.exit(f'环境变量 {ARGS.password_env} 是空的')
# 主机密钥存在主目录旁边，重启这个脚本还是同一把；每次现生成的话，ssh 第二次连接就报
# 「主机密钥变了」，要先去 known_hosts 里删掉上一把
KEY_FILE = HOME.rstrip('\\/') + '.host_key'
if not os.path.exists(KEY_FILE):
    paramiko.RSAKey.generate(2048).write_private_key_file(KEY_FILE)
HOST_KEY = paramiko.RSAKey(filename=KEY_FILE)


def log(*parts):
    print(*parts, flush=True)


class Server(paramiko.ServerInterface):
    def __init__(self):
        # direct-tcpip 通道的编号 → 要转接到哪里
        self.forwards = {}

    def get_allowed_auths(self, username):
        if PASSWORD is None:
            return 'none,publickey'
        return 'keyboard-interactive' if ARGS.kbd else 'password'

    def check_auth_none(self, username):
        if PASSWORD is not None:
            return paramiko.AUTH_FAILED
        log('登录', username)
        return paramiko.AUTH_SUCCESSFUL

    def check_auth_publickey(self, username, key):
        return paramiko.AUTH_SUCCESSFUL if PASSWORD is None else paramiko.AUTH_FAILED

    def check_auth_password(self, username, password):
        if PASSWORD is None or ARGS.kbd:
            return paramiko.AUTH_FAILED
        ok = password == PASSWORD
        log('密码登录', username, '对' if ok else '错')
        return paramiko.AUTH_SUCCESSFUL if ok else paramiko.AUTH_FAILED

    def check_auth_interactive(self, username, submethods):
        if PASSWORD is None or not ARGS.kbd:
            return paramiko.AUTH_FAILED
        self.kbd_user = username
        return paramiko.InteractiveQuery('', '', ('Password: ', False))

    def check_auth_interactive_response(self, responses):
        ok = len(responses) == 1 and responses[0] == PASSWORD
        log('键盘交互式登录', getattr(self, 'kbd_user', '?'), '对' if ok else '错')
        return paramiko.AUTH_SUCCESSFUL if ok else paramiko.AUTH_FAILED

    def check_channel_request(self, kind, chanid):
        if kind == 'session':
            return paramiko.OPEN_SUCCEEDED
        return paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED

    def check_channel_direct_tcpip_request(self, chanid, origin, destination):
        if not ARGS.forward or destination[0] not in ('127.0.0.1', 'localhost'):
            log('拒绝转接', destination)
            return paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED
        self.forwards[chanid] = destination
        return paramiko.OPEN_SUCCEEDED

    def check_channel_exec_request(self, channel, command):
        threading.Thread(target=bridge, args=(channel, command), daemon=True).start()
        return True


def bridge(channel, command):
    log('执行', command[:60])
    # kuaid 把这个目录当主目录，它自己的东西（~/.kuai）也放在这下面，不碰真的
    env = dict(os.environ, USERPROFILE=HOME, HOME=HOME, FR_KUAID_HOME=HOME)
    proc = subprocess.Popen([KUAID, '--stdio'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, env=env, cwd=HOME)
    channel.sendall(b'KUAID-START\n')

    def up():
        try:
            while True:
                data = channel.recv(65536)
                if not data:
                    break
                proc.stdin.write(data)
                proc.stdin.flush()
        except Exception as err:
            log('客户端到 kuaid 的方向断开', err)
        finally:
            try:
                proc.stdin.close()
            except Exception:
                pass

    threading.Thread(target=up, daemon=True).start()
    try:
        while True:
            data = proc.stdout.read1(65536)
            if not data:
                break
            channel.sendall(data)
    except Exception as err:
        log('kuaid 到客户端的方向断开', err)
    code = proc.wait()
    try:
        channel.send_exit_status(code)
        channel.close()
    except Exception:
        pass
    log('kuaid 退出', code)


def forward(channel, destination):
    """跳板机转接：通道和到目标的连接两头对拷，两个方向都完了关掉通道（ssh -W 等的是关，不是 EOF）。"""
    log('转接到', destination)
    try:
        sock = socket.create_connection(destination, timeout=10)
    except OSError as err:
        log('转接连不上', err)
        channel.close()
        return
    sock.settimeout(None)

    def down():
        try:
            while True:
                data = sock.recv(65536)
                if not data:
                    break
                channel.sendall(data)
        except Exception:
            pass
        finally:
            try:
                channel.shutdown_write()
            except Exception:
                pass

    reader = threading.Thread(target=down, daemon=True)
    reader.start()
    try:
        while True:
            data = channel.recv(65536)
            if not data:
                break
            sock.sendall(data)
    except Exception:
        pass
    finally:
        try:
            sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass
    reader.join(30)
    for item in (channel, sock):
        try:
            item.close()
        except Exception:
            pass
    log('转接结束', destination)


def serve(client):
    transport = paramiko.Transport(client)
    transport.add_server_key(HOST_KEY)
    server = Server()
    try:
        transport.start_server(server=server)
    except Exception as err:
        log('握手失败', err)
        return
    # accept 给的通道要留着：paramiko 自己只弱引用，没人拿着的通道会被回收、关掉，
    # 客户端的执行请求还没到就断了（ssh -v 里是 exit status -1）
    channels = []
    while transport.is_active():
        channel = transport.accept(1)
        if channel is None:
            continue
        channels.append(channel)
        destination = server.forwards.pop(channel.get_id(), None)
        if destination:
            threading.Thread(target=forward, args=(channel, destination), daemon=True).start()


listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(('127.0.0.1', PORT))
listener.listen(8)
log('监听', PORT, '主目录', HOME, '要密码' if PASSWORD else '不要密码',
    'keyboard-interactive' if ARGS.kbd else '', '可转接' if ARGS.forward else '')
while True:
    client, addr = listener.accept()
    threading.Thread(target=serve, args=(client,), daemon=True).start()
