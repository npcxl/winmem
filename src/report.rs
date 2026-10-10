//! 报告格式化与历史记录。
//!
//! 报告的原则是**如实描述**：
//! - 工作集减少量不等于“释放的物理内存”；
//! - 测量失败显示为“未知”，不显示为 0；
//! - 无法确认改善时明确标注，不做虚假成功判断。

use std::collections::VecDeque;
use std::path::PathBuf;

use crate::engine::{Observation, Plan, RunOutcome};
use crate::policy::Reason;
use crate::win;

/// 人类可读容量。
pub fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let v = n as f64;
    if v >= GB {
        format!("{:.2} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else if v >= KB {
        format!("{:.0} KB", v / KB)
    } else {
        format!("{} B", n)
    }
}

/// 带符号的差值（可能为负）。
pub fn fmt_delta(v: i64) -> String {
    let sign = if v >= 0 { "+" } else { "-" };
    format!("{}{}", sign, fmt_bytes(v.unsigned_abs()))
}

/// 未知指标的占位文本。
pub const UNKNOWN: &str = "未知";

/// 计划摘要（`optimize --dry-run` 与 GUI 预览共用）。
pub fn format_plan(plan: &Plan) -> String {
    let mut s = String::new();

    if let Some(p) = &plan.pressure {
        s.push_str(&format!(
            "压力状态：{}\n",
            match p.kind {
                crate::policy::PressureKind::None => "无（可用内存充足）",
                crate::policy::PressureKind::Physical => "物理内存不足",
                crate::policy::PressureKind::Commit => "提交量偏高",
                crate::policy::PressureKind::Both => "物理内存不足 + 提交量偏高",
            }
        ));
        s.push_str(&format!(
            "  可用物理内存占比 {:.1}%，提交量占上限 {:.1}%\n",
            p.free_ratio * 100.0,
            p.commit_ratio * 100.0
        ));
        for n in &p.notes {
            s.push_str(&format!("  · {}\n", n));
        }
        s.push_str(&format!(
            "  是否可执行：{}\n",
            if p.should_act {
                "是"
            } else {
                "否（压力未持续）"
            }
        ));
    }

    let candidates: Vec<_> = plan.candidates().collect();
    s.push_str(&format!("\n候选进程（{} 个）：\n", candidates.len()));
    if candidates.is_empty() {
        s.push_str("  （无）\n");
    }
    for c in &candidates {
        s.push_str(&format!(
            "  {:>7}  {:<28} 工作集 {}\n",
            c.pid,
            truncate(&c.name, 28),
            c.working_set
                .map(fmt_bytes)
                .unwrap_or_else(|| UNKNOWN.into())
        ));
        if !c.notes.is_empty() {
            let w: Vec<&str> = c.notes.iter().map(|r| r.describe()).collect();
            s.push_str(&format!("           注意：{}\n", w.join("、")));
        }
    }

    let skipped: Vec<_> = plan.skipped().collect();
    s.push_str(&format!("\n受保护 / 跳过（{} 个）：\n", skipped.len()));
    for it in skipped.iter().take(60) {
        s.push_str(&format!(
            "  {:>7}  {:<28} {}\n",
            it.pid,
            truncate(&it.name, 28),
            it.reason.as_ref().map(|r| r.describe()).unwrap_or("已跳过")
        ));
    }
    if skipped.len() > 60 {
        s.push_str(&format!("  …另有 {} 个\n", skipped.len() - 60));
    }
    s
}

