//! 系统与进程指标采集。
//!
//! 原则：
//! - 读取失败一律表现为 `None`（未知），**绝不伪装成 0**；
//! - 高成本指标（`GetPerformanceInfo`）带缓存与最小间隔；
//! - 差分类指标（CPU / I/O）需要两次采样，**首次返回 `None`**；
//! - 差分**必须同一进程身份**（PID + 创建时间）才有效，PID 复用时不计算；
//! - 基线在**每次采样时都会保存**（包括轻量采样），因此一次基线采样之后即可得到有效差分；
//! - CPU 百分比口径：`Δ(CPU 时间) / Δ(墙上时间)`，即相对**单个逻辑处理器**的占用率，
//!   多核机器上可能超过 100%，这是进程级 CPU 时间的标准算法（任务管理器先除以核心数）。

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
    /// 最近一次 `perf` 读取是否失败（用于区分「过期」与「未知」）
    perf_failed: bool,
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
            perf_failed: false,
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
    ///
    /// 缓存条目的年龄超过 `max_age_ms` 时尝试刷新；刷新失败时**返回上一次的旧值**，
    /// 同时把 `perf_failed` 置位，调用方可通过 [`SystemSampler::perf_stale`] 判断数据是否过期，
    /// 避免把旧数据伪装成最新。
    pub fn perf(&mut self, max_age_ms: u64) -> Option<win::PERFORMANCE_INFORMATION> {
        let now = monotonic_ms();
        let stale = match &self.perf_cache {
            Some((t, _)) => now.saturating_sub(*t) > max_age_ms,
            None => true,
        };
        if stale {
            match win::performance_info() {
                Some(p) => {
                    self.perf_cache = Some((now, p));
                    self.perf_failed = false;
                }
                None => self.perf_failed = true,
            }
        }
        self.perf_cache.as_ref().map(|(_, p)| *p)
    }

    /// 最近一次刷新是否失败（此时 `perf()` 返回的是旧数据）。
    pub fn perf_stale(&self, max_age_ms: u64) -> bool {
        if self.perf_failed {
            return true;
        }
        match &self.perf_cache {
            Some((t, _)) => monotonic_ms().saturating_sub(*t) > max_age_ms,
            None => true,
        }
    }

    /// 组装压力快照。提交量读取失败时对应字段为 0（下层按“不可用”处理）。
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
    /// 可执行文件完整路径；用于「同一应用相关进程」的可靠识别
    pub exe_path: Option<String>,
    pub working_set: Option<u64>,
    pub private_bytes: Option<u64>,
    /// 采样区间内的 CPU 占比（相对单个逻辑处理器的百分比），首次采样为 None
    pub cpu_percent: Option<f64>,
    /// 采样区间内是否有 I/O 活动；任一侧读取失败为 None（未知）
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
    /// 上一样本的进程身份创建时间；不一致说明 PID 已被复用
    creation_time: u64,
    cpu_time: Option<u64>,
    io_bytes: Option<u64>,
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
    /// 每次调用都会保存本轮样本作为下一轮的差分基线；因此**一次基线采样之后**
    /// 下一次采样即可得到有效的 CPU / I/O 差分。差分仅在进程身份
    /// （PID + 创建时间）一致时才计算。
    pub fn collect(&mut self) -> Vec<ProcessMetrics> {
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
                exe_path: None,
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
                    m.exe_path = win::process_path(h.raw()).ok();
                    m.os_critical = win::is_process_critical(h.raw()).ok();

                    let cpu_time = win::cpu_time_of(h.raw()).ok();
                    let io_total = win::io_counters(h.raw()).ok().map(|c| c.transfer_total());

                    // ---- 差分（必须同一身份）----
                    if let (Some(ct), Some(p)) = (m.creation_time, self.prev.get(&e.pid)) {
                        if p.creation_time == ct {
                            let dt = now.saturating_sub(p.at_ms);
                            if dt > 0 {
                                if let (Some(t), Some(pc)) = (cpu_time, p.cpu_time) {
                                    let d = t.saturating_sub(pc);
                                    // CPU 时间单位为 100ns
                                    m.cpu_percent = Some(d as f64 / 10_000.0 / dt as f64 * 100.0);
                                }
                                if let (Some(io), Some(pio)) = (io_total, p.io_bytes) {
                                    m.io_active = Some(io.saturating_sub(pio) > 0);
                                }
                            }
                        }
                    }

                    // ---- 保存本轮基线 ----
                    if let Some(ct) = m.creation_time {
                        new_prev.insert(
                            e.pid,
                            PrevSample {
                                creation_time: ct,
                                cpu_time,
                                io_bytes: io_total,
                                at_ms: now,
                            },
                        );
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

/// 记录每个进程 / 映像最近一次处于前台的时间。
///
/// 身份记录采用 **(PID, 创建时间)**，避免 PID 复用把「很久以前的进程」误判成「刚用过」；
/// 同时按**可执行文件完整路径**记录，使同一应用的多个进程都能得到保护
/// （例如同一路径下的多个 `chrome.exe`），而**不会**把所有同名进程一律视为同一应用。
#[derive(Default)]
pub struct ForegroundTracker {
    last_identity: HashMap<(u32, u64), u64>,
    last_image: HashMap<String, u64>,
    current: Option<u32>,
}

impl ForegroundTracker {
    pub fn new() -> ForegroundTracker {
        ForegroundTracker::default()
    }

    /// 采样一次前台窗口；返回当前前台 PID。
    pub fn poll(&mut self, now_secs: u64) -> Option<u32> {
        let pid = win::foreground_pid();
        self.current = pid;
        if let Some(p) = pid {
            if let Ok(id) = win::ProcessIdentity::query(p) {
                self.last_identity
                    .insert((id.pid, id.creation_time), now_secs);
            }
            if let Ok(h) = win::open_handle(p, win::PROCESS_QUERY_LIMITED_INFORMATION) {
                if let Ok(path) = win::process_path(h.raw()) {
                    self.last_image.insert(path.to_ascii_lowercase(), now_secs);
                }
            }
        }
        pid
    }

    pub fn current(&self) -> Option<u32> {
        self.current
    }

    /// 距最近一次前台活动的秒数。
    ///
    /// - 优先用 **(PID, 创建时间)** 精确匹配；
    /// - 身份不可用时退回**映像路径**匹配（保护同一应用的其它进程）；
    /// - 都没有记录时返回 `None`（未知），调用方应保守处理。
    pub fn age_secs(
        &self,
        pid: u32,
        creation_time: Option<u64>,
        exe_path: Option<&str>,
        now_secs: u64,
    ) -> Option<u64> {
        if let Some(ct) = creation_time {
            if let Some(t) = self.last_identity.get(&(pid, ct)) {
                return Some(now_secs.saturating_sub(*t));
            }
        }
        if let Some(p) = exe_path {
            if let Some(t) = self.last_image.get(&p.to_ascii_lowercase()) {
                return Some(now_secs.saturating_sub(*t));
            }
        }
        None
    }

    /// 清理长时间未出现的记录，避免长期运行后无限增长。
    pub fn purge(&mut self, older_than_secs: u64, now_secs: u64) {
        self.last_identity
            .retain(|_, t| now_secs.saturating_sub(*t) < older_than_secs);
        self.last_image
            .retain(|_, t| now_secs.saturating_sub(*t) < older_than_secs);
    }

    #[cfg(test)]
    pub fn tracked_identities(&self) -> usize {
        self.last_identity.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_age_requires_identity_match() {
        let mut t = ForegroundTracker::default();
        // 直接注入记录，避免依赖真实前台窗口
        t.last_identity.insert((100, 111), 1000);
        t.last_image.insert(r"c:\app\a.exe".into(), 1000);

        // 身份一致 → 有年龄
        assert_eq!(t.age_secs(100, Some(111), None, 1010), Some(10));
        // 身份不一致（PID 复用）但映像路径一致 → 按同应用保护
        assert_eq!(
            t.age_secs(100, Some(999), Some(r"C:\APP\A.EXE"), 1010),
            Some(10)
        );
        // 都不匹配 → 未知
        assert_eq!(
            t.age_secs(100, Some(999), Some(r"C:\other\b.exe"), 1010),
            None
        );
        // 没有创建时间且没有路径 → 未知（不会盲目认为“刚用过”）
        assert_eq!(t.age_secs(100, None, None, 1010), None);
    }

    #[test]
    fn purge_removes_old_entries() {
        let mut t = ForegroundTracker::default();
        t.last_identity.insert((1, 1), 100);
        t.last_image.insert("x".into(), 100);
        t.purge(50, 200);
        assert_eq!(t.tracked_identities(), 0);
        assert!(t.last_image.is_empty());
    }
}
