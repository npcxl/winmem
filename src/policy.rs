//! 保护规则、压力判定与候选选择。
//!
//! 本模块**不依赖 Windows**，全部为纯逻辑，便于单元测试。
//! 规则分两级：
//! - [`Decision::Blocked`]：绝对禁止，任何标志（包括 `--force`）都无法绕过。
//! - [`Decision::Skip`]：默认保护；仅在用户**显式指定**目标时放行，并给出警告。

use std::collections::HashMap;

use crate::config::{Config, PolicyConfig};

/// 规范化进程名：去首尾空白、转小写、去掉 `.exe` 后缀。
pub fn normalize_name(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    match n.strip_suffix(".exe") {
        Some(s) => s.to_string(),
        None => n,
    }
}

/// 绝对禁止操作的核心系统进程（已规范化的名称）。
/// 这些进程被结束或裁剪都可能导致系统不稳定，任何模式都不允许。
pub const CORE_PROTECTED: &[&str] = &[
    "system",
    "registry",
    "idle",
    "secure system",
    "memory compression",
    "smss",
    "csrss",
    "wininit",
    "winlogon",
    "services",
    "lsass",
    "lsaiso",
    "dwm",
];

/// 默认保护（可被用户显式指定绕过，但会给出警告）。
pub const DEFAULT_PROTECTED: &[&str] = &[
    "explorer",
    "sihost",
    "taskhostw",
    "fontdrvhost",
    "audiodg",
    "spoolsv",
    "searchindexer",
    "startmenuexperiencehost",
    "shellexperiencehost",
    "textinputhost",
    "ctfmon",
    "wmiprvse",
    "msmpeng",
    "securityhealthservice",
    "windefend",
];

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 可作为优化候选
    Candidate,
    /// 绝对禁止（无法绕过）
    Blocked(Reason),
    /// 默认跳过（用户显式指定时放行并警告）
    Skip(Reason),
}

/// 跳过 / 保护原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    CoreCritical,
    /// 系统通过 IsProcessCritical 明确标记
    OsCritical,
    OtherSession,
    UnknownIdentity,
    UserProtected,
    DefaultProtected,
    Foreground,
    RecentForeground,
    Busy,
    /// CPU / I/O 等活动指标未知（无法区分「闲置」与「正在工作」），自动模式跳过
    UnknownActivity,
    Cooling,
    TooSmall,
    NotInAllowList,
}

impl Reason {
    pub fn describe(&self) -> &'static str {
        match self {
            Reason::CoreCritical => "核心系统进程",
            Reason::OsCritical => "系统标记为关键进程",
            Reason::OtherSession => "属于其他用户会话",
            Reason::UnknownIdentity => "无法确认进程身份",
            Reason::UserProtected => "在用户保护名单中",
            Reason::DefaultProtected => "系统外壳/安全组件，默认保护",
            Reason::Foreground => "当前前台应用",
            Reason::RecentForeground => "最近使用过，避免影响交互",
            Reason::Busy => "正在活跃计算或 I/O",
            Reason::UnknownActivity => "活动指标未知（无法确认闲置）",
            Reason::Cooling => "处于冷却期",
            Reason::TooSmall => "工作集过小，收益有限",
            Reason::NotInAllowList => "不在自动优化允许名单中",
        }
    }
}

/// 决策附带的信息。
#[derive(Debug, Clone)]
pub struct Verdict {
    pub decision: Decision,
    /// 放行但需要提醒用户的原因（例如手动强制处理了默认保护进程）
    pub warnings: Vec<Reason>,
}

/// 单个进程的已知事实（由 metrics 层采集）。
#[derive(Debug, Clone)]
pub struct ProcessFacts {
    pub pid: u32,
    pub name: String,
    /// 所属会话；None 表示无法获取
    pub session: Option<u32>,
    pub working_set: u64,
    pub private_bytes: u64,
    /// 系统标记的关键进程；None 表示接口不支持或查询失败
    pub os_critical: Option<bool>,
    /// 距最近一次前台活动的时间（秒）；None 表示未知
    pub last_foreground_secs: Option<u64>,
    /// 采样区间内的 CPU 占用百分比；None 表示不可用
    pub cpu_percent: Option<f64>,
    /// 是否在冷却期内
    pub cooling: bool,
    /// 是否为本程序自身
    pub is_self: bool,
}

