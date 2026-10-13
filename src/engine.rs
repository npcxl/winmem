//! 统一执行引擎：采样 → 压力判定 → 候选选择 → 小批执行 → 观察 → 冷却。
//!
//! CLI 与 GUI **共用本模块**，不各自实现一套“一键优化”。
//!
//! 关键约束：
//! - 同一时刻只允许一个任务；跨实例用命名互斥体 [`win::OPTIMIZE_MUTEX`] 协调；
//! - UI 线程不得调用本模块的阻塞方法，必须在后台线程执行；
//! - 执行前用**同一已核验句柄**完成校验与操作，避免 PID 复用导致误操作；
//! - 单次运行**必须确认持续压力**，不能只采一次样就下“可用内存充足”的结论。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::metrics::{self, ForegroundTracker, ProcessMetrics, ProcessSampler, SystemSampler};
use crate::policy::Purpose;
use crate::policy::{
    self, Cooldowns, Decision, PolicyInput, PressureKind, PressureReport, PressureTracker,
    ProcessFacts, Reason, Verdict,
};
use crate::track::{ProcObservation, TrackPoint, TrackSession, TrackTarget};
use crate::win::{self, WinError};

/// 观察期结束后工作集回涨超过该比例，视为本次裁剪无实际效果。
pub const REGROWTH_INEFFECTIVE: f64 = 0.5;

// ---------------------------------------------------------------------------
// 取消
// ---------------------------------------------------------------------------

/// 可跨线程共享的取消标志。
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 计划
// ---------------------------------------------------------------------------

/// 计划中的一项。
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub pid: u32,
    pub name: String,
    pub creation_time: Option<u64>,
    pub working_set: Option<u64>,
    pub private_bytes: Option<u64>,
    /// 允许处理的候选为 true
    pub accepted: bool,
    /// 拒绝原因（仅首个原因用于展示）
    pub reason: Option<Reason>,
    /// 全部相关原因（手动模式下作为警告）
    pub notes: Vec<Reason>,
}

/// 一次优化计划。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub pressure: Option<PressureReport>,
    pub items: Vec<PlanItem>,
}

impl Plan {
    pub fn candidates(&self) -> impl Iterator<Item = &PlanItem> {
        self.items.iter().filter(|i| i.accepted)
    }

    pub fn skipped(&self) -> impl Iterator<Item = &PlanItem> {
        self.items.iter().filter(|i| !i.accepted)
    }

    pub fn creation_time_of(&self, pid: u32) -> Option<u64> {
        self.items
            .iter()
            .find(|i| i.pid == pid)
            .and_then(|i| i.creation_time)
    }
}

// ---------------------------------------------------------------------------
// 单进程执行
// ---------------------------------------------------------------------------

/// 单个进程的裁剪结果。
#[derive(Debug, Clone)]
pub struct TrimOutcome {
    pub pid: u32,
    pub name: String,
    /// 裁剪前的工作集（可测量时）
    pub working_set_before: Option<u64>,
    /// 工作集减少量。`None` 表示前后测量失败，**结果未知**（不等于 0）。
    pub working_set_reduction: Option<u64>,
    /// 观察期结束后的工作集
    pub working_set_after: Option<u64>,
    /// 回涨比例：1.0 表示减少量被完全吃回，`None` 表示无法测量
    pub regrowth_ratio: Option<f64>,
    /// 失败原因；`None` 表示调用成功
    pub error: Option<WinError>,
    /// 因身份变化 / 进程退出被跳过
    pub skipped: Option<Reason>,
}

/// 工作集裁剪执行器。
pub struct TrimExecutor {
    /// 调用 `EmptyWorkingSet` 后等待回收稳定的时长
    pub settle_ms: u64,
}

impl Default for TrimExecutor {
    fn default() -> Self {
        TrimExecutor { settle_ms: 120 }
    }
}

impl TrimExecutor {
    pub fn new(settle_ms: u64) -> TrimExecutor {
        TrimExecutor { settle_ms }
    }

    /// 对单个进程执行工作集裁剪。
    ///
    /// 全程使用**同一个已核验句柄**：先校验创建时间与预期身份一致，
    /// 再在同一句柄上测量与操作，避免“校验后按 PID 重新打开”带来的 PID 复用风险。
    pub fn trim_one(&self, expect: &win::ProcessIdentity, name: &str) -> TrimOutcome {
        let mut out = TrimOutcome {
            pid: expect.pid,
            name: name.to_string(),
            working_set_before: None,
            working_set_reduction: None,
            working_set_after: None,
            regrowth_ratio: None,
            error: None,
            skipped: None,
        };

        let h = match win::open_handle(
            expect.pid,
            win::PROCESS_QUERY_LIMITED_INFORMATION | win::PROCESS_SET_QUOTA,
        ) {
            Ok(h) => h,
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        };

        // 身份校验：创建时间不一致说明 PID 已被复用
        if !expect.matches(h.raw()) {
            out.skipped = Some(Reason::UnknownIdentity);
            return out;
        }

        let before = win::memory_counters(h.raw())
            .ok()
            .map(|c| c.WorkingSetSize as u64);
        out.working_set_before = before;

        if unsafe { win::EmptyWorkingSet(h.raw()) } == 0 {
            out.error = Some(WinError::from_code(win::last_error()));
            return out;
        }

        if self.settle_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.settle_ms));
        }

        let after = win::memory_counters(h.raw())
            .ok()
            .map(|c| c.WorkingSetSize as u64);

        // 任一侧读取失败 → 未知，绝不记为 0
        out.working_set_reduction = match (before, after) {
            (Some(b), Some(a)) => Some(b.saturating_sub(a)),
            _ => None,
        };
        out
    }

    /// 结束一个已核验身份的进程（不进入自动流程，仅手动 kill 使用）。
    pub fn kill_one(&self, expect: &win::ProcessIdentity) -> Result<(), WinError> {
        let h = win::open_handle(
            expect.pid,
            win::PROCESS_TERMINATE | win::PROCESS_QUERY_LIMITED_INFORMATION,
        )?;
        if !expect.matches(h.raw()) {
            return Err(WinError::ProcessGone(0));
        }
        win::terminate_handle(h.raw())
    }
}

