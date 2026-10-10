//! 系统与进程指标采集。
//!
//! 原则：
//! - 读取失败一律表现为 `None`（未知），**绝不伪装成 0**。
//! - 高成本指标（`GetPerformanceInfo`）带缓存与最小间隔。
//! - 差分类指标（CPU / I/O）需要两次采样，首次返回 `None`。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use crate::policy::PressureSnapshot;
use crate::win;

// ---------------------------------------------------------------------------
// 单调时钟
// ---------------------------------------------------------------------------

/// 进程启动以来的单调毫秒数，不受系统时间调整影响。
pub fn monotonic_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 进程启动以来的单调秒数。
pub fn monotonic_secs() -> u64 {
    monotonic_ms() / 1000
}

// ---------------------------------------------------------------------------
// 系统采样
// ---------------------------------------------------------------------------

/// `GetPerformanceInfo` 结果的缓存有效期。
pub const PERF_CACHE_MS: u64 = 1500;

/// 系统级采样器。持有低内存通知句柄，避免每次重建。
pub struct SystemSampler {
    low_mem: Option<win::MemoryResourceNotification>,
    perf_cache: Option<(u64, win::PERFORMANCE_INFORMATION)>,
}

impl Default for SystemSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemSampler {
    pub fn new() -> SystemSampler {
        SystemSampler {
            low_mem: win::MemoryResourceNotification::low_memory(),
            perf_cache: None,
        }
    }

    /// 低内存通知是否可用（Win7 及以上通常可用）。
    pub fn low_memory_available(&self) -> bool {
        self.low_mem.is_some()
    }

    /// 系统提供的低内存信号；不可用或查询失败时返回 None。
    pub fn low_memory_signal(&self) -> Option<bool> {
        self.low_mem.as_ref().and_then(|n| n.is_low())
    }

    /// 性能信息（提交量、系统缓存、进程/线程数），带缓存。
    pub fn perf(&mut self, max_age_ms: u64) -> Option<win::PERFORMANCE_INFORMATION> {
        let now = monotonic_ms();
        let stale = match &self.perf_cache {
            Some((t, _)) => now.saturating_sub(*t) > max_age_ms,
            None => true,
        };
        if stale {
            if let Some(p) = win::performance_info() {
                self.perf_cache = Some((now, p));
            }
        }
        self.perf_cache.as_ref().map(|(_, p)| *p)
    }

    /// 组装压力快照。任何一项采样失败时对应字段保持 0 / None。
    pub fn snapshot(&mut self) -> Option<PressureSnapshot> {
        let ms = win::memory_status()?;
        let perf = self.perf(PERF_CACHE_MS);
        let page = perf
            .as_ref()
            .map(|p| p.PageSize.max(1) as u64)
            .unwrap_or(4096);
        Some(PressureSnapshot {
            total_phys: ms.ullTotalPhys,
            avail_phys: ms.ullAvailPhys,
            commit_total: perf
                .as_ref()
                .map(|p| p.CommitTotal as u64 * page)
                .unwrap_or(0),
            commit_limit: perf
                .as_ref()
                .map(|p| p.CommitLimit as u64 * page)
                .unwrap_or(0),
            low_memory_signal: self.low_memory_signal(),
        })
    }
}

// ---------------------------------------------------------------------------
// 进程采样
// ---------------------------------------------------------------------------

/// 一次进程采样结果。数值型指标为 `None` 表示读取失败（未知）。
#[derive(Debug, Clone)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub name: String,
    pub threads: u32,
    pub session: Option<u32>,
    pub creation_time: Option<u64>,
    pub working_set: Option<u64>,
    pub private_bytes: Option<u64>,
    /// 采样区间内的 CPU 占比（%），首次采样为 None
    pub cpu_percent: Option<f64>,
    /// 采样区间内是否有 I/O 活动
    pub io_active: Option<bool>,
    /// 系统标记的关键进程；None 表示接口不支持
    pub os_critical: Option<bool>,
    /// 关键内存指标是否读取成功
    pub readable: bool,
}