/// 操作意图。不同意图下，“软保护”的适用范围不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 自动优化：最严格，任何软保护原因都直接跳过
    Auto,
    /// 手动裁剪工作集：保留与“是否值得优化”相关的保护
    ManualTrim,
    /// 手动结束进程：与工作集大小、CPU 活跃度无关，只保留交互与身份类保护
    ManualKill,
}

impl Purpose {
    /// 是否是用户显式发起的操作（此时软保护降级为警告）。
    pub fn is_manual(self) -> bool {
        !matches!(self, Purpose::Auto)
    }
}

/// 策略输入。
pub struct PolicyInput<'a> {
    pub facts: &'a ProcessFacts,
    pub config: &'a Config,
    /// 本程序所在会话
    pub self_session: Option<u32>,
    /// 当前前台窗口所属进程
    pub foreground_pid: Option<u32>,
    /// 操作意图
    pub purpose: Purpose,
}

/// 统一的策略校验入口。所有操作（CLI trim/kill/optimize、GUI）都必须经过它。
pub fn decide(input: &PolicyInput<'_>) -> Verdict {
    let f = input.facts;
    let cfg = &input.config;

    // ---- 第一级：绝对禁止 ----
    if f.is_self {
        return Verdict {
            decision: Decision::Blocked(Reason::CoreCritical),
            warnings: vec![],
        };
    }
    if f.pid <= 4 {
        return Verdict {
            decision: Decision::Blocked(Reason::CoreCritical),
            warnings: vec![],
        };
    }
    let name = normalize_name(&f.name);
    if CORE_PROTECTED.contains(&name.as_str()) {
        return Verdict {
            decision: Decision::Blocked(Reason::CoreCritical),
            warnings: vec![],
        };
    }
    if f.os_critical == Some(true) {
        return Verdict {
            decision: Decision::Blocked(Reason::OsCritical),
            warnings: vec![],
        };
    }

    // ---- 第二级：默认保护 ----
    let mut soft: Vec<Reason> = Vec::new();

    // 身份未知：不能确认会话 → 默认跳过
    match f.session {
        None => soft.push(Reason::UnknownIdentity),
        Some(s) => {
            if let Some(mine) = input.self_session {
                if s != mine {
                    soft.push(Reason::OtherSession);
                }
            }
        }
    }

    if cfg.protect.iter().any(|p| normalize_name(p) == name) {
        soft.push(Reason::UserProtected);
    }
    if DEFAULT_PROTECTED.contains(&name.as_str()) {
        soft.push(Reason::DefaultProtected);
    }
    if let Some(fg) = input.foreground_pid {
        if fg == f.pid {
            soft.push(Reason::Foreground);
        }
    }
    if let Some(age) = f.last_foreground_secs {
        if age < cfg.policy.recent_foreground_secs {
            soft.push(Reason::RecentForeground);
        }
    }
    if let Some(cpu) = f.cpu_percent {
        if cpu >= cfg.policy.busy_cpu_percent {
            soft.push(Reason::Busy);
        }
    }
    // 活动指标未知时，无法区分「闲置」与「正在工作」：
    // 自动模式必须保守跳过；手动模式下保留为警告（已由下面按意图处理）。
    if f.cpu_percent.is_none() {
        soft.push(Reason::UnknownActivity);
    }
    if f.cooling {
        soft.push(Reason::Cooling);
    }
    if f.working_set < cfg.policy.min_candidate_ws {
        soft.push(Reason::TooSmall);
    }
    if !input.purpose.is_manual() {
        let allowed = cfg.allow.iter().any(|a| normalize_name(a) == name);
        if !allowed && !cfg.policy.auto_allow_unlisted {
            soft.push(Reason::NotInAllowList);
        }
    }

    // 结束进程与“是否值得做内存优化”无关，这些原因不应阻挡手动结束
    if input.purpose == Purpose::ManualKill {
        soft.retain(|r| {
            !matches!(
                r,
                Reason::TooSmall
                    | Reason::Busy
                    | Reason::UnknownActivity
                    | Reason::Cooling
                    | Reason::NotInAllowList
            )
        });
    }

    if soft.is_empty() {
        Verdict {
            decision: Decision::Candidate,
            warnings: vec![],
        }
    } else if input.purpose.is_manual() {
        // 手动指定：放行，但把全部保护原因作为警告返回
        Verdict {
            decision: Decision::Candidate,
            warnings: soft,
        }
    } else {
        Verdict {
            decision: Decision::Skip(soft[0].clone()),
            warnings: soft,
        }
    }
}

