//! Windows 系统内存配置检查与配置变更（内存压缩 / 页面合并 / 分页文件 / 提交容量）。
//!
//! 接口选择：
//! - 内存压缩、页面合并：受支持的 `Get-MMAgent`（PowerShell，结构化 JSON）
//! - 分页文件与卷：受支持的 CIM 类 `Win32_ComputerSystem` / `Win32_PageFileSetting`
//!   / `Win32_PageFileUsage` / `Win32_LogicalDisk`
//! - 提交容量：Win32 `GetPerformanceInfo`（不走 PowerShell）
//!
//! 不使用非公开内核接口，不做高频采样（PowerShell 查询只在用户显式请求时发起）。
//!
//! 状态语义严格区分，**不把「不支持 / 不可用 / 查询失败」当成「已关闭」**。

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::Json;
use crate::win;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PS_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// 三态查询结果
// ---------------------------------------------------------------------------

/// 查询结果。后三种状态**绝不**等同于「关闭」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe<T> {
    /// 已查询到真实值
    Value(T),
    /// 当前系统不支持该能力
    NotSupported,
    /// 接口存在，但该属性当前不可用
    Unavailable(String),
    /// 查询失败，附带原因
    Failed(String),
}

impl<T> Probe<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Probe::Value(v) => Some(v),
            _ => None,
        }
    }

    /// 状态短标签。
    pub fn label(&self) -> &'static str {
        match self {
            Probe::Value(_) => "已查询",
            Probe::NotSupported => "不支持",
            Probe::Unavailable(_) => "不可用",
            Probe::Failed(_) => "查询失败",
        }
    }

    /// 是否拿到了真实值。
    pub fn is_value(&self) -> bool {
        matches!(self, Probe::Value(_))
    }
}

/// 布尔型能力的人类可读描述。注意：只有 `Value(false)` 才是「已关闭」。
pub fn describe_bool(p: &Probe<bool>) -> String {
    match p {
        Probe::Value(true) => "已开启".into(),
        Probe::Value(false) => "已关闭".into(),
        Probe::NotSupported => "不支持（当前系统无该能力）".into(),
        Probe::Unavailable(why) => format!("不可用（{}）", why),
        Probe::Failed(why) => format!("查询失败（{}）", why),
    }
}

// ---------------------------------------------------------------------------
// PowerShell 执行（固定脚本，不接受外部拼接）
// ---------------------------------------------------------------------------

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 PowerShell：{}", e))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("查询超时".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待子进程失败：{}", e)),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(summarize_error(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 把 PowerShell 的错误输出压缩成一行短信息（不解析含义，只做截断）。
fn summarize_error(stderr: &str) -> String {
    let first = stderr
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let clipped: String = first.chars().take(160).collect();
    if clipped.is_empty() {
        "PowerShell 返回非零退出码".into()
    } else {
        clipped
    }
}

/// 执行一段**硬编码**的 PowerShell 脚本并返回标准输出。
fn run_powershell(script: &str) -> Result<String, String> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .creation_flags(CREATE_NO_WINDOW);
    run_with_timeout(cmd, PS_TIMEOUT)
}

/// 判断当前进程是否具有管理员权限（仅用于提示；实际变更由命令自身再校验一次）。
pub fn is_elevated() -> bool {
    win::is_elevated()
}

/// 当查询失败且当前没有管理员权限时，给出一句统一的补充说明。
pub fn elevation_hint() -> &'static str {
    if is_elevated() {
        ""
    } else {
        "（当前进程不是管理员；部分查询与全部配置变更需要管理员权限）"
    }
}

// ---------------------------------------------------------------------------
// 内存压缩 / 页面合并
// ---------------------------------------------------------------------------

/// 内存压缩与页面合并的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionState {
    pub memory_compression: Probe<bool>,
    pub page_combining: Probe<bool>,
}

const SCRIPT_MMAGENT: &str = concat!(
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8;",
    "$ErrorActionPreference='Stop';",
    "if(-not (Get-Command Get-MMAgent -ErrorAction SilentlyContinue)){'null'}",
    "else{Get-MMAgent|Select-Object MemoryCompression,PageCombining|ConvertTo-Json -Compress}"
);

