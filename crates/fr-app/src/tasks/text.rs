//! 定时任务界面上的说法：时间怎么写、多久以后、用了多久、各个选项叫什么。

use fr_core::i18n::{tr, trc};
use fr_core::schedule::{Schedule, Time, Wall};
use fr_core::task::{ArgStyle, Confirm, Launch};
use fr_core::tasks::{Missed, Overlap};
use fr_core::tf;

/// 星期几的简称：0 是周日。
pub fn weekday(day: u32) -> &'static str {
    match day % 7 {
        0 => tr("周日"),
        1 => tr("周一"),
        2 => tr("周二"),
        3 => tr("周三"),
        4 => tr("周四"),
        5 => tr("周五"),
        _ => tr("周六"),
    }
}

/// 「每周」后面那几天里的一天：中文只写「一」「三」，英文写 Mon、Wed。
fn weekday_bare(day: u32) -> &'static str {
    match day % 7 {
        0 => trc("星期", "日"),
        1 => trc("星期", "一"),
        2 => trc("星期", "二"),
        3 => trc("星期", "三"),
        4 => trc("星期", "四"),
        5 => trc("星期", "五"),
        _ => trc("星期", "六"),
    }
}

/// 每周的几天，周一在前、周日在最后。
pub fn sorted_days(days: &[u32]) -> Vec<u32> {
    let mut days: Vec<u32> = days.iter().map(|day| day % 7).collect();
    days.sort_by_key(|day| (*day + 6) % 7);
    days.dedup();
    days
}

/// 什么时候运行，一句话：「工作日 02:00」「每周一、三、五 09:00」「表达式 0 8-20 * * 1-5」。
pub fn schedule(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Once { date, time } => {
            tf!(
                "仅一次 {date} {time}",
                date = date.to_string(),
                time = time.to_string()
            )
        }
        Schedule::Hourly => tr("每小时").to_string(),
        Schedule::Daily { time } => tf!("每天 {time}", time = time.to_string()),
        Schedule::Weekdays { time } => tf!("工作日 {time}", time = time.to_string()),
        Schedule::Weekly { days, time } => {
            let days: Vec<&str> = sorted_days(days).into_iter().map(weekday_bare).collect();
            tf!(
                "每周{days} {time}",
                days = days.join(trc("星期", "、")),
                time = time.to_string()
            )
        }
        Schedule::Cron { expr } => tf!("表达式 {expr}", expr = expr.trim()),
        Schedule::Manual => tr("手动").to_string(),
    }
}

fn day_number(at: &Wall) -> i64 {
    at.minutes().div_euclid(1440)
}

fn clock(at: &Wall) -> String {
    Time::new(at.hour, at.minute).to_string()
}

/// 某一刻相对现在怎么写：「今天 13:00」「明天 02:00」「周一 02:00」（七天之内的）、「昨天 02:00」，
/// 再远的「11-25 02:00」，不是今年的带年份。
pub fn when(at: &Wall, now: &Wall) -> String {
    let days = day_number(at) - day_number(now);
    let time = clock(at);
    match days {
        0 => tf!("今天 {time}", time),
        1 => tf!("明天 {time}", time),
        -1 => tf!("昨天 {time}", time),
        2..=6 => format!("{} {time}", weekday(at.weekday())),
        _ if at.year == now.year => format!("{:02}-{:02} {time}", at.month, at.day),
        _ => format!("{:04}-{:02}-{:02} {time}", at.year, at.month, at.day),
    }
}

/// 短的开始时间，同一个任务的几次运行在会话页里靠它区分：当天的「14:46」，别的日子「11-26 14:46」，
/// 不是今年的带上年份。不知道主机现在几点时只写时分。
pub fn short_when(at: &Wall, now: Option<&Wall>) -> String {
    let time = clock(at);
    match now {
        Some(now) if day_number(at) != day_number(now) => match at.year == now.year {
            true => format!("{:02}-{:02} {time}", at.month, at.day),
            false => format!("{:04}-{:02}-{:02} {time}", at.year, at.month, at.day),
        },
        _ => time,
    }
}

