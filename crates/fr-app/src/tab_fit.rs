//! 标题栏上的标签页放不下时怎么排（第二版，设计见 `design/server-workspace/spec.md`「标签多了怎么办」）。
//!
//! 只做算术，不碰界面：给每个标签不挤时要多宽、各段最窄缩到多少、一共有多宽，算出每个画多宽、
//! 哪些收进「⌄ N」。
//!
//! - 先缩最宽的：放不下时找一个上限，宽过它的都压到它，短名字尽量完整。仓库标签最窄缩到
//!   [`REPO_MIN`]，终端标签 [`TERM_MIN`]，比这还窄的本来多宽就多宽。两段一起压，同一个上限。
//!   当前的那个不压：选中的标签还多一个关闭按钮，压到最窄只剩名字的头两三个字（2026-11-19 在屏幕上
//!   看到 `erp-api` 成了 `erp…`），窄得连它都放不全时才压。
//! - 都压到最窄还放不下：多一个「⌄ N」按钮。仓库那一段先保住标题栏的 [`REPO_SHARE`]，终端那一段
//!   [`TERM_SHARE`]（需要的不到这么多就只给需要的），剩下的地方先给仓库、再给终端；每段从前往后
//!   放，放不下的收进「⌄」，放下的再按剩下的地方撑开一点。
//! - 当前的标签总在外面：被挤出去时和最后一个露着的换位置（只换画的位置，不改标签页的顺序）。

/// 仓库标签最窄缩到这么宽（图标加名字的头几个字）
pub const REPO_MIN: f32 = 110.;
/// 终端标签最窄缩到这么宽（状态点加名字的头几个字）
pub const TERM_MIN: f32 = 72.;
/// 放不下时仓库那一段至少保住标题栏的这么多
pub const REPO_SHARE: f32 = 0.38;
/// 放不下时终端那一段至少保住标题栏的这么多
pub const TERM_SHARE: f32 = 0.20;

/// 一段标签：各自不挤时多宽、最窄缩到多少、当前的是第几个。
pub struct Segment<'a> {
    pub natural: &'a [f32],
    pub min: f32,
    pub active: Option<usize>,
}

/// 一段排出来的样子。
#[derive(Debug, PartialEq)]
pub struct Placed {
    /// 按画的先后：第几个标签、画多宽。收进「⌄」的不在里面
    pub shown: Vec<(usize, f32)>,
}

impl Placed {
    pub fn hidden(&self, total: usize) -> usize {
        total - self.shown.len()
    }
}

