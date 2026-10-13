//! Windows 系统内存配置检查与配置变更（内存压缩 / 页面合并 / 分页文件 / 提交容量）。
//!
//! 接口选择：
//! - 内存压缩、页面合并：受支持的 `Get-MMAgent`（PowerShell，结构化 JSON）
//! - 分页文件与卷：受支持的 CIM 类 `Win32_ComputerSystem` / `Win32_PageFileSetting`
//!   / `Win32_PageFileUsage` / `Win32_LogicalDisk`
//! - 提交容量：Win32 `GetPerformanceInfo`（不走 PowerShell）
//!
//! 状态语义严格区分，**不把「不支持 / 不可用 / 查询失败」当成「已关闭」**：
//! - [`Snapshot`] 是**结构化**状态，恢复逻辑只看它，**绝不解析显示字符串**；
//! - 每个 CIM 查询独立返回「成功（含条数）/ 失败」，
//!   属性缺失保持“不可用”，绝不被 `[bool]$null` 强制转换成 `false`；
//! - 变更历史记录结构化前后状态，无法可靠恢复时**拒绝恢复**并说明限制。

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::Json;
use crate::win;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PS_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// 结构化状态
// ---------------------------------------------------------------------------

/// 某项开关的**结构化**状态。
///
/// 只有 [`Snapshot::Bool`] 才是可靠的原状态；其余状态都**不允许**被当作恢复目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// 已查询到真实布尔值
    Bool(bool),
    /// 当前系统不支持该能力
    NotSupported,
    /// 接口存在，但该属性当前不可用
    Unavailable(String),
    /// 查询失败，附带原因
    Failed(String),
}

impl Snapshot {
    pub fn is_known(&self) -> bool {
        matches!(self, Snapshot::Bool(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Snapshot::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// 人类可读描述（**仅用于展示**）。
    pub fn describe(&self) -> String {
        match self {
            Snapshot::Bool(true) => "已开启".into(),
            Snapshot::Bool(false) => "已关闭".into(),
            Snapshot::NotSupported => "不支持（当前系统无该能力）".into(),
            Snapshot::Unavailable(why) => format!("不可用（{}）", why),
            Snapshot::Failed(why) => format!("查询失败（{}）", why),
        }
    }

    /// 机器可读编码（写入变更历史）。
    fn encode(&self) -> String {
        match self {
            Snapshot::Bool(true) => "on".into(),
            Snapshot::Bool(false) => "off".into(),
            Snapshot::NotSupported => "unsupported".into(),
            Snapshot::Unavailable(w) => format!("unavailable:{}", sanitize_field(w)),
            Snapshot::Failed(w) => format!("failed:{}", sanitize_field(w)),
        }
    }

    /// 解析历史编码。旧版本写入的是显示字符串（如 `已开启`），也在这里精确映射，
    /// 只有**精确等于** `已开启` / `已关闭` 才会被识别，绝不使用 `starts_with` 之类的前缀判断。
    fn decode(s: &str) -> Snapshot {
        match s {
            "on" | "已开启" => Snapshot::Bool(true),
            "off" | "已关闭" => Snapshot::Bool(false),
            "unsupported" | "不支持（当前系统无该能力）" => Snapshot::NotSupported,
            other => {
                if let Some(rest) = other.strip_prefix("unavailable:") {
                    Snapshot::Unavailable(rest.to_string())
                } else if let Some(rest) = other.strip_prefix("failed:") {
                    Snapshot::Failed(rest.to_string())
                } else {
                    // 无法识别的历史值 → 视为不可用，恢复逻辑会因此拒绝
                    Snapshot::Unavailable(format!("无法识别的历史状态“{}”", other))
                }
            }
        }
    }
}

fn sanitize_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// 布尔型能力的人类可读描述。注意：只有 `Snapshot::Bool(false)` 才是「已关闭」。
pub fn describe_bool(p: &Snapshot) -> String {
    p.describe()
}

// ---------------------------------------------------------------------------
// PowerShell 执行（固定脚本，不接受外部拼接）
// ---------------------------------------------------------------------------

/// 使用**可靠系统路径**定位 PowerShell，不依赖任意 PATH 查找。
fn powershell_exe() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let p = root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// 运行子进程并**持续读取** stdout / stderr，避免等待退出时管道塞满导致假超时。
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 PowerShell：{}", e))?;

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = out_pipe {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = err_pipe {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!("等待子进程失败：{}", e));
            }
        }
    }

    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    if timed_out {
        return Err("查询超时（子进程已终止）".into());
    }
    let stdout = String::from_utf8_lossy(&out).to_string();
    let stderr = String::from_utf8_lossy(&err).to_string();
    // 退出码需要单独获取（此时进程已结束）
    match child.try_wait() {
        Ok(Some(status)) if status.success() => Ok(stdout),
        Ok(Some(status)) => Err(format!(
            "PowerShell 退出码 {:?}：{}",
            status.code(),
            summarize_error(&stderr)
        )),
        _ => {
            if stdout.trim().is_empty() {
                Err(summarize_error(&stderr))
            } else {
                Ok(stdout)
            }
        }
    }
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
        "PowerShell 返回非空错误但无输出".into()
    } else {
        clipped
    }
}