/// 单次执行结果摘要。
pub fn format_outcome(o: &RunOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("结束原因：{}\n", o.finished));

    let ok: Vec<_> = o
        .outcomes
        .iter()
        .filter(|r| r.error.is_none() && r.skipped.is_none())
        .collect();
    let failed: Vec<_> = o.outcomes.iter().filter(|r| r.error.is_some()).collect();
    let skipped: Vec<_> = o.outcomes.iter().filter(|r| r.skipped.is_some()).collect();

    s.push_str(&format!(
        "执行：成功 {}，失败 {}，执行前跳过 {}\n",
        ok.len(),
        failed.len(),
        skipped.len()
    ));

    if !ok.is_empty() {
        s.push_str("\n各进程工作集减少量：\n");
        for r in &ok {
            let delta = match r.working_set_reduction {
                Some(v) => fmt_bytes(v),
                None => format!("{}（前后测量失败）", UNKNOWN),
            };
            s.push_str(&format!(
                "  {:>7}  {:<28} {}\n",
                r.pid,
                truncate(&r.name, 28),
                delta
            ));
        }
    }

    if !skipped.is_empty() {
        s.push_str("\n执行前跳过：\n");
        for r in &skipped {
            s.push_str(&format!(
                "  {:>7}  {:<28} {}\n",
                r.pid,
                truncate(&r.name, 28),
                r.skipped.as_ref().map(|x| x.describe()).unwrap_or("已跳过")
            ));
        }
    }

    if !failed.is_empty() {
        s.push_str("\n失败明细：\n");
        for r in &failed {
            s.push_str(&format!(
                "  {:>7}  {:<28} {}\n",
                r.pid,
                truncate(&r.name, 28),
                r.error.as_ref().map(|e| e.to_string()).unwrap_or_default()
            ));
        }
    }

    s.push_str(&format!(
        "\n合计工作集减少：{}（含 {} 个结果未知）\n",
        fmt_bytes(o.observation.reduction_total),
        o.observation.unknown_count
    ));
    s.push_str(&format!("{}\n", format_observation(&o.observation)));
    s
}

/// 系统级观测摘要。明确区分“工作集减少”与“系统可用内存净变化”。
pub fn format_observation(obs: &Observation) -> String {
    let mut s = String::new();
    match obs.avail_delta() {
        Some(d) => s.push_str(&format!(
            "系统可用物理内存：{} → {}（净变化 {}）\n",
            obs.avail_before
                .map(fmt_bytes)
                .unwrap_or_else(|| UNKNOWN.into()),
            obs.avail_after
                .map(fmt_bytes)
                .unwrap_or_else(|| UNKNOWN.into()),
            fmt_delta(d)
        )),
        None => s.push_str("系统可用物理内存：未知（采样失败）\n"),
    }
    match obs.commit_delta() {
        Some(d) => s.push_str(&format!(
            "系统提交量：{} → {}（净变化 {}）\n",
            obs.commit_before
                .map(fmt_bytes)
                .unwrap_or_else(|| UNKNOWN.into()),
            obs.commit_after
                .map(fmt_bytes)
                .unwrap_or_else(|| UNKNOWN.into()),
            fmt_delta(d)
        )),
        None => s.push_str("系统提交量：未知（采样失败）\n"),
    }
    s.push_str(
        "说明：可用内存净变化包含同期其他程序的正常活动，不能全部归因于本次操作；\n\
         工作集减少并不等于释放了同等大小的物理内存，也不减少应用的私有提交需求。\n",
    );
    s
}