/// 查询内存压缩与页面合并状态。
pub fn probe_compression() -> CompressionState {
    match run_powershell(SCRIPT_MMAGENT) {
        Ok(out) => {
            let trimmed = out.trim();
            if trimmed.is_empty() {
                return CompressionState {
                    memory_compression: Probe::Failed("未返回任何数据".into()),
                    page_combining: Probe::Failed("未返回任何数据".into()),
                };
            }
            let parsed = Json::parse(trimmed);
            match parsed {
                Ok(Json::Null) => CompressionState {
                    memory_compression: Probe::NotSupported,
                    page_combining: Probe::NotSupported,
                },
                Ok(v) => {
                    let get = |key: &str| match v.get(key) {
                        Some(Json::Bool(b)) => Probe::Value(*b),
                        Some(Json::Null) | None => {
                            Probe::Unavailable("该系统版本未提供此属性".into())
                        }
                        Some(other) => Probe::Unavailable(format!("非布尔值：{:?}", other)),
                    };
                    CompressionState {
                        memory_compression: get("MemoryCompression"),
                        page_combining: get("PageCombining"),
                    }
                }
                Err(e) => CompressionState {
                    memory_compression: Probe::Failed(format!("解析输出失败：{}", e)),
                    page_combining: Probe::Failed(format!("解析输出失败：{}", e)),
                },
            }
        }
        Err(e) => CompressionState {
            memory_compression: Probe::Failed(e.clone()),
            page_combining: Probe::Failed(e),
        },
    }
}

// ---------------------------------------------------------------------------
// 提交容量（Win32，不使用 PowerShell）
// ---------------------------------------------------------------------------

/// 系统提交容量信息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitInfo {
    pub total: u64,
    pub limit: u64,
    pub peak: u64,
    pub page_size: u64,
}

impl CommitInfo {
    /// 提交余量；`limit` 未知时返回 None。
    pub fn headroom(&self) -> Option<u64> {
        if self.limit == 0 {
            None
        } else {
            Some(self.limit.saturating_sub(self.total))
        }
    }

    pub fn ratio(&self) -> Option<f64> {
        if self.limit == 0 {
            None
        } else {
            Some(self.total as f64 / self.limit as f64)
        }
    }
}

/// 读取提交容量。注意：提交上限会随系统活动变化，不是固定不变的值。
pub fn commit_info() -> Option<CommitInfo> {
    let pi = win::performance_info()?;
    let page = pi.PageSize.max(1) as u64;
    Some(CommitInfo {
        total: pi.CommitTotal as u64 * page,
        limit: pi.CommitLimit as u64 * page,
        peak: pi.CommitPeak as u64 * page,
        page_size: page,
    })
}

// ---------------------------------------------------------------------------
// 分页文件与卷
// ---------------------------------------------------------------------------

/// 一个分页文件条目。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageFileEntry {
    pub path: String,
    /// 已配置的初始大小（MB）；0 表示由系统决定
    pub initial_mb: Option<u64>,
    pub maximum_mb: Option<u64>,
    /// 当前已分配大小（MB）
    pub allocated_mb: Option<u64>,
    /// 当前使用量（MB）。**这不是磁盘分页吞吐**
    pub current_usage_mb: Option<u64>,
    pub peak_usage_mb: Option<u64>,
}

/// 逻辑卷信息。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VolumeInfo {
    pub device: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

/// 分页文件整体报告。单个属性失败不影响其他结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PageFileReport {
    pub automatic_managed: Probe<bool>,
    pub entries: Vec<PageFileEntry>,
    pub volumes: Vec<VolumeInfo>,
    /// 整段查询失败时的原因
    pub error: Option<String>,
}

impl Default for PageFileReport {
    fn default() -> Self {
        PageFileReport {
            automatic_managed: Probe::Unavailable("尚未查询".into()),
            entries: Vec::new(),
            volumes: Vec::new(),
            error: None,
        }
    }
}

