//! Windows 原生 API 绑定层（纯 FFI，无任何第三方 crate）。
#![allow(
    non_snake_case,
    non_camel_case_types,
    dead_code,
    clippy::upper_case_acronyms
)]

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

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

impl FILETIME {
    #[inline]
    pub fn to_u64(&self) -> u64 {
        ((self.dwHighDateTime as u64) << 32) | self.dwLowDateTime as u64
    }
}

/// 进程仍在运行时的退出码。
pub const STILL_ACTIVE: u32 = 259;

/// `CreateMemoryResourceNotification` 的通知类型。
pub const LOW_MEMORY_RESOURCE_NOTIFICATION: u32 = 0;
pub const HIGH_MEMORY_RESOURCE_NOTIFICATION: u32 = 1;

/// `QueryFullProcessImageNameW` 的 dwFlags。
pub const PROCESS_NAME_WIN32: u32 = 0;

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
    fn FreeLibrary(hLibModule: *mut c_void) -> BOOL;
    /// 查询已加载模块（不增加引用计数，无需释放）。
    fn GetModuleHandleW(lpModuleName: *const u16) -> *mut c_void;

    // 进程身份判定
    fn GetExitCodeProcess(hProcess: HANDLE, lpExitCode: *mut u32) -> BOOL;
    fn GetProcessTimes(
        hProcess: HANDLE,
        lpCreationTime: *mut FILETIME,
        lpExitTime: *mut FILETIME,
        lpKernelTime: *mut FILETIME,
        lpUserTime: *mut FILETIME,
    ) -> BOOL;
    fn QueryFullProcessImageNameW(
        hProcess: HANDLE,
        dwFlags: u32,
        lpExeName: *mut u16,
        lpdwSize: *mut u32,
    ) -> BOOL;
    fn ProcessIdToSessionId(dwProcessId: u32, pSessionId: *mut u32) -> BOOL;
    fn GetProcessIoCounters(hProcess: HANDLE, lpIoCounters: *mut IO_COUNTERS) -> BOOL;
    fn SystemParametersInfoW(
        uiAction: u32,
        uiParam: u32,
        pvParam: *mut c_void,
        fWinIni: u32,
    ) -> BOOL;
    fn GetSysColor(nIndex: i32) -> u32;
    fn RegGetValueW(
        hkey: isize,
        lpSubKey: *const u16,
        lpValue: *const u16,
        dwFlags: u32,
        pdwType: *mut u32,
        pvData: *mut c_void,
        pcbData: *mut u32,
    ) -> i32;

    // 低内存资源通知
    fn CreateMemoryResourceNotification(MemoryResourceNotificationType: u32) -> HANDLE;
    fn QueryMemoryResourceNotification(
        MemoryResourceNotificationHandle: HANDLE,
        ResourceState: *mut BOOL,
    ) -> BOOL;

    // 命名互斥体（跨实例协调，禁止并行优化）
    fn CreateMutexW(
        lpMutexAttributes: *const c_void,
        bInitialOwner: BOOL,
        lpName: *const u16,
    ) -> HANDLE;
    fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: u32) -> u32;
    fn ReleaseMutex(hMutex: HANDLE) -> BOOL;
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
    fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> BOOL;
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
    // 开机启动（当前用户 HKCU\...\Run）
    fn RegSetValueExW(
        hKey: isize,
        lpValueName: *const u16,
        Reserved: u32,
        dwType: u32,
        lpData: *const u8,
        cbData: u32,
    ) -> i32;
    fn RegDeleteValueW(hKey: isize, lpValueName: *const u16) -> i32;
    fn RegCloseKey(hKey: isize) -> i32;
    fn RegCreateKeyExW(
        hKey: isize,
        lpSubKey: *const u16,
        Reserved: u32,
        lpClass: *const u16,
        dwOptions: u32,
        samDesired: u32,
        lpSecurityAttributes: *const c_void,
        phkResult: *mut isize,
        lpdwDisposition: *mut u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct IO_COUNTERS {
    pub ReadOperationCount: u64,
    pub WriteOperationCount: u64,
    pub OtherOperationCount: u64,
    pub ReadTransferCount: u64,
    pub WriteTransferCount: u64,
    pub OtherTransferCount: u64,
}

impl IO_COUNTERS {
    /// 读写字节总数，用于计算 I/O 活跃度。
    pub fn transfer_total(&self) -> u64 {
        self.ReadTransferCount
            .saturating_add(self.WriteTransferCount)
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> HANDLE;
    fn GetWindowThreadProcessId(hWnd: HANDLE, lpdwProcessId: *mut u32) -> u32;
    fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SYSTEMTIME {
    pub wYear: u16,
    pub wMonth: u16,
    pub wDayOfWeek: u16,
    pub wDay: u16,
    pub wHour: u16,
    pub wMinute: u16,
    pub wSecond: u16,
    pub wMilliseconds: u16,
}

/// 本地时间字符串（`YYYY-MM-DD HH:MM:SS`），不受显示语言影响。
pub fn local_time_string() -> String {
    unsafe {
        let mut st = SYSTEMTIME::default();
        GetLocalTime(&mut st);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

#[repr(C)]
pub struct HIGHCONTRASTW {
    pub cbSize: u32,
    pub dwFlags: u32,
    pub lpszDefaultScheme: *mut u16,
}

pub const SPI_GETHIGHCONTRAST: u32 = 0x0042;
pub const HCF_HIGHCONTRASTON: u32 = 0x0000_0001;

// 系统颜色索引
pub const COLOR_WINDOW: i32 = 5;
pub const COLOR_WINDOWTEXT: i32 = 8;
pub const COLOR_HIGHLIGHT: i32 = 13;
pub const COLOR_BTNFACE: i32 = 15;
pub const COLOR_GRAYTEXT: i32 = 17;

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const KEY_SET_VALUE: u32 = 0x0002;
const REG_SZ: u32 = 1;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const WAIT_OBJECT_0: u32 = 0;

/// 当前用户开机启动项所在的注册表路径与值名。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "winmem-gui";

/// 高对比度是否开启（受支持的系统设置接口，不依赖 undocumented API）。
pub fn high_contrast_enabled() -> bool {
    unsafe {
        let mut hc = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            dwFlags: 0,
            lpszDefaultScheme: std::ptr::null_mut(),
        };
        if SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            &mut hc as *mut HIGHCONTRASTW as *mut c_void,
            0,
        ) == 0
        {
            return false;
        }
        hc.dwFlags & HCF_HIGHCONTRASTON != 0
    }
}

/// 读取系统颜色（高对比度下用于保证可读性）。
pub fn sys_color(index: i32) -> u32 {
    unsafe { GetSysColor(index) }
}

/// 应用是否使用浅色主题。读取失败返回 None（此时按浅色处理）。
pub fn apps_use_light_theme() -> Option<bool> {
    unsafe {
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        );
        if rc == 0 {
            Some(data != 0)
        } else {
            None
        }
    }
}

/// 当前是否应使用深色配色（高对比度开启时不使用自定义配色）。
pub fn prefer_dark() -> bool {
    if high_contrast_enabled() {
        return false;
    }
    matches!(apps_use_light_theme(), Some(false))
}

/// 当前前台窗口所属进程 PID；没有前台窗口时返回 None。
pub fn foreground_pid() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == NULL_HANDLE {
            return None;
        }
        let mut pid: u32 = 0;
        if GetWindowThreadProcessId(hwnd, &mut pid) == 0 || pid == 0 {
            None
        } else {
            Some(pid)
        }
    }
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
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
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

        let ok =
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
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
    snapshot_process_entries()
        .into_iter()
        .map(|e| (e.pid, e.threads, e.name))
        .collect()
}