fn truncate(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    if s.chars().count() > width {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// 历史记录
// ---------------------------------------------------------------------------

/// 一条历史记录。
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub at: String,
    pub line: String,
}

/// 有上限的环形历史，落盘时整文件重写，不做高频刷盘。
pub struct History {
    entries: VecDeque<HistoryEntry>,
    cap: usize,
    file_cap: usize,
    path: Option<PathBuf>,
}

impl Default for History {
    fn default() -> Self {
        Self::new(50, 200)
    }
}

impl History {
    pub fn new(cap: usize, file_cap: usize) -> History {
        let path = std::env::var_os("APPDATA").map(|base| {
            let mut p = PathBuf::from(base);
            p.push("winmem");
            p.push("history.log");
            p
        });
        let mut h = History {
            entries: VecDeque::with_capacity(cap),
            cap,
            file_cap,
            path,
        };
        h.load();
        h
    }

    fn load(&mut self) {
        let Some(p) = &self.path else { return };
        let Ok(text) = std::fs::read_to_string(p) else {
            return;
        };
        for line in text.lines().take(self.file_cap) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (at, rest) = match line.split_once('|') {
                Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                None => (String::new(), line.to_string()),
            };
            self.entries.push_back(HistoryEntry { at, line: rest });
        }
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// 追加一条记录（内存），不立即落盘。
    pub fn push(&mut self, line: &str) {
        self.entries.push_back(HistoryEntry {
            at: win::local_time_string(),
            line: line.to_string(),
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// 落盘（整文件重写，最多 `file_cap` 行）。失败静默忽略，不影响主流程。
    pub fn flush(&self) {
        let Some(p) = &self.path else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = String::new();
        let start = self.entries.len().saturating_sub(self.file_cap);
        for e in self.entries.iter().skip(start) {
            text.push_str(&format!("{} | {}\n", e.at, e.line));
        }
        let _ = std::fs::write(p, text);
    }

    /// 生成一行摘要文本（用于记录一次运行）。
    pub fn summarize(o: &RunOutcome) -> String {
        let reason = o
            .plan
            .pressure
            .as_ref()
            .map(|p| match p.kind {
                crate::policy::PressureKind::None => "无压力",
                crate::policy::PressureKind::Physical => "物理内存不足",
                crate::policy::PressureKind::Commit => "提交量偏高",
                crate::policy::PressureKind::Both => "内存与提交双重压力",
            })
            .unwrap_or("未知");
        format!(
            "触发={} 执行={} 成功={} 工作集减少={} 可用内存变化={} 结束={}",
            reason,
            if o.dry_run { "dry-run" } else { "trim" },
            o.observation.trimmed_count,
            fmt_bytes(o.observation.reduction_total),
            o.observation
                .avail_delta()
                .map(fmt_delta)
                .unwrap_or_else(|| UNKNOWN.into()),
            o.finished
        )
    }
}

/// 供测试与其他模块使用的保护原因文本。
pub fn reason_text(r: &Reason) -> &'static str {
    r.describe()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Observation, PlanItem, RunOutcome, TrimOutcome};

    #[test]
    fn formats_bytes_and_deltas() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1024), "1 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(fmt_delta(1024), "+1 KB");
        assert_eq!(fmt_delta(-1024), "-1 KB");
        assert_eq!(fmt_delta(0), "+0 B");
    }

    #[test]
    fn unknown_measurement_is_reported_as_unknown() {
        // 读取失败必须显示“未知”，不能显示成 0
        let o = RunOutcome {
            plan: Plan::default(),
            outcomes: vec![TrimOutcome {
                pid: 1234,
                name: "app.exe".into(),
                working_set_before: None,
                working_set_reduction: None,
                working_set_after: None,
                regrowth_ratio: None,
                error: None,
                skipped: None,
            }],
            observation: Observation {
                reduction_total: 0,
                unknown_count: 1,
                trimmed_count: 1,
                ..Default::default()
            },
            cancelled: false,
            dry_run: false,
            finished: "done".into(),
        };
        let text = format_outcome(&o);
        assert!(text.contains("未知"), "应标注结果未知：{}", text);
        assert!(text.contains("含 1 个结果未知"));
    }

    #[test]
    fn observation_distinguishes_system_delta_from_trimming() {
        let obs = Observation {
            avail_before: Some(1000),
            avail_after: Some(900),
            reduction_total: 500,
            ..Default::default()
        };
        let text = format_observation(&obs);
        // 净变化可以为负，且必须说明不能全部归因于本次操作
        assert!(text.contains("-100 B"), "应显示负的净变化：{}", text);
        assert!(text.contains("不能全部归因"));
        assert!(text.contains("不等于释放了同等大小的物理内存"));
    }

    #[test]
    fn plan_lists_candidates_and_reasons() {
        let plan = Plan {
            pressure: None,
            items: vec![
                PlanItem {
                    pid: 1,
                    name: "a.exe".into(),
                    creation_time: None,
                    working_set: Some(1024),
                    private_bytes: None,
                    accepted: true,
                    reason: None,
                    notes: vec![],
                },
                PlanItem {
                    pid: 2,
                    name: "b.exe".into(),
                    creation_time: None,
                    working_set: Some(2048),
                    private_bytes: None,
                    accepted: false,
                    reason: Some(Reason::Foreground),
                    notes: vec![],
                },
            ],
        };
        let text = format_plan(&plan);
        assert!(text.contains("候选进程（1 个）"));
        assert!(text.contains("受保护 / 跳过（1 个）"));
        assert!(text.contains("当前前台应用"));
    }

    #[test]
    fn history_summary_mentions_outcome() {
        let o = RunOutcome {
            plan: Plan::default(),
            outcomes: vec![],
            observation: Observation {
                avail_before: Some(2048),
                avail_after: Some(3072),
                reduction_total: 4096,
                trimmed_count: 2,
                ..Default::default()
            },
            cancelled: false,
            dry_run: false,
            finished: "候选处理完毕".into(),
        };
        let s = History::summarize(&o);
        assert!(s.contains("成功=2"));
        assert!(s.contains("+1 KB"));
        assert!(s.contains("候选处理完毕"));
    }
}
