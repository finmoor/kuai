//! 标题栏上的标签页放不下时怎么排（第二版，设计见 `docs/design/server-workspace/spec.md`「标签多了怎么办」）。
//!
//! 只做算术，不碰界面：给每个标签不挤时要多宽、最窄缩到多少、一共有多宽，算出每个画多宽、
//! 哪些收进「⌄ N」。
//!
//! - 先缩最宽的：放不下时找一个上限，宽过它的都压到它，短名字尽量完整。仓库标签最窄缩到
//!   [`REPO_MIN`]，比这还窄的本来多宽就多宽。当前的那个不压：选中的标签还多一个关闭按钮，压到最窄
//!   只剩名字的头两三个字（2026-11-19 在屏幕上看到 `erp-api` 成了 `erp…`），窄得连它都放不全时才压。
//! - 都压到最窄还放不下：多一个「⌄ N」按钮，从前往后放，放不下的收进「⌄」，放下的再按剩下的地方
//!   撑开一点。
//! - 当前的标签总在外面：被挤出去时和最后一个露着的换位置（只换画的位置，不改标签页的顺序）。
//!
//! 1.17 标题栏右边还有一段终端的标签页，两段分地方；2026-11-20 终端标签页去掉了，只剩这一段。

/// 仓库标签最窄缩到这么宽（图标加名字的头几个字）
pub const REPO_MIN: f32 = 110.;

/// 一排标签：各自不挤时多宽、最窄缩到多少、当前的是第几个。
pub struct Segment<'a> {
    pub natural: &'a [f32],
    pub min: f32,
    pub active: Option<usize>,
}

/// 排出来的样子。
#[derive(Debug, PartialEq)]
pub struct Fit {
    /// 按画的先后：第几个标签、画多宽。收进「⌄」的不在里面
    pub shown: Vec<(usize, f32)>,
    /// 有标签收进了「⌄」（要画那个按钮）
    pub overflow: bool,
}

impl Fit {
    pub fn hidden(&self, total: usize) -> usize {
        total - self.shown.len()
    }
}

/// 各个标签最窄缩到多少：比下限还窄的本来多宽就多宽，当前的不缩。
fn floors(seg: &Segment) -> Vec<f32> {
    seg.natural
        .iter()
        .enumerate()
        .map(|(at, w)| match seg.active == Some(at) {
            true => *w,
            false => w.min(seg.min),
        })
        .collect()
}

fn gaps(n: usize, gap: f32) -> f32 {
    gap * n.saturating_sub(1) as f32
}

