//! 提交图布局：给每个提交分配泳道，并算出每一行要画的线段。
//!
//! 输入必须是子提交在前、父提交在后的顺序（`git log --date-order` 或 `--topo-order`）。
//! 每条泳道记着它在等哪个提交。处理到某个提交时：
//! - 所有在等它的泳道都汇入它的节点，节点放在其中最左的一条；
//! - 第一个父提交沿用节点所在泳道；
//! - 其余父提交（合并）接到已经在等它的泳道，没有就新开一条。
//!
//! 空出来的泳道会被后面新开的分支复用，所以图的宽度只取决于同时活跃的分支数。

use crate::log::Commit;

/// 提交图那一列最多画这么多条泳道，更右边的截掉。截掉的泳道上的线段不存，只记这一行有截掉的：
/// 分叉很多的历史里同时活跃的泳道可能上千条，每行每条都存一段直通的线，二十万行能存到上 GB，
/// 又都画不出来。
pub const MAX_LANES: u16 = 40;

/// 一行里的一条线段。坐标用泳道编号表示，界面层再换算成像素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    /// 起点泳道
    pub from: u16,
    /// 终点泳道
    pub to: u16,
    /// 颜色编号，界面层对调色板取模
    pub color: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// 从行顶直通到行底，泳道不变
    Through,
    /// 从行顶的 `from` 泳道连到本行提交节点（`to` 泳道）
    ToNode,
    /// 从本行提交节点（`from` 泳道）连到行底的 `to` 泳道
    FromNode,
}

/// 一行的布局。
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// 提交节点所在泳道
    pub lane: u16,
    pub color: u8,
    /// 这一行有画不下的（节点或线在 [`MAX_LANES`] 条以外），界面在右边标一下
    pub clipped: bool,
    seg_start: u32,
    seg_len: u16,
}

/// 整个提交列表的布局。线段集中存放在一个数组里，每行只记起止位置。
pub struct Graph {
    pub rows: Vec<Row>,
    segments: Vec<Segment>,
    /// 任意一行里同时存在的最多泳道数，用来定提交图那一列的宽度
    pub max_lanes: u16,
}

impl Graph {
    pub fn segments(&self, row: usize) -> &[Segment] {
        let r = &self.rows[row];
        let start = r.seg_start as usize;
        &self.segments[start..start + r.seg_len as usize]
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub fn build(commits: &[Commit]) -> Graph {
        use SegmentKind::*;

        // lanes[l] = 泳道 l 正在等的提交下标；None 表示空闲
        let mut lanes: Vec<Option<u32>> = Vec::new();
        let mut colors: Vec<u8> = Vec::new();
        let mut next_color = 0u8;
        let mut rows = Vec::with_capacity(commits.len());
        let mut segments = Vec::with_capacity(commits.len() * 4);
        let mut max_lanes = 0usize;
        let mut waiting = Vec::new();

        for (index, commit) in commits.iter().enumerate() {
            let index = index as u32;
            waiting.clear();
            waiting.extend(
                lanes
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| **slot == Some(index))
                    .map(|(l, _)| l),
            );

            let seg_start = segments.len() as u32;
            let lane = match waiting.first() {
                Some(&l) => l,
                // 没有泳道在等它：分支的最新提交，新开一条泳道
                None => {
                    let l = alloc(&mut lanes, &mut colors);
                    colors[l] = take_color(&mut next_color);
                    l
                }
            };
            let color = colors[lane];
            let mut clipped = lane >= MAX_LANES as usize;

            // 上半行：在等本提交的泳道汇入节点，其余活跃泳道直通
            for (l, slot) in lanes.iter().enumerate() {
                let segment = match *slot {
                    None => continue,
                    Some(c) if c == index => Segment {
                        kind: ToNode,
                        from: l as u16,
                        to: lane as u16,
                        color: colors[l],
                    },
                    Some(_) => Segment {
                        kind: Through,
                        from: l as u16,
                        to: l as u16,
                        color: colors[l],
                    },
                };
                keep(&mut segments, &mut clipped, segment);
            }
            for &l in waiting.iter().skip(1) {
                lanes[l] = None;
            }

            // 下半行：第一个父提交沿用本泳道，其余父提交接到已有泳道或新开泳道
            match commit.parents.split_first() {
                None => lanes[lane] = None,
                Some((&first, rest)) => {
                    lanes[lane] = Some(first);
                    let segment = Segment {
                        kind: FromNode,
                        from: lane as u16,
                        to: lane as u16,
                        color,
                    };
                    keep(&mut segments, &mut clipped, segment);
                    for &parent in rest {
                        let target = match lanes.iter().position(|s| *s == Some(parent)) {
                            Some(l) => l,
                            None => {
                                let l = alloc(&mut lanes, &mut colors);
                                colors[l] = take_color(&mut next_color);
                                lanes[l] = Some(parent);
                                l
                            }
                        };
                        let segment = Segment {
                            kind: FromNode,
                            from: lane as u16,
                            to: target as u16,
                            color: colors[target],
                        };
                        keep(&mut segments, &mut clipped, segment);
                    }
                }
            }

            max_lanes = max_lanes.max(lanes.len());
            // 去掉末尾的空泳道，图在分支合并后能变窄
            while lanes.last() == Some(&None) {
                lanes.pop();
                colors.pop();
            }
            rows.push(Row {
                lane: lane as u16,
                color,
                clipped,
                seg_start,
                seg_len: (segments.len() as u32 - seg_start) as u16,
            });
        }

        Graph {
            rows,
            segments,
            max_lanes: max_lanes as u16,
        }
    }
}