/// 查询单个进程的内存计数。
/// 查询已打开句柄的内存计数（更安全的入口：句柄已由调用方校验过身份）。
pub fn memory_counters(h: HANDLE) -> Result<PROCESS_MEMORY_COUNTERS_EX, WinError> {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::new();
        if GetProcessMemoryInfo(h, &mut counters, counters.cb) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(counters)
    }
}

/// 打开进程并查询内存计数（便捷版）。
pub fn process_memory(pid: u32) -> Result<PROCESS_MEMORY_COUNTERS_EX, WinError> {
    let h = open_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    memory_counters(h.raw())
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
/// 结束**已校验身份**的句柄所对应的进程。
pub fn terminate_handle(h: HANDLE) -> Result<(), WinError> {
    unsafe {
        if TerminateProcess(h, 1) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(())
    }
}

/// 通过 NtSetSystemInformation 清空系统待机列表（Standby List）。
/// 需要 SeProfileSingleProcessPrivilege 特权。
pub fn purge_standby_list() -> Result<(), i32> {
    const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 0x50;
    const MEMORY_PURGE_STANDBY_LIST: u32 = 4;

    let Some(ntdll) = OwnedModule::load("ntdll.dll") else {
        return Err(-1);
    };
    let addr = ntdll.proc("NtSetSystemInformation");
    if addr.is_null() {
        return Err(-2);
    }
    let func: unsafe extern "system" fn(i32, *mut c_void, u32) -> i32 =
        unsafe { std::mem::transmute(addr) };
    let mut command = MEMORY_PURGE_STANDBY_LIST;
    let status = unsafe {
        func(
            SYSTEM_MEMORY_LIST_INFORMATION,
            &mut command as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        Err(status)
    }
}

// ---------------------------------------------------------------------------
// 特权与低内存通知
// ---------------------------------------------------------------------------

/// 已启用的特权，Drop 时恢复启用前的状态。
pub struct PrivilegeGuard {
    token: OwnedHandle,
    previous: TOKEN_PRIVILEGES,
}

impl PrivilegeGuard {
    /// 启用指定特权；返回 None 表示进程没有该特权或启用失败。
    pub fn enable(name: &str) -> Option<PrivilegeGuard> {
        unsafe {
            let token = {
                let mut t: HANDLE = NULL_HANDLE;
                if OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut t,
                ) == 0
                {
                    return None;
                }
                OwnedHandle::new(t)?
            };

            let mut luid = LUID::default();
            if LookupPrivilegeValueW(std::ptr::null(), wide(name).as_ptr(), &mut luid) == 0 {
                return None;
            }

            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut ret_len: u32 = 0;
            let ok = AdjustTokenPrivileges(
                token.raw(),
                0,
                &tp,
                std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut previous,
                &mut ret_len,
            );
            // 必须紧跟 AdjustTokenPrivileges 读取错误码，之后不能再调用其他 API
            let err = GetLastError();
            if ok == 0 || err == ERROR_NOT_ALL_ASSIGNED {
                return None;
            }
            Some(PrivilegeGuard { token, previous })
        }
    }
}

impl Drop for PrivilegeGuard {
    fn drop(&mut self) {
        if self.previous.PrivilegeCount == 0 {
            return;
        }
        unsafe {
            AdjustTokenPrivileges(
                self.token.raw(),
                0,
                &self.previous,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }
}

/// 低内存资源通知（`CreateMemoryResourceNotification`）。
/// 这是系统提供的受支持接口，用于判断是否处于低内存状态，无需轮询猜测。
pub struct MemoryResourceNotification(OwnedHandle);

impl MemoryResourceNotification {
    pub fn low_memory() -> Option<MemoryResourceNotification> {
        unsafe {
            let h = CreateMemoryResourceNotification(LOW_MEMORY_RESOURCE_NOTIFICATION);
            OwnedHandle::new(h).map(MemoryResourceNotification)
        }
    }

    /// 当前是否处于低内存状态；接口失败时返回 None（而非伪造成 false）。
    pub fn is_low(&self) -> Option<bool> {
        unsafe {
            let mut state: BOOL = 0;
            if QueryMemoryResourceNotification(self.0.raw(), &mut state) == 0 {
                None
            } else {
                Some(state != 0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 错误模型
// ---------------------------------------------------------------------------

pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_NOT_FOUND: u32 = 1168;

/// 操作失败原因分类。上层据此区分权限不足、进程已退出、接口不支持等。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinError {
    /// 打开进程/句柄被拒绝，多为权限不足或受保护进程。
    AccessDenied(u32),
    /// 进程不存在或已退出。
    ProcessGone(u32),
    /// 当前系统不支持该接口（例如 Win7 上的 IsProcessCritical）。
    NotSupported,
    /// 其他 Win32 / NTSTATUS 错误。
    Os(u32),
}

impl WinError {
    pub fn from_code(code: u32) -> WinError {
        match code {
            ERROR_ACCESS_DENIED => WinError::AccessDenied(code),
            ERROR_INVALID_PARAMETER | ERROR_NOT_FOUND => WinError::ProcessGone(code),
            _ => WinError::Os(code),
        }
    }

    pub fn code(&self) -> u32 {
        match self {
            WinError::AccessDenied(c) | WinError::ProcessGone(c) | WinError::Os(c) => *c,
            WinError::NotSupported => 0,
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            WinError::AccessDenied(_) => "权限不足",
            WinError::ProcessGone(_) => "进程已退出",
            WinError::NotSupported => "当前系统不支持该接口",
            WinError::Os(_) => "系统调用失败",
        }
    }
}

impl std::fmt::Display for WinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WinError::AccessDenied(c) => write!(f, "权限不足（错误码 {}）", c),
            WinError::ProcessGone(c) => write!(f, "进程已退出（错误码 {}）", c),
            WinError::NotSupported => write!(f, "当前系统不支持该接口"),
            WinError::Os(c) => write!(f, "系统错误（错误码 {}）", c),
        }
    }
}

// ---------------------------------------------------------------------------
// RAII 资源管理
// ---------------------------------------------------------------------------

/// 自有句柄，Drop 时自动 `CloseHandle`。
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// 接管句柄；传入 0 / INVALID_HANDLE_VALUE 时返回 None。
    pub fn new(h: HANDLE) -> Option<OwnedHandle> {
        if h == NULL_HANDLE || h == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(OwnedHandle(h))
        }
    }

    #[inline]
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// 交出所有权，返回裸句柄（调用方负责关闭）。
    #[inline]
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// 动态加载的模块（Drop 时 `FreeLibrary`，避免引用计数泄漏）。
pub struct OwnedModule(*mut c_void);

impl OwnedModule {
    pub fn load(name: &str) -> Option<OwnedModule> {
        let h = unsafe { LoadLibraryW(wide(name).as_ptr()) };
        if h.is_null() {
            None
        } else {
            Some(OwnedModule(h))
        }
    }

    /// 取导出函数地址。返回的指针在 `self` 存活期间有效。
    pub fn proc(&self, name: &str) -> *mut c_void {
        let mut buf: Vec<u8> = name.bytes().collect();
        buf.push(0);
        unsafe { GetProcAddress(self.0, buf.as_ptr()) }
    }
}

impl Drop for OwnedModule {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// 进程身份（PID 会被复用，必须结合创建时间）
// ---------------------------------------------------------------------------

/// 进程身份。PID 会被系统复用，因此必须同时记录创建时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// 进程创建时间（FILETIME，100ns 单位）
    pub creation_time: u64,
}

impl ProcessIdentity {
    /// 打开进程并读取身份（只需 QUERY_LIMITED_INFORMATION）。
    pub fn query(pid: u32) -> Result<ProcessIdentity, WinError> {
        let h = open_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let creation_time = creation_time_of(h.raw())?;
        Ok(ProcessIdentity { pid, creation_time })
    }

    /// 校验已打开的句柄是否仍指向同一进程（创建时间一致）。
    pub fn matches(&self, h: HANDLE) -> bool {
        matches!(creation_time_of(h), Ok(t) if t == self.creation_time)
    }
}

/// 打开进程句柄，失败时立即保存错误码并分类。
pub fn open_handle(pid: u32, access: u32) -> Result<OwnedHandle, WinError> {
    unsafe {
        let h = OpenProcess(access, 0, pid);
        if let Some(h) = OwnedHandle::new(h) {
            Ok(h)
        } else {
            Err(WinError::from_code(GetLastError()))
        }
    }
}

/// 读取进程创建时间。
pub fn creation_time_of(h: HANDLE) -> Result<u64, WinError> {
    unsafe {
        let mut c = FILETIME::default();
        let mut e = FILETIME::default();
        let mut k = FILETIME::default();
        let mut u = FILETIME::default();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(c.to_u64())
    }
}

/// 读取进程 CPU 时间（内核 + 用户，100ns 单位），用于计算 CPU 占用差分。
pub fn cpu_time_of(h: HANDLE) -> Result<u64, WinError> {
    unsafe {
        let mut c = FILETIME::default();
        let mut e = FILETIME::default();
        let mut k = FILETIME::default();
        let mut u = FILETIME::default();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(k.to_u64().saturating_add(u.to_u64()))
    }
}

/// 读取进程 I/O 计数器，用于判断是否处于活跃 I/O。
pub fn io_counters(h: HANDLE) -> Result<IO_COUNTERS, WinError> {
    unsafe {
        let mut c = IO_COUNTERS::default();
        if GetProcessIoCounters(h, &mut c) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(c)
    }
}

/// 进程是否仍在运行。
pub fn is_process_alive(h: HANDLE) -> Result<bool, WinError> {
    unsafe {
        let mut code: u32 = 0;
        if GetExitCodeProcess(h, &mut code) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(code == STILL_ACTIVE)
    }
}

type IsCriticalFn = unsafe extern "system" fn(HANDLE, *mut BOOL) -> BOOL;

/// 动态解析 `IsProcessCritical`（Win8.1+ 才有；Win7 上返回 None 由调用方降级）。
fn is_critical_entry() -> Option<IsCriticalFn> {
    static ENTRY: std::sync::OnceLock<Option<IsCriticalFn>> = std::sync::OnceLock::new();
    *ENTRY.get_or_init(|| unsafe {
        let module = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if module.is_null() {
            return None;
        }
        let p = GetProcAddress(module, c"IsProcessCritical".as_ptr() as *const u8);
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, IsCriticalFn>(p))
        }
    })
}

/// 系统是否把该进程标记为关键进程。
/// 系统不支持该接口时返回 `Err(NotSupported)`，调用方需降级处理。
pub fn is_process_critical(h: HANDLE) -> Result<bool, WinError> {
    let Some(f) = is_critical_entry() else {
        return Err(WinError::NotSupported);
    };
    unsafe {
        let mut crit: BOOL = 0;
        if f(h, &mut crit) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(crit != 0)
    }
}

/// 进程可执行文件完整路径。
pub fn process_path(h: HANDLE) -> Result<String, WinError> {
    unsafe {
        let mut buf = vec![0u16; 32768];
        let mut size = buf.len() as u32;
        if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size) == 0 {
            return Err(WinError::from_code(GetLastError()));
        }
        Ok(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// 进程所属会话 ID。
pub fn process_session(pid: u32) -> Option<u32> {
    unsafe {
        let mut sid: u32 = 0;
        if ProcessIdToSessionId(pid, &mut sid) != 0 {
            Some(sid)
        } else {
            None
        }
    }
}

/// 当前进程所属会话 ID。
pub fn current_session() -> Option<u32> {
    process_session(current_pid())
}

/// 快照中的一条进程记录。
#[derive(Debug, Clone)]
pub struct ProcEntry {
    pub pid: u32,
    pub parent_pid: u32,
    pub threads: u32,
    pub name: String,
}

/// 枚举全部进程（含父进程 PID，可用于构建应用分组）。
pub fn snapshot_process_entries() -> Vec<ProcEntry> {
    let mut out = Vec::with_capacity(256);
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if let Some(snap) = OwnedHandle::new(snap) {
            let mut entry = PROCESSENTRY32W::default();
            if Process32FirstW(snap.raw(), &mut entry) != 0 {
                loop {
                    out.push(ProcEntry {
                        pid: entry.th32ProcessID,
                        parent_pid: entry.th32ParentProcessID,
                        threads: entry.cntThreads,
                        name: utf16_to_string(&entry.szExeFile),
                    });
                    if Process32NextW(snap.raw(), &mut entry) == 0 {
                        break;
                    }
                }
            }
        }
    }
    out
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

// ---------------------------------------------------------------------------
// 跨实例协调：命名互斥体
// ---------------------------------------------------------------------------

/// 优化协调用的命名互斥体名称（本机会话范围，不跨用户）。
pub const OPTIMIZE_MUTEX: &str = "Local\\winmem-optimize-guard";

/// 命名互斥体。持有期间其它实例无法获取，用于**禁止多个实例并行优化**。
pub struct NamedMutex {
    handle: OwnedHandle,
    owned: bool,
}

impl NamedMutex {
    /// 尝试获取命名互斥体；已被其它进程/线程持有时返回 `None`，调用方应跳过本次操作。
    pub fn try_acquire(name: &str) -> Option<NamedMutex> {
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr());
            let handle = OwnedHandle::new(h)?;
            if WaitForSingleObject(handle.raw(), 0) == WAIT_OBJECT_0 {
                Some(NamedMutex {
                    handle,
                    owned: true,
                })
            } else {
                None
            }
        }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                ReleaseMutex(self.handle.raw());
            }
            self.owned = false;
        }
    }
}

