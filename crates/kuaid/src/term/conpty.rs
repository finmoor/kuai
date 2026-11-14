//! Windows 的伪终端（ConPTY）：开一个伪控制台，在里面启动 shell。只在终端托管进程（[`super::host`]）里用。
//!
//! shell 放进一个作业对象：托管进程退出（或者明说结束）时，里面的程序连同它们起的子进程一起结束；
//! 自己声明要脱离作业的程序（`CREATE_BREAKAWAY_FROM_JOB`）可以留下，相当于 Linux 上的 nohup。

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::FromRawHandle as _;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::Size;

/// 一个伪控制台和里面的 shell。
pub struct Console {
    hpc: HPCON,
    process: HANDLE,
    job: HANDLE,
    pub pid: u32,
    closed: AtomicBool,
}

// SAFETY: 里面都是内核句柄，哪个线程用都一样；关伪控制台只做一次（`closed`）
unsafe impl Send for Console {}
unsafe impl Sync for Console {}

impl Console {
    /// 开伪控制台，在里面启动 `program`（第一项是程序，其余是参数）。
    /// 返回它、读程序输出的那一头、写程序输入的那一头。
    pub fn spawn(
        program: &[String],
        cwd: &str,
        env: &[(String, String)],
        size: Size,
    ) -> Result<(Console, File, File)> {
        if program.is_empty() {
            bail!("没有可启动的 shell");
        }
        let (in_read, in_write) = pipe()?;
        let (out_read, out_write) = pipe()?;
        let mut hpc: HPCON = 0;
        // SAFETY: 两个句柄是刚开的管道；hpc 是合法的输出位置
        let created = unsafe { CreatePseudoConsole(coord(size), in_read, out_write, 0, &mut hpc) };
        // 伪控制台自己留了一份，这两头我们用不着了
        // SAFETY: 刚开的句柄，只关这一次
        unsafe {
            CloseHandle(in_read);
            CloseHandle(out_write);
        }
        // SAFETY: 这两头归返回的 File 管
        let (output, input) = unsafe {
            (
                File::from_raw_handle(out_read),
                File::from_raw_handle(in_write),
            )
        };
        if created < 0 {
            bail!("无法创建伪控制台（0x{created:08x}）");
        }
        let started = start_process(hpc, program, cwd, env);
        let (process, pid) = match started {
            Ok(started) => started,
            Err(err) => {
                // SAFETY: hpc 是刚创建的
                unsafe { ClosePseudoConsole(hpc) };
                return Err(err);
            }
        };
        Ok((
            Console {
                hpc,
                process: process.process,
                job: process.job,
                pid,
                closed: AtomicBool::new(false),
            },
            output,
            input,
        ))
    }

    pub fn resize(&self, size: Size) {
        if !self.closed.load(Ordering::Acquire) {
            // SAFETY: hpc 还没关
            unsafe { ResizePseudoConsole(self.hpc, coord(size)) };
        }
    }

    /// 关掉伪控制台：里面的程序收到「窗口关了」，输出的那一头读到头。可能要等里面的程序退出，
    /// 所以放在自己的线程里调用。只做一次。
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            // SAFETY: 只关这一次
            unsafe { ClosePseudoConsole(self.hpc) };
        }
    }

    /// 等 shell 退出，最多等 `ms` 毫秒（`None` 一直等）。返回退出码；没等到是 `None`。
    pub fn wait(&self, ms: Option<u32>) -> Option<i32> {
        // SAFETY: process 是我们启动的进程的句柄，一直留着
        unsafe {
            if WaitForSingleObject(self.process, ms.unwrap_or(INFINITE)) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process, &mut code);
            Some(code as i32)
        }
    }

    /// 强行结束 shell 和它起的所有程序。
    pub fn terminate(&self) {
        // SAFETY: 两个句柄一直留着
        unsafe {
            if !self.job.is_null() {
                TerminateJobObject(self.job, 1);
            }
            TerminateProcess(self.process, 1);
        }
    }
}

struct Started {
    process: HANDLE,
    job: HANDLE,
}