// ---------------------------------------------------------------------------
// 运行选项与结果
// ---------------------------------------------------------------------------

/// 优化运行选项。
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// 只生成计划，不执行
    pub dry_run: bool,
    /// 手动指定目标 PID；空表示自动模式
    pub manual_targets: Vec<u32>,
    /// 忽略压力判定直接执行（手动/显式请求）
    pub force_run: bool,
    /// 单次运行的最大处理进程数；0 表示使用配置值
    pub budget: usize,
    /// 自动模式下等待“持续压力”确认的最长秒数；0 表示不等待（只报告“压力尚未确认”）
    pub confirm_secs: u64,
    /// 执行后是否进行 1 分钟 / 5 分钟效果跟踪
    pub track: bool,
}

/// 进度回调参数。
pub struct RunProgress {
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
    pub message: String,
}

/// 系统级观测。`None` 表示该指标读取失败。
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub avail_before: Option<u64>,
    pub avail_after: Option<u64>,
    pub commit_before: Option<u64>,
    pub commit_after: Option<u64>,
    /// 所有进程工作集减少量之和（不是“释放的物理内存”）
    pub reduction_total: u64,
    /// 结果未知（前后测量失败）的进程数
    pub unknown_count: usize,
    /// 成功执行的进程数
    pub trimmed_count: usize,
}

impl Observation {
    /// 系统可用物理内存的净变化，可能为负。
    pub fn avail_delta(&self) -> Option<i64> {
        match (self.avail_before, self.avail_after) {
            (Some(b), Some(a)) => Some(a as i64 - b as i64),
            _ => None,
        }
    }

    pub fn commit_delta(&self) -> Option<i64> {
        match (self.commit_before, self.commit_after) {
            (Some(b), Some(a)) => Some(a as i64 - b as i64),
            _ => None,
        }
    }
}

/// 一次运行的完整结果。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub plan: Plan,
    pub outcomes: Vec<TrimOutcome>,
    pub observation: Observation,
    pub cancelled: bool,
    pub dry_run: bool,
    /// 结束原因
    pub finished: String,
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 统一优化引擎。CLI / GUI 都通过它执行。
pub struct Engine {
    pub config: Config,
    sampler: SystemSampler,
    procs: ProcessSampler,
    tracker: PressureTracker,
    foreground: ForegroundTracker,
    cooldowns: Cooldowns,
    executor: TrimExecutor,
    /// 是否已建立差分基线（CPU / I/O 需要两次采样）
    baseline_ready: bool,
    /// 最近一次进程采样的缓存 `(采样时刻, 数据)`。
    ///
    /// 目的：让「界面展示」与「策略判定」使用**同一份**数据，
    /// 避免紧接着再扫一次导致差分区间过短、CPU 指标失真或变成未知。
    metrics_cache: Option<(u64, Vec<ProcessMetrics>)>,
}

impl Engine {
    pub fn new(config: Config) -> Engine {
        Engine {
            config,
            sampler: SystemSampler::new(),
            procs: ProcessSampler::new(),
            tracker: PressureTracker::new(),
            foreground: ForegroundTracker::new(),
            cooldowns: Cooldowns::new(),
            executor: TrimExecutor::default(),
            baseline_ready: false,
            metrics_cache: None,
        }
    }

    /// 取得进程指标；若缓存年龄在 `max_age_ms` 内则直接复用。
    ///
    /// - GUI 展示用 `max_age_ms = 0`（强制新采样）；
    /// - 策略判定用较大的 `max_age_ms`，复用刚采过的同一份数据。
    pub fn collect_metrics(&mut self, max_age_ms: u64) -> Vec<ProcessMetrics> {
        let now = metrics::monotonic_ms();
        if let Some((t, v)) = &self.metrics_cache {
            if now.saturating_sub(*t) <= max_age_ms {
                return v.clone();
            }
        }
        let v = self.procs.collect();
        self.baseline_ready = true;
        self.metrics_cache = Some((now, v.clone()));
        v
    }

    pub fn system_snapshot(&mut self) -> Option<policy::PressureSnapshot> {
        self.sampler.snapshot()
    }

