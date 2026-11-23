//! Linux：收到 SIGTERM（注销桌面、`kill`）、SIGHUP（启动它的终端关了）、SIGINT（在终端里按 Ctrl+C）
//! 时照常退出，和托盘菜单的「退出」一样：映射到本机的目录断开、托盘图标撤掉。默认的处理是当场结束
//! 进程，映射的文件夹会留成一个断了的挂载点（读它报「传输端点尚未连接」），要到下次映射才清掉。
//!
//! 信号处理函数里只往管道里写一个字节；另一个线程读出来交给界面线程去退出。退出卡住时（主机上的
//! 文件操作没回来，断开映射要等）再来一次同样的信号，就按默认处理当场结束。处理函数在 exec 时恢复
//! 成默认，拉起的 kuaid、终端里的程序不受影响（屏蔽信号的话会被继承，所以不用 sigwait 那一套）。

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use futures::channel::mpsc;

/// 管道的写端，处理函数往这里写。
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
/// 已经收到过一次：再来就当场结束。
static GOT: AtomicBool = AtomicBool::new(false);

const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

/// 开始接这几个信号，收到的信号号从返回的通道里出来。只调一次；建不起来（管道、线程）返回 `None`，
/// 照默认处理。
pub fn listen() -> Option<mpsc::UnboundedReceiver<i32>> {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;

    let mut fds = [0; 2];
    // SAFETY：fds 是两个 int 的数组，pipe2 往里填读端和写端
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    // SAFETY：读端是刚建的、只有这里用
    let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let (tx, rx) = mpsc::unbounded();
    let spawned = std::thread::Builder::new()
        .name("kuai-signals".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            while read.read_exact(&mut byte).is_ok() {
                if tx.unbounded_send(i32::from(byte[0])).is_err() {
                    break;
                }
            }
        });
    if spawned.is_err() {
        // SAFETY：写端还没交出去
        unsafe { libc::close(fds[1]) };
        return None;
    }
    WRITE_FD.store(fds[1], Ordering::Relaxed);
    for signal in SIGNALS {
        // SAFETY：sigaction 结构清零后只填处理函数、标志和空的屏蔽集
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    Some(rx)
}

/// 信号处理函数：只调异步信号安全的 write、signal、raise。
extern "C" fn on_signal(signal: libc::c_int) {
    if GOT.swap(true, Ordering::Relaxed) {
        // 第二次：恢复默认处理，再发给自己一次，当场结束
        // SAFETY：signal、raise 都是异步信号安全的
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        return;
    }
    let byte = signal as u8;
    // SAFETY：写端在 listen 里建好，一直不关；写一个字节
    unsafe {
        libc::write(
            WRITE_FD.load(Ordering::Relaxed),
            (&byte as *const u8).cast(),
            1,
        );
    }
}
