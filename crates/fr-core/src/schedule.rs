//! 定时任务的时间：常用的几种（每天、工作日、每周……）和自定义表达式，算「这一分钟该不该运行」
//! 「下一次是什么时候」「刚才错过了哪一次」。
//!
//! 时间一律是任务所在主机的本地时间（[`Wall`]，墙上的钟），这里不管时区：kuaid 拿主机的本地时间
//! 来问，客户端拿主机报上来的本地时间来算显示用的「下次运行」。夏令时拨快的那一小时里的时间
//! 墙上没有，那一次就不运行；拨回的那一小时出现两遍，运行两次。cron 也是这样。
//!
//! 自定义表达式是 cron 的五段：分 时 日 月 周。支持 `*`、`,`、`-`、`/`；周日是 0 或 7；日和周都
//! 写了的时候满足一个就算；不认月份、星期的英文名。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 墙上的钟的一分钟。字段的先后就是时间的先后，可以直接比大小。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Wall {
    pub year: i32,
    /// 1～12
    pub month: u32,
    /// 1～31
    pub day: u32,
    /// 0～23
    pub hour: u32,
    /// 0～59
    pub minute: u32,
}

/// 公历的某一天是 1970-01-01 之后的第几天（Howard Hinnant 的算法，对 1970 年以前也成立）。
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// [`days_from_civil`] 反过来。
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

/// 这个月有几天。
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// 1970-01-01 00:00（只拿来当没填时的占位）。
impl Default for Wall {
    fn default() -> Wall {
        Wall::new(1970, 1, 1, 0, 0)
    }
}

impl Wall {
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Wall {
        Wall {
            year,
            month,
            day,
            hour,
            minute,
        }
    }

    /// 是不是一个真的时间（2 月 30 日、25 点不是）。
    pub fn valid(&self) -> bool {
        (1..=12).contains(&self.month)
            && (1..=days_in_month(self.year, self.month)).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
    }

    fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// 星期几：0 是周日，1 是周一……6 是周六。
    pub fn weekday(&self) -> u32 {
        // 1970-01-01 是周四
        (self.days() + 4).rem_euclid(7) as u32
    }

    /// 1970-01-01 00:00 之后的第几分钟。只拿来算两个时间差多少、往后挪几分钟。
    pub fn minutes(&self) -> i64 {
        self.days() * 1440 + i64::from(self.hour) * 60 + i64::from(self.minute)
    }

    /// [`Wall::minutes`] 反过来。
    pub fn from_minutes(minutes: i64) -> Wall {
        let (year, month, day) = civil_from_days(minutes.div_euclid(1440));
        let rest = minutes.rem_euclid(1440) as u32;
        Wall::new(year, month, day, rest / 60, rest % 60)
    }

    /// 往后（负数是往前）挪这么多分钟。
    pub fn plus_minutes(&self, minutes: i64) -> Wall {
        Wall::from_minutes(self.minutes() + minutes)
    }
}

/// 一天里的几点几分，存成 `"09:00"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    pub hour: u32,
    pub minute: u32,
}

impl Time {
    pub fn new(hour: u32, minute: u32) -> Time {
        Time { hour, minute }
    }

    /// 读 `"9:05"`、`"09:05"`；不是时间的是 None。
    pub fn parse(text: &str) -> Option<Time> {
        let (hour, minute) = text.trim().split_once(':')?;
        let (hour, minute) = (hour.parse().ok()?, minute.parse().ok()?);
        (hour < 24 && minute < 60).then_some(Time { hour, minute })
    }
}

impl std::fmt::Display for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Time, D::Error> {
        let text = String::deserialize(deserializer)?;
        Time::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("不是时间：{text}")))
    }
}

/// 某一天，存成 `"2026-11-28"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub fn new(year: i32, month: u32, day: u32) -> Date {
        Date { year, month, day }
    }

    /// 读 `"2026-11-28"`；不是真的日期的是 None。
    pub fn parse(text: &str) -> Option<Date> {
        let mut parts = text.trim().splitn(3, '-');
        let year = parts.next()?.parse().ok()?;
        let month = parts.next()?.parse().ok()?;
        let day = parts.next()?.parse().ok()?;
        Wall::new(year, month, day, 0, 0)
            .valid()
            .then_some(Date { year, month, day })
    }
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Date, D::Error> {
        let text = String::deserialize(deserializer)?;
        Date::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("不是日期：{text}")))
    }
}