    pub fn pressure_phase(&self) -> &'static str {
        self.tracker.phase_name()
    }

    pub fn cooldown_count(&self) -> usize {
        self.cooldowns.len()
    }

    /// 差分基线是否已建立（未建立时不会得到 CPU / I/O 指标，自动模式因此不会执行）。
    pub fn baseline_ready(&self) -> bool {
        self.baseline_ready
    }

    /// 建立差分基线：进程采样一次 + 记录当前前台。
    ///
    /// 「建立基线期间不自动裁剪」由策略层保证：基线未建立时 CPU 指标为未知，
    /// 自动模式会因 [`Reason::UnknownActivity`] 跳过。
    pub fn establish_baseline(&mut self) {
        let _ = self.procs.collect();
        self.foreground.poll(metrics::monotonic_secs());
        self.baseline_ready = true;
    }

    /// 采样一次前台窗口，持续跟踪应用切换。
    pub fn poll_foreground(&mut self) -> Option<u32> {
        let now = metrics::monotonic_secs();
        let pid = self.foreground.poll(now);
        // 定期清理过期记录，避免长期运行后无限增长
        let window = self.config.policy.recent_foreground_secs.max(600);
        self.foreground.purge(window.saturating_mul(2), now);
        pid
    }

    /// 用一次采样推进压力状态机（自动模式与 status 命令共用同一状态机）。
    pub fn update_pressure(
        &mut self,
        s: &crate::policy::PressureSnapshot,
        now_secs: u64,
    ) -> PressureReport {
        self.tracker.update(s, &self.config.policy, now_secs)
    }

    /// 生成计划但**不执行**，供 GUI 预览 / dry-run 使用。
    pub fn preview_plan(&mut self) -> Plan {
        self.build_plan(Purpose::Auto, &[])
    }

    /// 当前记录到的前台窗口所属进程。
    pub fn foreground_pid(&self) -> Option<u32> {
        self.foreground.current()
    }

    /// 供界面展示的判定：**与策略判定使用同一份状态**（同一 Engine 的前台记录与冷却表）。
    ///
    /// 这样界面上显示的「可优化 / 仅手动 / 禁止操作」就是策略实际会采用的结果，
    /// 不会出现界面说可以优化、执行时却被跳过的偏差。
    pub fn evaluate_for_display(&self, m: &ProcessMetrics, now_secs: u64) -> Verdict {
        self.evaluate(
            m,
            Purpose::Auto,
            now_secs,
            win::current_pid(),
            win::current_session(),
            self.foreground.current(),
        )
    }

    /// 结束一个**已核验身份**的进程。仅用于手动 kill，绝不进入自动流程。
    pub fn kill_verified(&self, expect: &win::ProcessIdentity) -> Result<(), WinError> {
        self.executor.kill_one(expect)
    }

    /// 直接应用一份配置（例如 GUI 刚保存的配置，无需等待下一轮指纹比对）。
    pub fn apply_config(&mut self, cfg: Config) {
        self.config = cfg;
    }

    /// 从磁盘重新加载配置；指纹变化时应用并返回 true。
    ///
    /// 后台会话**只读取、不写回**，因此不会覆盖用户刚修改的配置；
    /// `winmem auto --off`、GUI 保存、手改配置文件都会在这里被感知并及时生效。
    pub fn reload_config_if_changed(&mut self) -> bool {
        let disk = Config::load();
        if disk.fingerprint() == self.config.fingerprint() {
            return false;
        }
        self.config.allow = disk.allow;
        self.config.protect = disk.protect;
        self.config.auto_enabled = disk.auto_enabled;
        self.config.policy = disk.policy;
        self.config.path = disk.path;
        self.config.warnings = disk.warnings;
        true
    }

    /// 把一次进程采样转换为策略判定。
    fn evaluate(
        &self,
        m: &ProcessMetrics,
        purpose: Purpose,
        now_secs: u64,
        self_pid: u32,
        self_session: Option<u32>,
        foreground: Option<u32>,
    ) -> Verdict {
        // “活跃度”取 CPU 占比与 I/O 活动的较大者；两者都未知时为 None（未知）
        let activity = match (m.cpu_percent, m.io_active) {
            (Some(c), Some(true)) => Some(c.max(100.0)),
            (Some(c), _) => Some(c),
            (None, Some(true)) => Some(100.0),
            (None, _) => None,
        };
        let facts = ProcessFacts {
            pid: m.pid,
            name: m.name.clone(),
            session: m.session,
            working_set: m.working_set.unwrap_or(0),
            private_bytes: m.private_bytes.unwrap_or(0),
            os_critical: m.os_critical,
            last_foreground_secs: self.foreground.age_secs(
                m.pid,
                m.creation_time,
                m.exe_path.as_deref(),
                now_secs,
            ),
            cpu_percent: activity,
            cooling: self.cooldowns.is_cooling(m.pid, now_secs),
            is_self: m.pid == self_pid,
        };
        policy::decide(&PolicyInput {
            facts: &facts,
            config: &self.config,
            self_session,
            foreground_pid: foreground,
            purpose,
        })
    }

    /// 采集进程并生成计划（不执行任何操作）。
    pub fn build_plan(&mut self, purpose: Purpose, targets: &[u32]) -> Plan {
        // 一次性运行（CLI optimize / GUI 一键 / 预览）没有历史样本：
        // 先采一次建立差分基线，再等待一小段时间，CPU / I/O 指标才可用。
        // 否则活动指标一律「未知」，自动策略会保守地把所有进程都跳过。
        if !self.baseline_ready {
            self.establish_baseline();
            let gap = self.config.policy.baseline_gap_ms;
            if gap > 0 {
                std::thread::sleep(Duration::from_millis(gap));
            }
        }
        let now_secs = metrics::monotonic_secs();
        self.poll_foreground();
        self.cooldowns.purge(now_secs);

        let list = self.collect_metrics(1500);
        let self_pid = win::current_pid();
        let self_session = win::current_session();
        let fg = self.foreground.current();
        let target_set: Option<std::collections::HashSet<u32>> = if targets.is_empty() {
            None
        } else {
            Some(targets.iter().copied().collect())
        };

        let mut items = Vec::with_capacity(list.len());
        for m in &list {
            if let Some(set) = &target_set {
                if !set.contains(&m.pid) {
                    continue;
                }
            }
            let verdict = self.evaluate(m, purpose, now_secs, self_pid, self_session, fg);
            let (accepted, reason) = match verdict.decision {
                Decision::Candidate => (true, None),
                Decision::Skip(r) => (false, Some(r)),
                Decision::Blocked(r) => (false, Some(r)),
            };
            items.push(PlanItem {
                pid: m.pid,
                name: m.name.clone(),
                creation_time: m.creation_time,
                working_set: m.working_set,
                private_bytes: m.private_bytes,
                accepted,
                reason,
                notes: verdict.warnings,
            });
        }

        // 候选在前，按工作集降序
        items.sort_by(|a, b| {
            b.accepted
                .cmp(&a.accepted)
                .then(b.working_set.unwrap_or(0).cmp(&a.working_set.unwrap_or(0)))
        });
        Plan {
            pressure: None,
            items,
        }
    }

    /// 执行一次优化。
    ///
    /// 流程：采样 → **持续压力确认** → 计划 → 小批执行 → 批后观察 → 冷却。
    /// 满足「压力恢复 / 用户取消 / 候选耗尽 / 预算用尽 / 收益不足」任一条件即停止。
    pub fn run(
        &mut self,
        opts: &RunOptions,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(RunProgress),
    ) -> RunOutcome {
        let purpose = if opts.manual_targets.is_empty() {
            Purpose::Auto
        } else {
            Purpose::ManualTrim
        };
        let manual = purpose.is_manual();
        let budget = if opts.budget > 0 {
            opts.budget
        } else {
            self.config.policy.run_budget
        };

        // 跨实例互斥：非预览的运行必须拿到命名互斥体，避免多个实例并行优化
        let _guard = if opts.dry_run {
            None
        } else {
            match win::NamedMutex::try_acquire(win::OPTIMIZE_MUTEX) {
                Some(g) => Some(g),
                None => {
                    return RunOutcome {
                        plan: Plan::default(),
                        outcomes: Vec::new(),
                        observation: Observation::default(),
                        cancelled: false,
                        dry_run: false,
                        finished: "已有其它优化任务正在执行，本次未执行（禁止并行优化）".into(),
                    }
                }
            }
        };

        // 1) 系统采样 + 压力判定
        let before = self.sampler.snapshot();
        let start_secs = metrics::monotonic_secs();
        let mut pressure = before
            .as_ref()
            .map(|s| self.tracker.update(s, &self.config.policy, start_secs));
        let mut observation = Observation {
            avail_before: before.map(|s| s.avail_phys),
            commit_before: before.map(|s| s.commit_total),
            ..Default::default()
        };

        let mut cancelled = false;
        let mut confirm_waited = 0u64;
        let mut should_act =
            manual || opts.force_run || pressure.as_ref().map(|p| p.should_act).unwrap_or(false);

        // 预览：只报告计划，不做压力等待
        if opts.dry_run {
            let mut plan = self.build_plan(purpose, &opts.manual_targets);
            plan.pressure = pressure.clone();
            observation.avail_after = before.map(|s| s.avail_phys);
            observation.commit_after = before.map(|s| s.commit_total);
            let note = if should_act {
                "已达到执行条件"
            } else if manual {
                "手动指定目标"
            } else {
                "当前未达到持续压力条件，真实执行时不会做任何处理"
            };
            return RunOutcome {
                plan,
                outcomes: Vec::new(),
                observation,
                cancelled: false,
                dry_run: true,
                finished: format!(
                    "仅预览候选与跳过原因（--dry-run），未执行任何操作；{}",
                    note
                ),
            };
        }

        // 2) 自动模式：等待“持续压力”确认，而不是单次采样就下结论
        if !should_act && !manual && !opts.force_run && opts.confirm_secs > 0 {
            let deadline = start_secs + opts.confirm_secs;
            loop {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
                let now = metrics::monotonic_secs();
                if let Some(s) = self.sampler.snapshot() {
                    let rep = self.tracker.update(&s, &self.config.policy, now);
                    confirm_waited = now.saturating_sub(start_secs);
                    pressure = Some(rep);
                    progress(RunProgress {
                        phase: "confirm",
                        done: 0,
                        total: 0,
                        message: format!(
                            "等待持续压力确认（{} / {} 秒）",
                            confirm_waited, opts.confirm_secs
                        ),
                    });
                    if pressure.as_ref().map(|p| p.should_act).unwrap_or(false) {
                        should_act = true;
                        break;
                    }
                } else {
                    confirm_waited = now.saturating_sub(start_secs);
                }
                if now >= deadline {
                    break;
                }
            }
        }

        if cancelled {
            observation.avail_after = self.sampler.snapshot().map(|s| s.avail_phys);
            return RunOutcome {
                plan: Plan::default(),
                outcomes: Vec::new(),
                observation,
                cancelled: true,
                dry_run: false,
                finished: "用户取消（压力确认阶段）".into(),
            };
        }

        // 3) 未确认持续压力 → 明确说明，绝不误报“可用内存充足”
        if !should_act {
            let after_snap = self.sampler.snapshot();
            observation.avail_after = after_snap.as_ref().map(|s| s.avail_phys);
            observation.commit_after = after_snap.as_ref().map(|s| s.commit_total);
            let finished = if before.is_none() {
                "无法读取系统内存状态，未执行任何操作（不假设压力）".to_string()
            } else {
                let free_ratio = pressure.as_ref().map(|p| p.free_ratio).unwrap_or(1.0);
                let low = free_ratio < self.config.policy.press_enter_free_ratio;
                match pressure.as_ref().map(|p| p.kind) {
                    Some(PressureKind::Commit) => {
                        "仅检测到提交量偏高：裁剪工作集无法解决该问题，未执行任何操作".to_string()
                    }
                    _ if !low => "未检测到内存压力（当前可用内存充足），未执行任何操作".to_string(),
                    _ => format!(
                        "压力尚未确认：可用内存偏低但未持续满足触发条件（需持续 {} 秒，已等待 {} 秒），未执行任何操作",
                        self.config.policy.press_hold_secs, confirm_waited
                    ),
                }
            };
            return RunOutcome {
                plan: Plan::default(),
                outcomes: Vec::new(),
                observation,
                cancelled: false,
                dry_run: false,
                finished,
            };
        }

        // 4) 生成计划
        progress(RunProgress {
            phase: "scan",
            done: 0,
            total: 0,
            message: "正在采集进程指标…".into(),
        });
        let mut plan = self.build_plan(purpose, &opts.manual_targets);
        plan.pressure = pressure.clone();

        // 5) 小批执行
        let candidates: Vec<PlanItem> = plan.candidates().cloned().collect();
        let total = candidates.len();
        let batch = self.config.policy.batch_size.max(1);
        let cooldown = self.config.policy.cooldown_secs;

        let mut outcomes: Vec<TrimOutcome> = Vec::new();
        let mut done = 0usize;
        let mut finished = String::from("候选处理完毕");
        let mut idx = 0usize;

        'outer: while idx < candidates.len() {
            if cancel.is_cancelled() {
                cancelled = true;
                finished = "用户取消".into();
                break;
            }
            if done >= budget {
                finished = format!("已达到执行预算（{} 个进程）", budget);
                break;
            }

            let end = (idx + batch).min(candidates.len());
            let batch_start = outcomes.len();
            let mut batch_reduction = 0u64;

            for item in &candidates[idx..end] {
                if cancel.is_cancelled() {
                    cancelled = true;
                    finished = "用户取消".into();
                    break 'outer;
                }
                if done >= budget {
                    finished = format!("已达到执行预算（{} 个进程）", budget);
                    break 'outer;
                }

                // 执行边界二次校验（不依赖 UI 之前生成的列表）
                if !self.recheck_before_act(item) {
                    outcomes.push(TrimOutcome {
                        pid: item.pid,
                        name: item.name.clone(),
                        working_set_before: None,
                        working_set_reduction: None,
                        working_set_after: None,
                        regrowth_ratio: None,
                        error: None,
                        skipped: Some(Reason::RecentForeground),
                    });
                    continue;
                }

                let Some(ct) = item.creation_time else {
                    outcomes.push(TrimOutcome {
                        pid: item.pid,
                        name: item.name.clone(),
                        working_set_before: None,
                        working_set_reduction: None,
                        working_set_after: None,
                        regrowth_ratio: None,
                        error: None,
                        skipped: Some(Reason::UnknownIdentity),
                    });
                    continue;
                };

                let identity = win::ProcessIdentity {
                    pid: item.pid,
                    creation_time: ct,
                };
                let r = self.executor.trim_one(&identity, &item.name);
                if let Some(v) = r.working_set_reduction {
                    batch_reduction = batch_reduction.saturating_add(v);
                    observation.reduction_total = observation.reduction_total.saturating_add(v);
                } else if r.error.is_none() && r.skipped.is_none() {
                    // 执行成功但测量失败 → 结果未知
                    observation.unknown_count += 1;
                }
                if r.error.is_none() && r.skipped.is_none() {
                    observation.trimmed_count += 1;
                    let now = metrics::monotonic_secs();
                    self.cooldowns.mark(item.pid, now + cooldown);
                }
                outcomes.push(r);
                done += 1;

                progress(RunProgress {
                    phase: "trim",
                    done,
                    total,
                    message: format!("正在处理 {}（PID {}）", item.name, item.pid),
                });
            }

            let batch_first = idx;
            idx = end;

            // 6) 批后观察：等待回收稳定，再测量工作集是否迅速回涨
            let observe_ms = self.config.policy.observe_secs * 1000;
            let mut elapsed = 0u64;
            while elapsed < observe_ms {
                if cancel.is_cancelled() {
                    cancelled = true;
                    finished = "用户取消".into();
                    break 'outer;
                }
                std::thread::sleep(Duration::from_millis(250));
                elapsed += 250;
            }

            let mut ineffective = 0usize;
            for item in &candidates[batch_first..end] {
                let Some(ct) = item.creation_time else {
                    continue;
                };
                let id = win::ProcessIdentity {
                    pid: item.pid,
                    creation_time: ct,
                };
                let after = match win::open_handle(item.pid, win::PROCESS_QUERY_LIMITED_INFORMATION)
                {
                    Ok(h) if id.matches(h.raw()) => win::memory_counters(h.raw())
                        .ok()
                        .map(|c| c.WorkingSetSize as u64),
                    _ => None,
                };
                let Some(now_ws) = after else { continue };
                let Some(o) = outcomes
                    .iter_mut()
                    .find(|o| o.pid == item.pid && o.error.is_none() && o.skipped.is_none())
                else {
                    continue;
                };
                o.working_set_after = Some(now_ws);
                if let (Some(before), Some(reduction)) =
                    (o.working_set_before, o.working_set_reduction)
                {
                    o.regrowth_ratio = if reduction > 0 {
                        let trimmed = before.saturating_sub(reduction);
                        Some(now_ws.saturating_sub(trimmed) as f64 / reduction as f64)
                    } else {
                        Some(0.0)
                    };
                }
            }

            // 回涨过多 ⇒ 这次裁剪没有实际效果，延长冷却，避免反复做无用功
            let now_cd = metrics::monotonic_secs();
            for o in outcomes[batch_start..].iter() {
                if let Some(r) = o.regrowth_ratio {
                    if r > REGROWTH_INEFFECTIVE {
                        ineffective += 1;
                        self.cooldowns
                            .mark(o.pid, now_cd + cooldown.saturating_mul(3));
                    }
                }
            }

            if idx >= candidates.len() {
                break;
            }

            if ineffective > 0 && !manual {
                finished = format!(
                    "上一批有 {} 个进程工作集迅速回涨，已延长冷却并停止后续操作",
                    ineffective
                );
                break;
            }

            if let Some(s) = self.sampler.snapshot() {
                let rep = self
                    .tracker
                    .update(&s, &self.config.policy, metrics::monotonic_secs());
                let recovered = !rep.should_act && !manual;
                plan.pressure = Some(rep);
                if recovered {
                    finished = "可用内存已恢复，提前停止".into();
                    break;
                }
            }

            // 7) 收益不足则停止
            if batch_reduction < self.config.policy.min_gain_bytes && !manual {
                finished = format!(
                    "最近一批仅减少 {} 工作集，低于收益阈值，提前停止",
                    crate::report::fmt_bytes(batch_reduction)
                );
                break;
            }
        }

        // 8) 收尾观测
        std::thread::sleep(Duration::from_millis(300));
        let after = self.sampler.snapshot();
        observation.avail_after = after.map(|s| s.avail_phys);
        observation.commit_after = after.map(|s| s.commit_total);

        RunOutcome {
            plan,
            outcomes,
            observation,
            cancelled,
            dry_run: false,
            finished,
        }
    }

    /// 执行边界的最终校验。
    ///
    /// 每次真正操作前依次检查：当前前台与近期使用、冷却、活动状态、核心保护与身份。
    /// 不依赖 UI 之前生成的列表或计划。
    fn recheck_before_act(&mut self, item: &PlanItem) -> bool {
        let now = metrics::monotonic_secs();
        self.poll_foreground();
        if self.foreground.current() == Some(item.pid) {
            return false;
        }
        if self.cooldowns.is_cooling(item.pid, now) {
            return false;
        }
        // 重新读取该进程的当前状态（活动指标未知或已变活跃则跳过）
        let list = self.collect_metrics(0);
        let Some(m) = list.iter().find(|m| m.pid == item.pid) else {
            return false;
        };
        if m.creation_time != item.creation_time {
            return false;
        }
        let verdict = self.evaluate(
            m,
            Purpose::Auto,
            now,
            win::current_pid(),
            win::current_session(),
            self.foreground.current(),
        );
        matches!(verdict.decision, Decision::Candidate)
    }

    /// 执行 1 分钟 / 5 分钟效果跟踪。
    ///
    /// 调度逻辑位于 [`TrackSession`]（可用可控时钟单测）；此处只负责按节点采样，
    /// 采样之间用短睡眠切分，因此**可随时取消**，且不会阻塞 UI（在后台线程调用）。
    pub fn run_tracking(
        &mut self,
        outcome: &RunOutcome,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&str),
    ) -> Vec<TrackPoint> {
        let targets: Vec<TrackTarget> = outcome
            .outcomes
            .iter()
            .filter(|o| o.error.is_none() && o.skipped.is_none())
            .map(|o| TrackTarget {
                pid: o.pid,
                name: o.name.clone(),
                creation_time: outcome.plan.creation_time_of(o.pid),
                working_set_before: o.working_set_before,
                reduction: o.working_set_reduction,
            })
            .collect();
        if targets.is_empty() {
            return Vec::new();
        }

        let nodes = self.config.policy.track_secs.clone();
        let start = metrics::monotonic_ms();
        let mut session = TrackSession::new(start, &nodes, targets);

        while !session.is_done() {
            if cancel.is_cancelled() {
                break;
            }
            let now = metrics::monotonic_ms();
            if session.due(now) {
                let snap = self.sampler.snapshot();
                let sys = (
                    snap.as_ref().map(|s| s.avail_phys),
                    snap.as_ref().map(|s| s.commit_total),
                );
                session.advance(now, sys, probe_tracked_process);
                if let Some(p) = session.points().last() {
                    progress(&format!("效果跟踪：已完成 {} 秒节点", p.at_secs));
                }
            } else {
                let sleep = session
                    .next_node_ms()
                    .map(|n| n.saturating_sub(now).min(500))
                    .unwrap_or(500)
                    .max(50);
                std::thread::sleep(Duration::from_millis(sleep));
            }
        }
        session.points().to_vec()
    }
}