const SCRIPT_PAGEFILE: &str = concat!(
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8;",
    "$ErrorActionPreference='Stop';",
    "$cs=Get-CimInstance Win32_ComputerSystem;",
    "$set=@(Get-CimInstance Win32_PageFileSetting -ErrorAction SilentlyContinue|",
    "Select-Object Name,InitialSize,MaximumSize);",
    "$use=@(Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue|",
    "Select-Object Name,AllocatedBaseSize,CurrentUsage,PeakUsage);",
    "$vol=@(Get-CimInstance Win32_LogicalDisk -ErrorAction SilentlyContinue|",
    "Select-Object DeviceID,Size,FreeSpace);",
    "[pscustomobject]@{Automatic=[bool]$cs.AutomaticManagedPagefile;",
    "Settings=$set;Usage=$use;Volumes=$vol}|ConvertTo-Json -Compress -Depth 4"
);

/// 查询分页文件配置、使用量、所属卷空间。
pub fn probe_page_files() -> PageFileReport {
    let out = match run_powershell(SCRIPT_PAGEFILE) {
        Ok(o) => o,
        Err(e) => {
            return PageFileReport {
                error: Some(e),
                ..Default::default()
            }
        }
    };
    let v = match Json::parse(out.trim()) {
        Ok(v) => v,
        Err(e) => {
            return PageFileReport {
                error: Some(format!("解析输出失败：{}", e)),
                ..Default::default()
            }
        }
    };

    let automatic_managed = match v.get("Automatic") {
        Some(Json::Bool(b)) => Probe::Value(*b),
        Some(_) => Probe::Unavailable("属性类型异常".into()),
        None => Probe::Unavailable("该系统未提供 AutomaticManagedPagefile".into()),
    };

    let mut entries: Vec<PageFileEntry> = Vec::new();
    if let Some(s) = v.get("Settings") {
        for item in s.as_arr_or_single() {
            let path = item
                .get("Name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            if path.is_empty() {
                continue;
            }
            entries.push(PageFileEntry {
                path,
                initial_mb: item.get("InitialSize").and_then(Json::as_u64),
                maximum_mb: item.get("MaximumSize").and_then(Json::as_u64),
                ..Default::default()
            });
        }
    }
    if let Some(u) = v.get("Usage") {
        for item in u.as_arr_or_single() {
            let name = item.get("Name").and_then(Json::as_str).unwrap_or("");
            let entry = entries
                .iter_mut()
                .find(|e| e.path.eq_ignore_ascii_case(name));
            let (alloc, cur, peak) = (
                item.get("AllocatedBaseSize").and_then(Json::as_u64),
                item.get("CurrentUsage").and_then(Json::as_u64),
                item.get("PeakUsage").and_then(Json::as_u64),
            );
            match entry {
                Some(e) => {
                    e.allocated_mb = alloc;
                    e.current_usage_mb = cur;
                    e.peak_usage_mb = peak;
                }
                None => entries.push(PageFileEntry {
                    path: name.to_string(),
                    allocated_mb: alloc,
                    current_usage_mb: cur,
                    peak_usage_mb: peak,
                    ..Default::default()
                }),
            }
        }
    }

    let mut volumes = Vec::new();
    if let Some(vs) = v.get("Volumes") {
        for item in vs.as_arr_or_single() {
            volumes.push(VolumeInfo {
                device: item
                    .get("DeviceID")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string(),
                total_bytes: item.get("Size").and_then(Json::as_u64),
                free_bytes: item.get("FreeSpace").and_then(Json::as_u64),
            });
        }
    }

    PageFileReport {
        automatic_managed,
        entries,
        volumes,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// 诊断
// ---------------------------------------------------------------------------

/// 诊断级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "提示",
            Severity::Warn => "注意",
            Severity::Critical => "警告",
        }
    }
}

/// 一条诊断结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub severity: Severity,
    pub text: String,
}

