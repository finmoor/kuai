"""测试用的 SSH 服务：只听 127.0.0.1，谁登录都放行，客户端要执行的命令一律不执行，
回一行 KUAID-START 以后把通道接到本机的 `kuaid --stdio` 上。客户端就能一路走到「已连接」，
目录浏览、打开仓库、git 操作看到的都是 <主目录> 下面的东西。见 README.md「登录服务器的各种情况」。

用法：python fake-sshd.py <主目录> <kuaid.exe> <端口>
设置文件里服务器的地址写 ssh://tester@127.0.0.1:<端口>。要装 paramiko。

测完要收拾的：真的 ssh.exe 会把这个服务记进 ~/.ssh/known_hosts（`[127.0.0.1]:<端口>` 那一行），删掉。
"""
import os
import socket
import subprocess
import sys
import threading

import paramiko

# 输出重定向到文件时也用 UTF-8，不跟着系统的代码页
sys.stdout.reconfigure(encoding='utf-8')
HOME, KUAID, PORT =os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2]), int(sys.argv[3])
# 主机密钥存在主目录旁边，重启这个脚本还是同一把；每次现生成的话，ssh 第二次连接就报
# 「主机密钥变了」，要先去 known_hosts 里删掉上一把
KEY_FILE = HOME.rstrip('\\/') + '.host_key'
if not os.path.exists(KEY_FILE):
    paramiko.RSAKey.generate(2048).write_private_key_file(KEY_FILE)
HOST_KEY = paramiko.RSAKey(filename=KEY_FILE)


def log(*parts):
    print(*parts, flush=True)


class Server(paramiko.ServerInterface):
    def check_auth_none(self, username):
        log('登录', username)
        return paramiko.AUTH_SUCCESSFUL

    def check_auth_publickey(self, username, key):
        return paramiko.AUTH_SUCCESSFUL

    def get_allowed_auths(self, username):
        return 'none,publickey'

    def check_channel_request(self, kind, chanid):
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


def serve(client):
    transport = paramiko.Transport(client)
    transport.add_server_key(HOST_KEY)
    try:
        transport.start_server(server=Server())
    except Exception as err:
        log('握手失败', err)
        return
    # accept 给的通道要留着：paramiko 自己只弱引用，没人拿着的通道会被回收、关掉，
    # 客户端的执行请求还没到就断了（ssh -v 里是 exit status -1）
    channels = []
    while transport.is_active():
        channel = transport.accept(1)
        if channel is not None:
            channels.append(channel)


listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(('127.0.0.1', PORT))
listener.listen(8)
log('监听', PORT, '主目录', HOME)
while True:
    client, addr = listener.accept()
    threading.Thread(target=serve, args=(client,), daemon=True).start()