// ---------------------------------------------------------------------------
// 压力判定（带滞回）
// ---------------------------------------------------------------------------

/// 一次系统采样。
#[derive(Debug, Clone, Copy, Default)]
pub struct PressureSnapshot {
    pub total_phys: u64,
    pub avail_phys: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    /// `CreateMemoryResourceNotification` 的结果；None 表示不可用
    pub low_memory_signal: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureKind {
    None,
    /// 物理内存不足：允许裁剪合格候选
    Physical,
    /// 提交量接近上限：只提示，不做无意义的反复裁剪
    Commit,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Normal,
    Candidate,
    Pressing,
}

/// 压力状态机：进入与退出使用不同阈值，避免频繁切换。
pub struct PressureTracker {
    phase: Phase,
    since_secs: u64,
}

/// 一次压力判定结果。
#[derive(Debug, Clone)]
pub struct PressureReport {
    pub kind: PressureKind,
    /// 是否已满足“持续压力”，可以执行优化
    pub should_act: bool,
    pub free_ratio: f64,
    pub commit_ratio: f64,
    /// 人类可读的判断依据
    pub notes: Vec<String>,
}

impl Default for PressureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PressureTracker {
    pub fn new() -> Self {
        PressureTracker {
            phase: Phase::Normal,
            since_secs: 0,
        }
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Normal => "正常",
            Phase::Candidate => "疑似压力",
            Phase::Pressing => "压力中",
        }
    }

    /// 用一次采样推进状态机。`now_secs` 必须来自单调时钟。
    pub fn update(
        &mut self,
        s: &PressureSnapshot,
        cfg: &PolicyConfig,
        now_secs: u64,
    ) -> PressureReport {
        let free_ratio = if s.total_phys > 0 {
            s.avail_phys as f64 / s.total_phys as f64
        } else {
            1.0
        };
        let commit_ratio = if s.commit_limit > 0 {
            s.commit_total as f64 / s.commit_limit as f64
        } else {
            0.0
        };

        let mut notes = Vec::new();

        let phys_low = free_ratio < cfg.press_enter_free_ratio || s.low_memory_signal == Some(true);
        let phys_recovered =
            free_ratio >= cfg.press_exit_free_ratio && s.low_memory_signal != Some(true);
        let commit_high = commit_ratio >= cfg.commit_warn_ratio;

        if phys_low {
            match self.phase {
                Phase::Normal => {
                    self.phase = Phase::Candidate;
                    self.since_secs = now_secs;
                    notes.push(format!(
                        "可用物理内存降至 {:.1}%，开始计时（需持续 {} 秒）",
                        free_ratio * 100.0,
                        cfg.press_hold_secs
                    ));
                }
                Phase::Candidate => {
                    if now_secs.saturating_sub(self.since_secs) >= cfg.press_hold_secs {
                        self.phase = Phase::Pressing;
                        notes.push(format!(
                            "低内存已持续 {} 秒，进入压力处理",
                            now_secs.saturating_sub(self.since_secs)
                        ));
                    }
                }
                Phase::Pressing => {}
            }
        } else if phys_recovered {
            if self.phase != Phase::Normal {
                notes.push(format!(
                    "可用物理内存回升至 {:.1}%，退出压力状态",
                    free_ratio * 100.0
                ));
            }
            self.phase = Phase::Normal;
            self.since_secs = 0;
        }
        // 介于两个阈值之间时保持当前状态（滞回区）

        if commit_high {
            notes.push(format!(
                "系统提交量已达上限的 {:.1}%，属于提交压力",
                commit_ratio * 100.0
            ));
            if commit_ratio >= cfg.commit_alert_ratio {
                notes.push(
                    "提交压力不能靠裁剪工作集解决，请检查异常增长的程序、分页文件设置".into(),
                );
            }
        }

        let pressing = self.phase == Phase::Pressing;
        let kind = match (pressing, commit_high) {
            (true, true) => PressureKind::Both,
            (true, false) => PressureKind::Physical,
            (false, true) => PressureKind::Commit,
            (false, false) => PressureKind::None,
        };

        PressureReport {
            kind,
            should_act: pressing,
            free_ratio,
            commit_ratio,
            notes,
        }
    }
}