/// 根据分页文件与提交容量给出诊断。
///
/// 不使用「RAM 的 1.5 倍 / 2 倍」之类未经证实的一刀切规则。
pub fn diagnose(pagefile: &PageFileReport, commit: Option<CommitInfo>) -> Vec<Diagnosis> {
    let mut out = Vec::new();

    if let Some(err) = &pagefile.error {
        out.push(Diagnosis {
            severity: Severity::Warn,
            text: format!("分页文件信息查询失败：{}", err),
        });
    }

    match &pagefile.automatic_managed {
        Probe::Value(false) if pagefile.entries.is_empty() => out.push(Diagnosis {
            severity: Severity::Critical,
            text: "未检测到分页文件配置：系统既未自动管理，也没有显式配置的分页文件。\
                   提交量上限可能受限，建议在系统虚拟内存设置中确认。"
                .into(),
        }),
        Probe::Value(false) => out.push(Diagnosis {
            severity: Severity::Info,
            text: "分页文件为手动配置（非系统自动管理）。".into(),
        }),
        Probe::Value(true) => out.push(Diagnosis {
            severity: Severity::Info,
            text: "分页文件由系统自动管理。".into(),
        }),
        Probe::NotSupported => out.push(Diagnosis {
            severity: Severity::Info,
            text: "当前系统不支持查询自动管理分页文件状态。".into(),
        }),
        Probe::Unavailable(w) => out.push(Diagnosis {
            severity: Severity::Info,
            text: format!("分页文件管理状态不可用（{}）。", w),
        }),
        Probe::Failed(w) => out.push(Diagnosis {
            severity: Severity::Warn,
            text: format!("分页文件管理状态查询失败（{}）。", w),
        }),
    }

    if let Some(c) = commit {
        if let Some(ratio) = c.ratio() {
            let headroom = c.headroom().unwrap_or(0);
            if ratio >= 0.95 || headroom < 1024 * 1024 * 1024 {
                out.push(Diagnosis {
                    severity: Severity::Critical,
                    text: format!(
                        "提交余量严重不足：当前已提交 {:.1}%，剩余约 {}。",
                        ratio * 100.0,
                        crate::report::fmt_bytes(headroom)
                    ),
                });
            } else if ratio >= 0.85 {
                out.push(Diagnosis {
                    severity: Severity::Warn,
                    text: format!(
                        "提交量已达上限的 {:.1}%，建议检查异常增长的程序。",
                        ratio * 100.0
                    ),
                });
            } else {
                out.push(Diagnosis {
                    severity: Severity::Info,
                    text: format!("提交量占上限 {:.1}%，处于正常范围。", ratio * 100.0),
                });
            }

            if c.peak > c.limit && c.limit > 0 {
                out.push(Diagnosis {
                    severity: Severity::Warn,
                    text: "历史提交峰值曾超过当前上限，说明提交上限会随系统配置变化，\
                           不要把它当成固定不变的值。"
                        .into(),
                });
            }
        } else {
            out.push(Diagnosis {
                severity: Severity::Info,
                text: "提交上限不可用，无法判断提交余量。".into(),
            });
        }
    } else {
        out.push(Diagnosis {
            severity: Severity::Info,
            text: "提交容量数据不足，需要继续观察。".into(),
        });
    }

    for e in &pagefile.entries {
        let Some(drive) = drive_of(&e.path) else {
            continue;
        };
        if let Some(v) = pagefile
            .volumes
            .iter()
            .find(|v| v.device.eq_ignore_ascii_case(&drive))
        {
            if let (Some(free), Some(total)) = (v.free_bytes, v.total_bytes) {
                let ratio = if total > 0 {
                    free as f64 / total as f64
                } else {
                    1.0
                };
                if free < 2 * 1024 * 1024 * 1024 || ratio < 0.10 {
                    out.push(Diagnosis {
                        severity: Severity::Warn,
                        text: format!(
                            "分页文件所在卷 {} 可用空间偏低（剩余 {}，占 {:.1}%）；\
                             空间不足会限制分页文件增长。",
                            drive,
                            crate::report::fmt_bytes(free),
                            ratio * 100.0
                        ),
                    });
                }
            }
        }
    }

    out
}