// ---------------------------------------------------------------------------
// 开机启动（当前用户 HKCU\...\Run）
// ---------------------------------------------------------------------------

/// 期望写入运行项的启动命令。GUI 以「最小化到托盘」方式启动，路径含空格时用引号包裹。
pub fn expected_autostart_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --minimized", exe.display()))
}

/// 读取当前用户运行项的值；未设置时返回 `Ok(None)`。
pub fn autostart_value() -> Result<Option<String>, WinError> {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut size = buf.len() as u32;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(RUN_VALUE).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        );
        if rc == 0 {
            if size < 2 {
                return Ok(Some(String::new()));
            }
            let w: &[u16] =
                std::slice::from_raw_parts(buf.as_ptr() as *const u16, size as usize / 2);
            let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
            Ok(Some(String::from_utf16_lossy(&w[..end])))
        } else if rc as u32 == ERROR_FILE_NOT_FOUND {
            Ok(None)
        } else {
            Err(WinError::from_code(rc as u32))
        }
    }
}

/// 当前用户的开机启动是否已按**本程序期望的路径与参数**注册。
///
/// 指向其它路径时返回 `false`（不声称已开启），避免产生「假开关」。
pub fn autostart_enabled() -> Result<bool, WinError> {
    let expected = expected_autostart_command();
    match autostart_value()? {
        None => Ok(false),
        Some(v) => Ok(expected
            .map(|e| e.eq_ignore_ascii_case(&v))
            .unwrap_or(false)),
    }
}

