//! 后退、前进的两串，和浏览器一样：换页时记下离开的那一页，前进的那一串作废；后退时把现在这一页
//! 放进前进的那一串。回不去了的（仓库的标签页关了）跳过。标题栏最左边的两个按钮用（见
//! `workspace.rs` 的 `Nav`）。

/// 后退最多记这么多步
const MAX: usize = 50;

pub(crate) struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl<T: PartialEq + Clone> History<T> {
    /// 换页了：记下离开的那一页 `from`，前进的那一串作废。
    pub fn remember(&mut self, from: T) {
        if self.back.last() != Some(&from) {
            self.back.push(from);
        }
        if self.back.len() > MAX {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// 后退（`back`）、前进会到哪：最近的一个不是现在这一页、`ok` 说还回得去的。
    pub fn peek(&self, back: bool, here: &T, ok: impl Fn(&T) -> bool) -> Option<&T> {
        let stack = match back {
            true => &self.back,
            false => &self.forward,
        };
        stack.iter().rev().find(|page| *page != here && ok(page))
    }

    /// 后退、前进一步：拿出要去的那一页（中间回不去的扔掉），现在这一页 `here` 放进另一串。
    /// 没得去时是 None，两串都不动。
    pub fn step(&mut self, back: bool, here: T, ok: impl Fn(&T) -> bool) -> Option<T> {
        self.peek(back, &here, &ok)?;
        let (from, to) = match back {
            true => (&mut self.back, &mut self.forward),
            false => (&mut self.forward, &mut self.back),
        };
        while let Some(page) = from.pop() {
            if page != here && ok(&page) {
                to.push(here);
                return Some(page);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::History;

    /// 依次去了 `pages`（第一个是起点）。
    fn walked(pages: &[&'static str]) -> History<&'static str> {
        let mut history = History::default();
        for pair in pages.windows(2) {
            history.remember(pair[0]);
        }
        history
    }

    #[test]
    fn 后退前进和浏览器一样() {
        let mut history = walked(&["会话", "kuai", "工作区"]);
        let ok = |_: &&str| true;
        assert_eq!(history.peek(true, &"工作区", ok), Some(&"kuai"));
        assert_eq!(history.step(true, "工作区", ok), Some("kuai"));
        assert_eq!(history.step(true, "kuai", ok), Some("会话"));
        assert_eq!(history.step(true, "会话", ok), None);
        assert_eq!(history.step(false, "会话", ok), Some("kuai"));
        assert_eq!(history.peek(false, &"kuai", ok), Some(&"工作区"));
        // 后退以后去了别处：前进的那一串作废
        history.remember("kuai");
        assert_eq!(history.peek(false, &"主机", ok), None);
        assert_eq!(history.step(true, "主机", ok), Some("kuai"));
    }

    #[test]
    fn 回不去的跳过_和现在这一页一样的也跳过() {
        let mut history = walked(&["会话", "kuai", "会话", "agent-config"]);
        // kuai 的标签页关了
        let ok = |page: &&str| *page != "kuai";
        assert_eq!(history.step(true, "agent-config", ok), Some("会话"));
        // 再退：kuai 回不去，跳过；下一个「会话」就是现在这一页，也跳过
        assert_eq!(history.step(true, "会话", ok), None);
        // 没得退时两串都没动：还能前进回 agent-config
        assert_eq!(history.step(false, "会话", ok), Some("agent-config"));
    }

    #[test]
    fn 同一页连着记只记一次_最多记五十步() {
        let mut history = History::default();
        history.remember("会话");
        history.remember("会话");
        assert_eq!(history.back, ["会话"]);
        for n in 0..80 {
            history.remember(if n % 2 == 0 { "甲" } else { "乙" });
        }
        assert_eq!(history.back.len(), super::MAX);
    }
}