/// 从 `C:\pagefile.sys` 提取 `C:`。
fn drive_of(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        Some(format!("{}:", (bytes[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 配置变更与恢复
// ---------------------------------------------------------------------------

/// 可变更的系统配置项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    MemoryCompression,
    PageCombining,
    AutomaticPagefile,
}

impl ChangeKind {
    pub fn id(self) -> &'static str {
        match self {
            ChangeKind::MemoryCompression => "MemoryCompression",
            ChangeKind::PageCombining => "PageCombining",
            ChangeKind::AutomaticPagefile => "AutomaticPagefile",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            ChangeKind::MemoryCompression => "内存压缩",
            ChangeKind::PageCombining => "页面合并",
            ChangeKind::AutomaticPagefile => "系统自动管理分页文件",
        }
    }

    /// 该变更是否需要重启才能确认生效。
    pub fn needs_reboot(self) -> bool {
        match self {
            // MMAgent 的改动不保证立即完全生效
            ChangeKind::MemoryCompression | ChangeKind::PageCombining => true,
            ChangeKind::AutomaticPagefile => false,
        }
    }
}

/// 一次变更执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOutcome {
    pub ok: bool,
    /// `None` 表示无法可靠判断是否需要重启
    pub reboot_required: Option<bool>,
    pub message: String,
    /// 变更后重新查询到的状态（用于区分「配置已改变」与「效果已确认」）
    pub verified: String,
}

const PS_ADMIN_GUARD: &str = concat!(
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8;",
    "$ErrorActionPreference='Stop';",
    "$id=[Security.Principal.WindowsIdentity]::GetCurrent();",
    "$pr=New-Object Security.Principal.WindowsPrincipal($id);",
    "if(-not $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))",
    "{'NEED_ADMIN';exit 0};"
);

/// 读取某项配置的当前状态（人类可读字符串，用于记录与比对）。
pub fn snapshot_of(kind: ChangeKind) -> String {
    match kind {
        ChangeKind::MemoryCompression => describe_bool(&probe_compression().memory_compression),
        ChangeKind::PageCombining => describe_bool(&probe_compression().page_combining),
        ChangeKind::AutomaticPagefile => describe_bool(&probe_page_files().automatic_managed),
    }
}

fn script_for(kind: ChangeKind, enable: bool) -> String {
    match kind {
        ChangeKind::MemoryCompression => format!(
            "{}{}",
            PS_ADMIN_GUARD,
            if enable {
                "Enable-MMAgent -MemoryCompression -ErrorAction Stop;'OK'"
            } else {
                "Disable-MMAgent -MemoryCompression -ErrorAction Stop;'OK'"
            }
        ),
        ChangeKind::PageCombining => format!(
            "{}{}",
            PS_ADMIN_GUARD,
            if enable {
                "Enable-MMAgent -PageCombining -ErrorAction Stop;'OK'"
            } else {
                "Disable-MMAgent -PageCombining -ErrorAction Stop;'OK'"
            }
        ),
        ChangeKind::AutomaticPagefile => format!(
            "{}$cs=Get-CimInstance Win32_ComputerSystem;{}\
             $cs|Set-CimInstance -Property @{{AutomaticManagedPagefile=${} }};'OK'",
            PS_ADMIN_GUARD,
            "",
            if enable { "true" } else { "false" }
        ),
    }
}

/// 执行一次明确的配置变更，并在变更后重新查询验证。
pub fn apply_change(kind: ChangeKind, enable: bool) -> ChangeOutcome {
    let before = snapshot_of(kind);
    let script = script_for(kind, enable);
    match run_powershell(&script) {
        Ok(out) => {
            let t = out.trim();
            if t.contains("NEED_ADMIN") {
                return ChangeOutcome {
                    ok: false,
                    reboot_required: None,
                    message: "需要管理员权限才能修改系统配置（本程序不会自动提权）".into(),
                    verified: before,
                };
            }
            if !t.contains("OK") {
                return ChangeOutcome {
                    ok: false,
                    reboot_required: None,
                    message: format!("命令未返回成功标记：{}", t),
                    verified: before,
                };
            }
            let after = snapshot_of(kind);
            let same_as_target = after.starts_with(if enable { "已开启" } else { "已关闭" });
            let reboot = kind.needs_reboot();
            let message = if same_as_target {
                if reboot {
                    format!(
                        "{}已提交变更：配置已改为「{}」；该改动可能需要重新启动后才能完全生效，请重启后再次检查确认。",
                        kind.title(),
                        if enable { "开启" } else { "关闭" }
                    )
                } else {
                    format!("{}已提交变更并已重新查询确认：{}。", kind.title(), after)
                }
            } else {
                format!(
                    "{}的命令已执行成功，但重新查询得到的仍是「{}」，无法确认已生效；建议重新启动后验证。",
                    kind.title(),
                    after
                )
            };
            let change = ConfigChange {
                at: win::local_time_string(),
                kind: kind.id().to_string(),
                before: before.clone(),
                target: if enable { "已开启" } else { "已关闭" }.to_string(),
                result: "OK".to_string(),
                verified: after.clone(),
                reboot_required: reboot && !same_as_target || reboot,
            };
            let _ = append_change(&change);
            ChangeOutcome {
                ok: true,
                reboot_required: Some(reboot),
                message,
                verified: after,
            }
        }
        Err(e) => ChangeOutcome {
            ok: false,
            reboot_required: None,
            message: format!("变更失败：{}", e),
            verified: before,
        },
    }
}

/// 恢复「本工具上次修改前」的配置。
///
/// 若当前状态与工具修改后的目标不一致（说明被其他软件或用户改过），
/// **只报告差异，不覆盖**。
pub fn restore_last(kind: ChangeKind) -> ChangeOutcome {
    let Some(change) = last_change_of(kind.id()) else {
        return ChangeOutcome {
            ok: false,
            reboot_required: None,
            message: format!("没有找到 {} 的历史变更记录，无法自动恢复", kind.title()),
            verified: snapshot_of(kind),
        };
    };
    let current = snapshot_of(kind);
    if !current.starts_with(&change.target) {
        return ChangeOutcome {
            ok: false,
            reboot_required: None,
            message: format!(
                "当前状态「{}」与工具上次修改后的「{}」不一致（可能已被其他软件修改），\
                 为避免覆盖他人的配置，未执行恢复。请手动在系统设置中确认。",
                current, change.target
            ),
            verified: current,
        };
    }
    let enable = change.before.starts_with("已开启");
    apply_change(kind, enable)
}

// ---------------------------------------------------------------------------
// 变更历史
// ---------------------------------------------------------------------------

/// 一次配置变更记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub at: String,
    pub kind: String,
    pub before: String,
    pub target: String,
    pub result: String,
    pub verified: String,
    pub reboot_required: bool,
}

/// 变更记录文件路径：`%APPDATA%\winmem\sysconf.log`
pub fn history_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    let mut p = std::path::PathBuf::from(base);
    p.push("winmem");
    p.push("sysconf.log");
    Some(p)
}