/// 执行一段**硬编码**的 PowerShell 脚本并返回标准输出。
fn run_powershell(script: &str) -> Result<String, String> {
    let Some(exe) = powershell_exe() else {
        return Err(
            "找不到系统 PowerShell（%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe）"
                .into(),
        );
    };
    let mut cmd = Command::new(exe);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ])
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
    pub memory_compression: Snapshot,
    pub page_combining: Snapshot,
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
                    memory_compression: Snapshot::Failed("未返回任何数据".into()),
                    page_combining: Snapshot::Failed("未返回任何数据".into()),
                };
            }
            match Json::parse(trimmed) {
                Ok(Json::Null) => CompressionState {
                    memory_compression: Snapshot::NotSupported,
                    page_combining: Snapshot::NotSupported,
                },
                Ok(v) => {
                    let get = |key: &str| match v.get(key) {
                        Some(Json::Bool(b)) => Snapshot::Bool(*b),
                        Some(Json::Null) | None => {
                            Snapshot::Unavailable("该系统版本未提供此属性".into())
                        }
                        Some(other) => Snapshot::Unavailable(format!("非布尔值：{:?}", other)),
                    };
                    CompressionState {
                        memory_compression: get("MemoryCompression"),
                        page_combining: get("PageCombining"),
                    }
                }
                Err(e) => CompressionState {
                    memory_compression: Snapshot::Failed(format!("解析输出失败：{}", e)),
                    page_combining: Snapshot::Failed(format!("解析输出失败：{}", e)),
                },
            }
        }
        Err(e) => CompressionState {
            memory_compression: Snapshot::Failed(e.clone()),
            page_combining: Snapshot::Failed(e),
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

/// 分页文件的**配置**项（可写入快照 / 用于恢复比对）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageFileSetting {
    pub path: String,
    pub initial_mb: Option<u64>,
    pub maximum_mb: Option<u64>,
}

impl PageFileSetting {
    fn encode(&self) -> String {
        format!(
            "{}|{}|{}",
            sanitize_field(&self.path),
            self.initial_mb.map(|v| v.to_string()).unwrap_or_default(),
            self.maximum_mb.map(|v| v.to_string()).unwrap_or_default()
        )
    }

    fn decode(s: &str) -> PageFileSetting {
        let mut it = s.split('|');
        let path = it.next().unwrap_or("").to_string();
        let num = |x: Option<&str>| x.filter(|v| !v.is_empty()).and_then(|v| v.parse().ok());
        PageFileSetting {
            path,
            initial_mb: num(it.next()),
            maximum_mb: num(it.next()),
        }
    }
}

fn encode_settings(list: &[PageFileSetting]) -> String {
    list.iter()
        .map(|s| s.encode())
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_settings(s: &str) -> Vec<PageFileSetting> {
    s.split(';')
        .filter(|x| !x.trim().is_empty())
        .map(PageFileSetting::decode)
        .collect()
}

/// 逻辑卷信息。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VolumeInfo {
    pub device: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

/// 单个 CIM 查询的结果：成功（附带条数，0 表示空结果）或失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryStatus {
    Ok(usize),
    Failed(String),
}

impl QueryStatus {
    pub fn is_failed(&self) -> bool {
        matches!(self, QueryStatus::Failed(_))
    }
    pub fn describe(&self) -> String {
        match self {
            QueryStatus::Ok(0) => "成功（空结果）".into(),
            QueryStatus::Ok(n) => format!("成功（{} 条）", n),
            QueryStatus::Failed(e) => format!("失败（{}）", e),
        }
    }
}

/// 分页文件整体报告。单个属性 / 单个 CIM 查询失败不影响其他结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PageFileReport {
    pub automatic_managed: Snapshot,
    pub entries: Vec<PageFileEntry>,
    pub volumes: Vec<VolumeInfo>,
    /// 整段查询 / 解析失败时的原因
    pub error: Option<String>,
    pub settings_status: QueryStatus,
    pub usage_status: QueryStatus,
    pub volume_status: QueryStatus,
}

