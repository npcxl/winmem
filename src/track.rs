//! 操作后的效果跟踪（1 分钟 / 5 分钟节点）。
//!
//! 设计原则：
//! - **调度逻辑与时间源解耦**：所有方法接收 `now_ms`，因此可以用可控时钟做单元测试；
//! - **结果未知不回填 0**：读取失败为 `None`，明确展示为「未知」；
//! - **不把工作集之和当作真实回收量**：只记录每个进程的回涨比例与存活状态；
//! - 采样中断（例如时钟回退或长时间无有效样本）时，持续压力确认需保守重置，
//!   由调用方通过 [`TrackSession::reset_if_stale`] 处理。

/// 被跟踪的进程（按身份记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackTarget {
    pub pid: u32,
    pub name: String,
    /// PID + 创建时间，避免 PID 复用污染统计
    pub creation_time: Option<u64>,
    /// 裁剪前工作集（未知为 None）
    pub working_set_before: Option<u64>,
    /// 本次裁剪的工作集减少量（未知为 None）
    pub reduction: Option<u64>,
}

/// 单进程在某个跟踪节点的状态。
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedProcState {
    pub pid: u32,
    pub name: String,
    pub alive: bool,
    pub working_set: Option<u64>,
    /// 回涨比例：相对本次减少量。1.0 表示减少量被完全吃回；未知为 None。
    pub regrowth_ratio: Option<f64>,
}

/// 某个跟踪节点的采样结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TrackPoint {
    /// 相对跟踪开始的秒数（即 track_secs 节点）
    pub at_secs: u64,
    pub avail_phys: Option<u64>,
    pub commit_total: Option<u64>,
    pub procs: Vec<TrackedProcState>,
}

/// 进程探针返回的单次观测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcObservation {
    pub alive: bool,
    pub working_set: Option<u64>,
}

/// 一次跟踪会话。
pub struct TrackSession {
    start_ms: u64,
    nodes_ms: Vec<u64>,
    next: usize,
    targets: Vec<TrackTarget>,
    points: Vec<TrackPoint>,
    last_sample_ms: u64,
}

impl TrackSession {
    /// 依据配置的秒级节点创建会话。节点会被去重并升序排列。
    pub fn new(start_ms: u64, track_secs: &[u64], targets: Vec<TrackTarget>) -> TrackSession {
        let mut nodes: Vec<u64> = track_secs.iter().map(|s| s.saturating_mul(1000)).collect();
        nodes.sort_unstable();
        nodes.dedup();
        TrackSession {
            start_ms,
            nodes_ms: nodes,
            next: 0,
            targets,
            points: Vec::new(),
            last_sample_ms: start_ms,
        }
    }

    pub fn targets(&self) -> &[TrackTarget] {
        &self.targets
    }

    pub fn points(&self) -> &[TrackPoint] {
        &self.points
    }

    /// 是否所有节点都已记录。
    pub fn is_done(&self) -> bool {
        self.next >= self.nodes_ms.len()
    }

    /// 下一个待记录节点距开始的时间（毫秒）。已完成时返回 None。
    pub fn next_node_ms(&self) -> Option<u64> {
        self.nodes_ms.get(self.next).map(|ms| self.start_ms + ms)
    }

    /// 距开始已经过的时间（毫秒）。
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.start_ms)
    }

    /// 该时刻是否有节点到期。
    pub fn due(&self, now_ms: u64) -> bool {
        match self.next_node_ms() {
            Some(t) => now_ms >= t,
            None => false,
        }
    }

    /// 样本长时间中断（超过 `stale_after_ms`）时返回 true，调用方应保守重置压力确认。
    pub fn reset_if_stale(&self, now_ms: u64, stale_after_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_sample_ms) > stale_after_ms
    }

    /// 记录所有已到期节点。
    ///
    /// `sys` 为 `(可用物理内存, 提交量)`，任一项为 `None` 表示读取失败（未知）。
    /// `probe` 按目标进程查询存活与工作集；返回 `None` 表示读取失败。
    pub fn advance<F>(
        &mut self,
        now_ms: u64,
        sys: (Option<u64>, Option<u64>),
        mut probe: F,
    ) -> usize
    where
        F: FnMut(&TrackTarget) -> Option<ProcObservation>,
    {
        let mut recorded = 0usize;
        while let Some(node_abs) = self.next_node_ms() {
            if now_ms < node_abs {
                break;
            }
            let at_secs = (node_abs - self.start_ms) / 1000;

            // 每个目标进程只探针一次，结果复用到该节点（避免重复查询造成的偏差）
            let mut procs = Vec::with_capacity(self.targets.len());
            for t in &self.targets {
                let obs = probe(t);
                let (alive, working_set) = match obs {
                    Some(o) => (o.alive, o.working_set),
                    None => (false, None),
                };
                let regrowth_ratio = match (working_set, t.working_set_before, t.reduction) {
                    (Some(ws), Some(before), Some(red)) if red > 0 => {
                        let trimmed = before.saturating_sub(red);
                        Some(ws.saturating_sub(trimmed) as f64 / red as f64)
                    }
                    _ => None,
                };
                procs.push(TrackedProcState {
                    pid: t.pid,
                    name: t.name.clone(),
                    alive,
                    working_set,
                    regrowth_ratio,
                });
            }

            self.points.push(TrackPoint {
                at_secs,
                avail_phys: sys.0,
                commit_total: sys.1,
                procs,
            });
            self.next += 1;
            recorded += 1;
        }
        self.last_sample_ms = now_ms;
        recorded
    }
}