/// 表达式哪里写错了。界面拿它说明是第几段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    /// 不是五段。值是实际有几段
    Fields(usize),
    /// 第几段（0 是分）写错了：不是数字、超出范围、范围写反了、步长是 0
    Field { index: usize, text: String },
}

/// 解析好的表达式。每一段是一个位图：第 n 位是 1 就是「n 可以」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u32,
    /// 第 1～31 位
    dom: u32,
    /// 第 1～12 位
    month: u16,
    /// 第 0～6 位，0 是周日
    dow: u8,
    /// 日那一段写的是 `*`
    dom_any: bool,
    /// 周那一段写的是 `*`
    dow_any: bool,
}

/// 读一段：逗号分开的几项，每项是 `*`、`n`、`a-b`，后面可以跟 `/步长`；`n/步长` 是从 n 到最大值。
fn cron_field(text: &str, low: u32, high: u32) -> Option<u64> {
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, Some(step.parse::<u32>().ok().filter(|n| *n > 0)?)),
            None => (part, None),
        };
        let (from, to) = match range {
            "*" => (low, high),
            _ => match range.split_once('-') {
                Some((from, to)) => (number(from)?, number(to)?),
                None => {
                    let from = number(range)?;
                    (from, if step.is_some() { high } else { from })
                }
            },
        };
        if from < low || to > high || from > to {
            return None;
        }
        let mut value = from;
        while value <= to {
            bits |= 1 << value;
            value += step.unwrap_or(1);
        }
    }
    Some(bits)
}

/// 只认十进制数字（`+5`、空串不算）。
fn number(text: &str) -> Option<u32> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, CronError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::Fields(fields.len()));
        }
        let field = |index: usize, low: u32, high: u32| {
            cron_field(fields[index], low, high).ok_or_else(|| CronError::Field {
                index,
                text: fields[index].to_string(),
            })
        };
        let dow = field(4, 0, 7)?;
        Ok(Cron {
            minute: field(0, 0, 59)?,
            hour: field(1, 0, 23)? as u32,
            dom: field(2, 1, 31)? as u32,
            month: field(3, 1, 12)? as u16,
            // 7 也是周日
            dow: ((dow | (dow >> 7)) & 0x7f) as u8,
            dom_any: fields[2] == "*",
            dow_any: fields[4] == "*",
        })
    }

    /// 这一天可不可以。日和周都限定了时满足一个就算，只限定一个时看那一个。
    fn day_matches(&self, year: i32, month: u32, day: u32) -> bool {
        if self.month & (1 << month) == 0 {
            return false;
        }
        let dom = self.dom & (1 << day) != 0;
        let dow = self.dow & (1 << Wall::new(year, month, day, 0, 0).weekday()) != 0;
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            (dom_any, dow_any) => (dom_any || dom) && (dow_any || dow),
        }
    }

    /// 这一分钟该不该运行。
    pub fn matches(&self, at: &Wall) -> bool {
        self.minute & (1 << at.minute) != 0
            && self.hour & (1 << at.hour) != 0
            && self.day_matches(at.year, at.month, at.day)
    }

    /// `from` 之后（不含）最近的一次。往后找五年还没有（例如 `0 0 30 2 *`）就是 None。
    pub fn next_after(&self, from: &Wall) -> Option<Wall> {
        let first_day = from.days();
        for offset in 0..=(366 * 5) {
            let (year, month, day) = civil_from_days(first_day + offset);
            if !self.day_matches(year, month, day) {
                continue;
            }
            for hour in (0..24).filter(|hour| self.hour & (1 << hour) != 0) {
                for minute in (0..60).filter(|minute| self.minute & (1 << minute) != 0) {
                    let at = Wall::new(year, month, day, hour, minute);
                    if at > *from {
                        return Some(at);
                    }
                }
            }
        }
        None
    }

    /// `from` 起的七天里平均每天运行几次。界面拿它提醒「太密了」。
    pub fn per_day(&self, from: &Wall) -> f64 {
        let first_day = from.days();
        let each = self.hour.count_ones() * self.minute.count_ones();
        let days = (0..7)
            .filter(|offset| {
                let (year, month, day) = civil_from_days(first_day + offset);
                self.day_matches(year, month, day)
            })
            .count();
        f64::from(each) * days as f64 / 7.0
    }
}