/// 离那一刻还有多久：「46 分钟后」「21 小时后」「3 天后」（隔了几天按日子算，不按 24 小时）。
pub fn until(at: &Wall, now: &Wall) -> String {
    let minutes = at.minutes() - now.minutes();
    if minutes < 1 {
        return tr("即将运行").to_string();
    }
    if minutes < 60 {
        return tf!("{n} 分钟后", n = minutes);
    }
    let days = day_number(at) - day_number(now);
    if minutes < 24 * 60 || days < 1 {
        return tf!("{n} 小时后", n = minutes / 60);
    }
    tf!("{n} 天后", n = days.max(1))
}

/// 用了多久：「40 秒」「26 分 00 秒」「1 小时 05 分」。
pub fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => tf!("{s} 秒", s = seconds),
        60..3600 => tf!(
            "{m} 分 {s} 秒",
            m = seconds / 60,
            s = format!("{:02}", seconds % 60)
        ),
        _ => tf!(
            "{h} 小时 {m} 分",
            h = seconds / 3600,
            m = format!("{:02}", seconds % 3600 / 60)
        ),
    }
}

/// 常驻进程运行了多久：「5 分钟」「2 小时 10 分钟」「3 天 2 小时」。
pub fn uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86400, seconds % 86400 / 3600, seconds % 3600 / 60);
    match (days, hours) {
        (0, 0) => tf!("{n} 分钟", n = minutes),
        (0, _) => tf!("{h} 小时 {m} 分钟", h = hours, m = minutes),
        _ => tf!("{d} 天 {h} 小时", d = days, h = hours),
    }
}

/// 思考深度的说法；空着是「默认」。
pub fn effort(effort: &str) -> String {
    match effort {
        "" => tr("默认").to_string(),
        "low" => trc("思考深度", "低").to_string(),
        "medium" => trc("思考深度", "中").to_string(),
        "high" => trc("思考深度", "高").to_string(),
        "xhigh" => trc("思考深度", "超高").to_string(),
        "max" => trc("思考深度", "最高").to_string(),
        other => other.to_string(),
    }
}

/// 「将运行」命令后面那个小牌：提示词前面要跟的参数（Antigravity CLI 的 `-i`、OpenCode 的
/// `--prompt`）也写出来，免得以为命令后面直接跟提示词。
pub fn prompt_chip(launch: &Launch) -> String {
    match launch.prompt_flag.is_empty() {
        true => tr("提示词").to_string(),
        false => format!("{} {}", launch.prompt_flag, tr("提示词")),
    }
}

/// 「需要确认时」的几种。
pub fn confirm(confirm: Confirm) -> &'static str {
    match confirm {
        Confirm::Default => tr("按程序设置"),
        Confirm::ReadOnly => tr("只读，不修改文件"),
        Confirm::AcceptEdits => tr("自动接受文件修改"),
        Confirm::Auto => tr("自动处理"),
        Confirm::SkipAll => tr("全部跳过"),
    }
}

/// 「需要确认时」选项下面的说明。各工具的做法不全一样：Codex 的「自动接受文件修改」是工作区
/// 沙箱加从不询问（`-s workspace-write -a never`），命令也直接跑，越界的由沙箱拦下。`root`：任务所在
/// 主机的常驻进程以 root 运行（Claude Code 这时不许「全部跳过」）。
pub fn confirm_tip(style: Option<ArgStyle>, confirm: Confirm, root: bool) -> &'static str {
    match (style, confirm) {
        (Some(ArgStyle::Claude), Confirm::SkipAll) if root => {
            tr("以 root 用户运行时 Claude Code 不允许跳过全部确认，保存时改为「按程序设置」。")
        }
        (Some(ArgStyle::Codex), Confirm::AcceptEdits) => tr(
            "在仓库里修改文件、运行命令都不再询问；在仓库以外写文件、联网会被沙箱拦下，不会停下来问。",
        ),
        (_, Confirm::Default) => tr("不加参数，按程序自己的设置；停下来问时等人处理。"),
        (_, Confirm::ReadOnly) => tr("只看不改：程序不修改文件、不运行会改动东西的命令。"),
        (_, Confirm::AcceptEdits) => tr("修改仓库里的文件不再询问，运行其他命令仍会停下来问。"),
        (_, Confirm::Auto) => tr("由程序判断哪些可以直接做，拿不准的仍会停下来问。"),
        (_, Confirm::SkipAll) => tr("不经确认执行任何命令。只在隔离的环境中使用。"),
    }
}