/// 节点回涨是否表明本次裁剪无效（与 engine 的判定口径一致）。
pub fn point_ineffective(p: &TrackPoint, threshold: f64) -> bool {
    p.procs
        .iter()
        .any(|p| p.regrowth_ratio.map(|r| r > threshold).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pid: u32, before: u64, red: u64) -> TrackTarget {
        TrackTarget {
            pid,
            name: format!("app{}.exe", pid),
            creation_time: Some(1000 + pid as u64),
            working_set_before: Some(before),
            reduction: Some(red),
        }
    }

    #[test]
    fn schedules_only_declared_nodes_with_controllable_clock() {
        let mut s = TrackSession::new(0, &[60, 300], vec![target(10, 1_000, 400)]);

        // 59.9 秒：未到节点
        let n = s.advance(59_900, (Some(5), Some(9)), |_| {
            Some(ProcObservation {
                alive: true,
                working_set: Some(700),
            })
        });
        assert_eq!(n, 0);
        assert!(!s.due(59_900));
        assert!(s.due(60_000));

        // 60 秒：记录第一个节点
        let n = s.advance(60_100, (Some(5), Some(9)), |_| {
            Some(ProcObservation {
                alive: true,
                working_set: Some(700),
            })
        });
        assert_eq!(n, 1);
        assert_eq!(s.points()[0].at_secs, 60);

        // 200 秒：仍无新节点
        assert_eq!(
            s.advance(200_000, (Some(6), Some(9)), |_| Some(ProcObservation {
                alive: true,
                working_set: Some(600)
            })),
            0
        );

        // 300 秒：记录第二个节点
        assert_eq!(
            s.advance(300_000, (Some(7), Some(9)), |_| Some(ProcObservation {
                alive: true,
                working_set: Some(650)
            })),
            1
        );
        assert_eq!(s.points()[1].at_secs, 300);
        assert!(s.is_done());
        assert_eq!(s.next_node_ms(), None);
    }

    #[test]
    fn tracks_by_identity_and_reports_regrowth() {
        let mut s = TrackSession::new(0, &[60], vec![target(10, 1_000, 400)]);
        s.advance(60_000, (Some(1), Some(2)), |_| {
            // 回涨到 1000 ⇒ 减少量被完全吃回 ⇒ 比例 1.0
            Some(ProcObservation {
                alive: true,
                working_set: Some(1_000),
            })
        });
        let p = &s.points()[0].procs[0];
        assert_eq!(p.regrowth_ratio, Some(1.0));
        assert!(point_ineffective(&s.points()[0], 0.5));
    }

    #[test]
    fn unknown_measurements_stay_unknown() {
        let mut s = TrackSession::new(0, &[60], vec![target(11, 500, 100)]);
        s.advance(60_000, (None, None), |_| None);
        let pt = &s.points()[0];
        assert_eq!(pt.avail_phys, None);
        assert_eq!(pt.commit_total, None);
        assert!(!pt.procs[0].alive, "探针失败不应假设存活");
        assert_eq!(pt.procs[0].working_set, None);
        assert_eq!(pt.procs[0].regrowth_ratio, None, "未知不能回填为 0");
    }

    #[test]
    fn dead_process_reports_not_alive() {
        let mut s = TrackSession::new(0, &[60], vec![target(12, 800, 300)]);
        s.advance(60_000, (Some(1), Some(1)), |_| {
            Some(ProcObservation {
                alive: false,
                working_set: None,
            })
        });
        assert!(!s.points()[0].procs[0].alive);
    }

    #[test]
    fn reset_if_stale_detects_interruption() {
        let s = TrackSession::new(0, &[60], vec![]);
        assert!(!s.reset_if_stale(5_000, 10_000));
        assert!(s.reset_if_stale(40_000, 10_000));
    }

    #[test]
    fn duplicate_nodes_are_deduplicated() {
        let s = TrackSession::new(0, &[60, 60, 300], vec![]);
        assert_eq!(s.nodes_ms.len(), 2);
    }
}