/// 排好的两段。
#[derive(Debug, PartialEq)]
pub struct Fit {
    pub repos: Placed,
    pub terms: Placed,
    /// 有标签收进了「⌄」（要画那个按钮）
    pub overflow: bool,
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

/// 一段里放得下哪几个：按最窄的宽度从前往后放，当前的要露在外面（放不下就和最后一个露着的换），
/// 放下的再按剩下的地方撑开。
fn place(seg: &Segment, room: f32, gap: f32) -> Placed {
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
    Placed {
        shown: order.into_iter().zip(widths).collect(),
    }
}

/// 排两段标签。`room`：标签能用的宽度（别的东西、两段之间的间隔已经扣掉）；`gap`：同一段里标签
/// 之间的间隔；`bar`：整个标题栏的宽度（各段保底按它算）；`more`：「⌄ N」按钮连同它前面的间隔
/// 要占的宽度，放得下时不画它。
pub fn fit(repos: &Segment, terms: &Segment, room: f32, gap: f32, bar: f32, more: f32) -> Fit {
    let (nr, nt) = (repos.natural.len(), terms.natural.len());
    let natural: Vec<f32> = repos.natural.iter().chain(terms.natural).copied().collect();
    let least: Vec<f32> = floors(repos).into_iter().chain(floors(terms)).collect();
    let room_tabs = room - gaps(nr, gap) - gaps(nt, gap);
    if let Some(widths) = squeeze(&natural, &least, room_tabs) {
        return Fit {
            repos: Placed {
                shown: widths[..nr].iter().copied().enumerate().collect(),
            },
            terms: Placed {
                shown: widths[nr..].iter().copied().enumerate().collect(),
            },
            overflow: false,
        };
    }
    // 都压到最窄还放不下：先各自保底，剩下的先给仓库
    let room = (room - more).max(0.);
    let need_r = floors(repos).iter().sum::<f32>() + gaps(nr, gap);
    let need_t = floors(terms).iter().sum::<f32>() + gaps(nt, gap);
    let base_r = need_r.min(REPO_SHARE * bar);
    let base_t = need_t.min(TERM_SHARE * bar);
    let (room_r, room_t) = if base_r + base_t > room {
        // 窄到连保底都给不起：按保底的比例分
        let scale = room / (base_r + base_t);
        (base_r * scale, base_t * scale)
    } else {
        let extra = room - base_r - base_t;
        let room_r = need_r.min(base_r + extra);
        let room_t = need_t.min(base_t + extra - (room_r - base_r));
        (room_r, room_t)
    };
    Fit {
        repos: place(repos, room_r, gap),
        terms: place(terms, room_t, gap),
        overflow: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(natural: &[f32], min: f32, active: Option<usize>) -> Segment<'_> {
        Segment {
            natural,
            min,
            active,
        }
    }

    fn widths(placed: &Placed) -> Vec<f32> {
        placed.shown.iter().map(|(_, w)| w.round()).collect()
    }

    #[test]
    fn 放得下就按各自的宽度() {
        let (repos, terms) = ([120., 200.], [90.]);
        let fit = fit(
            &seg(&repos, REPO_MIN, Some(0)),
            &seg(&terms, TERM_MIN, None),
            1000.,
            4.,
            1500.,
            48.,
        );
        assert!(!fit.overflow);
        assert_eq!(widths(&fit.repos), [120., 200.]);
        assert_eq!(widths(&fit.terms), [90.]);
    }

    #[test]
    fn 先缩最宽的_短名字留着() {
        // 100 本来就比最窄的还窄，不动；300、150 压到同一个上限 150
        let repos = [100., 300., 150.];
        let fit = fit(
            &seg(&repos, REPO_MIN, None),
            &seg(&[], TERM_MIN, None),
            400.,
            0.,
            1500.,
            48.,
        );
        assert!(!fit.overflow);
        assert_eq!(widths(&fit.repos), [100., 150., 150.]);
    }

    #[test]
    fn 两段一起压_同一个上限() {
        let (repos, terms) = ([240., 240.], [200., 100.]);
        // 不含间隔一共能用 600：上限 x 满足 2x + x + 100 = 600，x = 166.7
        let fit = fit(
            &seg(&repos, REPO_MIN, None),
            &seg(&terms, TERM_MIN, None),
            600.,
            0.,
            1500.,
            48.,
        );
        assert!(!fit.overflow);
        assert_eq!(widths(&fit.repos), [167., 167.]);
        assert_eq!(widths(&fit.terms), [167., 100.]);
    }

    #[test]
    fn 放不下的收进去_当前的和最后一个露着的换位置() {
        let repos = [200.; 10];
        // 没有终端：仓库保底 380（1000 的 38%），再加剩下的 80，一共 460；按最窄 110、间隔 4
        // 放得下 4 个（452）。当前的是第 8 个（下标 7），它不压、要 200：换掉第 3、4 个
        let fit = fit(
            &seg(&repos, REPO_MIN, Some(7)),
            &seg(&[], TERM_MIN, None),
            500.,
            4.,
            1000.,
            40.,
        );
        assert!(fit.overflow);
        let order: Vec<usize> = fit.repos.shown.iter().map(|(at, _)| *at).collect();
        assert_eq!(order, [0, 1, 7]);
        assert_eq!(fit.repos.hidden(10), 7);
        // 剩下的地方撑开别的：(460 - 8 - 200) / 2 = 126
        assert_eq!(widths(&fit.repos), [126., 126., 200.]);
    }

    #[test]
    fn 当前的标签不压_别的先缩() {
        // 一共 450：当前的（下标 2）留 200，另外两个分 250
        let repos = [200., 200., 200.];
        let fit = fit(
            &seg(&repos, REPO_MIN, Some(2)),
            &seg(&[], TERM_MIN, None),
            450.,
            0.,
            1500.,
            48.,
        );
        assert!(!fit.overflow);
        assert_eq!(widths(&fit.repos), [125., 125., 200.]);
    }

    #[test]
    fn 两段都放不下_各自保底_剩下的先给仓库() {
        let (repos, terms) = ([200.; 10], [150.; 6]);
        // 能用 900，扣掉「⌄」40 是 860；保底仓库 380、终端 200，剩下 280 先给仓库：仓库 660、终端 200
        let fit = fit(
            &seg(&repos, REPO_MIN, None),
            &seg(&terms, TERM_MIN, Some(5)),
            900.,
            4.,
            1000.,
            40.,
        );
        assert!(fit.overflow);
        // 仓库：110 + 4 一个，660 放得下 5 个（566），6 个要 680
        assert_eq!(fit.repos.shown.len(), 5);
        // 终端：72 + 4 一个，200 放得下 2 个（148）；当前的第 6 个不压、要 150，只放得下它自己
        let order: Vec<usize> = fit.terms.shown.iter().map(|(at, _)| *at).collect();
        assert_eq!(order, [5]);
        assert_eq!(widths(&fit.terms), [150.]);
    }

    #[test]
    fn 终端不多就只给它需要的() {
        let (repos, terms) = ([200.; 10], [80.]);
        let fit = fit(
            &seg(&repos, REPO_MIN, None),
            &seg(&terms, TERM_MIN, None),
            900.,
            4.,
            1000.,
            40.,
        );
        assert!(fit.overflow);
        // 终端只要 72：仓库拿到 860 - 72 = 788，放得下 6 个（680），7 个要 794
        assert_eq!(fit.terms.shown.len(), 1);
        assert_eq!(fit.repos.shown.len(), 6);
    }

    #[test]
    fn 窄得连保底都不够_按比例分_当前的还在() {
        let (repos, terms) = ([200.; 3], [150.; 3]);
        let fit = fit(
            &seg(&repos, REPO_MIN, Some(2)),
            &seg(&terms, TERM_MIN, Some(1)),
            200.,
            4.,
            1000.,
            40.,
        );
        assert!(fit.overflow);
        assert_eq!(fit.repos.shown.first().map(|(at, _)| *at), Some(2));
        assert_eq!(fit.terms.shown.first().map(|(at, _)| *at), Some(1));
    }
}