/// 按身份探测被跟踪进程。
fn probe_tracked_process(t: &TrackTarget) -> Option<ProcObservation> {
    let Ok(h) = win::open_handle(t.pid, win::PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Some(ProcObservation {
            alive: false,
            working_set: None,
        });
    };
    if let Some(ct) = t.creation_time {
        let expected = win::ProcessIdentity {
            pid: t.pid,
            creation_time: ct,
        };
        if !expected.matches(h.raw()) {
            return Some(ProcObservation {
                alive: false,
                working_set: None,
            });
        }
    }
    let alive = win::is_process_alive(h.raw()).unwrap_or(false);
    let ws = win::memory_counters(h.raw())
        .ok()
        .map(|c| c.WorkingSetSize as u64);
    Some(ProcObservation {
        alive,
        working_set: ws,
    })
}

// ---------------------------------------------------------------------------
// 自动优化会话
// ---------------------------------------------------------------------------

/// 自动优化的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoState {
    /// 开关已关闭
    #[default]
    Off,
    /// 监测中（未检测到持续压力）
    Monitoring,
    /// 压力确认中（低内存但尚未持续足够时长）
    Confirming,
    /// 正在执行小批裁剪
    Optimizing,
    /// 批后观察中（含效果跟踪）
    Observing,
    /// 暂缓执行（提交压力 / 冷却 / 名单为空 / 预算用尽 / 退避）
    Cooling,
}