pub fn missed(missed: Missed) -> &'static str {
    match missed {
        Missed::CatchUp => tr("主机恢复后补运行最近一次"),
        Missed::Skip => tr("跳过"),
    }
}

pub fn overlap(overlap: Overlap) -> &'static str {
    match overlap {
        Overlap::Skip => tr("跳过本次"),
        Overlap::Run => tr("照常运行"),
    }
}

/// 保留几次会话：0 是全部。
pub fn keep(keep: u32) -> String {
    match keep {
        0 => tr("全部").to_string(),
        n => tf!("最近 {n} 次", n),
    }
}

/// kuaid 写的跳过、失败的原因：认得的换成界面语言的说法，别的原样。
pub fn reason(text: &str) -> &str {
    match text {
        "上一次仍在运行" => tr("上一次仍在运行"),
        "主机未运行" => tr("主机未运行"),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{duration, schedule, sorted_days, until, uptime, when};
    use fr_core::schedule::{Date, Schedule, Time, Wall};

    #[test]
    fn 时间按今天明天星期几写() {
        // 2026-11-27 是周五
        let now = Wall::new(2026, 11, 27, 12, 14);
        assert_eq!(when(&Wall::new(2026, 11, 27, 13, 0), &now), "今天 13:00");
        assert_eq!(when(&Wall::new(2026, 11, 28, 2, 0), &now), "明天 02:00");
        assert_eq!(when(&Wall::new(2026, 11, 30, 2, 0), &now), "周一 02:00");
        assert_eq!(when(&Wall::new(2026, 11, 26, 2, 0), &now), "昨天 02:00");
        assert_eq!(when(&Wall::new(2026, 11, 25, 2, 0), &now), "11-25 02:00");
        assert_eq!(when(&Wall::new(2027, 1, 2, 8, 0), &now), "2027-01-02 08:00");
    }

    #[test]
    fn 多久以后() {
        let now = Wall::new(2026, 11, 27, 12, 14);
        assert_eq!(until(&Wall::new(2026, 11, 27, 13, 0), &now), "46 分钟后");
        assert_eq!(until(&Wall::new(2026, 11, 28, 9, 0), &now), "20 小时后");
        // 周五中午到周一凌晨：按日子算是 3 天
        assert_eq!(until(&Wall::new(2026, 11, 30, 2, 0), &now), "3 天后");
    }

    #[test]
    fn 用了多久_运行了多久() {
        assert_eq!(duration(40), "40 秒");
        assert_eq!(duration(26 * 60), "26 分 00 秒");
        assert_eq!(duration(3600 + 5 * 60), "1 小时 05 分");
        assert_eq!(uptime(5 * 60), "5 分钟");
        assert_eq!(uptime(2 * 3600 + 600), "2 小时 10 分钟");
        assert_eq!(uptime(3 * 86400 + 2 * 3600), "3 天 2 小时");
    }

    #[test]
    fn 时间的一句话() {
        let at = Time::new(9, 0);
        assert_eq!(schedule(&Schedule::Daily { time: at }), "每天 09:00");
        assert_eq!(schedule(&Schedule::Weekdays { time: at }), "工作日 09:00");
        assert_eq!(
            schedule(&Schedule::Weekly {
                days: vec![5, 1, 3],
                time: at
            }),
            "每周一、三、五 09:00"
        );
        assert_eq!(
            schedule(&Schedule::Once {
                date: Date::new(2026, 11, 28),
                time: at
            }),
            "仅一次 2026-11-28 09:00"
        );
        assert_eq!(sorted_days(&[0, 6, 1]), vec![1, 6, 0]);
    }
}