const MAX_CHANGE_LINES: usize = 100;

fn encode(change: &ConfigChange) -> String {
    format!(
        "at={}\tkind={}\tbefore={}\ttarget={}\tresult={}\tverified={}\treboot={}",
        change.at,
        change.kind,
        change.before.replace('\t', " "),
        change.target.replace('\t', " "),
        change.result.replace('\t', " "),
        change.verified.replace('\t', " "),
        if change.reboot_required { 1 } else { 0 }
    )
}

fn decode(line: &str) -> Option<ConfigChange> {
    let mut at = String::new();
    let mut kind = String::new();
    let mut before = String::new();
    let mut target = String::new();
    let mut result = String::new();
    let mut verified = String::new();
    let mut reboot = false;
    for field in line.split('\t') {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        match k {
            "at" => at = v.to_string(),
            "kind" => kind = v.to_string(),
            "before" => before = v.to_string(),
            "target" => target = v.to_string(),
            "result" => result = v.to_string(),
            "verified" => verified = v.to_string(),
            "reboot" => reboot = v == "1",
            _ => {}
        }
    }
    if kind.is_empty() {
        return None;
    }
    Some(ConfigChange {
        at,
        kind,
        before,
        target,
        result,
        verified,
        reboot_required: reboot,
    })
}

/// 追加一条变更记录（整文件重写，最多保留 `MAX_CHANGE_LINES` 行）。
pub fn append_change(change: &ConfigChange) -> std::io::Result<()> {
    let Some(path) = history_path() else {
        return Ok(());
    };
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    lines.push(encode(change));
    if lines.len() > MAX_CHANGE_LINES {
        let drop = lines.len() - MAX_CHANGE_LINES;
        lines.drain(0..drop);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, lines.join("\n") + "\n")
}