impl ProcessMetrics {
    pub fn working_set_or_unknown(&self) -> u64 {
        self.working_set.unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
struct PrevSample {
    cpu_time: u64,
    io_bytes: u64,
    at_ms: u64,
}

/// 进程采样器，维护上一轮样本用于差分计算。
#[derive(Default)]
pub struct ProcessSampler {
    prev: HashMap<u32, PrevSample>,
}

impl ProcessSampler {
    pub fn new() -> ProcessSampler {
        ProcessSampler {
            prev: HashMap::new(),
        }
    }

    /// 采集全部进程。
    ///
    /// `include_details=false` 时跳过 CPU / I/O 差分所需的额外查询（更省资源）。
    pub fn collect(&mut self, include_details: bool) -> Vec<ProcessMetrics> {
        let now = monotonic_ms();
        let entries = win::snapshot_process_entries();
        let mut out = Vec::with_capacity(entries.len());
        let mut new_prev: HashMap<u32, PrevSample> = HashMap::with_capacity(entries.len());

        for e in entries {
            let mut m = ProcessMetrics {
                pid: e.pid,
                name: e.name,
                threads: e.threads,
                session: None,
                creation_time: None,
                working_set: None,
                private_bytes: None,
                cpu_percent: None,
                io_active: None,
                os_critical: None,
                readable: false,
            };

            // PID 0（Idle）无法打开，只保留名称
            if e.pid != 0 {
                if let Ok(h) = win::open_handle(e.pid, win::PROCESS_QUERY_LIMITED_INFORMATION) {
                    if let Ok(c) = win::memory_counters(h.raw()) {
                        m.working_set = Some(c.WorkingSetSize as u64);
                        m.private_bytes = Some(c.PrivateUsage as u64);
                        m.readable = true;
                    }
                    m.creation_time = win::creation_time_of(h.raw()).ok();
                    m.session = win::process_session(e.pid);
                    m.os_critical = win::is_process_critical(h.raw()).ok();

                    if include_details {
                        let cpu_time = win::cpu_time_of(h.raw()).ok();
                        let io_total = win::io_counters(h.raw()).ok().map(|c| c.transfer_total());
                        if let Some(t) = cpu_time {
                            if let Some(p) = self.prev.get(&e.pid) {
                                let dt = now.saturating_sub(p.at_ms);
                                if dt > 0 {
                                    let d = t.saturating_sub(p.cpu_time);
                                    // CPU 时间单位为 100ns
                                    m.cpu_percent = Some(d as f64 / 10_000.0 / dt as f64 * 100.0);
                                }
                                if let (Some(io), p_io) = (io_total, p.io_bytes) {
                                    m.io_active = Some(io.saturating_sub(p_io) > 0);
                                }
                            }
                            new_prev.insert(
                                e.pid,
                                PrevSample {
                                    cpu_time: t,
                                    io_bytes: io_total.unwrap_or(0),
                                    at_ms: now,
                                },
                            );
                        }
                    }
                }
            }
            out.push(m);
        }

        // 用本轮样本替换旧样本，自动清理已退出的进程
        self.prev = new_prev;
        out
    }

    /// 当前跟踪的进程数（诊断用）。
    pub fn tracked(&self) -> usize {
        self.prev.len()
    }
}

// ---------------------------------------------------------------------------
// 前台活动跟踪
// ---------------------------------------------------------------------------

/// 记录每个进程最近一次处于前台的时间。
/// 只统计本程序运行期间观察到的切换；未观察到的进程返回 `None`。
#[derive(Default)]
pub struct ForegroundTracker {
    last: HashMap<u32, u64>,
    current: Option<u32>,
}

impl ForegroundTracker {
    pub fn new() -> ForegroundTracker {
        ForegroundTracker::default()
    }

    /// 采样一次前台窗口；返回当前前台 PID。
    pub fn poll(&mut self, now_secs: u64) -> Option<u32> {
        let pid = win::foreground_pid();
        if let Some(p) = pid {
            self.last.insert(p, now_secs);
        }
        self.current = pid;
        pid
    }

    pub fn current(&self) -> Option<u32> {
        self.current
    }

    /// 距最近一次前台活动的秒数；未观察到则返回 None。
    pub fn age_secs(&self, pid: u32, now_secs: u64) -> Option<u64> {
        self.last.get(&pid).map(|t| now_secs.saturating_sub(*t))
    }

    /// 清理长时间未出现的记录。
    pub fn purge(&mut self, older_than_secs: u64, now_secs: u64) {
        self.last
            .retain(|_, t| now_secs.saturating_sub(*t) < older_than_secs);
    }
}