impl AutoState {
    pub fn label(self) -> &'static str {
        match self {
            AutoState::Off => "已关闭",
            AutoState::Monitoring => "监测中",
            AutoState::Confirming => "压力确认中",
            AutoState::Optimizing => "优化中",
            AutoState::Observing => "观察中",
            AutoState::Cooling => "暂缓",
        }
    }
}

/// 自动优化的共享状态：后台线程写，UI 线程读。
#[derive(Debug, Clone, Default)]
pub struct AutoStatus {
    pub state: AutoState,
    pub message: String,
    /// 会话是否仍在运行（用于区分「刚启动」与「已结束」）
    pub running: bool,
    /// 累计执行的批次数
    pub batches: usize,
    /// 累计处理进程数
    pub trimmed: usize,
    /// 本次会话剩余执行预算（达到 0 后暂停到压力恢复）
    pub session_budget_left: usize,
    pub avail_phys: Option<u64>,
    pub total_phys: Option<u64>,
    /// 最近一次结束原因
    pub last_finished: Option<String>,
}

/// 自动会话向前台抛出的事件。
pub enum AutoEvent {
    /// 状态变化
    State(AutoState),
    /// 完成一次执行
    Finished(Box<RunOutcome>),
    /// 需要提示用户（不弹窗，由 UI 决定如何展示）
    Notice(String),
}