fn start_process(
    hpc: HPCON,
    program: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<(Started, u32)> {
    // 属性表：只有一项，告诉新进程用哪个伪控制台
    let mut size = 0usize;
    // SAFETY: 头一次调用只问要多大
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    let mut attrs = vec![0u8; size];
    let list = attrs.as_mut_ptr().cast();
    // SAFETY: attrs 有 size 那么大；hpc 在调用期间有效
    let ready = unsafe {
        InitializeProcThreadAttributeList(list, 1, 0, &mut size) != 0
            && UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const _,
                size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) != 0
    };
    if !ready {
        return Err(std::io::Error::last_os_error()).context("无法准备进程属性");
    }
    // SAFETY: 全零是这个结构体的合法初值
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    // 设了这个标记、三个句柄留空：新进程不沿用我们的标准输入输出，只用伪控制台
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.lpAttributeList = list;

    let mut command_line = wide(&command_line(program));
    let directory = wide(&cwd.replace('/', "\\"));
    let environment = env_block(env);
    // SAFETY: 全零是合法初值
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: 各个指针指向的缓冲区在调用期间都有效，命令行可写、以 0 结尾
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: list 是上面初始化过的
    unsafe { DeleteProcThreadAttributeList(list) };
    if created == 0 {
        return Err(error).with_context(|| format!("无法启动 {}", program[0]));
    }
    // 先放进作业再让它跑：它起的子进程都在作业里
    // SAFETY: 句柄都是刚拿到的；作业开不了也照常用，只是结束时带不走孙进程
    let job = unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            AssignProcessToJobObject(job, info.hProcess);
        }
        ResumeThread(info.hThread);
        CloseHandle(info.hThread);
        job
    };
    Ok((
        Started {
            process: info.hProcess,
            job,
        },
        info.dwProcessId,
    ))
}

fn pipe() -> Result<(HANDLE, HANDLE)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: 两个输出位置都合法
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(std::io::Error::last_os_error()).context("无法创建管道");
    }
    Ok((read, write))
}

fn coord(size: Size) -> COORD {
    COORD {
        X: size.cols.clamp(1, i16::MAX as u16) as i16,
        Y: size.rows.clamp(1, i16::MAX as u16) as i16,
    }
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain([0]).collect()
}

/// 环境变量块：`名=值` 一个接一个，各以 0 结尾，最后再一个 0。按名字排好（不分大小写），
/// 同名的留后面给的那个。
fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(String, &str, &str)> = Vec::new();
    for (key, value) in env {
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            continue;
        }
        let upper = key.to_uppercase();
        vars.retain(|(existing, _, _)| *existing != upper);
        vars.push((upper, key, value));
    }
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block = Vec::new();
    for (_, key, value) in vars {
        block.extend(OsStr::new(&format!("{key}={value}")).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// 把程序和参数拼成一条命令行，按 C 运行库拆命令行的规则加引号。
pub fn command_line(program: &[String]) -> String {
    program
        .iter()
        .map(|arg| quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // 引号前面的反斜杠要成对，引号自己再加一个
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
            }
        }
        if c != '\\' {
            quoted.push(c);
        }
    }
    // 结尾的反斜杠后面紧跟着收尾的引号，也要成对
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 命令行按规则加引号() {
        let line = |args: &[&str]| {
            command_line(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(line(&["pwsh.exe", "-NoLogo"]), "pwsh.exe -NoLogo");
        assert_eq!(
            line(&[r"C:\Program Files\PowerShell\7\pwsh.exe", "-NoLogo"]),
            r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#
        );
        assert_eq!(line(&["a", ""]), r#"a """#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        // 结尾的反斜杠不能把收尾的引号吃掉
        assert_eq!(line(&[r"C:\a b\"]), r#""C:\a b\\""#);
        assert_eq!(line(&[r#"a\\"b c"#]), r#""a\\\\\"b c""#);
    }

    #[test]
    fn 环境变量块排好序_同名的留后一个() {
        let env = [
            ("b".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "3".to_string()),
        ];
        let block = String::from_utf16(&env_block(&env)).unwrap();
        assert_eq!(block, "A=1\0B=3\0\0");
    }
}
