//! 统一执行引擎：采样 → 压力判定 → 候选选择 → 小批执行 → 观察 → 冷却。
//!
//! CLI 与 GUI **共用本模块**，不各自实现一套“一键优化”。
//!
//! 关键约束：
//! - 同一时刻只允许一个任务（由调用方持锁或检查 busy 标志）。
//! - UI 线程不得调用本模块的阻塞方法，必须在后台线程执行。
//! - 执行前用**同一已核验句柄**完成校验与操作，避免 PID 复用导致误操作。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::metrics::{self, ForegroundTracker, ProcessMetrics, ProcessSampler, SystemSampler};
use crate::policy::Purpose;
use crate::policy::{
    self, Cooldowns, Decision, PolicyInput, PressureReport, PressureTracker, ProcessFacts, Reason,
    Verdict,
};
use crate::win::{self, WinError};

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
}

// ---------------------------------------------------------------------------
// 单进程执行
// ---------------------------------------------------------------------------

/// 单个进程的裁剪结果。
#[derive(Debug, Clone)]
pub struct TrimOutcome {
    pub pid: u32,
    pub name: String,
    /// 工作集减少量。`None` 表示前后测量失败，**结果未知**（不等于 0）。
    pub working_set_reduction: Option<u64>,
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
            working_set_reduction: None,
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
        }
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

    /// 结束一个**已核验身份**的进程。仅用于手动 kill，绝不进入自动流程。
    pub fn kill_verified(&self, expect: &win::ProcessIdentity) -> Result<(), WinError> {
        self.executor.kill_one(expect)
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
        // “活跃度”取 CPU 占比与 I/O 活动的较大者
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
            last_foreground_secs: self.foreground.age_secs(m.pid, now_secs),
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
        let now_secs = metrics::monotonic_secs();
        self.foreground.poll(now_secs);
        self.cooldowns.purge(now_secs);

        let list = self.procs.collect(true);
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
    /// 流程：采样 → 压力判定 → 计划 → 小批执行 → 批后观察 → 冷却。
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

        // 1) 系统采样 + 压力判定
        let before = self.sampler.snapshot();
        let now_secs = metrics::monotonic_secs();
        let pressure = before.map(|s| self.tracker.update(&s, &self.config.policy, now_secs));

        let mut observation = Observation {
            avail_before: before.map(|s| s.avail_phys),
            commit_before: before.map(|s| s.commit_total),
            ..Default::default()
        };

        // 2) 生成计划
        progress(RunProgress {
            phase: "scan",
            done: 0,
            total: 0,
            message: "正在采集进程指标…".into(),
        });
        let mut plan = self.build_plan(purpose, &opts.manual_targets);
        plan.pressure = pressure.clone();

        let should_act =
            manual || opts.force_run || pressure.as_ref().map(|p| p.should_act).unwrap_or(false);

        // dry-run 优先：无论是否达到压力阈值，都要展示计划与跳过原因
        if opts.dry_run {
            observation.avail_after = before.map(|s| s.avail_phys);
            observation.commit_after = before.map(|s| s.commit_total);
            let note = if should_act {
                "已达到执行条件".to_string()
            } else if manual {
                "手动指定目标".to_string()
            } else {
                "当前未达到压力阈值，真实执行时不会做任何处理".to_string()
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

        if !should_act {
            observation.avail_after = before.map(|s| s.avail_phys);
            observation.commit_after = before.map(|s| s.commit_total);
            return RunOutcome {
                plan,
                outcomes: Vec::new(),
                observation,
                cancelled: false,
                dry_run: false,
                finished: "未检测到持续内存压力（可用内存充足），未执行任何操作".into(),
            };
        }

        // 3) 小批执行
        let candidates: Vec<PlanItem> = plan.candidates().cloned().collect();
        let total = candidates.len();
        let batch = self.config.policy.batch_size.max(1);
        let cooldown = self.config.policy.cooldown_secs;

        let mut outcomes: Vec<TrimOutcome> = Vec::new();
        let mut done = 0usize;
        let mut cancelled = false;
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

                // 批内二次校验：扫描后用户可能已切回该应用
                let fg_now = self.foreground.poll(metrics::monotonic_secs());
                if fg_now == Some(item.pid) {
                    outcomes.push(TrimOutcome {
                        pid: item.pid,
                        name: item.name.clone(),
                        working_set_reduction: None,
                        error: None,
                        skipped: Some(Reason::Foreground),
                    });
                    continue;
                }

                let Some(ct) = item.creation_time else {
                    outcomes.push(TrimOutcome {
                        pid: item.pid,
                        name: item.name.clone(),
                        working_set_reduction: None,
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

            idx = end;
            if idx >= candidates.len() {
                break;
            }

            // 4) 批后观察
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

            // 5) 收益不足则停止
            if batch_reduction < self.config.policy.min_gain_bytes && !manual {
                finished = format!(
                    "最近一批仅减少 {} 工作集，低于收益阈值，提前停止",
                    crate::report::fmt_bytes(batch_reduction)
                );
                break;
            }
        }

        // 6) 收尾观测
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
}