impl Default for PageFileReport {
    fn default() -> Self {
        PageFileReport {
            automatic_managed: Snapshot::Unavailable("尚未查询".into()),
            entries: Vec::new(),
            volumes: Vec::new(),
            error: None,
            settings_status: QueryStatus::Failed("尚未查询".into()),
            usage_status: QueryStatus::Failed("尚未查询".into()),
            volume_status: QueryStatus::Failed("尚未查询".into()),
        }
    }
}

impl PageFileReport {
    /// 当前配置的分页文件项（用于快照与恢复比对）。
    pub fn settings(&self) -> Vec<PageFileSetting> {
        self.entries
            .iter()
            .map(|e| PageFileSetting {
                path: e.path.clone(),
                initial_mb: e.initial_mb,
                maximum_mb: e.maximum_mb,
            })
            .collect()
    }
}

/// 各 CIM 查询使用 `try/catch` 独立上报成功与失败；自动管理属性缺失时保持 `$null`。
const SCRIPT_PAGEFILE: &str = concat!(
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8;",
    "$ErrorActionPreference='Stop';",
    "$r=[ordered]@{};",
    "try{$cs=Get-CimInstance -ClassName Win32_ComputerSystem;",
    "$p=$cs.PSObject.Properties['AutomaticManagedPagefile'];",
    "if($null -eq $p){$r.Auto=@{ok=$true;v=$null}}else{$r.Auto=@{ok=$true;v=[bool]$p.Value}}}",
    "catch{$r.Auto=@{ok=$false;e=[string]$_.Exception.Message}};",
    "try{$s=@(Get-CimInstance -ClassName Win32_PageFileSetting|Select-Object Name,InitialSize,MaximumSize);",
    "$r.Set=@{ok=$true;v=$s}}catch{$r.Set=@{ok=$false;e=[string]$_.Exception.Message}};",
    "try{$u=@(Get-CimInstance -ClassName Win32_PageFileUsage|Select-Object Name,AllocatedBaseSize,CurrentUsage,PeakUsage);",
    "$r.Use=@{ok=$true;v=$u}}catch{$r.Use=@{ok=$false;e=[string]$_.Exception.Message}};",
    "try{$v=@(Get-CimInstance -ClassName Win32_LogicalDisk|Select-Object DeviceID,Size,FreeSpace);",
    "$r.Vol=@{ok=$true;v=$v}}catch{$r.Vol=@{ok=$false;e=[string]$_.Exception.Message}};",
    "$r|ConvertTo-Json -Compress -Depth 5"
);

/// 解析一个带 `ok` 标记的子查询。
fn parse_branch<'a>(node: Option<&'a Json>, what: &str) -> (bool, Option<&'a Json>, String) {
    let Some(n) = node else {
        return (false, None, format!("查询结果缺少 {} 字段", what));
    };
    let ok = n.get("ok").and_then(Json::as_bool).unwrap_or(false);
    if !ok {
        let e = n
            .get("e")
            .and_then(Json::as_str)
            .unwrap_or("未知错误")
            .to_string();
        (false, None, e)
    } else {
        (true, n.get("v"), String::new())
    }
}