/// 可中断的等待：每 250ms 检查一次取消标志。
fn sleep_interruptible(secs: u64, cancel: &CancelToken) {
    let mut left = secs.saturating_mul(1000);
    while left > 0 {
        if cancel.is_cancelled() {
            return;
        }
        let step = left.min(250);
        std::thread::sleep(Duration::from_millis(step));
        left -= step;
    }
}

fn set_state(status: &Arc<Mutex<AutoStatus>>, state: AutoState, message: &str) {
    if let Ok(mut s) = status.lock() {
        s.state = state;
        s.message = message.to_string();
    }
}

/// 自动优化会话。
///
/// 设计要点（对应需求）：
/// - **默认关闭**：只有 `config.auto_enabled` 为真且允许名单非空才会执行；
/// - 允许名单为空**只监测与提示**，不做任何操作；
/// - **持续跟踪前台切换**：每轮都采样前台，覆盖监测期间的应用切换；
/// - **配置热更新**：每轮对比配置指纹，`auto --off` / 名单变化立即生效；
/// - 每轮重新走 `build_plan`（重新采样、重新校验前台与保护规则、重新读身份）；
/// - 与手动操作共用同一个 `Engine`，共享冷却表、压力状态机与前台记录；
/// - 会话级执行预算 + 连续无操作退避，避免形成连续清理风暴；
/// - 取消即时生效：等待切成 250ms 片段，执行内部也检查取消标志。
///
/// 自动会话的**协作式调度器**。
///
/// [`AutoSession::step`] 只执行**一轮**逻辑并立即返回，因此调用方（例如 GUI 的单工作线程）
/// 可以在两轮之间处理手动任务，从而让自动与手动**共享同一个 `Engine`**
/// （冷却表、压力状态机、前台记录、采样缓存）。
#[derive(Debug, Clone)]
pub struct AutoSession {
    interval: u64,
    allow_notice_sent: bool,
    session_budget: usize,
    noop_rounds: u32,
    backoff_until: u64,
    next_step_at: u64,
}