// ---------------------------------------------------------------------------
// 冷却表
// ---------------------------------------------------------------------------

/// 进程冷却表：记录每个进程下次可处理的单调时刻。
#[derive(Default, Debug, Clone)]
pub struct Cooldowns {
    entries: HashMap<u32, u64>,
}

impl Cooldowns {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记冷却到 `until`（单调秒）。
    pub fn mark(&mut self, pid: u32, until: u64) {
        let e = self.entries.entry(pid).or_insert(0);
        if until > *e {
            *e = until;
        }
    }

    pub fn is_cooling(&self, pid: u32, now: u64) -> bool {
        self.entries.get(&pid).map(|t| *t > now).unwrap_or(false)
    }

    pub fn remaining(&self, pid: u32, now: u64) -> u64 {
        self.entries
            .get(&pid)
            .map(|t| t.saturating_sub(now))
            .unwrap_or(0)
    }

    /// 清理已过期条目，避免长期运行后无限增长。
    pub fn purge(&mut self, now: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, t| *t > now);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_facts() -> ProcessFacts {
        ProcessFacts {
            pid: 4000,
            name: "someapp.exe".into(),
            session: Some(1),
            working_set: 512 * 1024 * 1024,
            private_bytes: 300 * 1024 * 1024,
            os_critical: Some(false),
            last_foreground_secs: None,
            cpu_percent: Some(0.0),
            cooling: false,
            is_self: false,
        }
    }

    fn cfg_with_allow() -> Config {
        Config {
            allow: vec!["someapp.exe".into()],
            ..Default::default()
        }
    }

    fn verdict(f: &ProcessFacts, cfg: &Config, manual: bool, fg: Option<u32>) -> Verdict {
        decide(&PolicyInput {
            facts: f,
            config: cfg,
            self_session: Some(1),
            foreground_pid: fg,
            purpose: if manual {
                Purpose::ManualTrim
            } else {
                Purpose::Auto
            },
        })
    }

    fn kill_verdict(f: &ProcessFacts, cfg: &Config) -> Verdict {
        decide(&PolicyInput {
            facts: f,
            config: cfg,
            self_session: Some(1),
            foreground_pid: None,
            purpose: Purpose::ManualKill,
        })
    }

    #[test]
    fn force_cannot_bypass_core_protection() {
        let cfg = cfg_with_allow();
        // 即使 manual = true（等价于 --force），核心系统进程仍被硬阻止
        for name in [
            "csrss.exe",
            "lsass",
            "winlogon.exe",
            "System",
            "dwm.exe",
            "services",
        ] {
            let mut f = base_facts();
            f.name = name.to_string();
            let v = verdict(&f, &cfg, true, None);
            assert!(
                matches!(v.decision, Decision::Blocked(_)),
                "{} 应被硬保护阻止",
                name
            );
        }
    }

    #[test]
    fn force_cannot_bypass_os_critical_or_self() {
        let cfg = cfg_with_allow();
        let mut f = base_facts();
        f.os_critical = Some(true);
        assert!(matches!(
            verdict(&f, &cfg, true, None).decision,
            Decision::Blocked(Reason::OsCritical)
        ));

        let mut me = base_facts();
        me.is_self = true;
        assert!(matches!(
            verdict(&me, &cfg, true, None).decision,
            Decision::Blocked(Reason::CoreCritical)
        ));

        let mut idle = base_facts();
        idle.pid = 4;
        assert!(matches!(
            verdict(&idle, &cfg, true, None).decision,
            Decision::Blocked(Reason::CoreCritical)
        ));
    }

    #[test]
    fn manual_mode_reports_warnings_but_allows_soft_protected() {
        let cfg = Config {
            protect: vec!["someapp".into()],
            ..cfg_with_allow()
        };
        let f = base_facts();
        let v = verdict(&f, &cfg, true, None);
        assert_eq!(v.decision, Decision::Candidate);
        assert!(v.warnings.contains(&Reason::UserProtected));

        // 自动模式下则直接跳过
        let v2 = verdict(&f, &cfg, false, None);
        assert!(matches!(v2.decision, Decision::Skip(Reason::UserProtected)));
    }

