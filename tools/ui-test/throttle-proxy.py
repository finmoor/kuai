"""测试用的限速转发：听 127.0.0.1:<听的端口>，接到 127.0.0.1:<目标端口>（一般是 fake-sshd.py 的端口）。
目标到客户端的方向限速，客户端到目标不限，用来模拟慢线路：主机上的终端刷屏时 kuaid 那边积压、
停发、追上；也可以让握手慢下来，测「还在连」时的界面。

用法：python throttle-proxy.py <听的端口> <目标端口> <每秒字节数> [开头不发的秒数]

- 第四个参数：每条连接开头这么多秒里，目标到客户端的方向什么都不发（模拟握手很慢的线路）。
- 设置文件里主机的地址写 ssh://tester@127.0.0.1:<听的端口>。
"""
import socket
import sys
import threading
import time

# 输出重定向到文件时也用 UTF-8，不跟着系统的代码页
sys.stdout.reconfigure(encoding='utf-8')
sys.stderr.reconfigure(encoding='utf-8')
if len(sys.argv) < 4:
    sys.exit(__doc__)
listen_port, target_port, rate = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
hold = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0


def log(*parts):
    print(*parts, flush=True)


def pipe(src, dst, limit):
    """把 src 收到的转给 dst；limit 是每秒字节数，0 是不限。

    像真的线路一样一块块排着传：每块要等前面的按 limit 传完。不能按「连接开始以来的平均速度」算：
    连接闲了一阵以后攒下的额度会让一大块一下子过去，测不出慢线路上收一大块要多久。"""
    start = time.monotonic()
    free_at = start
    try:
        while True:
            data = src.recv(16384)
            if not data:
                break
            if limit and hold:
                wait = hold - (time.monotonic() - start)
                if wait > 0:
                    time.sleep(wait)
            dst.sendall(data)
            if limit:
                now = time.monotonic()
                free_at = max(free_at, now) + len(data) / limit
                time.sleep(max(0.0, free_at - now))
    except OSError as err:
        log('断开', err)
    finally:
        # 一个方向完了两个方向都关：只关一半的话 ssh 不退出
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(('127.0.0.1', listen_port))
server.listen(8)
log('听', listen_port, '->', target_port, '限速', rate, '字节/秒', f'开头不发 {hold} 秒' if hold else '')
while True:
    client, addr = server.accept()
    upstream = socket.create_connection(('127.0.0.1', target_port))
    log('连接', addr)
    threading.Thread(target=pipe, args=(client, upstream, 0), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client, rate), daemon=True).start()