/// 一轮自动会话的产出。
#[derive(Default)]
pub struct AutoStep {
    /// 本轮结束后的状态
    pub state: AutoState,
    /// 完成了一次执行（供调用方记录/展示）
    pub finished: Option<Box<RunOutcome>>,
    /// 需要提示用户
    pub notice: Option<String>,
    /// 会话应当结束（被取消，或配置中已关闭）
    pub stop: bool,
}

impl AutoSession {
    pub fn new(engine: &Engine) -> AutoSession {
        AutoSession {
            interval: engine.config.policy.sample_interval_secs.max(2),
            allow_notice_sent: false,
            session_budget: engine.config.policy.run_budget.saturating_mul(3).max(1),
            noop_rounds: 0,
            backoff_until: 0,
            next_step_at: 0,
        }
    }

    /// 下一次调度应在的单调秒时刻。
    pub fn next_due_secs(&self) -> u64 {
        self.next_step_at
    }

    /// 建议的采样间隔（秒）。
    pub fn interval_secs(&self) -> u64 {
        self.interval
    }

    /// 执行一轮。调用方负责在返回后处理其它任务，并在 `next_due_secs` 之前让出 CPU。
    pub fn step(
        &mut self,
        engine: &mut Engine,
        cancel: &CancelToken,
        status: &Arc<Mutex<AutoStatus>>,
    ) -> AutoStep {
        let mut out = AutoStep {
            state: AutoState::Monitoring,
            ..Default::default()
        };
        if cancel.is_cancelled() {
            out.stop = true;
            out.state = AutoState::Off;
            return out;
        }

        // 持续前台跟踪 + 配置热更新
        engine.poll_foreground();
        if engine.reload_config_if_changed() {
            self.interval = engine.config.policy.sample_interval_secs.max(2);
            self.allow_notice_sent = false;
            out.notice = Some("配置已更新并生效".into());
            if !engine.config.auto_enabled {
                set_state(status, AutoState::Off, "配置中已关闭自动优化，会话结束");
                out.stop = true;
                out.state = AutoState::Off;
                return out;
            }
        }

        let now = metrics::monotonic_secs();
        let Some(snap) = engine.system_snapshot() else {
            set_state(
                status,
                AutoState::Monitoring,
                "系统内存采样失败，继续监测（不执行任何操作）",
            );
            self.next_step_at = now + self.interval;
            return out;
        };

        let report = engine.update_pressure(&snap, now);
        {
            if let Ok(mut s) = status.lock() {
                s.avail_phys = Some(snap.avail_phys);
                s.total_phys = Some(snap.total_phys);
                s.session_budget_left = self.session_budget;
            }
        }

        let allow_empty = engine.config.allow.is_empty();
        let auto_ready = engine.config.auto_enabled && !allow_empty;

        if !report.should_act {
            // 压力恢复：重置预算与退避
            self.session_budget = engine.config.policy.run_budget.saturating_mul(3).max(1);
            self.noop_rounds = 0;
            self.backoff_until = 0;
            let state = if engine.pressure_phase() == "疑似压力" {
                AutoState::Confirming
            } else if matches!(report.kind, PressureKind::Commit | PressureKind::Both) {
                AutoState::Cooling
            } else {
                AutoState::Monitoring
            };
            let msg = if report.notes.is_empty() {
                format!(
                    "可用物理内存 {:.1}%，未达到压力阈值",
                    report.free_ratio * 100.0
                )
            } else {
                report.notes.join("；")
            };
            set_state(status, state, &msg);
            out.state = state;
            self.next_step_at = now + self.interval;
            return out;
        }

        // 达到压力条件，但当前不允许自动执行 → 只提示
        if !auto_ready {
            let msg = if allow_empty {
                "检测到持续内存压力，但自动优化允许名单为空，仅提示不执行"
            } else {
                "检测到持续内存压力，但自动优化未开启，仅提示不执行"
            };
            set_state(status, AutoState::Cooling, msg);
            if !self.allow_notice_sent {
                self.allow_notice_sent = true;
                out.notice = Some(msg.to_string());
            }
            out.state = AutoState::Cooling;
            self.next_step_at = now + self.interval;
            return out;
        }

        // 预算用尽 / 退避中 → 暂缓
        if self.session_budget == 0 {
            set_state(
                status,
                AutoState::Cooling,
                "本次会话已达执行预算，暂停处理直到压力恢复",
            );
            out.state = AutoState::Cooling;
            self.next_step_at = now + self.interval;
            return out;
        }
        if now < self.backoff_until {
            set_state(
                status,
                AutoState::Cooling,
                &format!("暂缓执行（退避中，还有 {} 秒）", self.backoff_until - now),
            );
            out.state = AutoState::Cooling;
            self.next_step_at = now + self.interval;
            return out;
        }

        // 执行一次小批处理
        set_state(
            status,
            AutoState::Optimizing,
            "检测到持续内存压力，开始小批处理",
        );
        out.state = AutoState::Optimizing;

        let run_opts = RunOptions {
            confirm_secs: 0, // 会话已经通过状态机确认了持续压力
            track: false,    // 跟踪在下面单独执行，便于把状态切到 Observing
            ..Default::default()
        };
        let outcome = engine.run(&run_opts, cancel, &mut |p| {
            let st = if p.phase == "trim" {
                AutoState::Optimizing
            } else {
                AutoState::Observing
            };
            set_state(status, st, &p.message);
        });

        {
            if let Ok(mut s) = status.lock() {
                s.batches += 1;
                s.trimmed += outcome.observation.trimmed_count;
                s.last_finished = Some(outcome.finished.clone());
            }
        }
        self.session_budget = self
            .session_budget
            .saturating_sub(outcome.observation.trimmed_count);

        // 观察阶段：1 分钟 / 5 分钟非阻塞跟踪
        if !outcome.cancelled && !outcome.outcomes.is_empty() {
            set_state(
                status,
                AutoState::Observing,
                "观察裁剪效果（含 1 / 5 分钟跟踪）",
            );
            out.state = AutoState::Observing;
            let points = engine.run_tracking(&outcome, cancel, &mut |m| {
                set_state(status, AutoState::Observing, m)
            });
            if let Some(p) = points.last() {
                let msg = format!(
                    "跟踪至 {} 秒：可用内存 {}；工作集回涨过快的进程已延长冷却",
                    p.at_secs,
                    p.avail_phys
                        .map(crate::report::fmt_bytes)
                        .unwrap_or_else(|| "未知".into())
                );
                set_state(status, AutoState::Monitoring, &msg);
                out.state = AutoState::Monitoring;
            }
        }

        // 只有确实发生过操作才写历史，避免“无操作循环频繁写盘”
        if outcome.observation.trimmed_count > 0 {
            let mut history = crate::report::History::default();
            history.push(&crate::report::History::summarize(&outcome));
            history.flush();
        } else {
            // 无候选 / 无收益 → 退避，避免连续扫描形成清理风暴
            self.noop_rounds = self.noop_rounds.saturating_add(1);
            let backoff = (60u64.saturating_mul(self.noop_rounds as u64)).min(600);
            self.backoff_until = metrics::monotonic_secs() + backoff;
        }

        self.next_step_at = metrics::monotonic_secs() + self.interval;
        out.finished = Some(Box::new(outcome));
        if cancel.is_cancelled() {
            out.stop = true;
            out.state = AutoState::Off;
        }
        out
    }
}