/// 读取全部变更记录（按时间先后）。
pub fn read_changes() -> Vec<ConfigChange> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(decode)
        .collect()
}

/// 读取某项配置最近一次由本工具产生的变更。
pub fn last_change_of(kind: &str) -> Option<ConfigChange> {
    read_changes().into_iter().rev().find(|c| c.kind == kind)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(path: &str, alloc: u64) -> PageFileEntry {
        PageFileEntry {
            path: path.into(),
            allocated_mb: Some(alloc),
            ..Default::default()
        }
    }

    #[test]
    fn probe_states_are_not_confused_with_off() {
        // 只有 Value(false) 才是「已关闭」
        assert_eq!(describe_bool(&Probe::Value(false)), "已关闭");
        assert_eq!(describe_bool(&Probe::Value(true)), "已开启");
        assert!(describe_bool(&Probe::NotSupported).contains("不支持"));
        assert!(describe_bool(&Probe::Unavailable("x".into())).contains("不可用"));
        assert!(describe_bool(&Probe::Failed("y".into())).contains("查询失败"));
        assert!(!describe_bool(&Probe::Failed("y".into())).contains("已关闭"));
    }

    #[test]
    fn drive_extraction() {
        assert_eq!(drive_of(r"C:\pagefile.sys").as_deref(), Some("C:"));
        assert_eq!(drive_of(r"d:\pagefile.sys").as_deref(), Some("D:"));
        assert_eq!(drive_of("no-drive"), None);
    }

    #[test]
    fn diagnose_flags_missing_pagefile() {
        let report = PageFileReport {
            automatic_managed: Probe::Value(false),
            entries: vec![],
            volumes: vec![],
            error: None,
        };
        let d = diagnose(&report, None);
        assert!(
            d.iter().any(|x| x.severity == Severity::Critical),
            "未配置分页文件应给出警告：{:?}",
            d
        );
    }

    #[test]
    fn diagnose_flags_commit_pressure() {
        let report = PageFileReport {
            automatic_managed: Probe::Value(true),
            entries: vec![],
            volumes: vec![],
            error: None,
        };
        let commit = CommitInfo {
            total: 9_000,
            limit: 10_000,
            peak: 12_000,
            page_size: 4096,
        };
        let d = diagnose(&report, Some(commit));
        assert!(d
            .iter()
            .any(|x| x.text.contains("提交余量") || x.text.contains("提交量已达")));
        // 峰值超过当前上限时应提示上限会变化，而不是断言配置不足
        assert!(d.iter().any(|x| x.text.contains("固定不变")));
    }

    #[test]
    fn diagnose_flags_volume_space() {
        let report = PageFileReport {
            automatic_managed: Probe::Value(true),
            entries: vec![entries(r"C:\pagefile.sys", 4096)],
            volumes: vec![VolumeInfo {
                device: "C:".into(),
                total_bytes: Some(100 * 1024 * 1024 * 1024),
                free_bytes: Some(1024 * 1024 * 1024),
            }],
            error: None,
        };
        let d = diagnose(&report, None);
        assert!(d.iter().any(|x| x.text.contains("可用空间偏低")));
    }

    #[test]
    fn volume_query_failure_does_not_block_others() {
        // 只有一个卷查询失败（缺失）时，其他诊断仍然给出
        let report = PageFileReport {
            automatic_managed: Probe::Failed("超时".into()),
            entries: vec![],
            volumes: vec![],
            error: None,
        };
        let d = diagnose(&report, None);
        assert!(d.iter().any(|x| x.text.contains("查询失败")));
        assert!(d.iter().any(|x| x.text.contains("数据不足")));
    }

    #[test]
    fn change_record_roundtrip() {
        let c = ConfigChange {
            at: "2026-01-01 00:00:00".into(),
            kind: "MemoryCompression".into(),
            before: "已关闭".into(),
            target: "已开启".into(),
            result: "OK".into(),
            verified: "已开启".into(),
            reboot_required: true,
        };
        let line = encode(&c);
        let back = decode(&line).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn malformed_change_line_is_ignored() {
        assert!(decode("garbage").is_none());
        assert!(decode("at=1").is_none());
    }
}