/// 查询分页文件配置、使用量、所属卷空间。
pub fn probe_page_files() -> PageFileReport {
    let out = match run_powershell(SCRIPT_PAGEFILE) {
        Ok(o) => o,
        Err(e) => {
            return PageFileReport {
                error: Some(e.clone()),
                settings_status: QueryStatus::Failed(e.clone()),
                usage_status: QueryStatus::Failed(e.clone()),
                volume_status: QueryStatus::Failed(e),
                ..Default::default()
            }
        }
    };
    let v = match Json::parse(out.trim()) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("解析输出失败：{}", e);
            return PageFileReport {
                error: Some(msg.clone()),
                settings_status: QueryStatus::Failed(msg.clone()),
                usage_status: QueryStatus::Failed(msg.clone()),
                volume_status: QueryStatus::Failed(msg),
                ..Default::default()
            };
        }
    };

    // ---- 自动管理（属性缺失 ⇒ 不可用，绝不强制成 false）----
    let automatic_managed = match parse_branch(v.get("Auto"), "Auto") {
        (false, _, e) => Snapshot::Failed(e),
        (true, Some(Json::Bool(b)), _) => Snapshot::Bool(*b),
        (true, _, _) => Snapshot::Unavailable("该系统未提供 AutomaticManagedPagefile".into()),
    };

    // ---- 配置项 ----
    let (settings_ok, settings_v, settings_err) = parse_branch(v.get("Set"), "Set");
    let mut entries: Vec<PageFileEntry> = Vec::new();
    let mut settings_count = 0usize;
    if settings_ok {
        if let Some(s) = settings_v {
            for item in s.as_arr_or_single() {
                let path = item
                    .get("Name")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                if path.is_empty() {
                    continue;
                }
                settings_count += 1;
                entries.push(PageFileEntry {
                    path,
                    initial_mb: item.get("InitialSize").and_then(Json::as_u64),
                    maximum_mb: item.get("MaximumSize").and_then(Json::as_u64),
                    ..Default::default()
                });
            }
        }
    }

    // ---- 使用量（独立状态）----
    let (usage_ok, usage_v, usage_err) = parse_branch(v.get("Use"), "Use");
    let mut usage_count = 0usize;
    if usage_ok {
        if let Some(u) = usage_v {
            for item in u.as_arr_or_single() {
                let name = item.get("Name").and_then(Json::as_str).unwrap_or("");
                usage_count += 1;
                let (alloc, cur, peak) = (
                    item.get("AllocatedBaseSize").and_then(Json::as_u64),
                    item.get("CurrentUsage").and_then(Json::as_u64),
                    item.get("PeakUsage").and_then(Json::as_u64),
                );
                match entries
                    .iter_mut()
                    .find(|e| e.path.eq_ignore_ascii_case(name))
                {
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
    }

    // ---- 卷（独立状态）----
    let (vol_ok, vol_v, vol_err) = parse_branch(v.get("Vol"), "Vol");
    let mut volumes = Vec::new();
    let mut vol_count = 0usize;
    if vol_ok {
        if let Some(vs) = vol_v {
            for item in vs.as_arr_or_single() {
                vol_count += 1;
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
    }

    PageFileReport {
        automatic_managed,
        entries,
        volumes,
        error: None,
        settings_status: if settings_ok {
            QueryStatus::Ok(settings_count)
        } else {
            QueryStatus::Failed(settings_err)
        },
        usage_status: if usage_ok {
            QueryStatus::Ok(usage_count)
        } else {
            QueryStatus::Failed(usage_err)
        },
        volume_status: if vol_ok {
            QueryStatus::Ok(vol_count)
        } else {
            QueryStatus::Failed(vol_err)
        },
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
    for (name, st) in [
        ("分页文件配置", &pagefile.settings_status),
        ("分页文件使用量", &pagefile.usage_status),
        ("卷空间", &pagefile.volume_status),
    ] {
        if let QueryStatus::Failed(e) = st {
            out.push(Diagnosis {
                severity: Severity::Warn,
                text: format!("{}查询失败：{}", name, e),
            });
        }
    }

    match &pagefile.automatic_managed {
        Snapshot::Bool(false) if pagefile.settings_status == QueryStatus::Ok(0) => {
            out.push(Diagnosis {
                severity: Severity::Critical,
                text: "未检测到分页文件配置：系统既未自动管理，也没有显式配置的分页文件。\
                       提交量上限可能受限，建议在系统虚拟内存设置中确认。"
                    .into(),
            })
        }
        Snapshot::Bool(false) => out.push(Diagnosis {
            severity: Severity::Info,
            text: "分页文件为手动配置（非系统自动管理）。".into(),
        }),
        Snapshot::Bool(true) => out.push(Diagnosis {
            severity: Severity::Info,
            text: "分页文件由系统自动管理。".into(),
        }),
        Snapshot::NotSupported => out.push(Diagnosis {
            severity: Severity::Info,
            text: "当前系统不支持查询自动管理分页文件状态。".into(),
        }),
        Snapshot::Unavailable(w) => out.push(Diagnosis {
            severity: Severity::Info,
            text: format!("分页文件管理状态不可用（{}）。", w),
        }),
        Snapshot::Failed(w) => out.push(Diagnosis {
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

    /// 该变更**是否需要重启后才能完全生效**。
    ///
    /// 这里只区分「明确需要重启后验证」与「无法判断」，**绝不硬编码为“无需重启”**。
    pub fn reboot_state(self) -> RebootState {
        match self {
            // MMAgent 的改动不保证立即完全生效
            ChangeKind::MemoryCompression | ChangeKind::PageCombining => RebootState::Pending,
            // 分页文件配置改动后，运行时容量是否立即生效无法确认
            ChangeKind::AutomaticPagefile => RebootState::Unknown,
        }
    }
}

/// 重启验证状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootState {
    /// 已知需要重启后才能完全生效（重启待验证）
    Pending,
    /// 无法判断是否需要重启（状态未知）
    Unknown,
}

impl RebootState {
    pub fn label(self) -> &'static str {
        match self {
            RebootState::Pending => "重启待验证",
            RebootState::Unknown => "是否需要重启未知",
        }
    }
}

/// 验证程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyLevel {
    /// 命令未执行成功
    Failed,
    /// 命令执行成功，但重新查询未得到期望值
    CommandOnly,
    /// 配置已重新查询确认为期望值
    ConfigVerified,
}

impl VerifyLevel {
    pub fn label(self) -> &'static str {
        match self {
            VerifyLevel::Failed => "未完成",
            VerifyLevel::CommandOnly => "命令已执行，配置未确认",
            VerifyLevel::ConfigVerified => "配置已验证",
        }
    }
}

/// 一次变更执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOutcome {
    pub ok: bool,
    pub verify: VerifyLevel,
    pub reboot: RebootState,
    pub message: String,
    /// 变更后重新查询到的状态（用于区分「配置已改变」与「效果已确认」）
    pub verified: String,
    /// 原状态无法可靠读取 ⇒ 无法提供自动恢复保证
    pub recoverable: bool,
}

impl ChangeOutcome {
    fn fail(message: impl Into<String>, verified: impl Into<String>) -> ChangeOutcome {
        ChangeOutcome {
            ok: false,
            verify: VerifyLevel::Failed,
            reboot: RebootState::Unknown,
            message: message.into(),
            verified: verified.into(),
            recoverable: false,
        }
    }
}

const PS_ADMIN_GUARD: &str = concat!(
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8;",
    "$ErrorActionPreference='Stop';",
    "$id=[Security.Principal.WindowsIdentity]::GetCurrent();",
    "$pr=New-Object Security.Principal.WindowsPrincipal($id);",
    "if(-not $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))",
    "{'NEED_ADMIN';exit 0};"
);

/// 读取某项配置的**结构化**当前状态。
pub fn snapshot_of(kind: ChangeKind) -> Snapshot {
    match kind {
        ChangeKind::MemoryCompression => probe_compression().memory_compression,
        ChangeKind::PageCombining => probe_compression().page_combining,
        ChangeKind::AutomaticPagefile => probe_page_files().automatic_managed,
    }
}

/// 读取分页文件的完整结构化配置（自动管理状态 + 各项路径与配置大小）。
pub fn pagefile_snapshot() -> (Snapshot, Vec<PageFileSetting>) {
    let pf = probe_page_files();
    let automatic = pf.automatic_managed.clone();
    (automatic, pf.settings())
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
            "{}$cs=Get-CimInstance -ClassName Win32_ComputerSystem;\
             $cs|Set-CimInstance -Property @{{AutomaticManagedPagefile=${} }};'OK'",
            PS_ADMIN_GUARD,
            if enable { "$true" } else { "$false" }
        ),
    }
}

/// 执行一次明确的配置变更，并在变更后**重新查询验证**。
///
/// 记录结构化的「修改前 / 目标」状态；原状态不可读时仍允许变更（用户显式要求），
/// 但会把 `recoverable` 置为 false，且 `restore_last` 会拒绝自动恢复。
pub fn apply_change(kind: ChangeKind, enable: bool) -> ChangeOutcome {
    let before = snapshot_of(kind);
    let before_entries = if kind == ChangeKind::AutomaticPagefile {
        pagefile_snapshot().1
    } else {
        Vec::new()
    };
    let recoverable = before.is_known();

    let script = script_for(kind, enable);
    let out = match run_powershell(&script) {
        Ok(o) => o,
        Err(e) => return ChangeOutcome::fail(format!("变更失败：{}", e), before.describe()),
    };
    let t = out.trim();
    if t.contains("NEED_ADMIN") {
        return ChangeOutcome::fail(
            "需要管理员权限才能修改系统配置（本程序不会自动提权）",
            before.describe(),
        );
    }
    if !t.contains("OK") {
        return ChangeOutcome::fail(format!("命令未返回成功标记：{}", t), before.describe());
    }

    let after = snapshot_of(kind);
    let after_entries = if kind == ChangeKind::AutomaticPagefile {
        pagefile_snapshot().1
    } else {
        Vec::new()
    };
    let target = Snapshot::Bool(enable);
    let verified = after == target;
    let reboot = kind.reboot_state();

    let message = if verified {
        match reboot {
            RebootState::Pending => format!(
                "{}已提交变更：配置已重新查询确认为「{}」；该改动可能需要重新启动后才能完全生效（{}）。",
                kind.title(),
                after.describe(),
                reboot.label()
            ),
            RebootState::Unknown => format!(
                "{}已提交变更，配置已重新查询确认为「{}」；运行时容量是否立即生效无法确认（{}）。",
                kind.title(),
                after.describe(),
                reboot.label()
            ),
        }
    } else {
        format!(
            "{}的命令已执行成功，但重新查询得到的仍是「{}」（未确认生效）；{}。",
            kind.title(),
            after.describe(),
            reboot.label()
        )
    };

    let change = ConfigChange {
        at: win::local_time_string(),
        kind: kind.id().to_string(),
        before: before.clone(),
        before_entries: before_entries.clone(),
        target: target.clone(),
        target_entries: after_entries.clone(),
        result: "OK".to_string(),
        verified: after.describe(),
        reboot_required: reboot == RebootState::Pending,
    };
    let mut history_ok = true;
    if append_change(&change).is_err() {
        history_ok = false;
    }

    let mut message = message;
    if !recoverable {
        message.push_str("（原状态无法可靠读取，本工具不提供自动恢复保证）");
    }
    if !history_ok {
        message.push_str("（注意：变更历史写入失败，恢复记录可能不完整）");
    }

    ChangeOutcome {
        ok: true,
        verify: if verified {
            VerifyLevel::ConfigVerified
        } else {
            VerifyLevel::CommandOnly
        },
        reboot,
        message,
        verified: after.describe(),
        recoverable,
    }
}

/// 恢复「本工具上次修改前」的配置。
///
/// 基于**结构化快照**判断，任何一步不可靠都拒绝恢复：
/// 1. 历史记录中的原状态必须是可靠的布尔值；
/// 2. 当前状态必须仍等于工具上次修改后的目标（否则说明被外部修改，拒绝覆盖）；
/// 3. 分页文件额外要求配置项一致，否则拒绝（并建议在系统设置中手动处理）。
pub fn restore_last(kind: ChangeKind) -> ChangeOutcome {
    let Some(change) = last_change_of(kind.id()) else {
        return ChangeOutcome::fail(
            format!("没有找到 {} 的历史变更记录，无法自动恢复", kind.title()),
            snapshot_of(kind).describe(),
        );
    };

    // 1) 原状态必须可靠
    let Some(restore_to) = change.before.as_bool() else {
        return ChangeOutcome::fail(
            format!(
                "上次变更前的状态为「{}」，不是可靠的可恢复原状态；为避免把系统改错，未执行恢复。\
                 请在系统设置中手动确认。",
                change.before.describe()
            ),
            snapshot_of(kind).describe(),
        );
    };
    // 记录的目标也必须是可靠布尔值
    let Some(target_bool) = change.target.as_bool() else {
        return ChangeOutcome::fail(
            "历史记录中的目标状态不可靠，未执行恢复。",
            snapshot_of(kind).describe(),
        );
    };

    // 2) 当前状态必须仍与工具上次修改后的记录一致
    let current = snapshot_of(kind);
    if current != Snapshot::Bool(target_bool) {
        return ChangeOutcome::fail(
            format!(
                "当前状态「{}」与工具上次修改后的「{}」不一致（可能已被其它软件或用户修改）；\
                 为避免覆盖他人配置，未执行恢复。",
                current.describe(),
                change.target.describe()
            ),
            current.describe(),
        );
    }

    // 3) 分页文件：必须能确认「配置项」也能回到原状
    if kind == ChangeKind::AutomaticPagefile {
        if restore_to {
            // 恢复为自动管理：不依赖具体条目
        } else {
            if change.before_entries.is_empty() {
                return ChangeOutcome::fail(
                    "上次变更前为手动配置，但未记录到具体的分页文件项，无法可靠恢复；\
                     请在系统设置中手动配置虚拟内存。",
                    current.describe(),
                );
            }
            let (_, now_entries) = pagefile_snapshot();
            if now_entries != change.before_entries {
                return ChangeOutcome::fail(
                    "当前分页文件配置项与历史记录不一致，无法可靠恢复；\
                     请在系统设置中手动配置虚拟内存。",
                    current.describe(),
                );
            }
        }
    }

    apply_change(kind, restore_to)
}

/// 打开系统的虚拟内存（性能选项）设置界面。
///
/// 仅在无法可靠自动恢复时使用；这只是一个受支持的设置入口，不会自动改动任何配置。
pub fn open_pagefile_settings() -> Result<(), String> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let rundll = root.join("System32").join("rundll32.exe");
    let sysdm = root.join("System32").join("sysdm.cpl");
    if !rundll.exists() || !sysdm.exists() {
        return Err("找不到 system32\\rundll32.exe 或 sysdm.cpl".into());
    }
    Command::new(rundll)
        .arg(format!("{},Control_RunDLL", sysdm.display()))
        .arg(",3")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开系统虚拟内存设置：{}", e))
}