/// 前台（阻塞式）运行自动会话：CLI `auto --run` 与 `monitor` 使用。
///
/// GUI 使用 [`AutoSession::step`] 与手动任务协作调度，两者共用同一套逻辑。
pub fn run_auto_session(
    engine: &mut Engine,
    cancel: &CancelToken,
    status: &Arc<Mutex<AutoStatus>>,
    event: &mut dyn FnMut(AutoEvent),
) {
    // 建立差分基线：基线期间不会执行任何操作
    engine.establish_baseline();

    let mut session = AutoSession::new(engine);
    if let Ok(mut s) = status.lock() {
        s.running = true;
        s.state = AutoState::Monitoring;
        s.session_budget_left = engine.config.policy.run_budget.saturating_mul(3).max(1);
        s.message = "自动优化已启动，正在建立采样基线".into();
    }

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let now = metrics::monotonic_secs();
        if now < session.next_due_secs() && session.next_due_secs() != 0 {
            sleep_interruptible(1, cancel);
            continue;
        }

        let step = session.step(engine, cancel, status);
        let state = step.state;
        if let Some(n) = step.notice {
            event(AutoEvent::Notice(n));
        }
        if let Some(f) = step.finished {
            event(AutoEvent::Finished(f));
        }
        if step.stop {
            break;
        }
        event(AutoEvent::State(state));
        sleep_interruptible(session.interval_secs(), cancel);
    }

    if let Ok(mut s) = status.lock() {
        s.running = false;
        s.state = AutoState::Off;
        s.message = "自动优化已停止".into();
    }
    event(AutoEvent::State(AutoState::Off));
}
