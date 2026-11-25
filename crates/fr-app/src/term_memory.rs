//! 会话页记在盘上的「看过」：重开 Kuai、断线重连以后，离开期间有过新输出的终端才认得出来
//! （`docs/design/sessions-status/` 第 3 节）。
//!
//! 每台主机记两样：最后一次还在收它的终端列表的时刻（`until`），和各个终端最后一次看过的时刻。
//! 终端按 kuaid 给的编号加开出来的时刻认：kuaid 重新启动以后编号从 t1 重新数，光看编号会认错。
//! 时刻都是这台电脑的钟（Unix 毫秒）：「多久没有输出了」是 kuaid 按它自己的钟算好的一段时长，
//! 收到时换成这边的时刻，和这边记的比先后。
//!
//! 存在设置文件旁边单独一个文件里（不进设置：几秒就可能变一次，也不是用户的配置），有变化时隔
//! 一会儿写一次、退出时写一次。读坏了就当没有，回到「已经停着的都算看过」。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 存下的开出来的时刻和这次列表里倒推的差这么多秒以内算同一个终端（同 `presets::BORN_SLACK`）
pub const BORN_SLACK: u64 = 30;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub machines: BTreeMap<String, MachineMemo>,
}

/// 一台主机（键同 `machines`：本机是空串）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineMemo {
    /// 最后一次还在收这台的终端列表的时刻（Unix 毫秒）
    pub until: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen: Vec<SeenMemo>,
}

/// 一个终端最后一次看过的时刻。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenMemo {
    pub term: String,
    /// 开出来的时刻（Unix 秒）
    pub born: u64,
    /// 看过的时刻（Unix 毫秒）
    pub at: u64,
}

impl MachineMemo {
    /// 这个终端存下的看过的时刻。
    pub fn seen(&self, term: &str, born: u64) -> Option<u64> {
        self.seen
            .iter()
            .find(|memo| memo.term == term && memo.born.abs_diff(born) <= BORN_SLACK)
            .map(|memo| memo.at)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 这边的一个时刻换成 Unix 毫秒（`now`、`now_ms` 是同一刻）。
pub fn to_ms(at: Instant, now: Instant, now_ms: u64) -> u64 {
    now_ms.saturating_sub(now.saturating_duration_since(at).as_millis() as u64)
}

/// Unix 毫秒换成这边的时刻；比程序启动还早、换不出来的是 None。
pub fn from_ms(at: u64, now: Instant, now_ms: u64) -> Option<Instant> {
    now.checked_sub(std::time::Duration::from_millis(now_ms.saturating_sub(at)))
}

/// 设置文件旁边的那个文件：`settings.json` 旁边叫 `settings.terms.json`（测试时 FR_SETTINGS 指到哪，
/// 它就在哪，几个测试实例互不相干）。
fn file() -> PathBuf {
    let settings = crate::settings::file();
    let stem = settings
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".to_string());
    settings.with_file_name(format!("{stem}.terms.json"))
}

pub fn load() -> Memory {
    std::fs::read(file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 写进去（先写临时文件再改名，写到一半退出也不会坏）。失败只记日志：丢了也只是少提醒几个。
pub fn save(memory: &Memory) {
    let path = file();
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let written = serde_json::to_vec(memory)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        log::warn!("无法保存终端的查看记录：{err}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MachineMemo, SeenMemo, from_ms, to_ms};

    #[test]
    fn 按编号和开出来的时刻认终端() {
        let memo = MachineMemo {
            until: 0,
            seen: vec![SeenMemo {
                term: "t1".into(),
                born: 1_000,
                at: 5_000,
            }],
        };
        assert_eq!(memo.seen("t1", 1_000), Some(5_000));
        // 整秒倒推会差一两秒
        assert_eq!(memo.seen("t1", 1_020), Some(5_000));
        // kuaid 重新启动以后新开的 t1：开出来的时刻差得远
        assert_eq!(memo.seen("t1", 9_000), None);
        assert_eq!(memo.seen("t2", 1_000), None);
    }

    #[test]
    fn 时刻和毫秒来回换() {
        let now = Instant::now() + Duration::from_secs(3600);
        let now_ms = 10_000_000;
        let at = now - Duration::from_millis(2_500);
        assert_eq!(to_ms(at, now, now_ms), 9_997_500);
        let back = from_ms(9_997_500, now, now_ms).unwrap();
        assert_eq!(now.duration_since(back), Duration::from_millis(2_500));
    }
}