// ---------------------------------------------------------------------------
// 变更历史
// ---------------------------------------------------------------------------

/// 一次配置变更记录（结构化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub at: String,
    pub kind: String,
    /// 结构化「修改前」状态
    pub before: Snapshot,
    /// 修改前的分页文件项（仅分页文件有意义）
    pub before_entries: Vec<PageFileSetting>,
    /// 结构化「工具修改后」目标状态
    pub target: Snapshot,
    /// 修改后的分页文件项
    pub target_entries: Vec<PageFileSetting>,
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
        "at={}\tkind={}\tbefore={}\tbentries={}\ttarget={}\ttentries={}\tresult={}\tverified={}\treboot={}",
        sanitize_field(&change.at),
        change.kind,
        change.before.encode(),
        encode_settings(&change.before_entries),
        change.target.encode(),
        encode_settings(&change.target_entries),
        sanitize_field(&change.result),
        sanitize_field(&change.verified),
        if change.reboot_required { 1 } else { 0 }
    )
}

fn decode(line: &str) -> Option<ConfigChange> {
    let mut at = String::new();
    let mut kind = String::new();
    let mut before = Snapshot::Failed("缺失".into());
    let mut before_entries = Vec::new();
    let mut target = Snapshot::Failed("缺失".into());
    let mut target_entries = Vec::new();
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
            "before" => before = Snapshot::decode(v),
            "bentries" => before_entries = decode_settings(v),
            "target" => target = Snapshot::decode(v),
            "tentries" => target_entries = decode_settings(v),
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
        before_entries,
        target,
        target_entries,
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
        std::fs::create_dir_all(dir)?;
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

    fn report(automatic: Snapshot, settings: Vec<PageFileEntry>) -> PageFileReport {
        PageFileReport {
            automatic_managed: automatic,
            settings_status: QueryStatus::Ok(settings.len()),
            entries: settings,
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_states_are_not_confused_with_off() {
        assert_eq!(Snapshot::Bool(false).describe(), "已关闭");
        assert_eq!(Snapshot::Bool(true).describe(), "已开启");
        assert!(Snapshot::NotSupported.describe().contains("不支持"));
        assert!(Snapshot::Unavailable("x".into())
            .describe()
            .contains("不可用"));
        assert!(Snapshot::Failed("y".into()).describe().contains("查询失败"));
        assert!(!Snapshot::Failed("y".into()).describe().contains("已关闭"));
        // 只有 Bool 才是可靠原状态
        assert!(Snapshot::Bool(true).is_known());
        assert!(!Snapshot::NotSupported.is_known());
        assert!(!Snapshot::Failed("x".into()).is_known());
    }

    #[test]
    fn snapshot_encode_decode_roundtrip() {
        for s in [
            Snapshot::Bool(true),
            Snapshot::Bool(false),
            Snapshot::NotSupported,
            Snapshot::Unavailable("属性缺失".into()),
            Snapshot::Failed("超时".into()),
        ] {
            assert_eq!(Snapshot::decode(&s.encode()), s, "结构化状态往返失败");
        }
        // 旧格式（显示字符串）精确映射
        assert_eq!(Snapshot::decode("已开启"), Snapshot::Bool(true));
        assert_eq!(Snapshot::decode("已关闭"), Snapshot::Bool(false));
        // 旧格式里的“查询失败（...）”绝不能被当成已开启/已关闭
        let old_failed = Snapshot::decode("查询失败（超时）");
        assert!(!old_failed.is_known());
        assert_eq!(old_failed.as_bool(), None);
        // 前缀相似的字符串也不能被误判
        assert_eq!(Snapshot::decode("已开启（未验证）").as_bool(), None);
    }

    #[test]
    fn pagefile_settings_roundtrip() {
        let list = vec![
            PageFileSetting {
                path: r"C:\pagefile.sys".into(),
                initial_mb: Some(4096),
                maximum_mb: Some(8192),
            },
            PageFileSetting {
                path: r"D:\pagefile.sys".into(),
                initial_mb: None,
                maximum_mb: None,
            },
        ];
        let enc = encode_settings(&list);
        assert_eq!(decode_settings(&enc), list);
        assert!(decode_settings("").is_empty());
    }

    #[test]
    fn drive_extraction() {
        assert_eq!(drive_of(r"C:\pagefile.sys").as_deref(), Some("C:"));
        assert_eq!(drive_of(r"d:\pagefile.sys").as_deref(), Some("D:"));
        assert_eq!(drive_of("no-drive"), None);
    }

    #[test]
    fn diagnose_flags_missing_pagefile() {
        let d = diagnose(&report(Snapshot::Bool(false), vec![]), None);
        assert!(
            d.iter().any(|x| x.severity == Severity::Critical),
            "未配置分页文件应给出警告：{:?}",
            d
        );
    }

    #[test]
    fn diagnose_does_not_treat_query_failure_as_disabled() {
        let mut r = report(Snapshot::Failed("超时".into()), vec![]);
        r.settings_status = QueryStatus::Failed("超时".into());
        let d = diagnose(&r, None);
        assert!(d.iter().any(|x| x.text.contains("查询失败")));
        assert!(
            !d.iter().any(|x| x.text.contains("未检测到分页文件配置")),
            "查询失败不能被当成“没有配置分页文件”：{:?}",
            d
        );
    }

    #[test]
    fn diagnose_reports_each_cim_status_independently() {
        let mut r = report(Snapshot::Bool(true), vec![]);
        r.volume_status = QueryStatus::Failed("卷查询失败".into());
        let d = diagnose(&r, None);
        assert!(d.iter().any(|x| x.text.contains("卷空间查询失败")));
        // 自动管理状态仍然正常给出
        assert!(d.iter().any(|x| x.text.contains("系统自动管理")));
    }

    #[test]
    fn query_status_distinguishes_empty_from_failure() {
        assert!(!QueryStatus::Ok(0).is_failed());
        assert_eq!(QueryStatus::Ok(0).describe(), "成功（空结果）");
        assert!(QueryStatus::Failed("x".into()).is_failed());
    }

    #[test]
    fn diagnose_flags_commit_pressure() {
        let commit = CommitInfo {
            total: 9_000,
            limit: 10_000,
            peak: 12_000,
            page_size: 4096,
        };
        let d = diagnose(&report(Snapshot::Bool(true), vec![]), Some(commit));
        assert!(d
            .iter()
            .any(|x| x.text.contains("提交余量") || x.text.contains("提交量已达")));
        assert!(d.iter().any(|x| x.text.contains("固定不变")));
    }

    #[test]
    fn diagnose_flags_volume_space() {
        let r = PageFileReport {
            automatic_managed: Snapshot::Bool(true),
            settings_status: QueryStatus::Ok(1),
            entries: vec![entries(r"C:\pagefile.sys", 4096)],
            volumes: vec![VolumeInfo {
                device: "C:".into(),
                total_bytes: Some(100 * 1024 * 1024 * 1024),
                free_bytes: Some(1024 * 1024 * 1024),
            }],
            ..Default::default()
        };
        let d = diagnose(&r, None);
        assert!(d.iter().any(|x| x.text.contains("可用空间偏低")));
    }

    #[test]
    fn volume_query_failure_does_not_block_others() {
        let mut r = report(Snapshot::Failed("超时".into()), vec![]);
        r.settings_status = QueryStatus::Failed("超时".into());
        let d = diagnose(&r, None);
        assert!(d.iter().any(|x| x.text.contains("查询失败")));
        assert!(d.iter().any(|x| x.text.contains("数据不足")));
    }

    #[test]
    fn change_record_roundtrip() {
        let c = ConfigChange {
            at: "2026-01-01 00:00:00".into(),
            kind: "AutomaticPagefile".into(),
            before: Snapshot::Bool(false),
            before_entries: vec![PageFileSetting {
                path: r"C:\pagefile.sys".into(),
                initial_mb: Some(2048),
                maximum_mb: Some(4096),
            }],
            target: Snapshot::Bool(true),
            target_entries: vec![],
            result: "OK".into(),
            verified: "已开启".into(),
            reboot_required: false,
        };
        let line = encode(&c);
        let back = decode(&line).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn unknown_before_state_is_not_decoded_as_disabled() {
        // 模拟旧版本用显示字符串判断的错误来源：先前的 before 是查询失败
        let line = "at=1\tkind=MemoryCompression\tbefore=failed:超时\ttarget=on\tresult=OK\tverified=已开启\treboot=1";
        let c = decode(line).unwrap();
        assert_eq!(
            c.before.as_bool(),
            None,
            "查询失败的原状态不可当作可恢复目标"
        );
        assert!(c.target.as_bool().is_some());
    }

    #[test]
    fn malformed_change_line_is_ignored() {
        assert!(decode("garbage").is_none());
        assert!(decode("at=1").is_none());
    }

    #[test]
    fn reboot_state_is_never_hardcoded_to_no_reboot() {
        // 只存在「待验证」与「未知」两种，不提供“无需重启”的断言
        assert_eq!(
            ChangeKind::MemoryCompression.reboot_state(),
            RebootState::Pending
        );
        assert_eq!(
            ChangeKind::AutomaticPagefile.reboot_state(),
            RebootState::Unknown
        );
    }
}