/// 一个任务什么时候运行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// 只在这一刻运行一次
    Once {
        date: Date,
        time: Time,
    },
    /// 每小时整点
    Hourly,
    Daily {
        time: Time,
    },
    /// 周一到周五
    Weekdays {
        time: Time,
    },
    /// 每周的这几天：0 是周日……6 是周六
    Weekly {
        days: Vec<u32>,
        time: Time,
    },
    /// 自定义表达式
    Cron {
        expr: String,
    },
    /// 不定时，只在点「立即运行」时运行
    Manual,
}

impl Schedule {
    /// 换成表达式的写法：「常用」切到「高级」时带过去。只运行一次的、手动的没有。
    pub fn cron_text(&self) -> Option<String> {
        match self {
            Schedule::Hourly => Some("0 * * * *".into()),
            Schedule::Daily { time } => Some(format!("{} {} * * *", time.minute, time.hour)),
            Schedule::Weekdays { time } => Some(format!("{} {} * * 1-5", time.minute, time.hour)),
            Schedule::Weekly { days, time } => {
                // 周一在前、周日在最后，和界面上的顺序一样
                let mut days: Vec<u32> = days.iter().map(|day| day % 7).collect();
                days.sort_by_key(|day| (*day + 6) % 7);
                days.dedup();
                if days.is_empty() {
                    return None;
                }
                let days: Vec<String> = days.iter().map(u32::to_string).collect();
                Some(format!(
                    "{} {} * * {}",
                    time.minute,
                    time.hour,
                    days.join(",")
                ))
            }
            Schedule::Cron { expr } => Some(expr.trim().to_string()),
            Schedule::Once { .. } | Schedule::Manual => None,
        }
    }

    /// 重复运行的那几种对应的表达式。写错的自定义表达式、一天都没选的每周是 None。
    fn cron(&self) -> Option<Cron> {
        Cron::parse(&self.cron_text()?).ok()
    }