/// 注册 / 撤销当前用户的开机启动，并在写入后**读回验证实际状态**。
///
/// 只写 HKCU，不需要管理员权限，也不需要整个程序以管理员运行。
pub fn set_autostart(enabled: bool) -> Result<(), WinError> {
    unsafe {
        let mut key: isize = 0;
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return Err(WinError::from_code(rc as u32));
        }
        let rc = if enabled {
            match expected_autostart_command() {
                Some(cmd) => {
                    let data = wide(&cmd);
                    RegSetValueExW(
                        key,
                        wide(RUN_VALUE).as_ptr(),
                        0,
                        REG_SZ,
                        data.as_ptr() as *const u8,
                        (data.len() * 2) as u32,
                    )
                }
                None => {
                    RegCloseKey(key);
                    return Err(WinError::NotSupported);
                }
            }
        } else {
            let r = RegDeleteValueW(key, wide(RUN_VALUE).as_ptr());
            if r as u32 == ERROR_FILE_NOT_FOUND {
                0
            } else {
                r
            }
        };
        RegCloseKey(key);
        if rc != 0 {
            return Err(WinError::from_code(rc as u32));
        }
    }
    // 写回后读回验证，不满足期望则报告失败
    if autostart_enabled()? != enabled {
        return Err(WinError::Os(0));
    }
    Ok(())
}
