//! 这台主机的本地时间和时区：定时任务按它运行（[`fr_core::schedule`]）。
//!
//! 标准库没有本地时间。Linux 上用 C 库的 `localtime_r`（静态编译的 musl 也读 `TZ` 和
//! `/etc/localtime`），Windows 上用 `GetLocalTime` 和 `GetTimeZoneInformation`。
//!
//! 测试时设 `FR_KUAID_CLOCK=2026-11-27 08:59:50`：钟从这一刻开始走（UTC+0），不用真的等到点。

use std::sync::OnceLock;
use std::time::Instant;

use fr_core::schedule::Wall;
use fr_core::tasks::Clock;

/// 现在：哪一分钟，和时区。
pub struct Now {
    pub wall: Wall,
    pub clock: Clock,
}

/// 现在的本地时间。
pub fn now() -> Now {
    if let Some(now) = fake() {
        return now;
    }
    let (wall, _, offset) = local();
    Now {
        wall,
        clock: Clock {
            now: wall,
            offset,
            zone: zone(),
        },
    }
}

/// `FR_KUAID_CLOCK` 设的假钟：从设的那一刻起，按真实流逝的时间往后走。
fn fake() -> Option<Now> {
    static START: OnceLock<Option<(Instant, i64)>> = OnceLock::new();
    let start = START.get_or_init(|| {
        let text = std::env::var("FR_KUAID_CLOCK").ok()?;
        let (date, time) = text.trim().split_once(' ')?;
        let date = fr_core::schedule::Date::parse(date)?;
        let mut parts = time.split(':');
        let hour: u32 = parts.next()?.parse().ok()?;
        let minute: u32 = parts.next()?.parse().ok()?;
        let second: i64 = parts.next().unwrap_or("0").parse().ok()?;
        let wall = Wall::new(date.year, date.month, date.day, hour, minute);
        wall.valid()
            .then(|| (Instant::now(), wall.minutes() * 60 + second))
    });
    let (at, seconds) = (*start)?;
    let total = seconds + at.elapsed().as_secs() as i64;
    let wall = Wall::from_minutes(total.div_euclid(60));
    Some(Now {
        wall,
        clock: Clock {
            now: wall,
            offset: 0,
            zone: "test".into(),
        },
    })
}

/// `GetTimeZoneInformation` 的返回值：现在用的是标准时间、夏令时
#[cfg(windows)]
const TIME_ZONE_ID_STANDARD: u32 = 1;
#[cfg(windows)]
const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

/// 本地时间的这一分钟、第几秒、比 UTC 快几分钟。
#[cfg(unix)]
fn local() -> (Wall, u32, i32) {
    // SAFETY：`localtime_r` 只写传进去的结构，`time` 传空指针只返回现在
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    let wall = Wall::new(
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    );
    // 闰秒时是 60
    (wall, tm.tm_sec.min(59) as u32, (tm.tm_gmtoff / 60) as i32)
}

#[cfg(windows)]
fn local() -> (Wall, u32, i32) {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY：两个函数都只写传进去的结构
    let (st, tzi, which) = unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let which = GetTimeZoneInformation(&mut tzi);
        (st, tzi, which)
    };
    let wall = Wall::new(
        i32::from(st.wYear),
        u32::from(st.wMonth),
        u32::from(st.wDay),
        u32::from(st.wHour),
        u32::from(st.wMinute),
    );
    // Bias 是 UTC 比本地快几分钟
    let bias = tzi.Bias
        + match which {
            TIME_ZONE_ID_DAYLIGHT => tzi.DaylightBias,
            TIME_ZONE_ID_STANDARD => tzi.StandardBias,
            _ => 0,
        };
    (wall, u32::from(st.wSecond).min(59), -bias)
}

/// 时区的名字。Linux 上看 `TZ`、`/etc/localtime` 指向哪、`/etc/timezone`；Windows 上是系统显示的名字。
#[cfg(unix)]
fn zone() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').trim();
        if !tz.is_empty() && !tz.starts_with('/') {
            return tz.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some((_, name)) = target.split_once("zoneinfo/") {
            return name.to_string();
        }
    }
    std::fs::read_to_string("/etc/timezone")
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

#[cfg(windows)]
fn zone() -> String {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY：只写传进去的结构
    let (tzi, which) = unsafe {
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let which = GetTimeZoneInformation(&mut tzi);
        (tzi, which)
    };
    let name: &[u16] = match which {
        TIME_ZONE_ID_DAYLIGHT => &tzi.DaylightName,
        _ => &tzi.StandardName,
    };
    let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本地时间是真的时间() {
        let (wall, second, offset) = local();
        assert!(wall.valid(), "{wall:?}");
        assert!(wall.year >= 2024);
        assert!(second < 60);
        assert!((-14 * 60..=14 * 60).contains(&offset), "{offset}");
    }
}
