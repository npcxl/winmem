//! Windows 原生 API 绑定层（纯 FFI，无任何第三方 crate）。
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// 基础类型
// ---------------------------------------------------------------------------

pub type HANDLE = isize;
pub type BOOL = i32;
pub type DWORD = u32;

pub const INVALID_HANDLE_VALUE: HANDLE = -1;
pub const NULL_HANDLE: HANDLE = 0;

// 进程访问权限
pub const PROCESS_TERMINATE: u32 = 0x0001;
pub const PROCESS_SET_QUOTA: u32 = 0x0100;
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

// 快照
pub const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

// 令牌权限
pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
pub const TOKEN_QUERY: u32 = 0x0008;
pub const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
pub const TOKEN_ELEVATION_CLASS: u32 = 20;

// 标准句柄
pub const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

pub const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;

// ---------------------------------------------------------------------------
// 结构体
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct PROCESSENTRY32W {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ProcessID: u32,
    pub th32DefaultHeapID: usize,
    pub th32ModuleID: u32,
    pub cntThreads: u32,
    pub th32ParentProcessID: u32,
    pub pcPriClassBase: i32,
    pub dwFlags: u32,
    pub szExeFile: [u16; 260],
}

impl Default for PROCESSENTRY32W {
    fn default() -> Self {
        PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            cntUsage: 0,
            th32ProcessID: 0,
            th32DefaultHeapID: 0,
            th32ModuleID: 0,
            cntThreads: 0,
            th32ParentProcessID: 0,
            pcPriClassBase: 0,
            dwFlags: 0,
            szExeFile: [0u16; 260],
        }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct PROCESS_MEMORY_COUNTERS_EX {
    pub cb: u32,
    pub PageFaultCount: u32,
    pub PeakWorkingSetSize: usize,
    pub WorkingSetSize: usize,
    pub QuotaPeakPagedPoolUsage: usize,
    pub QuotaPagedPoolUsage: usize,
    pub QuotaPeakNonPagedPoolUsage: usize,
    pub QuotaNonPagedPoolUsage: usize,
    pub PagefileUsage: usize,
    pub PeakPagefileUsage: usize,
    /// 私有提交量（最贴近“真实占用”的指标）
    pub PrivateUsage: usize,
}

impl PROCESS_MEMORY_COUNTERS_EX {
    pub fn new() -> Self {
        PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct MEMORYSTATUSEX {
    pub dwLength: u32,
    pub dwMemoryLoad: u32,
    pub ullTotalPhys: u64,
    pub ullAvailPhys: u64,
    pub ullTotalPageFile: u64,
    pub ullAvailPageFile: u64,
    pub ullTotalVirtual: u64,
    pub ullAvailVirtual: u64,
    pub ullAvailExtendedVirtual: u64,
}

impl MEMORYSTATUSEX {
    pub fn new() -> Self {
        MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct PERFORMANCE_INFORMATION {
    pub cb: u32,
    pub CommitTotal: usize,
    pub CommitLimit: usize,
    pub CommitPeak: usize,
    pub PhysicalTotal: usize,
    pub PhysicalAvailable: usize,
    pub SystemCache: usize,
    pub KernelTotal: usize,
    pub KernelPaged: usize,
    pub KernelNonpaged: usize,
    pub PageSize: usize,
    pub HandleCount: u32,
    pub ProcessCount: u32,
    pub ThreadCount: u32,
}

impl PERFORMANCE_INFORMATION {
    pub fn new() -> Self {
        PERFORMANCE_INFORMATION {
            cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID {
    pub LowPart: u32,
    pub HighPart: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID_AND_ATTRIBUTES {
    pub Luid: LUID,
    pub Attributes: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TOKEN_PRIVILEGES {
    pub PrivilegeCount: u32,
    pub Privileges: [LUID_AND_ATTRIBUTES; 1],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TOKEN_ELEVATION {
    pub TokenIsElevated: u32,
}

// ---------------------------------------------------------------------------
// 外部函数
// ---------------------------------------------------------------------------

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> HANDLE;
    fn Process32FirstW(hSnapshot: HANDLE, lppe: *mut PROCESSENTRY32W) -> BOOL;
    fn Process32NextW(hSnapshot: HANDLE, lppe: *mut PROCESSENTRY32W) -> BOOL;
    pub fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: BOOL, dwProcessId: u32) -> HANDLE;
    fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub fn TerminateProcess(hProcess: HANDLE, uExitCode: u32) -> BOOL;
    fn GetCurrentProcess() -> HANDLE;
    pub fn GetCurrentProcessId() -> u32;
    pub fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> BOOL;
    fn GetLastError() -> u32;
    fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
    fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    fn GetStdHandle(nStdHandle: u32) -> HANDLE;
    fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut u32) -> BOOL;
    fn SetConsoleMode(hConsoleHandle: HANDLE, dwMode: u32) -> BOOL;
}

#[link(name = "psapi")]
unsafe extern "system" {
    pub fn GetProcessMemoryInfo(
        hProcess: HANDLE,
        ppsmemCounters: *mut PROCESS_MEMORY_COUNTERS_EX,
        cb: u32,
    ) -> BOOL;
    pub fn EmptyWorkingSet(hProcess: HANDLE) -> BOOL;
    fn GetPerformanceInfo(pPerformanceInformation: *mut PERFORMANCE_INFORMATION, cb: u32) -> BOOL;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(ProcessHandle: HANDLE, DesiredAccess: u32, TokenHandle: *mut HANDLE) -> BOOL;
    fn LookupPrivilegeValueW(
        lpSystemName: *const u16,
        lpName: *const u16,
        lpLuid: *mut LUID,
    ) -> BOOL;
    fn AdjustTokenPrivileges(
        TokenHandle: HANDLE,
        DisableAllPrivileges: BOOL,
        NewState: *const TOKEN_PRIVILEGES,
        BufferLength: u32,
        PreviousState: *mut TOKEN_PRIVILEGES,
        ReturnLength: *mut u32,
    ) -> BOOL;
    fn GetTokenInformation(
        TokenHandle: HANDLE,
        TokenInformationClass: u32,
        TokenInformation: *mut c_void,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> BOOL;
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// 开启 ANSI 转义序列（彩色输出），老终端失败时静默忽略。
pub fn enable_ansi() {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h == NULL_HANDLE || h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            let _ = SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// 判断当前进程是否以管理员（提权）身份运行。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = NULL_HANDLE;
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION_CLASS,
            &mut elev as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

/// 尝试为当前进程启用指定特权，返回是否成功。
pub fn enable_privilege(name: &str) -> bool {
    unsafe {
        let mut token: HANDLE = NULL_HANDLE;
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return false;
        }

        let mut luid = LUID::default();
        if LookupPrivilegeValueW(std::ptr::null(), wide(name).as_ptr(), &mut luid) == 0 {
            CloseHandle(token);
            return false;
        }

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };

        let ok = AdjustTokenPrivileges(
            token,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let err = GetLastError();
        CloseHandle(token);
        ok != 0 && err != ERROR_NOT_ALL_ASSIGNED
    }
}

/// 系统内存总览。
pub fn memory_status() -> Option<MEMORYSTATUSEX> {
    let mut ms = MEMORYSTATUSEX::new();
    if unsafe { GlobalMemoryStatusEx(&mut ms) } != 0 {
        Some(ms)
    } else {
        None
    }
}

/// 系统级性能信息（提交量、系统缓存、进程/线程数）。
pub fn performance_info() -> Option<PERFORMANCE_INFORMATION> {
    let mut pi = PERFORMANCE_INFORMATION::new();
    if unsafe { GetPerformanceInfo(&mut pi, pi.cb) } != 0 {
        Some(pi)
    } else {
        None
    }
}

/// 枚举系统中全部 (pid, 线程数, 进程名)。
pub fn snapshot_processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::with_capacity(256);
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry = PROCESSENTRY32W::default();
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let name = utf16_to_string(&entry.szExeFile);
                out.push((entry.th32ProcessID, entry.cntThreads, name));
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// 查询单个进程的内存计数。
pub fn process_memory(pid: u32) -> Option<PROCESS_MEMORY_COUNTERS_EX> {
    unsafe {
        let mut h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == NULL_HANDLE {
            h = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        }
        if h == NULL_HANDLE {
            return None;
        }
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::new();
        let ok = GetProcessMemoryInfo(h, &mut counters, counters.cb);
        CloseHandle(h);
        if ok != 0 {
            Some(counters)
        } else {
            None
        }
    }
}

/// 打开进程（可指定访问权限）。
pub fn open_process(pid: u32, access: u32) -> HANDLE {
    unsafe { OpenProcess(access, 0, pid) }
}

pub fn close_handle(h: HANDLE) {
    if h != NULL_HANDLE && h != INVALID_HANDLE_VALUE {
        unsafe {
            CloseHandle(h);
        }
    }
}

/// 强制结束进程。返回 Ok(()) 或错误码。
pub fn terminate_process(pid: u32) -> Result<(), u32> {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h == NULL_HANDLE {
            return Err(GetLastError());
        }
        let ok = TerminateProcess(h, 1);
        let err = GetLastError();
        CloseHandle(h);
        if ok != 0 {
            Ok(())
        } else {
            Err(err)
        }
    }
}

/// 通过 NtSetSystemInformation 清空系统待机列表（Standby List）。
/// 需要 SeProfileSingleProcessPrivilege 特权。
pub fn purge_standby_list() -> Result<(), i32> {
    const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 0x50;
    const MEMORY_PURGE_STANDBY_LIST: u32 = 4;

    unsafe {
        let ntdll = LoadLibraryW(wide("ntdll.dll").as_ptr());
        if ntdll.is_null() {
            return Err(-1);
        }
        let addr = GetProcAddress(ntdll, b"NtSetSystemInformation\0".as_ptr());
        if addr.is_null() {
            return Err(-2);
        }
        let func: unsafe extern "system" fn(i32, *mut c_void, u32) -> i32 =
            std::mem::transmute(addr);
        let mut command = MEMORY_PURGE_STANDBY_LIST;
        let status = func(
            SYSTEM_MEMORY_LIST_INFORMATION,
            &mut command as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if status >= 0 {
            Ok(())
        } else {
            Err(status)
        }
    }
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