/// 画得出来的线段（两头都在前 [`MAX_LANES`] 条泳道里）才存，画不出来的只记下这一行有截掉的。
fn keep(segments: &mut Vec<Segment>, clipped: &mut bool, segment: Segment) {
    match segment.from < MAX_LANES && segment.to < MAX_LANES {
        true => segments.push(segment),
        false => *clipped = true,
    }
}

/// 找一条空闲泳道，没有就在最右边新开一条。
fn alloc(lanes: &mut Vec<Option<u32>>, colors: &mut Vec<u8>) -> usize {
    if let Some(l) = lanes.iter().position(Option::is_none) {
        l
    } else {
        lanes.push(None);
        colors.push(0);
        lanes.len() - 1
    }
}

fn take_color(next: &mut u8) -> u8 {
    let c = *next;
    *next = next.wrapping_add(1);
    c
}

#[cfg(test)]
mod tests {
    use super::SegmentKind::*;
    use super::*;

    fn commit(parents: &[u32]) -> Commit {
        Commit {
            id: "x".into(),
            parents: parents.to_vec(),
            author: "a".into(),
            time: 0,
            refs: "".into(),
            subject: "".into(),
        }
    }

    fn kinds(g: &Graph, row: usize) -> Vec<(SegmentKind, u16, u16)> {
        g.segments(row)
            .iter()
            .map(|s| (s.kind, s.from, s.to))
            .collect()
    }

    #[test]
    fn 直线历史只用一条泳道() {
        // 0 -> 1 -> 2（根）
        let g = Graph::build(&[commit(&[1]), commit(&[2]), commit(&[])]);
        assert_eq!(g.max_lanes, 1);
        assert!(g.rows.iter().all(|r| r.lane == 0));
        assert_eq!(kinds(&g, 0), vec![(FromNode, 0, 0)]);
        assert_eq!(kinds(&g, 1), vec![(ToNode, 0, 0), (FromNode, 0, 0)]);
        assert_eq!(kinds(&g, 2), vec![(ToNode, 0, 0)]);
    }

    #[test]
    fn 分支和合并() {
        // 0 合并提交，父提交 1（主线）和 2（分支）；1、2 都基于 3
        let g = Graph::build(&[commit(&[1, 2]), commit(&[3]), commit(&[3]), commit(&[])]);
        assert_eq!(g.max_lanes, 2);
        assert_eq!(kinds(&g, 0), vec![(FromNode, 0, 0), (FromNode, 0, 1)]);
        assert_eq!(g.rows[1].lane, 0);
        assert_eq!(
            kinds(&g, 1),
            vec![(ToNode, 0, 0), (Through, 1, 1), (FromNode, 0, 0)]
        );
        // 分支上的提交在泳道 1，它的父提交 3 已经有泳道 0 在等，两条线并行到 3 再汇合
        assert_eq!(g.rows[2].lane, 1);
        assert_eq!(
            kinds(&g, 2),
            vec![(Through, 0, 0), (ToNode, 1, 1), (FromNode, 1, 1)]
        );
        assert_eq!(g.rows[3].lane, 0);
        assert_eq!(kinds(&g, 3), vec![(ToNode, 0, 0), (ToNode, 1, 0)]);
    }

    #[test]
    fn 泳道多到画不下时_画不出来的线段不存() {
        // 50 个分支都从同一个提交（第 50 个）分出来：越往下同时活跃的泳道越多
        let mut commits: Vec<Commit> = (0..50).map(|_| commit(&[50])).collect();
        commits.push(commit(&[]));
        let g = Graph::build(&commits);
        assert_eq!(g.max_lanes, 50);
        let max = MAX_LANES as usize;
        for row in 0..commits.len() {
            assert!(
                g.segments(row)
                    .iter()
                    .all(|s| s.from < MAX_LANES && s.to < MAX_LANES)
            );
        }
        // 前 40 个分支画得下；之后的节点在 40 条以外，这一行标成截掉
        assert!(!g.rows[max - 1].clipped);
        assert!(g.rows[max].clipped);
        // 每行最多存 40 段直通的线（原来第 49 行存 49 段）
        assert_eq!(g.segments(49).len(), max);
        // 根上 50 条线汇进来，画得出的 40 条
        assert!(g.rows[50].clipped);
        assert_eq!(g.segments(50).len(), max);
    }

    #[test]
    fn 合并后空出的泳道被复用() {
        // 0 和 1 是两个分支的最新提交（各自一条泳道），都基于 2；之后 3 是另一个分支的提交
        let g = Graph::build(&[
            commit(&[2]),
            commit(&[2]),
            commit(&[4]),
            commit(&[4]),
            commit(&[]),
        ]);
        assert_eq!(g.rows[0].lane, 0);
        assert_eq!(g.rows[1].lane, 1);
        // 2 汇合后泳道 1 空出，3 新开分支时复用泳道 1
        assert_eq!(g.rows[2].lane, 0);
        assert_eq!(g.rows[3].lane, 1);
        assert_eq!(g.max_lanes, 2);
    }
}