    #[test]
    fn auto_mode_requires_allow_list() {
        let cfg = Config::default(); // allow 为空
        let f = base_facts();
        assert!(matches!(
            verdict(&f, &cfg, false, None).decision,
            Decision::Skip(Reason::NotInAllowList)
        ));

        let allowed = cfg_with_allow();
        assert_eq!(
            verdict(&f, &allowed, false, None).decision,
            Decision::Candidate
        );
    }

    #[test]
    fn foreground_and_recent_activity_are_protected() {
        let cfg = cfg_with_allow();
        let f = base_facts();
        assert!(matches!(
            verdict(&f, &cfg, false, Some(4000)).decision,
            Decision::Skip(Reason::Foreground)
        ));

        let mut recent = base_facts();
        recent.last_foreground_secs = Some(30);
        assert!(matches!(
            verdict(&recent, &cfg, false, None).decision,
            Decision::Skip(Reason::RecentForeground)
        ));

        // 超过窗口则不再保护
        let mut old = base_facts();
        old.last_foreground_secs = Some(cfg.policy.recent_foreground_secs + 10);
        assert_eq!(
            verdict(&old, &cfg, false, None).decision,
            Decision::Candidate
        );
    }

    #[test]
    fn busy_cooling_and_small_are_skipped() {
        let cfg = cfg_with_allow();

        let mut busy = base_facts();
        busy.cpu_percent = Some(50.0);
        assert!(matches!(
            verdict(&busy, &cfg, false, None).decision,
            Decision::Skip(Reason::Busy)
        ));

        let mut cooling = base_facts();
        cooling.cooling = true;
        assert!(matches!(
            verdict(&cooling, &cfg, false, None).decision,
            Decision::Skip(Reason::Cooling)
        ));

        let mut small = base_facts();
        small.working_set = 8 * 1024 * 1024;
        assert!(matches!(
            verdict(&small, &cfg, false, None).decision,
            Decision::Skip(Reason::TooSmall)
        ));
    }

    #[test]
    fn unknown_activity_skips_auto_but_not_manual() {
        let cfg = cfg_with_allow();

        // CPU 指标未知：自动模式必须保守跳过，绝不能当成「闲置」
        let mut unknown = base_facts();
        unknown.cpu_percent = None;
        assert!(
            matches!(
                verdict(&unknown, &cfg, false, None).decision,
                Decision::Skip(Reason::UnknownActivity)
            ),
            "活动指标未知时自动模式必须跳过"
        );

        // 手动裁剪：降级为警告并放行
        let v = verdict(&unknown, &cfg, true, None);
        assert_eq!(v.decision, Decision::Candidate);
        assert!(v.warnings.contains(&Reason::UnknownActivity));

        // 手动结束进程：未知活动不应阻挡
        let kill = kill_verdict(&unknown, &cfg);
        assert_eq!(kill.decision, Decision::Candidate);
        assert!(!kill.warnings.contains(&Reason::UnknownActivity));
    }

    #[test]
    fn unknown_identity_and_other_session_are_skipped() {
        let cfg = cfg_with_allow();

        let mut unknown = base_facts();
        unknown.session = None;
        assert!(matches!(
            verdict(&unknown, &cfg, false, None).decision,
            Decision::Skip(Reason::UnknownIdentity)
        ));

        let mut other = base_facts();
        other.session = Some(9);
        assert!(matches!(
            verdict(&other, &cfg, false, None).decision,
            Decision::Skip(Reason::OtherSession)
        ));
    }

    // ---- 压力状态机 ----

    fn snap(free_ratio: f64, commit_ratio: f64) -> PressureSnapshot {
        PressureSnapshot {
            total_phys: 1000,
            avail_phys: (1000.0 * free_ratio) as u64,
            commit_limit: 1000,
            commit_total: (1000.0 * commit_ratio) as u64,
            low_memory_signal: None,
        }
    }

    #[test]
    fn pressure_needs_to_hold_before_acting() {
        let cfg = PolicyConfig::default();
        let mut t = PressureTracker::new();

        // 第一次低内存：只进入候选阶段
        let r = t.update(&snap(0.05, 0.5), &cfg, 0);
        assert!(!r.should_act, "刚进入低内存不应立即执行");

        // 未满持续时间
        let r = t.update(&snap(0.05, 0.5), &cfg, cfg.press_hold_secs - 1);
        assert!(!r.should_act);

        // 满足持续时间
        let r = t.update(&snap(0.05, 0.5), &cfg, cfg.press_hold_secs + 1);
        assert!(r.should_act, "持续低内存后应允许执行");
        assert!(matches!(
            r.kind,
            PressureKind::Physical | PressureKind::Both
        ));
    }