    /// 检查能不能用：自定义表达式有没有写错。
    pub fn check(&self) -> Result<(), CronError> {
        match self {
            Schedule::Cron { expr } => Cron::parse(expr).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// 这一分钟该不该运行。
    pub fn matches(&self, at: &Wall) -> bool {
        match self {
            Schedule::Once { date, time } => *at == once(date, time),
            Schedule::Manual => false,
            _ => self.cron().is_some_and(|cron| cron.matches(at)),
        }
    }

    /// `from` 之后（不含）最近的一次；不会再运行的是 None。
    pub fn next_after(&self, from: &Wall) -> Option<Wall> {
        match self {
            Schedule::Once { date, time } => Some(once(date, time)).filter(|at| at > from),
            Schedule::Manual => None,
            _ => self.cron()?.next_after(from),
        }
    }

    /// `after`（不含）到 `upto`（含）之间最晚的那一次：主机关机、kuaid 没在运行时错过的里面
    /// 最近的一次，补运行用。中间隔得太久只看 `upto` 之前的 [`LOOK_BACK_DAYS`] 天。
    pub fn latest_between(&self, after: &Wall, upto: &Wall) -> Option<Wall> {
        let floor = upto.plus_minutes(-LOOK_BACK_DAYS * 1440);
        let mut at = (*after).max(floor);
        let mut latest = None;
        while let Some(next) = self.next_after(&at).filter(|next| next <= upto) {
            latest = Some(next);
            at = next;
        }
        latest
    }

    /// `from` 起的七天里平均每天运行几次。
    pub fn per_day(&self, from: &Wall) -> f64 {
        match self {
            Schedule::Once { .. } | Schedule::Manual => 0.0,
            _ => self.cron().map_or(0.0, |cron| cron.per_day(from)),
        }
    }
}

/// 错过的运行最多往回看几天。
pub const LOOK_BACK_DAYS: i64 = 7;

fn once(date: &Date, time: &Time) -> Wall {
    Wall::new(date.year, date.month, date.day, time.hour, time.minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-27 是周五。
    fn friday(hour: u32, minute: u32) -> Wall {
        Wall::new(2026, 11, 27, hour, minute)
    }

    fn cron(expr: &str) -> Cron {
        Cron::parse(expr).unwrap()
    }

    fn next3(expr: &str, from: Wall) -> Vec<Wall> {
        let cron = cron(expr);
        let mut list = Vec::new();
        let mut at = from;
        while list.len() < 3 {
            at = cron.next_after(&at).unwrap();
            list.push(at);
        }
        list
    }

    #[test]
    fn 日期和分钟数来回换() {
        assert_eq!(Wall::new(1970, 1, 1, 0, 0).minutes(), 0);
        assert_eq!(Wall::new(1970, 1, 1, 0, 0).weekday(), 4);
        assert_eq!(friday(12, 14).weekday(), 5);
        assert_eq!(Wall::new(2026, 11, 29, 0, 0).weekday(), 0);
        for at in [
            friday(12, 14),
            Wall::new(2024, 2, 29, 23, 59),
            Wall::new(2000, 1, 1, 0, 0),
            Wall::new(1969, 12, 31, 23, 59),
            Wall::new(2100, 3, 1, 6, 30),
        ] {
            assert_eq!(Wall::from_minutes(at.minutes()), at);
        }
        // 跨月、跨年
        assert_eq!(
            Wall::new(2026, 12, 31, 23, 30).plus_minutes(45),
            Wall::new(2027, 1, 1, 0, 15)
        );
        assert_eq!(
            Wall::new(2024, 3, 1, 0, 0).plus_minutes(-1),
            Wall::new(2024, 2, 29, 23, 59)
        );
        assert!(friday(9, 0) < friday(9, 1));
        assert!(Wall::new(2026, 12, 1, 0, 0) > friday(23, 59));
    }

    #[test]
    fn 不是真的时间认得出() {
        assert!(Wall::new(2024, 2, 29, 0, 0).valid());
        assert!(!Wall::new(2026, 2, 29, 0, 0).valid());
        assert!(!Wall::new(2026, 4, 31, 0, 0).valid());
        assert!(!Wall::new(2026, 13, 1, 0, 0).valid());
        assert!(!Wall::new(2026, 1, 1, 24, 0).valid());
        assert_eq!(Date::parse("2026-11-28"), Some(Date::new(2026, 11, 28)));
        assert_eq!(Date::parse("2026-02-30"), None);
        assert_eq!(Time::parse("9:05"), Some(Time::new(9, 5)));
        assert_eq!(Time::parse("24:00"), None);
        assert_eq!(Time::new(9, 5).to_string(), "09:05");
    }

    #[test]
    fn 表达式_接下来三次() {
        // 这几条和原型（docs/design/scheduled-tasks）里核对过的一样
        assert_eq!(
            next3("0 8-20 * * 1-5", friday(12, 14)),
            [friday(13, 0), friday(14, 0), friday(15, 0)]
        );
        assert_eq!(
            next3("*/30 * * * *", friday(12, 14)),
            [friday(12, 30), friday(13, 0), friday(13, 30)]
        );
        assert_eq!(
            next3("0 9,18 * * 1-5", friday(12, 14)),
            [
                friday(18, 0),
                Wall::new(2026, 11, 30, 9, 0),
                Wall::new(2026, 11, 30, 18, 0)
            ]
        );
        assert_eq!(
            next3("0 3 1 * *", friday(12, 14)),
            [
                Wall::new(2026, 12, 1, 3, 0),
                Wall::new(2027, 1, 1, 3, 0),
                Wall::new(2027, 2, 1, 3, 0)
            ]
        );
        // n/步长：从 n 到最大值；周日写 0 或 7 都行
        assert_eq!(
            next3("5/20 6 * * 0,7", friday(12, 14)),
            [
                Wall::new(2026, 11, 29, 6, 5),
                Wall::new(2026, 11, 29, 6, 25),
                Wall::new(2026, 11, 29, 6, 45)
            ]
        );
    }

    #[test]
    fn 表达式_日和周都写了满足一个就算() {
        // 13 日，或者周五
        assert_eq!(
            next3("0 0 13 * 5", friday(12, 14)),
            [
                Wall::new(2026, 12, 4, 0, 0),
                Wall::new(2026, 12, 11, 0, 0),
                Wall::new(2026, 12, 13, 0, 0)
            ]
        );
        // 只限定了日：不看星期
        assert!(cron("0 0 13 * *").matches(&Wall::new(2026, 12, 13, 0, 0)));
        assert!(!cron("0 0 13 * *").matches(&Wall::new(2026, 12, 4, 0, 0)));
        // 只限定了周
        assert!(cron("0 0 * * 5").matches(&Wall::new(2026, 12, 4, 0, 0)));
        assert!(!cron("0 0 * * 5").matches(&Wall::new(2026, 12, 13, 0, 0)));
    }

    #[test]
    fn 表达式_从这一刻往后不含这一刻() {
        assert_eq!(
            cron("0 9 * * *").next_after(&friday(9, 0)),
            Some(Wall::new(2026, 11, 28, 9, 0))
        );
        assert_eq!(
            cron("0 9 * * *").next_after(&friday(8, 59)),
            Some(friday(9, 0))
        );
        // 四年一次的 2 月 29 日找得到，永远没有的 2 月 30 日是 None
        assert_eq!(
            cron("0 0 29 2 *").next_after(&friday(0, 0)),
            Some(Wall::new(2028, 2, 29, 0, 0))
        );
        assert_eq!(cron("0 0 30 2 *").next_after(&friday(0, 0)), None);
    }

    #[test]
    fn 表达式_写错的说得出是哪一段() {
        assert_eq!(Cron::parse("* * * *"), Err(CronError::Fields(4)));
        assert_eq!(Cron::parse(""), Err(CronError::Fields(0)));
        let field = |index: usize, text: &str| {
            Err(CronError::Field {
                index,
                text: text.to_string(),
            })
        };
        assert_eq!(Cron::parse("61 * * * *"), field(0, "61"));
        assert_eq!(Cron::parse("0 24 * * *"), field(1, "24"));
        assert_eq!(Cron::parse("0 9 0 * *"), field(2, "0"));
        assert_eq!(Cron::parse("0 9 * 13 *"), field(3, "13"));
        assert_eq!(Cron::parse("0 9 * * 1-8"), field(4, "1-8"));
        assert_eq!(Cron::parse("0 9 * * mon"), field(4, "mon"));
        assert_eq!(Cron::parse("*/0 * * * *"), field(0, "*/0"));
        assert_eq!(Cron::parse("30-10 * * * *"), field(0, "30-10"));
        assert_eq!(Cron::parse("1,,2 * * * *"), field(0, "1,,2"));
        assert_eq!(Cron::parse("+5 * * * *"), field(0, "+5"));
        // 多几个空格没关系
        assert!(Cron::parse("  0   9  *  *  1-5 ").is_ok());
    }

    #[test]
    fn 表达式_平均每天几次() {
        assert_eq!(cron("*/30 * * * *").per_day(&friday(12, 14)), 48.0);
        assert_eq!(cron("0 9 * * *").per_day(&friday(12, 14)), 1.0);
        // 工作日 13 个整点：七天里有五天
        let per_day = cron("0 8-20 * * 1-5").per_day(&friday(12, 14));
        assert!((per_day - 13.0 * 5.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn 常用的几种_下一次() {
        let now = friday(12, 14);
        let daily = Schedule::Daily {
            time: Time::new(9, 0),
        };
        assert_eq!(daily.next_after(&now), Some(Wall::new(2026, 11, 28, 9, 0)));
        // 周五中午以后，工作日的下一次是周一
        let weekdays = Schedule::Weekdays {
            time: Time::new(2, 0),
        };
        assert_eq!(
            weekdays.next_after(&now),
            Some(Wall::new(2026, 11, 30, 2, 0))
        );
        let weekly = Schedule::Weekly {
            days: vec![1],
            time: Time::new(10, 0),
        };
        assert_eq!(
            weekly.next_after(&now),
            Some(Wall::new(2026, 11, 30, 10, 0))
        );
        assert_eq!(Schedule::Hourly.next_after(&now), Some(friday(13, 0)));
        assert_eq!(Schedule::Manual.next_after(&now), None);
        // 一天都没选的每周：不运行
        let nothing = Schedule::Weekly {
            days: vec![],
            time: Time::new(10, 0),
        };
        assert_eq!(nothing.next_after(&now), None);
        assert!(!nothing.matches(&Wall::new(2026, 11, 30, 10, 0)));
    }

    #[test]
    fn 只运行一次的_过了就没有() {
        let once = Schedule::Once {
            date: Date::new(2026, 11, 28),
            time: Time::new(8, 30),
        };
        let at = Wall::new(2026, 11, 28, 8, 30);
        assert_eq!(once.next_after(&friday(12, 14)), Some(at));
        assert_eq!(once.next_after(&at), None);
        assert!(once.matches(&at));
        assert!(!once.matches(&at.plus_minutes(1)));
        assert_eq!(once.cron_text(), None);
    }

    #[test]
    fn 常用的换成表达式() {
        let text = |schedule: Schedule| schedule.cron_text().unwrap();
        assert_eq!(text(Schedule::Hourly), "0 * * * *");
        assert_eq!(
            text(Schedule::Daily {
                time: Time::new(9, 30)
            }),
            "30 9 * * *"
        );
        assert_eq!(
            text(Schedule::Weekdays {
                time: Time::new(2, 0)
            }),
            "0 2 * * 1-5"
        );
        // 周一在前、周日（0 或 7）在最后，重复的去掉
        assert_eq!(
            text(Schedule::Weekly {
                days: vec![0, 3, 1, 7],
                time: Time::new(10, 5)
            }),
            "5 10 * * 1,3,0"
        );
    }

    #[test]
    fn 错过的里面最近的一次() {
        let daily = Schedule::Daily {
            time: Time::new(9, 0),
        };
        // 周三 10 点以后关机，周五 11:40 才起来：补周五 9 点这一次
        let off = Wall::new(2026, 11, 25, 10, 0);
        assert_eq!(
            daily.latest_between(&off, &friday(11, 40)),
            Some(friday(9, 0))
        );
        // 没错过
        assert_eq!(daily.latest_between(&friday(9, 0), &friday(11, 40)), None);
        // 正好这一分钟到点的也算
        assert_eq!(
            daily.latest_between(&friday(8, 59), &friday(9, 0)),
            Some(friday(9, 0))
        );
        // 关了一个月：只往回看七天，补的还是最近的那一次
        let long_ago = Wall::new(2026, 10, 20, 0, 0);
        assert_eq!(
            daily.latest_between(&long_ago, &friday(11, 40)),
            Some(friday(9, 0))
        );
        // 每周一次、关了一个月：七天里有一次
        let weekly = Schedule::Weekly {
            days: vec![1],
            time: Time::new(10, 0),
        };
        assert_eq!(
            weekly.latest_between(&long_ago, &friday(11, 40)),
            Some(Wall::new(2026, 11, 23, 10, 0))
        );
        assert_eq!(
            Schedule::Manual.latest_between(&long_ago, &friday(11, 40)),
            None
        );
    }

    #[test]
    fn 存成的样子() {
        let json = |schedule: &Schedule| serde_json::to_string(schedule).unwrap();
        let daily = Schedule::Daily {
            time: Time::new(9, 0),
        };
        assert_eq!(json(&daily), r#"{"kind":"daily","time":"09:00"}"#);
        let once = Schedule::Once {
            date: Date::new(2026, 11, 28),
            time: Time::new(8, 30),
        };
        assert_eq!(
            json(&once),
            r#"{"kind":"once","date":"2026-11-28","time":"08:30"}"#
        );
        let cron = Schedule::Cron {
            expr: "0 8-20 * * 1-5".into(),
        };
        assert_eq!(json(&cron), r#"{"kind":"cron","expr":"0 8-20 * * 1-5"}"#);
        assert_eq!(json(&Schedule::Manual), r#"{"kind":"manual"}"#);
        for schedule in [daily, once, cron, Schedule::Hourly, Schedule::Manual] {
            let back: Schedule = serde_json::from_str(&json(&schedule)).unwrap();
            assert_eq!(back, schedule);
        }
        // 写错的时间读不进来
        assert!(serde_json::from_str::<Schedule>(r#"{"kind":"daily","time":"25:00"}"#).is_err());
        // 写错的表达式读得进来（存的是原文），用的时候才知道
        let bad: Schedule = serde_json::from_str(r#"{"kind":"cron","expr":"oops"}"#).unwrap();
        assert_eq!(bad.check(), Err(CronError::Fields(1)));
        assert_eq!(bad.next_after(&friday(0, 0)), None);
        assert_eq!(bad.per_day(&friday(0, 0)), 0.0);
    }
}
