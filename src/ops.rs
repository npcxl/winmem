//! 业务层：进程内存栈枚举、内存优化、进程关闭。

use crate::win;
use std::time::Duration;

/// 单个进程的内存栈信息。
#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub threads: u32,
    /// 工作集（物理内存中驻留的部分）
    pub working_set: u64,
    /// 私有提交量（最接近“真实占用”）
    pub private_bytes: u64,
    /// 页面文件占用
    pub pagefile: u64,
    /// 是否有权限读取内存信息
    pub readable: bool,
}

/// 永远不建议结束的核心系统进程（去掉 .exe、转小写后比较）。
const CRITICAL: &[&str] = &[
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
    "audiodg",
];

/// 判断进程是否为受保护的关键进程。
pub fn is_critical(pid: u32, name: &str) -> bool {
    if pid <= 4 {
        return true;
    }
    if pid == win::current_pid() {
        return true;
    }
    let n = name.trim_end_matches(".exe").to_ascii_lowercase();
    CRITICAL.contains(&n.as_str())
}

/// 枚举所有进程及其内存栈占用。
pub fn list_processes() -> Vec<ProcInfo> {
    let mut list = Vec::with_capacity(256);
    for (pid, threads, name) in win::snapshot_processes() {
        let mut info = ProcInfo {
            pid,
            name,
            threads,
            working_set: 0,
            private_bytes: 0,
            pagefile: 0,
            readable: false,
        };
        if let Some(m) = win::process_memory(pid) {
            info.working_set = m.WorkingSetSize as u64;
            info.private_bytes = m.PrivateUsage as u64;
            info.pagefile = m.PagefileUsage as u64;
            info.readable = true;
        }
        list.push(info);
    }
    list
}

/// 排序字段。
#[derive(Clone, Copy, PartialEq)]
pub enum SortBy {
    Mem,
    Private,
    Name,
    Pid,
}

impl SortBy {
    pub fn parse(s: &str) -> Option<SortBy> {
        match s.to_ascii_lowercase().as_str() {
            "mem" | "ws" | "working" | "working_set" => Some(SortBy::Mem),
            "private" | "priv" | "commit" | "真实" => Some(SortBy::Private),
            "name" | "进程名" => Some(SortBy::Name),
            "pid" => Some(SortBy::Pid),
            _ => None,
        }
    }
}

/// 排序 + 过滤 + 截断。
pub fn refine(
    mut list: Vec<ProcInfo>,
    sort: SortBy,
    filter: Option<&str>,
    top: Option<usize>,
) -> Vec<ProcInfo> {
    if let Some(f) = filter {
        let f = f.to_ascii_lowercase();
        list.retain(|p| p.name.to_ascii_lowercase().contains(&f) || p.pid.to_string() == f);
    }
    match sort {
        SortBy::Mem => list.sort_by(|a, b| b.working_set.cmp(&a.working_set)),
        SortBy::Private => list.sort_by(|a, b| b.private_bytes.cmp(&a.private_bytes)),
        SortBy::Name => list.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())),
        SortBy::Pid => list.sort_by_key(|p| p.pid),
    }
    if let Some(n) = top {
        list.truncate(n);
    }
    list
}

/// 按 PID 或进程名（子串、大小写不敏感）查找进程。
pub fn find(pid: Option<u32>, name: Option<&str>) -> Vec<ProcInfo> {
    let all = list_processes();
    if let Some(p) = pid {
        return all.into_iter().filter(|i| i.pid == p).collect();
    }
    if let Some(n) = name {
        let n = n.to_ascii_lowercase();
        let n = n.trim_end_matches(".exe").to_string();
        return all
            .into_iter()
            .filter(|i| i.name.to_ascii_lowercase().trim_end_matches(".exe") == n)
            .collect();
    }
    Vec::new()
}

/// 单进程内存优化结果。
pub struct TrimResult {
    pub pid: u32,
    pub name: String,
    pub freed: u64,
    pub error: Option<String>,
}

/// 释放单个进程的工作集（把不活跃页面换出到待机列表）。
pub fn trim_process(pid: u32, name: &str) -> TrimResult {
    let before = win::process_memory(pid).map(|m| m.WorkingSetSize as u64).unwrap_or(0);

    let mut h = win::open_process(pid, win::PROCESS_QUERY_LIMITED_INFORMATION | win::PROCESS_SET_QUOTA);
    if h == win::NULL_HANDLE {
        h = win::open_process(pid, win::PROCESS_QUERY_INFORMATION | win::PROCESS_SET_QUOTA);
    }
    if h == win::NULL_HANDLE {
        return TrimResult {
            pid,
            name: name.to_string(),
            freed: 0,
            error: Some("打开进程失败（权限不足）".into()),
        };
    }

    let ok = unsafe { win::EmptyWorkingSet(h) };
    win::close_handle(h);

    if ok == 0 {
        return TrimResult {
            pid,
            name: name.to_string(),
            freed: 0,
            error: Some(format!("EmptyWorkingSet 失败（错误码 {}）", win::last_error())),
        };
    }

    std::thread::sleep(Duration::from_millis(40));
    let after = win::process_memory(pid).map(|m| m.WorkingSetSize as u64).unwrap_or(0);
    TrimResult {
        pid,
        name: name.to_string(),
        freed: before.saturating_sub(after),
        error: None,
    }
}

/// 一键内存优化报告。
pub struct OptimizeReport {
    pub avail_before: u64,
    pub avail_after: u64,
    pub standby_ok: Option<Result<(), i32>>,
    pub trimmed: usize,
    pub freed_total: u64,
    pub failed: usize,
    pub skipped_critical: usize,
}

/// 一键优化：清空待机列表 + 释放所有可操作进程的工作集。
pub fn optimize() -> OptimizeReport {
    let before = win::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);

    // 1) 清空系统待机列表（需要特权，失败不致命）
    let standby_ok = Some(win::purge_standby_list());

    // 2) 逐个进程释放工作集
    let me = win::current_pid();
    let procs = list_processes();
    let mut trimmed = 0usize;
    let mut freed_total = 0u64;
    let mut failed = 0usize;
    let mut skipped_critical = 0usize;
    let mut self_info: Option<(u32, String)> = None;

    for p in procs {
        if p.working_set == 0 {
            continue;
        }
        if p.pid == me {
            self_info = Some((p.pid, p.name));
            continue;
        }
        if is_critical(p.pid, &p.name) {
            skipped_critical += 1;
            continue;
        }
        let r = trim_process(p.pid, &p.name);
        if r.error.is_some() {
            failed += 1;
        } else {
            trimmed += 1;
            freed_total += r.freed;
        }
    }

    // 3) 最后清理自身
    if let Some((pid, name)) = self_info {
        let r = trim_process(pid, &name);
        if r.error.is_none() {
            trimmed += 1;
            freed_total += r.freed;
        }
    }

    let after = win::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
    OptimizeReport {
        avail_before: before,
        avail_after: after,
        standby_ok,
        trimmed,
        freed_total,
        failed,
        skipped_critical,
    }
}