    #[test]
    fn pressure_uses_hysteresis() {
        let cfg = PolicyConfig::default();
        let mut t = PressureTracker::new();
        let _ = t.update(&snap(0.05, 0.5), &cfg, 0);
        let r = t.update(&snap(0.05, 0.5), &cfg, 100);
        assert!(r.should_act);

        // 回到两个阈值之间：保持压力状态（滞回）
        let mid = (cfg.press_enter_free_ratio + cfg.press_exit_free_ratio) / 2.0;
        let r = t.update(&snap(mid, 0.5), &cfg, 110);
        assert!(r.should_act, "滞回区间内应保持压力状态");

        // 超过退出阈值：恢复正常
        let r = t.update(&snap(0.9, 0.5), &cfg, 120);
        assert!(!r.should_act);
        assert!(matches!(r.kind, PressureKind::None));
    }

    #[test]
    fn commit_pressure_alone_does_not_trigger_trimming() {
        let cfg = PolicyConfig::default();
        let mut t = PressureTracker::new();
        let r = t.update(&snap(0.9, 0.95), &cfg, 0);
        assert!(!r.should_act, "提交压力不应触发工作集裁剪");
        assert!(matches!(r.kind, PressureKind::Commit));
        assert!(r.notes.iter().any(|n| n.contains("提交")));
    }

    #[test]
    fn cooldown_marks_and_expires() {
        let mut c = Cooldowns::new();
        assert!(!c.is_cooling(100, 0));
        c.mark(100, 600);
        assert!(c.is_cooling(100, 100));
        assert!(!c.is_cooling(100, 601));
        assert_eq!(c.remaining(100, 100), 500);

        // 重复标记应取较晚的时间
        c.mark(100, 300);
        assert_eq!(c.remaining(100, 100), 500);

        assert_eq!(c.purge(1000), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn manual_kill_ignores_optimization_only_reasons() {
        let cfg = Config::default(); // allow 为空，工作集阈值默认 128MB
        let mut small = base_facts();
        small.name = "tiny.exe".into();
        small.working_set = 2 * 1024 * 1024; // 远低于阈值
        small.cpu_percent = Some(80.0); // 且很活跃

        // 自动模式下会被跳过
        assert!(matches!(
            verdict(&small, &cfg, false, None).decision,
            Decision::Skip(_)
        ));

        // 手动结束进程时，工作集大小与 CPU 活跃度都不应阻挡
        let v = kill_verdict(&small, &cfg);
        assert_eq!(v.decision, Decision::Candidate);
        assert!(
            !v.warnings.contains(&Reason::TooSmall)
                && !v.warnings.contains(&Reason::Busy)
                && !v.warnings.contains(&Reason::NotInAllowList),
            "结束进程不应受这些原因影响：{:?}",
            v.warnings
        );

        // 但硬保护依然生效
        let mut critical = base_facts();
        critical.name = "csrss.exe".into();
        assert!(matches!(
            kill_verdict(&critical, &cfg).decision,
            Decision::Blocked(_)
        ));
    }

    #[test]
    fn manual_kill_still_warns_about_foreground_and_user_list() {
        let cfg = Config {
            protect: vec!["myapp".into()],
            ..Default::default()
        };
        let mut f = base_facts();
        f.name = "myapp.exe".into();

        let v = decide(&PolicyInput {
            facts: &f,
            config: &cfg,
            self_session: Some(1),
            foreground_pid: Some(f.pid),
            purpose: Purpose::ManualKill,
        });
        assert_eq!(v.decision, Decision::Candidate);
        assert!(v.warnings.contains(&Reason::UserProtected));
        assert!(v.warnings.contains(&Reason::Foreground));
    }

    #[test]
    fn name_normalization() {
        assert_eq!(normalize_name("Chrome.EXE "), "chrome");
        assert_eq!(normalize_name("LSASS"), "lsass");
        assert_eq!(normalize_name("my.app.exe"), "my.app");
    }
}