/// 把一串标签压进 `room`（不含间隔）：找一个上限，宽过它的压到它，但不低于各自的下限。全压到下限
/// 还放不下时是 None。
fn squeeze(natural: &[f32], floors: &[f32], room: f32) -> Option<Vec<f32>> {
    let total: f32 = natural.iter().sum();
    if total <= room {
        return Some(natural.to_vec());
    }
    let least: f32 = floors.iter().sum();
    if least > room + 0.01 {
        return None;
    }
    let width = |cap: f32| -> f32 {
        natural
            .iter()
            .zip(floors)
            .map(|(w, floor)| w.min(cap.max(*floor)))
            .sum()
    };
    let (mut lo, mut hi) = (0f32, natural.iter().copied().fold(0., f32::max));
    for _ in 0..40 {
        let mid = (lo + hi) / 2.;
        if width(mid) <= room {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(
        natural
            .iter()
            .zip(floors)
            .map(|(w, floor)| w.min(lo.max(*floor)))
            .collect(),
    )
}

/// 放得下哪几个：按最窄的宽度从前往后放，当前的要露在外面（放不下就和最后一个露着的换），
/// 放下的再按剩下的地方撑开。
fn place(seg: &Segment, room: f32, gap: f32) -> Vec<(usize, f32)> {
    let floors = floors(seg);
    let step = |order: &[usize], at: usize| floors[at] + if order.is_empty() { 0. } else { gap };
    let mut order: Vec<usize> = Vec::new();
    let mut used = 0.;
    for at in 0..floors.len() {
        let need = step(&order, at);
        if used + need > room + 0.01 {
            break;
        }
        used += need;
        order.push(at);
    }
    if let Some(active) = seg
        .active
        .filter(|at| *at < floors.len() && !order.contains(at))
    {
        loop {
            let need = step(&order, active);
            if used + need <= room + 0.01 || order.is_empty() {
                order.push(active);
                break;
            }
            let last = order.pop().expect("非空");
            used -= step(&order, last);
        }
    }
    let natural: Vec<f32> = order.iter().map(|at| seg.natural[*at]).collect();
    let least: Vec<f32> = order.iter().map(|at| floors[*at]).collect();
    let room_tabs = room - gaps(order.len(), gap);
    // 窄得连当前的那一个都放不全：它也按下限压
    let plain: Vec<f32> = natural.iter().map(|w| w.min(seg.min)).collect();
    let widths = squeeze(&natural, &least, room_tabs)
        .or_else(|| squeeze(&natural, &plain, room_tabs))
        .unwrap_or(plain);
    order.into_iter().zip(widths).collect()
}

/// 排标签。`room`：标签能用的宽度（别的东西已经扣掉）；`gap`：标签之间的间隔；`more`：「⌄ N」按钮
/// 连同它前面的间隔要占的宽度，放得下时不画它。
pub fn fit(tabs: &Segment, room: f32, gap: f32, more: f32) -> Fit {
    let n = tabs.natural.len();
    if let Some(widths) = squeeze(tabs.natural, &floors(tabs), room - gaps(n, gap)) {
        return Fit {
            shown: widths.into_iter().enumerate().collect(),
            overflow: false,
        };
    }
    Fit {
        shown: place(tabs, (room - more).max(0.), gap),
        overflow: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(natural: &[f32], active: Option<usize>) -> Segment<'_> {
        Segment {
            natural,
            min: REPO_MIN,
            active,
        }
    }

    fn widths(fit: &Fit) -> Vec<f32> {
        fit.shown.iter().map(|(_, w)| w.round()).collect()
    }

    #[test]
    fn 放得下就按各自的宽度() {
        let fit = fit(&seg(&[120., 200.], Some(0)), 1000., 4., 48.);
        assert!(!fit.overflow);
        assert_eq!(widths(&fit), [120., 200.]);
    }

    #[test]
    fn 先缩最宽的_短名字留着() {
        // 100 本来就比最窄的还窄，不动；300、150 压到同一个上限 150
        let fit = fit(&seg(&[100., 300., 150.], None), 400., 0., 48.);
        assert!(!fit.overflow);
        assert_eq!(widths(&fit), [100., 150., 150.]);
    }

    #[test]
    fn 放不下的收进去_当前的和最后一个露着的换位置() {
        // 扣掉「⌄」40 剩 460；按最窄 110、间隔 4 放得下 4 个（452）。当前的是第 8 个（下标 7），
        // 它不压、要 200：换掉第 3、4 个
        let fit = fit(&seg(&[200.; 10], Some(7)), 500., 4., 40.);
        assert!(fit.overflow);
        let order: Vec<usize> = fit.shown.iter().map(|(at, _)| *at).collect();
        assert_eq!(order, [0, 1, 7]);
        assert_eq!(fit.hidden(10), 7);
        // 剩下的地方撑开别的：(460 - 8 - 200) / 2 = 126
        assert_eq!(widths(&fit), [126., 126., 200.]);
    }

    #[test]
    fn 当前的标签不压_别的先缩() {
        // 一共 450：当前的（下标 2）留 200，另外两个分 250
        let fit = fit(&seg(&[200., 200., 200.], Some(2)), 450., 0., 48.);
        assert!(!fit.overflow);
        assert_eq!(widths(&fit), [125., 125., 200.]);
    }

    #[test]
    fn 窄得只放得下一个_当前的还在() {
        let fit = fit(&seg(&[200.; 3], Some(2)), 150., 4., 40.);
        assert!(fit.overflow);
        assert_eq!(fit.shown.first().map(|(at, _)| *at), Some(2));
    }
}
