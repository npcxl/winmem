//! winmem-gui —— winmem 的 Windows 原生图形界面。
//!
//! 纯 Win32 + GDI 手写界面，零第三方依赖：
//! - 顶部：内存总览（使用率进度条 + 提交/缓存/进程统计）
//! - 中部：进程内存栈列表（可点列头排序、可筛选、可多选）
//! - 工具栏：刷新 / 一键优化 / 释放选中内存 / 结束选中进程 / 全选 / 取消选择
//! - 底部：状态栏（操作结果 + 进度 + 权限提示）

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![cfg_attr(not(target_pointer_width = "64"), allow(unused))]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("winmem-gui 仅支持 64 位 Windows。");

mod w32;

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use winmem::config::Config;
use winmem::engine::{AutoState, AutoStatus, CancelToken, Engine, RunOptions};
use winmem::metrics;
use winmem::policy::{self, Decision, Purpose};
use winmem::report;
use winmem::term;
use winmem::win as wapi;

/// 列表排序方式。
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Mem,
    Private,
    Name,
    Pid,
}

/// 列表中的一行。
#[derive(Clone)]
struct Row {
    pid: u32,
    name: String,
    working_set: Option<u64>,
    private_bytes: Option<u64>,
    threads: u32,
    /// 策略判定结果文本：可优化 / 仅手动 / 禁止操作 / 本程序 / 无权读取
    state: &'static str,
    /// 硬保护（核心系统进程等），任何操作都不可绕过
    hard_blocked: bool,
    /// 进程身份（PID + 创建时间），执行前必须核验
    identity: Option<wapi::ProcessIdentity>,
}

impl Row {
    fn ws(&self) -> u64 {
        self.working_set.unwrap_or(0)
    }
    fn priv_bytes(&self) -> u64 {
        self.private_bytes.unwrap_or(0)
    }
}

/// 自动优化会话句柄。丢弃它不会中断线程：必须显式 `cancel()`。
struct AutoHandle {
    cancel: CancelToken,
    status: Arc<Mutex<AutoStatus>>,
}

/// 距上次采样超过该时长（毫秒）时重新建立差分基线。
///
/// 复用旧基线时 CPU 百分比是「自上次采样以来的平均值」，间隔太长会失去参考意义；
/// 上限设为 30 秒，兼顾开销与指标时效性。
const SAMPLER_REUSE_MS: u64 = 30_000;

/// 建立差分基线时的等待时长（毫秒）：太短则差分噪声大。
const BASELINE_GAP_MS: u64 = 300;

/// 按统一策略把进程指标标注为界面行。
fn rows_from(list: Vec<metrics::ProcessMetrics>, cfg: &Config) -> Vec<Row> {
    let self_pid = wapi::current_pid();
    let self_session = wapi::current_session();
    let fg = wapi::foreground_pid();

    list.into_iter()
        .map(|m| {
            let verdict = policy::decide(&policy::PolicyInput {
                facts: &policy::ProcessFacts {
                    pid: m.pid,
                    name: m.name.clone(),
                    session: m.session,
                    working_set: m.working_set.unwrap_or(0),
                    private_bytes: m.private_bytes.unwrap_or(0),
                    os_critical: m.os_critical,
                    last_foreground_secs: None,
                    cpu_percent: m.cpu_percent,
                    cooling: false,
                    is_self: m.pid == self_pid,
                },
                config: cfg,
                self_session,
                foreground_pid: fg,
                purpose: Purpose::Auto,
            });
            let hard_blocked = matches!(verdict.decision, Decision::Blocked(_));
            let state = if !m.readable {
                "无权读取"
            } else if m.pid == self_pid {
                "本程序"
            } else {
                match verdict.decision {
                    Decision::Candidate => "可优化",
                    Decision::Skip(_) => "仅手动",
                    Decision::Blocked(_) => "禁止操作",
                }
            };
            Row {
                pid: m.pid,
                name: m.name,
                working_set: m.working_set,
                private_bytes: m.private_bytes,
                threads: m.threads,
                state,
                hard_blocked,
                identity: m.creation_time.map(|t| wapi::ProcessIdentity {
                    pid: m.pid,
                    creation_time: t,
                }),
            }
        })
        .collect()
}

/// 最近一次进程采集的耗时（毫秒），供 `--bench` 报告使用。
static LAST_COLLECT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------------------
// 外观
// ---------------------------------------------------------------------------

/// 界面配色。跟随系统主题（浅色 / 深色）与高对比度设置。
#[derive(Clone, Copy, PartialEq)]
struct Palette {
    bg: u32,
    panel: u32,
    text: u32,
    dim: u32,
    border: u32,
    track: u32,
    green: u32,
    yellow: u32,
    red: u32,
    blue: u32,
}

impl Palette {
    /// Windows 11 浅色
    const LIGHT: Palette = Palette {
        bg: w32::rgb(243, 243, 243),
        panel: w32::rgb(255, 255, 255),
        text: w32::rgb(26, 26, 26),
        dim: w32::rgb(117, 117, 117),
        border: w32::rgb(229, 229, 229),
        track: w32::rgb(229, 229, 229),
        green: w32::rgb(16, 137, 62),
        yellow: w32::rgb(202, 138, 4),
        red: w32::rgb(196, 43, 28),
        blue: w32::rgb(0, 95, 184),
    };

    /// Windows 11 深色
    const DARK: Palette = Palette {
        bg: w32::rgb(32, 32, 32),
        panel: w32::rgb(43, 43, 43),
        text: w32::rgb(240, 240, 240),
        dim: w32::rgb(160, 160, 160),
        border: w32::rgb(60, 60, 60),
        track: w32::rgb(60, 60, 60),
        green: w32::rgb(76, 194, 116),
        yellow: w32::rgb(230, 180, 60),
        red: w32::rgb(240, 110, 95),
        blue: w32::rgb(90, 160, 240),
    };

    /// 高对比度：完全采用系统颜色，保证可读性（不使用自定义配色）。
    fn high_contrast() -> Palette {
        Palette {
            bg: wapi::sys_color(wapi::COLOR_BTNFACE),
            panel: wapi::sys_color(wapi::COLOR_WINDOW),
            text: wapi::sys_color(wapi::COLOR_WINDOWTEXT),
            dim: wapi::sys_color(wapi::COLOR_GRAYTEXT),
            border: wapi::sys_color(wapi::COLOR_BTNFACE),
            track: wapi::sys_color(wapi::COLOR_BTNFACE),
            green: wapi::sys_color(wapi::COLOR_WINDOWTEXT),
            yellow: wapi::sys_color(wapi::COLOR_WINDOWTEXT),
            red: wapi::sys_color(wapi::COLOR_WINDOWTEXT),
            blue: wapi::sys_color(wapi::COLOR_HIGHLIGHT),
        }
    }

    /// 根据当前系统设置选择配色。
    fn load() -> Palette {
        if wapi::high_contrast_enabled() {
            Palette::high_contrast()
        } else if wapi::prefer_dark() {
            Palette::DARK
        } else {
            Palette::LIGHT
        }
    }

    fn pressure(&self, pct: f64) -> u32 {
        if pct >= 90.0 {
            self.red
        } else if pct >= 70.0 {
            self.yellow
        } else {
            self.green
        }
    }
}

// 布局
const TOP_H: i32 = 96;
const TOOLBAR_H: i32 = 44;
const STATUS_H: i32 = 58;
/// 页面标签高度（「进程」/「系统内存」）
const TAB_H: i32 = 30;

/// 当前显示的页面。
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Procs,
    Sysmem,
}

// 控件 ID
const ID_BTN_REFRESH: usize = 1001;
const ID_BTN_OPTIMIZE: usize = 1002;
const ID_BTN_TRIM: usize = 1003;
const ID_BTN_KILL: usize = 1004;
const ID_BTN_PREVIEW: usize = 1009;

/// 智能自动优化开关（复选框）
const ID_CHK_AUTO: usize = 1010;

// 右键菜单项
const ID_MENU_SYSMEM: usize = 2006;
/// 编辑保护名单 / 自动优化允许名单
const ID_MENU_LISTS: usize = 2007;
const ID_MENU_SEL_ALL: usize = 2008;
const ID_MENU_SEL_NONE: usize = 2009;
/// 检查更新（热更新）
const ID_MENU_UPDATE: usize = 2010;
const ID_MENU_TRIM: usize = 2001;
const ID_MENU_KILL: usize = 2002;
const ID_MENU_PREVIEW: usize = 2003;
const ID_MENU_CONFIG: usize = 2004;
const ID_MENU_STANDBY: usize = 2005;
const ID_BTN_SELECT_ALL: usize = 1005;
const ID_BTN_CLEAR_SEL: usize = 1006;
const ID_EDIT_FILTER: usize = 1007;
const ID_LISTVIEW: usize = 1008;
/// 取消正在执行的后台任务
const ID_BTN_CANCEL: usize = 1017;

// 页面标签与「系统内存」页控件
const ID_TAB: usize = 1011;
const ID_EDIT_SYSMEM: usize = 1012;
const ID_BTN_SYSMEM_REFRESH: usize = 1013;
const ID_BTN_PF_SETTINGS: usize = 1014;
const ID_BTN_SYSMEM_RESTORE: usize = 1015;

// 托盘菜单项
const ID_TRAY_SHOW: usize = 3001;
const ID_TRAY_PAUSE: usize = 3002;
const ID_TRAY_RESUME: usize = 3003;
const ID_TRAY_EXIT: usize = 3004;

// 名单编辑对话框
const LISTS_CLASS: &str = "winmem_gui_lists";
const ID_LISTS_OK: usize = 1;
const ID_LISTS_CANCEL: usize = 2;

/// 名单编辑对话框的结果槽：确定时为 `(允许名单, 保护名单)` 的原始输入。
type ListsResult = Arc<Mutex<Option<(String, String)>>>;

/// 把「逗号 / 中文逗号 / 分号」分隔的输入解析为规范化名单。
fn parse_list(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in s.split([',', '，', ';', '\n']) {
        let n = policy::normalize_name(part);
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 400;
/// 窗口不可见（最小化 / 收进托盘）时的刷新间隔
const TIMER_MS_HIDDEN: u32 = 2000;

/// 列定义：(标题, 宽度, 右对齐)
const COLUMNS: [(&str, i32, bool); 6] = [
    ("PID", 72, true),
    ("进程名", 262, false),
    ("工作集", 110, true),
    ("私有提交", 110, true),
    ("线程", 64, true),
    ("状态", 100, false),
];

// 工具栏：刷新 / 智能优化 / 预览候选 / 释放选中内存 / 结束选中进程 / 取消
// （「全选 / 取消选择」是列表操作，已移入右键菜单，为「取消」腾出位置）
const BUTTON_WIDTHS: [i32; 6] = [78, 106, 96, 134, 134, 78];

/// 「系统内存」页按钮宽度：刷新诊断 / 打开虚拟内存设置 / 恢复上次配置
const SYSMEM_BUTTON_WIDTHS: [i32; 3] = [88, 196, 132];

// ---------------------------------------------------------------------------
// 后台任务消息
// ---------------------------------------------------------------------------

enum Task {
    Progress {
        done: usize,
        total: usize,
    },
    Done {
        text: String,
        kind: u8,
        refresh: bool,
    },
    /// 系统内存诊断结果（写入只读文本区，不改变状态行）
    SysmemText(String),
    /// 检查更新结束：发现新版本（`manual` 为 true 时弹窗询问是否安装）
    UpdateAvailable {
        info: Box<winmem::update::ReleaseInfo>,
        manual: bool,
    },
    /// 检查更新结束：已是最新（自动检查时静默）
    UpdateUpToDate {
        latest: String,
        manual: bool,
    },
    /// 检查更新失败
    UpdateCheckFailed {
        reason: String,
        manual: bool,
    },
    /// 更新包已下载、校验并替换完成
    UpdateInstalled {
        report: Box<winmem::update::ApplyReport>,
    },
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct AppState {
    hwnd: w32::HWND,
    hlist: w32::HWND,
    hfilter: w32::HWND,
    buttons: Vec<w32::HWND>,
    /// 「取消」按钮：任务执行期间反而是可用的
    hbtn_cancel: w32::HWND,
    /// 正在执行的后台任务；`Some` 时界面处于忙碌态
    task_cancel: Option<CancelToken>,
    /// 常驻进程采样器：跨刷新保留差分基线，
    /// 避免每次刷新都对全部进程做两遍高成本查询
    sampler: metrics::ProcessSampler,
    /// 上一次进程采样的单调毫秒时刻
    sampler_at_ms: Option<u64>,
    /// 页面标签（进程 / 系统内存）
    htab: w32::HWND,
    /// 「系统内存」页的只读诊断文本区
    hsymem: w32::HWND,
    /// 「系统内存」页的操作按钮
    sysmem_buttons: Vec<w32::HWND>,
    /// 当前页面
    view: View,
    /// 托盘图标是否已注册
    tray_added: bool,
    /// 名单编辑对话框的结果槽；对话框关闭后取回
    lists_result: Option<ListsResult>,

    /// 本进程启动时的命令行参数（重启时原样带上）
    start_args: Vec<String>,
    /// 启动时的自动更新检查是否已经跑过（每会话只跑一次）
    auto_update_checked: bool,
    /// 正在下载安装更新
    updating: bool,

    font_ui: w32::HFONT,
    font_title: w32::HFONT,
    font_small: w32::HFONT,
    /// 高 DPI 缩放系数（96 DPI = 1.0）
    scale: f32,

    /// 全部进程（未过滤）
    all: Vec<Row>,
    /// 当前显示的顺序
    rows: Vec<Row>,
    sort: SortBy,
    filter: String,
    /// 当前配色（跟随系统主题与高对比度）
    palette: Palette,
    /// 用户配置（保护名单 / 允许名单）
    config: Config,
    /// 智能自动优化会话；None 表示未运行
    auto: Option<AutoHandle>,
    hchk_auto: w32::HWND,
    /// 最近一次读取到的自动优化状态
    auto_state: AutoState,
    auto_msg: String,

    mem: Option<wapi::MEMORYSTATUSEX>,
    perf: Option<wapi::PERFORMANCE_INFORMATION>,

    /// 第一行：最近一次操作的结果
    status: String,
    status_kind: u8,
    /// 第二行：列表统计
    stats: String,
    busy: bool,
    progress: Option<(usize, usize)>,
    ticks: u32,

    tx: Sender<Task>,
    rx: Receiver<Task>,
}

impl AppState {
    fn new() -> AppState {
        let (tx, rx) = mpsc::channel();
        AppState {
            hwnd: 0,
            hlist: 0,
            hfilter: 0,
            buttons: Vec::new(),
            hbtn_cancel: 0,
            task_cancel: None,
            sampler: metrics::ProcessSampler::new(),
            sampler_at_ms: None,
            htab: 0,
            hsymem: 0,
            sysmem_buttons: Vec::new(),
            view: View::Procs,
            tray_added: false,
            lists_result: None,
            start_args: Vec::new(),
            auto_update_checked: false,
            updating: false,
            font_ui: 0,
            font_title: 0,
            font_small: 0,
            scale: 1.0,
            all: Vec::new(),
            rows: Vec::new(),
            sort: SortBy::Mem,
            filter: String::new(),
            config: Config::load(),
            palette: Palette::load(),
            auto: None,
            hchk_auto: 0,
            auto_state: AutoState::Off,
            auto_msg: String::new(),
            mem: None,
            perf: None,
            status: String::new(),
            status_kind: 0,
            stats: String::new(),
            busy: false,
            progress: None,
            ticks: 0,
            tx,
            rx,
        }
    }

    /// 设计尺寸 → 实际像素（按系统 DPI 缩放）。
    #[inline]
    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    fn set_status(&mut self, text: &str, kind: u8) {
        self.status = text.to_string();
        self.status_kind = kind;
    }

    fn status_color(&self) -> u32 {
        match self.status_kind {
            1 => self.palette.green,
            2 => self.palette.yellow,
            3 => self.palette.red,
            _ => self.palette.text,
        }
    }

    // -- 创建控件 ---------------------------------------------------------

    unsafe fn create_controls(&mut self, hwnd: w32::HWND, hinst: w32::HINSTANCE) {
        self.hwnd = hwnd;
        self.scale = w32::system_dpi() as f32 / 96.0;
        self.font_ui = w32::make_font(-self.px(14), w32::FW_NORMAL);
        self.font_title = w32::make_font(-self.px(21), w32::FW_SEMIBOLD);
        self.font_small = w32::make_font(-self.px(12), w32::FW_NORMAL);

        // 进程列表
        let class_lv = w32::w("SysListView32");
        let empty = w32::w("");
        let lv = w32::CreateWindowExW(
            w32::WS_EX_CLIENTEDGE,
            class_lv.as_ptr(),
            empty.as_ptr(),
            w32::WS_CHILD
                | w32::WS_VISIBLE
                | w32::WS_TABSTOP
                | w32::LVS_REPORT
                | w32::LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            hwnd,
            ID_LISTVIEW as w32::HMENU,
            hinst,
            std::ptr::null_mut(),
        );
        w32::SendMessageW(
            lv,
            w32::LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (w32::LVS_EX_FULLROWSELECT
                | w32::LVS_EX_GRIDLINES
                | w32::LVS_EX_DOUBLEBUFFER
                | w32::LVS_EX_LABELTIP) as w32::LPARAM,
        );
        w32::set_font(lv, self.font_ui);
        for (i, (title, width, right)) in COLUMNS.iter().enumerate() {
            w32::lv_add_column(lv, i as i32, title, self.px(*width), *right);
        }
        // 略微加大行高，更贴近 Win11 资源管理器的疏朗感
        let spacing = (((self.px(26) as u32) << 16) | self.px(60) as u32) as w32::LPARAM;
        w32::SendMessageW(lv, w32::LVM_SETICONSPACING, 0, spacing);
        self.hlist = lv;

        // 工具栏按钮
        let class_btn = w32::w("BUTTON");
        let labels = [
            (ID_BTN_REFRESH, "刷新"),
            (ID_BTN_OPTIMIZE, "智能优化"),
            (ID_BTN_PREVIEW, "预览候选"),
            (ID_BTN_TRIM, "释放选中内存"),
            (ID_BTN_KILL, "结束选中进程"),
            (ID_BTN_CANCEL, "取消"),
        ];
        for (id, text) in labels {
            let t = w32::w(text);
            let h = w32::CreateWindowExW(
                0,
                class_btn.as_ptr(),
                t.as_ptr(),
                w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::BS_PUSHBUTTON,
                0,
                0,
                0,
                0,
                hwnd,
                id as w32::HMENU,
                hinst,
                std::ptr::null_mut(),
            );
            w32::set_font(h, self.font_ui);
            if id == ID_BTN_CANCEL {
                self.hbtn_cancel = h;
                // 空闲时不可用，任务开始后才启用
                w32::EnableWindow(h, 0);
            }
            self.buttons.push(h);
        }

        // 智能自动优化开关
        let chk_text = w32::w("智能自动优化");
        let chk = w32::CreateWindowExW(
            0,
            class_btn.as_ptr(),
            chk_text.as_ptr(),
            w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::BS_AUTOCHECKBOX,
            0,
            0,
            0,
            0,
            hwnd,
            ID_CHK_AUTO as w32::HMENU,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(chk, self.font_ui);
        w32::SendMessageW(
            chk,
            w32::BM_SETCHECK,
            if self.config.auto_enabled {
                w32::BST_CHECKED
            } else {
                w32::BST_UNCHECKED
            },
            0,
        );
        self.hchk_auto = chk;

        // 过滤输入框
        let class_edit = w32::w("EDIT");
        let hint = w32::w("");
        let edit = w32::CreateWindowExW(
            0,
            class_edit.as_ptr(),
            hint.as_ptr(),
            w32::WS_CHILD
                | w32::WS_VISIBLE
                | w32::WS_TABSTOP
                | w32::WS_BORDER
                | w32::ES_AUTOHSCROLL,
            0,
            0,
            0,
            0,
            hwnd,
            ID_EDIT_FILTER as w32::HMENU,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(edit, self.font_ui);
        let cue = w32::w("筛选进程名…");
        w32::SendMessageW(edit, w32::EM_SETCUEBANNER, 1, cue.as_ptr() as w32::LPARAM);
        self.hfilter = edit;

        // ---- 页面标签（系统原生 Tab 控件）----
        let class_tab = w32::w("SysTabControl32");
        let tab = w32::CreateWindowExW(
            0,
            class_tab.as_ptr(),
            empty.as_ptr(),
            w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            hwnd,
            ID_TAB as w32::HMENU,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(tab, self.font_ui);
        w32::tab_add(tab, 0, "进程");
        w32::tab_add(tab, 1, "系统内存");
        self.htab = tab;

        // ---- 「系统内存」页：只读诊断文本区（原生多行只读编辑框）----
        let sym = w32::CreateWindowExW(
            w32::WS_EX_CLIENTEDGE,
            class_edit.as_ptr(),
            empty.as_ptr(),
            w32::WS_CHILD
                | w32::WS_VSCROLL
                | w32::ES_MULTILINE
                | w32::ES_AUTOVSCROLL
                | w32::ES_READONLY,
            0,
            0,
            0,
            0,
            hwnd,
            ID_EDIT_SYSMEM as w32::HMENU,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(sym, self.font_ui);
        self.hsymem = sym;

        // ---- 「系统内存」页：手动操作按钮（都不进入自动流程）----
        for (id, text) in [
            (ID_BTN_SYSMEM_REFRESH, "刷新诊断"),
            (ID_BTN_PF_SETTINGS, "打开 Windows 虚拟内存设置"),
            (ID_BTN_SYSMEM_RESTORE, "恢复上次配置…"),
        ] {
            let t = w32::w(text);
            let h = w32::CreateWindowExW(
                0,
                class_btn.as_ptr(),
                t.as_ptr(),
                w32::WS_CHILD | w32::WS_TABSTOP | w32::BS_PUSHBUTTON,
                0,
                0,
                0,
                0,
                hwnd,
                id as w32::HMENU,
                hinst,
                std::ptr::null_mut(),
            );
            w32::set_font(h, self.font_ui);
            self.sysmem_buttons.push(h);
        }

        // 初始页面：进程
        self.apply_view();

        // 托盘图标（常驻；最小化 / 关闭到托盘后仍可暂停、恢复与退出）
        self.add_tray_icon();
    }

    // -- 托盘 -------------------------------------------------------------

    unsafe fn add_tray_icon(&mut self) {
        if self.tray_added {
            return;
        }
        let mut data = w32::NOTIFYICONDATAW::zeroed();
        data.cbSize = std::mem::size_of::<w32::NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = w32::NIF_MESSAGE | w32::NIF_ICON | w32::NIF_TIP;
        data.uCallbackMessage = w32::WM_APP_TRAY;
        data.hIcon = w32::LoadIconW(0, w32::IDI_APPLICATION as *const u16);
        data.set_tip("winmem —— 内存优化工具");
        if w32::Shell_NotifyIconW(w32::NIM_ADD, &mut data) != 0 {
            self.tray_added = true;
        }
    }

    unsafe fn remove_tray_icon(&mut self) {
        if !self.tray_added {
            return;
        }
        let mut data = w32::NOTIFYICONDATAW::zeroed();
        data.cbSize = std::mem::size_of::<w32::NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        w32::Shell_NotifyIconW(w32::NIM_DELETE, &mut data);
        self.tray_added = false;
    }

    /// 托盘右键菜单：打开主窗口 / 暂停 / 恢复 / 退出。
    unsafe fn show_tray_menu(&mut self) {
        let menu = w32::CreatePopupMenu();
        if menu == 0 {
            return;
        }
        let running = self.auto.is_some();
        let items: [(usize, &str, bool); 5] = [
            (ID_TRAY_SHOW, "打开主窗口", true),
            (0, "", true),
            (ID_TRAY_PAUSE, "暂停智能自动优化", running),
            // 暂停后配置里的开关会被置为 false，因此「恢复」只要当前没在跑就可用
            (ID_TRAY_RESUME, "恢复智能自动优化", !running),
            (ID_TRAY_EXIT, "退出 winmem", true),
        ];
        for (id, text, enabled) in items {
            if text.is_empty() {
                w32::AppendMenuW(menu, w32::MF_SEPARATOR, 0, std::ptr::null());
                continue;
            }
            let t = w32::w(text);
            let flags = if enabled {
                w32::MF_STRING
            } else {
                w32::MF_STRING | w32::MF_GRAYED
            };
            w32::AppendMenuW(menu, flags, id, t.as_ptr());
        }
        w32::SetForegroundWindow(self.hwnd);
        let mut pt = w32::POINT::default();
        w32::GetCursorPos(&mut pt);
        let cmd = w32::TrackPopupMenu(
            menu,
            w32::TPM_RETURNCMD | w32::TPM_RIGHTBUTTON | w32::TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            self.hwnd,
            std::ptr::null(),
        );
        w32::DestroyMenu(menu);

        match cmd as usize {
            ID_TRAY_SHOW => self.restore_window(),
            ID_TRAY_PAUSE => self.pause_auto(),
            ID_TRAY_RESUME => self.resume_auto(),
            ID_TRAY_EXIT => {
                self.remove_tray_icon();
                w32::DestroyWindow(self.hwnd);
            }
            _ => {}
        }
    }

    /// 打开「保护名单 / 自动优化允许名单」编辑窗口（系统原生控件）。
    fn op_edit_lists(&mut self) {
        if self.lists_result.is_some() {
            // 已经打开，避免重复创建
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        self.lists_result = Some(slot.clone());
        unsafe {
            let hinst = w32::GetModuleHandleW(std::ptr::null());
            let font = w32::make_font(-((14.0 * self.scale).round() as i32), w32::FW_NORMAL);
            let ctx = Box::into_raw(Box::new(ListsCtx {
                parent: self.hwnd,
                allow: 0,
                protect: 0,
                font,
                result: slot.clone(),
                allow_init: self.config.allow.join(", "),
                protect_init: self.config.protect.join(", "),
            }));
            let class_name = w32::w(LISTS_CLASS);
            let title = w32::w("保护名单 / 自动优化允许名单");
            let hwnd = w32::CreateWindowExW(
                w32::WS_EX_DLGMODALFRAME,
                class_name.as_ptr(),
                title.as_ptr(),
                w32::WS_POPUP | w32::WS_CAPTION | w32::WS_SYSMENU | w32::WS_VISIBLE,
                w32::CW_USEDEFAULT,
                w32::CW_USEDEFAULT,
                (560.0 * self.scale).round() as i32,
                (280.0 * self.scale).round() as i32,
                self.hwnd,
                0,
                hinst,
                ctx as *mut c_void,
            );
            if hwnd == 0 {
                // 创建失败：立即回收，避免泄漏
                let _ = Box::from_raw(ctx);
                self.lists_result = None;
                self.set_status("无法打开名单编辑窗口。", 3);
                return;
            }
            w32::EnableWindow(self.hwnd, 0);
        }
    }

    /// 取回名单对话框的结果并保存（结果为空表示用户取消）。
    fn apply_lists_result(&mut self) {
        let taken = match &self.lists_result {
            Some(slot) => slot.lock().ok().and_then(|mut g| g.take()),
            None => None,
        };
        self.lists_result = None;
        let Some((allow, protect)) = taken else {
            self.set_status("已取消名单编辑。", 0);
            return;
        };
        self.config.allow = parse_list(&allow);
        self.config.protect = parse_list(&protect);
        match self.config.save() {
            Ok(_) => self.set_status(
                &format!(
                    "名单已保存：允许 {} 项、保护 {} 项（自动优化会话会自动读取新配置）。",
                    self.config.allow.len(),
                    self.config.protect.len()
                ),
                1,
            ),
            Err(e) => self.set_status(&format!("保存配置失败：{}", e), 3),
        }
        self.reload();
    }

    /// 从托盘 / 最小化恢复窗口。
    unsafe fn restore_window(&mut self) {
        w32::ShowWindow(self.hwnd, w32::SW_RESTORE);
        w32::SetForegroundWindow(self.hwnd);
        w32::SetTimer(self.hwnd, TIMER_ID, TIMER_MS, std::ptr::null());
    }

    /// 收进托盘：隐藏窗口并降低刷新频率。
    unsafe fn hide_to_tray(&mut self) {
        w32::ShowWindow(self.hwnd, w32::SW_HIDE);
        w32::SetTimer(self.hwnd, TIMER_ID, TIMER_MS_HIDDEN, std::ptr::null());
    }

    // -- 布局 -------------------------------------------------------------

    unsafe fn layout(&self) {
        let mut rc = w32::RECT::default();
        w32::GetClientRect(self.hwnd, &mut rc);
        let w = rc.width();
        let h = rc.height();

        let by = self.px(TOP_H) + self.px(8);
        let bh = self.px(28);
        let mut x = self.px(16);
        for (i, btn) in self.buttons.iter().enumerate() {
            let bw = self.px(BUTTON_WIDTHS[i]);
            w32::MoveWindow(*btn, x, by, bw, bh, 1);
            x += bw + self.px(8);
        }
        w32::MoveWindow(
            self.hchk_auto,
            x + self.px(10),
            by + self.px(5),
            self.px(140),
            self.px(22),
            1,
        );
        w32::MoveWindow(
            self.hfilter,
            w - self.px(240),
            by + 1,
            self.px(224),
            self.px(26),
            1,
        );

        // 页面标签
        let ty = self.px(TOP_H) + self.px(TOOLBAR_H) - self.px(4);
        let th = self.px(TAB_H);
        w32::MoveWindow(
            self.htab,
            self.px(10),
            ty,
            (w - self.px(20)).max(self.px(200)),
            th,
            1,
        );

        let cy = ty + th + self.px(6);
        let cw = (w - self.px(24)).max(self.px(200));
        if self.view == View::Sysmem {
            let mut bx = self.px(12);
            for (i, b) in self.sysmem_buttons.iter().enumerate() {
                let bw = self.px(SYSMEM_BUTTON_WIDTHS[i]);
                w32::MoveWindow(*b, bx, cy, bw, self.px(28), 1);
                bx += bw + self.px(8);
            }
            let ey = cy + self.px(36);
            let eh = (h - ey - self.px(STATUS_H) - self.px(8)).max(self.px(80));
            w32::MoveWindow(self.hsymem, self.px(12), ey, cw, eh, 1);
        } else {
            let lh = (h - cy - self.px(STATUS_H) - self.px(8)).max(self.px(80));
            w32::MoveWindow(self.hlist, self.px(12), cy, cw, lh, 1);
        }
    }

    /// 切换「进程」/「系统内存」页面并同步可见性。
    unsafe fn apply_view(&mut self) {
        let procs = self.view == View::Procs;
        let show = |v| if v { w32::SW_SHOW } else { w32::SW_HIDE };
        w32::ShowWindow(self.hlist, show(procs));
        w32::ShowWindow(self.hfilter, show(procs));
        w32::ShowWindow(self.hsymem, show(!procs));
        for b in self.sysmem_buttons.iter() {
            w32::ShowWindow(*b, show(!procs));
        }
        w32::SendMessageW(self.htab, w32::TCM_SETCURSEL, if procs { 0 } else { 1 }, 0);
        self.layout();
    }

    /// 切到「系统内存」页并启动一次后台诊断（不阻塞 UI）。
    fn switch_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        unsafe {
            self.apply_view();
        }
        if view == View::Sysmem {
            self.start_sysmem_diag();
        }
    }

    /// 处理 Per-Monitor V2 的 DPI 变化：重建字体、调整列宽（随后由调用方重新布局）。
    unsafe fn apply_dpi(&mut self, dpi: u32) {
        let new_scale = dpi as f32 / 96.0;
        if (new_scale - self.scale).abs() < 0.01 {
            return;
        }
        self.scale = new_scale;

        for f in [self.font_ui, self.font_title, self.font_small] {
            if f != 0 {
                w32::DeleteObject(f);
            }
        }
        self.font_ui = w32::make_font(-self.px(14), w32::FW_NORMAL);
        self.font_title = w32::make_font(-self.px(21), w32::FW_SEMIBOLD);
        self.font_small = w32::make_font(-self.px(12), w32::FW_NORMAL);

        for h in self.buttons.iter() {
            w32::set_font(*h, self.font_ui);
        }
        w32::set_font(self.hlist, self.font_ui);
        w32::set_font(self.hfilter, self.font_ui);
        w32::set_font(self.hchk_auto, self.font_ui);

        for (i, (_, width, _)) in COLUMNS.iter().enumerate() {
            w32::SendMessageW(
                self.hlist,
                w32::LVM_SETCOLUMNWIDTH,
                i,
                self.px(*width) as w32::LPARAM,
            );
        }
    }

    // -- 数据刷新 ---------------------------------------------------------

    fn refresh_memory(&mut self) {
        self.mem = wapi::memory_status();
        self.perf = wapi::performance_info();
    }

    /// 重新枚举进程并刷新列表（不影响第一行操作结果）。
    fn reload(&mut self) {
        // 重新读取配置，便于用户编辑保护名单后直接刷新生效
        self.config = Config::load();
        self.refresh_memory();
        let cfg = self.config.clone();
        self.all = self.collect_now(&cfg);
        self.rebuild();
    }

    /// 采集一次进程指标。
    ///
    /// 复用上一次采样作为差分基线时只做**一遍**全量枚举——这是 pord 明确要求的
    /// 「避免每次刷新反复对全部进程进行高成本查询」。基线缺失或已过期时，
    /// 才额外采一次基线并等待 [`BASELINE_GAP_MS`]，以保证活动指标可靠。
    fn collect_now(&mut self, cfg: &Config) -> Vec<Row> {
        let t = std::time::Instant::now();
        let stale = match self.sampler_at_ms {
            Some(at) => metrics::monotonic_ms().saturating_sub(at) > SAMPLER_REUSE_MS,
            None => true,
        };
        if stale {
            let _ = self.sampler.collect();
            std::thread::sleep(std::time::Duration::from_millis(BASELINE_GAP_MS));
        }
        let list = self.sampler.collect();
        self.sampler_at_ms = Some(metrics::monotonic_ms());
        LAST_COLLECT_MS.store(
            t.elapsed().as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        rows_from(list, cfg)
    }

    /// 根据当前筛选与排序重建列表，并更新第二行统计信息。
    fn rebuild(&mut self) {
        let selected: Vec<u32> = self.selected_procs().iter().map(|p| p.pid).collect();
        let filter = if self.filter.trim().is_empty() {
            None
        } else {
            Some(self.filter.trim().to_string())
        };
        let mut rows: Vec<Row> = match filter {
            Some(f) => {
                let f = f.to_ascii_lowercase();
                self.all
                    .iter()
                    .filter(|r| r.name.to_ascii_lowercase().contains(&f) || r.pid.to_string() == f)
                    .cloned()
                    .collect()
            }
            None => self.all.clone(),
        };
        match self.sort {
            SortBy::Mem => rows.sort_by_key(|r| std::cmp::Reverse(r.ws())),
            SortBy::Private => rows.sort_by_key(|r| std::cmp::Reverse(r.priv_bytes())),
            SortBy::Name => rows.sort_by(|a, b| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            }),
            SortBy::Pid => rows.sort_by_key(|r| r.pid),
        }
        self.rows = rows;

        unsafe {
            // 记录滚动位置，刷新后恢复，避免列表跳动
            let top_index = w32::lv_top_index(self.hlist);
            // 批量填充期间关闭重绘：否则每插入一个单元格都会触发一次列表重绘，
            // 几百行时开销非常明显（实测约占总刷新时间的六成以上）。
            w32::SendMessageW(self.hlist, w32::WM_SETREDRAW, 0, 0);
            w32::lv_clear(self.hlist);
            for (i, p) in self.rows.iter().enumerate() {
                let cells = vec![
                    p.pid.to_string(),
                    p.name.clone(),
                    p.working_set
                        .map(term::fmt_bytes)
                        .unwrap_or_else(|| report::UNKNOWN.to_string()),
                    p.private_bytes
                        .map(term::fmt_bytes)
                        .unwrap_or_else(|| report::UNKNOWN.to_string()),
                    p.threads.to_string(),
                    p.state.to_string(),
                ];
                w32::lv_add_row(self.hlist, i as i32, &cells);
            }
            for pid in selected {
                if let Some(idx) = self.rows.iter().position(|r| r.pid == pid) {
                    w32::lv_select_row(self.hlist, idx as i32, true);
                }
            }
            w32::SendMessageW(self.hlist, w32::WM_SETREDRAW, 1, 0);
            w32::lv_ensure_visible(self.hlist, top_index);
            w32::InvalidateRect(self.hlist, std::ptr::null(), 1);
        }

        let sum: u64 = self.rows.iter().map(|p| p.ws()).sum();
        let sort_name = match self.sort {
            SortBy::Mem => "工作集",
            SortBy::Private => "私有内存",
            SortBy::Name => "进程名",
            SortBy::Pid => "PID",
        };
        self.stats = format!(
            "共 {} 个进程，显示 {} 个，工作集合计 {}（按{}排序，点列头可切换）",
            self.all.len(),
            self.rows.len(),
            term::fmt_bytes(sum),
            sort_name
        );
    }

    fn selected_procs(&self) -> Vec<Row> {
        unsafe {
            w32::lv_selected_rows(self.hlist)
                .into_iter()
                .filter_map(|i| self.rows.get(i as usize).cloned())
                .collect()
        }
    }

    // -- 任务调度 ---------------------------------------------------------

    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        unsafe {
            for b in self.buttons.iter() {
                // 「取消」按钮只在「确实可取消的任务」执行期间可用
                let on = if *b == self.hbtn_cancel {
                    self.task_cancel.is_some()
                } else {
                    !busy
                };
                w32::EnableWindow(*b, if on { 1 } else { 0 });
            }
            for b in self.sysmem_buttons.iter() {
                w32::EnableWindow(*b, if busy { 0 } else { 1 });
            }
        }
    }

    /// 全选当前列表中的进程。
    fn select_all(&mut self) {
        let n = self.rows.len() as i32;
        unsafe {
            for i in 0..n {
                w32::lv_select_row(self.hlist, i, true);
            }
            w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
        self.set_status(&format!("已选中 {} 个进程。", n), 0);
    }

    /// 取消列表选择。
    fn clear_selection(&mut self) {
        unsafe {
            w32::lv_clear_selection(self.hlist);
            w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
        self.set_status("已取消选择。", 0);
    }

    /// 取消正在执行的后台任务（引擎会在批间 / 观察 / 等待阶段检查取消标志）。
    fn op_cancel(&mut self) {
        match &self.task_cancel {
            Some(t) => {
                t.cancel();
                self.set_status(
                    "已请求取消，正在停止剩余操作…（已发生的工作集裁剪无法撤销）",
                    2,
                );
            }
            None => self.set_status("当前没有正在执行的任务。", 0),
        }
        unsafe {
            w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 启动一个后台任务。
    ///
    /// `cancellable` 为 false 时（例如一次性的只读查询）不提供取消，
    /// 「取消」按钮保持不可用，避免给出「点了会停」的错误暗示。
    fn spawn_task<F>(&mut self, label: &str, cancellable: bool, f: F)
    where
        F: FnOnce(Sender<Task>, CancelToken) + Send + 'static,
    {
        if self.busy {
            return;
        }
        let cancel = CancelToken::new();
        self.task_cancel = if cancellable {
            Some(cancel.clone())
        } else {
            None
        };
        self.set_status(label, 0);
        self.progress = Some((0, 0));
        self.set_busy(true);
        let tx = self.tx.clone();
        std::thread::spawn(move || f(tx, cancel));
    }

    fn pump_tasks(&mut self) {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(Task::Progress { done, total }) => {
                    self.progress = Some((done, total));
                    self.set_status(&format!("处理中… {}/{}", done, total), 0);
                    changed = true;
                }
                Ok(Task::Done {
                    text,
                    kind,
                    refresh,
                }) => {
                    self.progress = None;
                    self.task_cancel = None;
                    self.set_busy(false);
                    self.set_status(&text, kind);
                    if refresh {
                        self.reload();
                    }
                    changed = true;
                }
                Ok(Task::SysmemText(text)) => {
                    unsafe {
                        w32::set_text(self.hsymem, &text);
                    }
                    changed = true;
                }
                // 更新相关的处理必须在 UI 线程（可能弹窗）
                Ok(Task::UpdateAvailable { info, manual }) => {
                    self.on_update_available(*info, manual);
                    changed = true;
                }
                Ok(Task::UpdateUpToDate { latest, manual }) => {
                    self.on_update_uptodate(&latest, manual);
                    changed = true;
                }
                Ok(Task::UpdateCheckFailed { reason, manual }) => {
                    self.on_update_failed(&reason, manual);
                    changed = true;
                }
                Ok(Task::UpdateInstalled { report }) => {
                    self.finish_update(*report);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        if changed {
            unsafe {
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
        }
    }

    // -- 操作 -------------------------------------------------------------

    fn op_optimize(&mut self) {
        let cfg = self.config.clone();
        self.spawn_task(
            "正在评估内存压力并按统一策略处理…",
            true,
            move |tx, cancel| {
                let mut engine = Engine::new(cfg);
                let outcome = engine.run(&RunOptions::default(), &cancel, &mut |p| {
                    let _ = tx.send(Task::Progress {
                        done: p.done,
                        total: p.total,
                    });
                });

                let obs = &outcome.observation;
                let summary = report::History::summarize(&outcome);
                let mut history = report::History::default();
                history.push(&summary);
                history.flush();

                let text = format!(
                    "{}（成功 {} 个进程，工作集减少 {}，可用内存变化 {}）",
                    outcome.finished,
                    obs.trimmed_count,
                    term::fmt_bytes(obs.reduction_total),
                    obs.avail_delta()
                        .map(term::fmt_delta)
                        .unwrap_or_else(|| "未知".into())
                );
                let _ = tx.send(Task::Done {
                    text,
                    kind: 1,
                    refresh: true,
                });
            },
        );
    }

    /// 预览候选与跳过原因（不执行）。
    /// 从共享状态同步自动优化进度；会话结束后释放句柄。
    fn pump_auto(&mut self) {
        let finished = match &self.auto {
            Some(a) => match a.status.lock() {
                Ok(s) => {
                    self.auto_state = s.state;
                    self.auto_msg = s.message.clone();
                    !s.running
                }
                Err(_) => false,
            },
            None => false,
        };
        if finished {
            self.auto = None;
            self.auto_state = AutoState::Off;
            self.auto_msg = "自动优化已停止".into();
            unsafe {
                w32::SendMessageW(self.hchk_auto, w32::BM_SETCHECK, w32::BST_UNCHECKED, 0);
            }
        }
    }

    /// 切换「智能自动优化」开关。
    fn op_toggle_auto(&mut self) {
        let checked = unsafe {
            w32::SendMessageW(self.hchk_auto, w32::BM_GETCHECK, 0, 0) as usize == w32::BST_CHECKED
        };

        if !checked {
            // 关闭：立即取消剩余任务（不撤销已经完成的工作集裁剪）
            if let Some(a) = self.auto.take() {
                a.cancel.cancel();
            }
            self.auto_state = AutoState::Off;
            self.auto_msg = "已关闭".into();
            self.config.auto_enabled = false;
            let _ = self.config.save();
            self.set_status(
                "智能自动优化已关闭，剩余任务已取消（已发生的工作集裁剪无法撤销）。",
                0,
            );
            return;
        }

        // 首次开启：展示说明
        if !self.config.auto_intro_shown {
            unsafe {
                let msg = "智能自动优化说明：\n\n\
                    · 后台监测内存，仅在可用物理内存低于 10% 并持续 30 秒时，\n\
                      对允许名单中的闲置后台应用执行工作集裁剪；\n\
                    · 不会结束进程、不会清空待机列表、不会修改任何系统配置；\n\
                    · 工作集裁剪只是把不活跃页面换出，不能修复内存泄漏，\n\
                      也不能保证完全没有影响；\n\
                    · 允许名单为空时只监测和提示，不做任何操作。\n\n\
                    确定开启吗？";
                let t = w32::w(msg);
                let cap = w32::w("开启智能自动优化");
                if w32::MessageBoxW(
                    self.hwnd,
                    t.as_ptr(),
                    cap.as_ptr(),
                    w32::MB_YESNO | w32::MB_ICONINFORMATION,
                ) != w32::IDYES
                {
                    w32::SendMessageW(self.hchk_auto, w32::BM_SETCHECK, w32::BST_UNCHECKED, 0);
                    return;
                }
            }
            self.config.auto_intro_shown = true;
        }

        self.config.auto_enabled = true;
        let _ = self.config.save();

        if self.config.allow.is_empty() {
            unsafe {
                let msg = "允许名单为空：程序只会监测和提示内存压力，不会执行任何操作。\n\n\
                    是否现在打开配置文件添加允许的进程？";
                let t = w32::w(msg);
                let cap = w32::w("允许名单为空");
                if w32::MessageBoxW(
                    self.hwnd,
                    t.as_ptr(),
                    cap.as_ptr(),
                    w32::MB_YESNO | w32::MB_ICONINFORMATION,
                ) == w32::IDYES
                {
                    self.op_open_config();
                }
            }
        }

        self.start_auto_session();
        self.set_status("智能自动优化已开启（后台监测中，不阻塞界面）。", 1);
    }

    /// 启动后台自动会话（开关与开机启动两条路径共用同一实现）。
    ///
    /// 不做任何弹窗：开机启动静默驻留时也走这里。
    fn start_auto_session(&mut self) {
        if self.auto.is_some() {
            return;
        }
        let cancel = CancelToken::new();
        let status = Arc::new(Mutex::new(AutoStatus::default()));
        let mut engine = Engine::new(self.config.clone());
        let c2 = cancel.clone();
        let s2 = status.clone();
        let hwnd = self.hwnd;
        std::thread::spawn(move || {
            winmem::engine::run_auto_session(&mut engine, &c2, &s2, &mut |_e| unsafe {
                w32::PostMessageW(hwnd, w32::WM_APP_AUTO, 0, 0);
            });
        });
        self.auto = Some(AutoHandle { cancel, status });
        self.auto_state = AutoState::Monitoring;
        self.auto_msg = "正在监测内存压力".into();
    }

    /// 组装「系统内存」只读诊断文本。
    ///
    /// 只做查询，**不会修改任何系统设置**；状态语义严格区分
    /// 「已开启 / 已关闭 / 不支持 / 不可用 / 查询失败」。
    fn build_sysmem_report() -> String {
        let mem = wapi::memory_status();
        let commit = winmem::sysmem::commit_info();
        let comp = winmem::sysmem::probe_compression();
        let pf = winmem::sysmem::probe_page_files();

        let mut s = String::new();
        s.push_str("【只读诊断，不会修改任何系统设置】\n\n");
        if let Some(m) = mem {
            let used = m.ullTotalPhys.saturating_sub(m.ullAvailPhys);
            s.push_str(&format!(
                "物理内存：{} / {}（可用 {}）\n",
                term::fmt_bytes(used),
                term::fmt_bytes(m.ullTotalPhys),
                term::fmt_bytes(m.ullAvailPhys)
            ));
        }
        match commit {
            Some(c) => s.push_str(&format!(
                "提交量：{} / {}（{:.1}%，剩余 {}）\n",
                term::fmt_bytes(c.total),
                term::fmt_bytes(c.limit),
                c.ratio().unwrap_or(0.0) * 100.0,
                c.headroom()
                    .map(term::fmt_bytes)
                    .unwrap_or_else(|| "未知".into())
            )),
            None => s.push_str("提交量：未知\n"),
        }
        s.push_str(&format!(
            "内存压缩：{}\n（部分页面压缩后留在物理内存，可减少磁盘换入，但需要 CPU 处理）\n",
            winmem::sysmem::describe_bool(&comp.memory_compression)
        ));
        s.push_str(&format!(
            "页面合并：{}\n（合并内容相同的物理页面，收益取决于负载，存在处理开销）\n",
            winmem::sysmem::describe_bool(&comp.page_combining)
        ));
        s.push_str(&format!(
            "分页文件：{}\n",
            match &pf.automatic_managed {
                winmem::sysmem::Snapshot::Bool(true) => "系统自动管理".to_string(),
                winmem::sysmem::Snapshot::Bool(false) => "手动配置".to_string(),
                // 不支持 / 不可用 / 查询失败：保持原语义展示，绝不当成「已关闭」
                other => other.describe(),
            }
        ));
        for e in &pf.entries {
            s.push_str(&format!(
                "  {}   已分配 {} MB   使用 {} MB   峰值 {} MB\n",
                e.path,
                e.allocated_mb
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                e.current_usage_mb
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                e.peak_usage_mb
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into())
            ));
        }
        for v in &pf.volumes {
            if let (Some(free), Some(total)) = (v.free_bytes, v.total_bytes) {
                s.push_str(&format!(
                    "  {} 卷可用 {} / {}\n",
                    v.device,
                    term::fmt_bytes(free),
                    term::fmt_bytes(total)
                ));
            }
        }
        s.push_str("\n诊断：\n");
        for d in winmem::sysmem::diagnose(&pf, commit) {
            s.push_str(&format!("  [{}] {}\n", d.severity.label(), d.text));
        }

        // 最近配置变更与恢复入口
        let changes = winmem::sysmem::read_changes();
        s.push_str("\n最近配置变更：\n");
        if changes.is_empty() {
            s.push_str("  （本工具尚未修改过任何系统配置）\n");
        } else {
            for c in changes.iter().rev().take(5) {
                s.push_str(&format!(
                    "  {}  {}：{} → {}（结果 {}，验证 {}{}）\n",
                    c.at,
                    c.kind,
                    c.before.describe(),
                    c.target.describe(),
                    c.result,
                    c.verified,
                    if c.reboot_required {
                        "，可能需要重启"
                    } else {
                        ""
                    }
                ));
            }
        }

        if !winmem::sysmem::is_elevated() {
            s.push_str("\n提示：内存压缩 / 页面合并的查询与全部配置变更需要管理员权限；\n分页文件与提交量诊断在普通权限下可用。\n");
        }
        s.push_str(
            "\n本页是只读诊断。系统配置变更是独立的人工操作（需管理员，可能需重启），\n\
             与「智能自动优化」开关互不相干 —— 自动优化永远不会修改系统配置。\n\
             命令行：winmem sysmem compression --enable\n",
        );
        s
    }

    /// 在后台线程执行一次只读诊断，结果写入文本区（不阻塞 UI）。
    fn start_sysmem_diag(&mut self) {
        // 单次查询（含一次 PowerShell 调用）不可中途取消
        self.spawn_task(
            "正在查询系统内存配置（只读，不会修改任何设置）…",
            false,
            move |tx, _cancel| {
                let text = AppState::build_sysmem_report();
                let _ = tx.send(Task::SysmemText(text));
                let _ = tx.send(Task::Done {
                    text: "系统内存诊断完成（只读，未修改任何设置）。".into(),
                    kind: 0,
                    refresh: false,
                });
            },
        );
    }

    // -- 热更新 -----------------------------------------------------------

    /// 检查更新。`manual` 为 true 时结果会弹窗；自动检查只在状态栏提示。
    ///
    /// 查询在后台线程执行（联网 + 超时），不阻塞界面。
    fn check_update(&mut self, manual: bool) {
        if self.updating {
            self.set_status("正在更新中，请稍候…", 2);
            return;
        }
        let repo = self.config.update_repo.clone();
        let tx = self.tx.clone();
        self.set_status(
            if manual {
                "正在检查更新…"
            } else {
                "正在后台检查更新…"
            },
            0,
        );
        self.config.last_update_check = winmem::update::now_unix();
        let _ = self.config.save();
        std::thread::spawn(move || match winmem::update::check(&repo) {
            winmem::update::CheckResult::Available(info) => {
                let _ = tx.send(Task::UpdateAvailable { info, manual });
            }
            winmem::update::CheckResult::UpToDate { latest, .. } => {
                let _ = tx.send(Task::UpdateUpToDate { latest, manual });
            }
            winmem::update::CheckResult::Failed(reason) => {
                let _ = tx.send(Task::UpdateCheckFailed { reason, manual });
            }
        });
    }

    /// 启动后按配置做一次静默检查（一天最多一次）。
    fn maybe_auto_check_update(&mut self) {
        if self.auto_update_checked || !self.config.auto_update_check {
            return;
        }
        self.auto_update_checked = true;
        let now = winmem::update::now_unix();
        let last = self.config.last_update_check;
        if last > 0 && now.saturating_sub(last) < winmem::update::AUTO_CHECK_INTERVAL_SECS {
            return;
        }
        self.check_update(false);
    }

    /// 后台下载并安装更新（校验 SHA-256，失败自动回滚）。
    fn install_update(&mut self, info: Box<winmem::update::ReleaseInfo>) {
        if self.updating {
            return;
        }
        let Some(exe) = winmem::update::self_exe() else {
            self.set_status("无法确定当前程序路径，已取消更新。", 3);
            return;
        };
        self.updating = true;
        let tx = self.tx.clone();
        self.set_status("正在下载并校验更新包…（请勿关闭程序）", 0);
        std::thread::spawn(move || {
            let self_name = exe
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let sibling = winmem::update::sibling_name(&self_name);
            let report = winmem::update::install(&info, &exe, &[sibling]);
            let _ = tx.send(Task::UpdateInstalled {
                report: Box::new(report),
            });
        });
    }

    /// 更新安装完成后的处置：成功则询问是否立即重启。
    fn finish_update(&mut self, report: winmem::update::ApplyReport) {
        self.updating = false;
        let ok: Vec<String> = report
            .files
            .iter()
            .filter(|f| f.ok)
            .map(|f| {
                f.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let failed: Vec<String> = report
            .files
            .iter()
            .filter(|f| !f.ok)
            .map(|f| {
                format!(
                    "{}：{}",
                    f.path.file_name().unwrap_or_default().to_string_lossy(),
                    f.message
                )
            })
            .collect();

        if report.all_ok {
            self.set_status(&format!("更新完成（{}）。重启后生效。", ok.join("、")), 1);
            unsafe {
                let msg = format!(
                    "更新已就位：{}\n\n重启程序后生效（旧版本已备份为 *.old，下次启动自动清理）。\n\n现在重启吗？",
                    ok.join("、")
                );
                let t = w32::w(&msg);
                let cap = w32::w("更新完成");
                if w32::MessageBoxW(
                    self.hwnd,
                    t.as_ptr(),
                    cap.as_ptr(),
                    w32::MB_YESNO | w32::MB_ICONINFORMATION,
                ) == w32::IDYES
                {
                    self.restart_self();
                }
            }
        } else {
            self.set_status("更新未全部完成，详见提示。", 3);
            unsafe {
                let msg = format!(
                    "部分文件未能更新：\n\n{}\n\n常见原因：程序所在目录不可写（需要管理员权限），\n或本次发布未提供该文件。\n\n替换失败时已自动回滚，原程序仍可正常使用。",
                    failed.join("\n")
                );
                let t = w32::w(&msg);
                let cap = w32::w("更新未完成");
                w32::MessageBoxW(
                    self.hwnd,
                    t.as_ptr(),
                    cap.as_ptr(),
                    w32::MB_OK | w32::MB_ICONWARNING,
                );
            }
        }
    }

    /// 用新版本重启（原样带上启动参数），随后退出当前进程。
    fn restart_self(&mut self) {
        let Some(exe) = winmem::update::self_exe() else {
            return;
        };
        match winmem::update::restart(&exe, &self.start_args) {
            Ok(()) => unsafe {
                self.remove_tray_icon();
                w32::DestroyWindow(self.hwnd);
            },
            Err(e) => self.set_status(&format!("无法重启：{}（请手动重启）", e), 3),
        }
    }

    /// 发现新版本：手动检查时弹窗询问，自动检查时只提示。
    fn on_update_available(&mut self, info: winmem::update::ReleaseInfo, manual: bool) {
        let name = winmem::update::self_exe()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();

        if !manual {
            // 自动检查：只在状态栏提示，不弹窗、不抢焦点、不打断工作
            self.set_status(
                &format!(
                    "发现新版本 {}，可在右键菜单「检查更新…」中安装（当前 {}）。",
                    info.version,
                    winmem::update::current_version()
                ),
                2,
            );
            return;
        }

        self.set_status(&format!("发现新版本 {}", info.version), 2);
        let size = info
            .asset(&name)
            .and_then(|a| a.size)
            .map(|s| format!("{:.1} MB", s as f64 / 1024.0 / 1024.0))
            .unwrap_or_else(|| "大小未知".into());
        unsafe {
            let msg = format!(
                "发现新版本 {}（当前 {}）\n\n更新包：{}（{}）\n· 下载后会校验 SHA-256，校验不通过不会安装；\n· 替换前旧版本会备份为 *.old，失败时自动回滚。\n\n现在下载并安装吗？\n\n{}",
                info.version,
                winmem::update::current_version(),
                name,
                size,
                winmem::update::NOTICE
            );
            let t = w32::w(&msg);
            let cap = w32::w("检查更新");
            if w32::MessageBoxW(
                self.hwnd,
                t.as_ptr(),
                cap.as_ptr(),
                w32::MB_YESNO | w32::MB_ICONINFORMATION,
            ) == w32::IDYES
            {
                self.install_update(Box::new(info));
            } else {
                self.set_status("已跳过本次更新，可随时在右键菜单中更新。", 0);
            }
        }
    }

    /// 已是最新。
    fn on_update_uptodate(&mut self, latest: &str, manual: bool) {
        if manual {
            self.set_status(
                &format!(
                    "已是最新版本（{}，远端 {}）。",
                    winmem::update::current_version(),
                    latest
                ),
                1,
            );
        } else {
            self.set_status(&format!("更新检查完成：已是最新版本（{}）。", latest), 0);
        }
    }

    /// 检查更新失败。自动检查失败不打扰用户。
    fn on_update_failed(&mut self, reason: &str, manual: bool) {
        if !manual {
            self.set_status(&format!("后台检查更新失败：{}", reason), 0);
            return;
        }
        self.set_status(&format!("检查更新失败：{}", reason), 3);
        unsafe {
            let msg = format!("检查更新失败：\n\n{}", reason);
            let t = w32::w(&msg);
            let cap = w32::w("检查更新");
            w32::MessageBoxW(
                self.hwnd,
                t.as_ptr(),
                cap.as_ptr(),
                w32::MB_OK | w32::MB_ICONWARNING,
            );
        }
    }

    /// 打开 Windows 自带的「虚拟内存」设置。
    fn op_pagefile_settings(&mut self) {
        match winmem::sysmem::open_pagefile_settings() {
            Ok(()) => self.set_status("已打开 Windows 虚拟内存设置；本工具不会替你修改。", 0),
            Err(e) => self.set_status(&format!("无法打开系统设置：{}", e), 3),
        }
    }

    /// 恢复本工具上次修改前的系统配置（人工确认，后台执行）。
    fn op_sysmem_restore(&mut self) {
        if self.busy {
            return;
        }
        unsafe {
            let msg = "将尝试恢复本工具**上次修改前**的系统配置：\n\n\
                · 逐项检查当前配置是否仍与上次修改结果一致；\n\
                · 若已被其他软件改动，只显示差异，不会覆盖；\n\
                · 需要管理员权限，部分项可能需要重启后验证；\n\
                · 这与「智能自动优化」开关是两件独立的事。\n\n\
                继续吗？";
            let t = w32::w(msg);
            let cap = w32::w("恢复系统配置");
            if w32::MessageBoxW(
                self.hwnd,
                t.as_ptr(),
                cap.as_ptr(),
                w32::MB_YESNO | w32::MB_ICONWARNING,
            ) != w32::IDYES
            {
                self.set_status("已取消恢复。", 0);
                return;
            }
        }
        self.spawn_task(
            "正在恢复上次修改前的系统配置…",
            false,
            move |tx, _cancel| {
                let kinds = [
                    winmem::sysmem::ChangeKind::MemoryCompression,
                    winmem::sysmem::ChangeKind::PageCombining,
                    winmem::sysmem::ChangeKind::AutomaticPagefile,
                ];
                let mut lines = Vec::new();
                let mut ok_all = true;
                for k in kinds {
                    let o = winmem::sysmem::restore_last(k);
                    if !o.ok {
                        ok_all = false;
                    }
                    lines.push(format!(
                        "{}：{}（验证 {}）",
                        k.title(),
                        o.message,
                        o.verify.label()
                    ));
                }
                let _ = tx.send(Task::Done {
                    text: format!("恢复完成：{}", lines.join("；")),
                    kind: if ok_all { 1 } else { 2 },
                    refresh: false,
                });
                // 恢复后重新查询，刷新诊断文本
                let text = AppState::build_sysmem_report();
                let _ = tx.send(Task::SysmemText(text));
            },
        );
    }

    /// 暂停智能自动优化：立即取消剩余任务（不撤销已发生的工作集裁剪）。
    fn pause_auto(&mut self) {
        if let Some(a) = self.auto.take() {
            a.cancel.cancel();
        }
        self.auto_state = AutoState::Off;
        self.auto_msg = "已暂停".into();
        self.config.auto_enabled = false;
        let _ = self.config.save();
        unsafe {
            w32::SendMessageW(self.hchk_auto, w32::BM_SETCHECK, w32::BST_UNCHECKED, 0);
        }
        self.set_status("智能自动优化已暂停，剩余任务已取消。", 0);
    }

    /// 恢复智能自动优化（等价于重新勾选开关）。
    fn resume_auto(&mut self) {
        if self.auto.is_some() {
            self.set_status("智能自动优化正在运行。", 0);
            return;
        }
        unsafe {
            w32::SendMessageW(self.hchk_auto, w32::BM_SETCHECK, w32::BST_CHECKED, 0);
        }
        self.op_toggle_auto();
    }

    /// 高级操作：清空系统待机列表（不进入默认优化流程）。
    fn op_standby(&mut self) {
        unsafe {
            let msg = "清空系统待机列表属于高级诊断操作：\n\n\
                · 会把系统缓存的页面直接释放为空闲内存；\n\
                · 之后需要时会重新从磁盘读取，可能造成短时卡顿；\n\
                · 日常「智能优化」不会执行该操作。\n\n\
                确定要执行吗？";
            let t = w32::w(msg);
            let cap = w32::w("高级操作：清空待机列表");
            if w32::MessageBoxW(
                self.hwnd,
                t.as_ptr(),
                cap.as_ptr(),
                w32::MB_YESNO | w32::MB_ICONWARNING,
            ) != w32::IDYES
            {
                self.set_status("已取消清空待机列表。", 0);
                return;
            }
        }
        // 按需启用特权，函数返回时由 RAII 恢复
        let _guard = wapi::PrivilegeGuard::enable("SeProfileSingleProcessPrivilege");
        match wapi::purge_standby_list() {
            Ok(()) => self.set_status("已清空系统待机列表（高级操作）。", 1),
            Err(status) => self.set_status(
                &format!(
                    "清空待机列表失败：NTSTATUS 0x{:08X}（请确认以管理员身份运行）",
                    status as u32
                ),
                3,
            ),
        }
    }

    /// 打开配置文件（不存在则先写出模板）。
    fn op_open_config(&mut self) {
        let Some(p) = Config::default_path() else {
            self.set_status("无法定位 %APPDATA%，无法打开配置文件。", 3);
            return;
        };
        if !p.exists() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::write(&p, Config::template()).is_err() {
                self.set_status("写入配置模板失败。", 3);
                return;
            }
        }
        unsafe {
            let file = w32::w(&p.display().to_string());
            let op = w32::w("open");
            w32::ShellExecuteW(
                self.hwnd,
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                w32::SW_SHOWNORMAL,
            );
        }
        self.set_status("已打开配置文件；保存后点「刷新」重新加载配置。", 0);
    }

    /// 右键菜单。
    unsafe fn show_context_menu(&mut self) {
        let menu = w32::CreatePopupMenu();
        if menu == 0 {
            return;
        }
        let items: [(usize, &str); 12] = [
            (ID_MENU_TRIM, "释放选中进程的内存"),
            (ID_MENU_KILL, "结束选中进程…"),
            (ID_MENU_SEL_ALL, "全选"),
            (ID_MENU_SEL_NONE, "取消选择"),
            (ID_MENU_PREVIEW, "预览优化候选与跳过原因…"),
            (ID_MENU_UPDATE, "检查更新…"),
            (0, ""),
            (ID_MENU_SYSMEM, "系统内存页（压缩 / 分页文件 / 提交量）"),
            (ID_MENU_LISTS, "编辑保护名单 / 自动优化允许名单…"),
            (ID_MENU_CONFIG, "打开配置文件…"),
            (ID_MENU_STANDBY, "清空系统待机列表（高级诊断）…"),
            (0, ""),
        ];
        for (id, text) in items {
            if text.is_empty() {
                w32::AppendMenuW(menu, w32::MF_SEPARATOR, 0, std::ptr::null());
            } else {
                let t = w32::w(text);
                w32::AppendMenuW(menu, w32::MF_STRING, id, t.as_ptr());
            }
        }
        let mut pt = w32::POINT::default();
        w32::GetCursorPos(&mut pt);
        let cmd = w32::TrackPopupMenu(
            menu,
            w32::TPM_RETURNCMD | w32::TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            self.hwnd,
            std::ptr::null(),
        );
        w32::DestroyMenu(menu);

        match cmd as usize {
            ID_MENU_TRIM => self.op_trim_selected(),
            ID_MENU_KILL => self.op_kill_selected(),
            ID_MENU_SEL_ALL => self.select_all(),
            ID_MENU_SEL_NONE => self.clear_selection(),
            ID_MENU_PREVIEW => self.op_preview(),
            ID_MENU_UPDATE => self.check_update(true),
            ID_MENU_SYSMEM => self.switch_view(View::Sysmem),
            ID_MENU_LISTS => self.op_edit_lists(),
            ID_MENU_CONFIG => self.op_open_config(),
            ID_MENU_STANDBY => self.op_standby(),
            _ => {}
        }
    }

    fn op_preview(&mut self) {
        let cfg = self.config.clone();
        let mut engine = Engine::new(cfg);
        let plan = engine.build_plan(Purpose::Auto, &[]);
        let text = report::format_plan(&plan);
        unsafe {
            let t = w32::w(&text);
            let cap = w32::w("优化候选预览（未执行任何操作）");
            w32::MessageBoxW(
                self.hwnd,
                t.as_ptr(),
                cap.as_ptr(),
                w32::MB_OK | w32::MB_ICONINFORMATION,
            );
        }
    }

    fn op_trim_selected(&mut self) {
        let procs = self.selected_procs();
        if procs.is_empty() {
            self.set_status("请先在列表中选中要释放内存的进程（可多选）。", 2);
            unsafe {
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
            return;
        }
        let targets: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        let cfg = self.config.clone();
        self.spawn_task(
            "正在按统一策略释放选中进程的内存…",
            true,
            move |tx, cancel| {
                let mut engine = Engine::new(cfg);
                let run = RunOptions {
                    manual_targets: targets,
                    force_run: true,
                    ..Default::default()
                };
                let outcome = engine.run(&run, &cancel, &mut |p| {
                    let _ = tx.send(Task::Progress {
                        done: p.done,
                        total: p.total,
                    });
                });
                let obs = &outcome.observation;
                let unknown = if obs.unknown_count > 0 {
                    format!("（{} 个结果未知）", obs.unknown_count)
                } else {
                    String::new()
                };
                let text = format!(
                    "{}：成功 {} 个，工作集减少 {}{}，可用内存变化 {}",
                    outcome.finished,
                    obs.trimmed_count,
                    term::fmt_bytes(obs.reduction_total),
                    unknown,
                    obs.avail_delta()
                        .map(term::fmt_delta)
                        .unwrap_or_else(|| "未知".into())
                );
                let _ = tx.send(Task::Done {
                    text,
                    kind: 1,
                    refresh: true,
                });
            },
        );
    }

    fn op_kill_selected(&mut self) {
        let procs = self.selected_procs();
        if procs.is_empty() {
            self.set_status("请先在列表中选中要结束的进程（可多选）。", 2);
            unsafe {
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
            return;
        }

        // 硬保护：状态为“禁止操作”或本程序的，任何情况下都不结束
        let (blocked, ready): (Vec<Row>, Vec<Row>) = procs
            .into_iter()
            .partition(|p| p.hard_blocked || p.state == "本程序");

        unsafe {
            if !blocked.is_empty() {
                let names: Vec<String> = blocked
                    .iter()
                    .take(10)
                    .map(|p| format!("  {}（PID {}）", p.name, p.pid))
                    .collect();
                let msg = format!(
                    "以下 {} 个进程受硬保护，已自动跳过（强制也无法绕过）：\n\n{}{}",
                    blocked.len(),
                    names.join("\n"),
                    if blocked.len() > 10 { "\n  …" } else { "" }
                );
                let text = w32::w(&msg);
                let cap = w32::w("已跳过受保护进程");
                w32::MessageBoxW(
                    self.hwnd,
                    text.as_ptr(),
                    cap.as_ptr(),
                    w32::MB_OK | w32::MB_ICONINFORMATION,
                );
            }
        }

        if ready.is_empty() {
            self.set_status("选中的进程全部受保护，已阻止操作。", 3);
            unsafe {
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
            return;
        }

        unsafe {
            let names: Vec<String> = ready
                .iter()
                .take(12)
                .map(|p| {
                    format!(
                        "  {}（PID {}，私有提交 {}，{}）",
                        p.name,
                        p.pid,
                        p.private_bytes
                            .map(term::fmt_bytes)
                            .unwrap_or_else(|| "未知".into()),
                        p.state
                    )
                })
                .collect();
            let msg = format!(
                "确定要结束以下 {} 个进程吗？\n\n{}{}\n\n未保存的数据将会丢失。\n（结束进程属于手动操作，不属于内存优化）",
                ready.len(),
                names.join("\n"),
                if ready.len() > 12 { "\n  …" } else { "" }
            );
            let text = w32::w(&msg);
            let cap = w32::w("确认结束进程");
            if w32::MessageBoxW(
                self.hwnd,
                text.as_ptr(),
                cap.as_ptr(),
                w32::MB_YESNO | w32::MB_ICONWARNING,
            ) != w32::IDYES
            {
                self.set_status("已取消结束进程。", 0);
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
                return;
            }
        }

        // 结束进程是逐项发生的，中途取消只会少结束几个，因此不提供取消
        self.spawn_task("正在结束选中进程…", false, move |tx, _cancel| {
            let before = wapi::memory_status().map(|m| m.ullAvailPhys);
            let engine = Engine::new(Config::load());
            let total = ready.len();
            let mut ok = 0usize;
            let mut fail = 0usize;
            let mut skipped = 0usize;
            for (i, p) in ready.iter().enumerate() {
                match &p.identity {
                    Some(id) => match engine.kill_verified(id) {
                        Ok(()) => ok += 1,
                        Err(_) => fail += 1,
                    },
                    None => skipped += 1,
                }
                let _ = tx.send(Task::Progress { done: i + 1, total });
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
            let after = wapi::memory_status().map(|m| m.ullAvailPhys);
            let delta = match (before, after) {
                (Some(b), Some(a)) => term::fmt_delta(a as i64 - b as i64),
                _ => "未知".to_string(),
            };
            let text = format!(
                "已结束 {} 个进程（失败 {}，因身份未知跳过 {}），可用内存变化 {}",
                ok, fail, skipped, delta
            );
            let _ = tx.send(Task::Done {
                text,
                kind: if fail > 0 { 2 } else { 1 },
                refresh: true,
            });
        });
    }

    // -- 绘制 -------------------------------------------------------------

    unsafe fn on_paint(&self) {
        let mut ps: w32::PAINTSTRUCT = std::mem::zeroed();
        let hdc = w32::BeginPaint(self.hwnd, &mut ps);
        let mut rc = w32::RECT::default();
        w32::GetClientRect(self.hwnd, &mut rc);

        let top_h = self.px(TOP_H);
        let status_h = self.px(STATUS_H);
        let pad = self.px(20);

        // 整体背景
        w32::fill_rect(hdc, &rc, self.palette.bg);

        // ---------- 顶部：内存总览 ----------
        let top = w32::RECT {
            left: 0,
            top: 0,
            right: rc.right,
            bottom: top_h,
        };
        w32::fill_rect(hdc, &top, self.palette.panel);
        let top_line = w32::RECT {
            left: 0,
            top: top_h - 1,
            right: rc.right,
            bottom: top_h,
        };
        w32::fill_rect(hdc, &top_line, self.palette.border);

        let title = w32::RECT {
            left: pad,
            top: self.px(13),
            right: rc.right - self.px(320),
            bottom: self.px(41),
        };
        w32::draw_text(
            hdc,
            "winmem  ·  内存优化",
            title,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE,
            self.palette.text,
            self.font_title,
        );

        if let Some(ms) = &self.mem {
            let total = ms.ullTotalPhys;
            let avail = ms.ullAvailPhys;
            let used = total.saturating_sub(avail);
            let pct = if total > 0 {
                used as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            let color = self.palette.pressure(pct);

            let ur = w32::RECT {
                left: rc.right - self.px(320),
                top: self.px(13),
                right: rc.right - pad,
                bottom: self.px(41),
            };
            w32::draw_text(
                hdc,
                &format!("占用 {:.1}%", pct),
                ur,
                w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
                color,
                self.font_title,
            );

            // Win11 风格圆角进度条
            let bar_h = self.px(8);
            let bar = w32::RECT {
                left: pad,
                top: self.px(52),
                right: rc.right - pad,
                bottom: self.px(52) + bar_h,
            };
            let radius = bar_h / 2;
            w32::fill_round_rect(hdc, &bar, radius, self.palette.track);
            let full = bar.width().max(1);
            let fill_w = (((full as f64) * (pct / 100.0)).round() as i32)
                .max(bar_h)
                .min(full);
            let fill = w32::RECT {
                left: bar.left,
                top: bar.top,
                right: bar.left + fill_w,
                bottom: bar.bottom,
            };
            w32::fill_round_rect(hdc, &fill, radius, color);

            // 统计行
            let mut stats = format!(
                "物理内存 {}/{}     可用 {}",
                term::fmt_bytes(used),
                term::fmt_bytes(total),
                term::fmt_bytes(avail)
            );
            if let Some(pi) = &self.perf {
                let page = pi.PageSize.max(1) as u64;
                stats.push_str(&format!(
                    "     提交 {}/{}     系统缓存 {}     进程 {} / 线程 {}",
                    term::fmt_bytes(pi.CommitTotal as u64 * page),
                    term::fmt_bytes(pi.CommitLimit as u64 * page),
                    term::fmt_bytes(pi.SystemCache as u64 * page),
                    pi.ProcessCount,
                    pi.ThreadCount
                ));
            }
            let sr = w32::RECT {
                left: pad,
                top: self.px(68),
                right: rc.right - pad,
                bottom: self.px(90),
            };
            w32::draw_text(
                hdc,
                &stats,
                sr,
                w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE | w32::DT_END_ELLIPSIS,
                self.palette.dim,
                self.font_small,
            );
        }

        // ---------- 工具栏：筛选标签（仅进程页）----------
        if self.view == View::Procs {
            let label = w32::RECT {
                left: rc.right - self.px(320),
                top: top_h + self.px(8),
                right: rc.right - self.px(244),
                bottom: top_h + self.px(36),
            };
            w32::draw_text(
                hdc,
                "筛选",
                label,
                w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
                self.palette.dim,
                self.font_ui,
            );
        }

        // ---------- 底部：状态栏 ----------
        let sbar = w32::RECT {
            left: 0,
            top: rc.bottom - status_h,
            right: rc.right,
            bottom: rc.bottom,
        };
        w32::fill_rect(hdc, &sbar, self.palette.panel);
        let sline = w32::RECT {
            left: 0,
            top: rc.bottom - status_h,
            right: rc.right,
            bottom: rc.bottom - status_h + 1,
        };
        w32::fill_rect(hdc, &sline, self.palette.border);

        let text = if self.status.is_empty() {
            "就绪".to_string()
        } else {
            self.status.clone()
        };
        let tr = w32::RECT {
            left: pad,
            top: rc.bottom - status_h + self.px(8),
            right: rc.right - self.px(290),
            bottom: rc.bottom - status_h + self.px(32),
        };
        w32::draw_text(
            hdc,
            &text,
            tr,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE | w32::DT_END_ELLIPSIS,
            self.status_color(),
            self.font_ui,
        );

        // 第一行右侧：自动优化状态（运行时优先），否则显示任务进度
        let right = if self.auto.is_some() {
            Some((
                format!("自动优化：{}", self.auto_state.label()),
                match self.auto_state {
                    AutoState::Optimizing => self.palette.green,
                    AutoState::Confirming | AutoState::Cooling => self.palette.yellow,
                    AutoState::Observing | AutoState::Monitoring => self.palette.blue,
                    AutoState::Off => self.palette.dim,
                },
            ))
        } else {
            self.progress.and_then(|(done, total)| {
                if total > 0 {
                    Some((format!("{}/{}", done, total), self.palette.blue))
                } else {
                    None
                }
            })
        };
        if let Some((text, color)) = right {
            let pr = w32::RECT {
                left: rc.right - self.px(280),
                top: rc.bottom - status_h + self.px(8),
                right: rc.right - pad,
                bottom: rc.bottom - status_h + self.px(32),
            };
            w32::draw_text(
                hdc,
                &text,
                pr,
                w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
                color,
                self.font_ui,
            );
        }

        // 第二行左侧：列表统计
        let sr2 = w32::RECT {
            left: pad,
            top: rc.bottom - status_h + self.px(30),
            right: (rc.right - self.px(280)).max(self.px(200)),
            bottom: rc.bottom - status_h + self.px(52),
        };
        let stats_line = if self.auto.is_some() && !self.auto_msg.is_empty() {
            format!("{}　｜　自动优化：{}", self.stats, self.auto_msg)
        } else {
            self.stats.clone()
        };
        w32::draw_text(
            hdc,
            &stats_line,
            sr2,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE | w32::DT_END_ELLIPSIS,
            self.palette.dim,
            self.font_small,
        );

        // 第二行右侧：权限提示
        let perm = if wapi::is_elevated() {
            "管理员权限：可操作系统进程与待机列表"
        } else {
            "普通权限：部分系统进程不可操作"
        };
        let pr2 = w32::RECT {
            left: (rc.right - self.px(280)).max(self.px(200)),
            top: rc.bottom - status_h + self.px(30),
            right: rc.right - pad,
            bottom: rc.bottom - status_h + self.px(52),
        };
        w32::draw_text(
            hdc,
            perm,
            pr2,
            w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
            self.palette.dim,
            self.font_small,
        );

        w32::EndPaint(self.hwnd, &ps);
    }
}

// ---------------------------------------------------------------------------
// 名单编辑对话框（系统原生控件）
// ---------------------------------------------------------------------------

/// 对话框上下文。窗口创建时装箱传入，`WM_NCDESTROY` 时回收。
struct ListsCtx {
    parent: w32::HWND,
    allow: w32::HWND,
    protect: w32::HWND,
    font: w32::HFONT,
    /// 确定时写入；取消则保持 None
    result: ListsResult,
    allow_init: String,
    protect_init: String,
}

impl ListsCtx {
    // 创建原生 STATIC 标签：Win32 参数本来就多，这里只是薄封装
    #[allow(clippy::too_many_arguments)]
    unsafe fn add_static(
        &self,
        hwnd: w32::HWND,
        hinst: w32::HINSTANCE,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> w32::HWND {
        let class_static = w32::w("STATIC");
        let t = w32::w(text);
        let hwd = w32::CreateWindowExW(
            0,
            class_static.as_ptr(),
            t.as_ptr(),
            w32::WS_CHILD | w32::WS_VISIBLE | w32::SS_LEFT,
            x,
            y,
            w,
            h,
            hwnd,
            0,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(hwd, self.font);
        hwd
    }

    unsafe fn create_controls(&mut self, hwnd: w32::HWND, hinst: w32::HINSTANCE) {
        let class_edit = w32::w("EDIT");
        let class_btn = w32::w("BUTTON");
        let empty = w32::w("");

        self.add_static(
            hwnd,
            hinst,
            "自动优化允许名单（只有这里的进程会被自动处理，逗号分隔，可省略 .exe）：",
            16,
            16,
            512,
            22,
        );
        let allow = w32::CreateWindowExW(
            w32::WS_EX_CLIENTEDGE,
            class_edit.as_ptr(),
            empty.as_ptr(),
            w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::ES_AUTOHSCROLL,
            16,
            42,
            512,
            26,
            hwnd,
            0,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(allow, self.font);
        w32::set_text(allow, &self.allow_init);
        self.allow = allow;

        self.add_static(
            hwnd,
            hinst,
            "保护名单（这些进程永不自动处理）：",
            16,
            82,
            512,
            22,
        );
        let protect = w32::CreateWindowExW(
            w32::WS_EX_CLIENTEDGE,
            class_edit.as_ptr(),
            empty.as_ptr(),
            w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::ES_AUTOHSCROLL,
            16,
            108,
            512,
            26,
            hwnd,
            0,
            hinst,
            std::ptr::null_mut(),
        );
        w32::set_font(protect, self.font);
        w32::set_text(protect, &self.protect_init);
        self.protect = protect;

        self.add_static(
            hwnd,
            hinst,
            "允许名单为空时，自动优化只监测和提示，不会执行任何操作。\n\
             名称比较不区分大小写，.exe 后缀可省略。",
            16,
            146,
            512,
            44,
        );

        for (id, text, x) in [(ID_LISTS_OK, "确定", 320), (ID_LISTS_CANCEL, "取消", 428)] {
            let t = w32::w(text);
            let b = w32::CreateWindowExW(
                0,
                class_btn.as_ptr(),
                t.as_ptr(),
                w32::WS_CHILD | w32::WS_VISIBLE | w32::WS_TABSTOP | w32::BS_PUSHBUTTON,
                x,
                200,
                100,
                30,
                hwnd,
                id as w32::HMENU,
                hinst,
                std::ptr::null_mut(),
            );
            w32::set_font(b, self.font);
        }
        w32::SetFocus(allow);
    }

    /// 确定：把两个输入框的内容写回结果槽。
    unsafe fn commit(&self) {
        let allow = w32::get_text(self.allow);
        let protect = w32::get_text(self.protect);
        if let Ok(mut g) = self.result.lock() {
            *g = Some((allow, protect));
        }
    }

    /// 通知主窗口并销毁自己。
    unsafe fn close(&self, hwnd: w32::HWND) {
        w32::PostMessageW(self.parent, w32::WM_APP_LISTS, 0, 0);
        w32::DestroyWindow(hwnd);
    }
}

impl Drop for ListsCtx {
    fn drop(&mut self) {
        // 对话框自己创建的字体必须在窗口销毁时释放
        unsafe {
            if self.font != 0 {
                w32::DeleteObject(self.font);
                self.font = 0;
            }
        }
    }
}

unsafe fn lists_state<'a>(hwnd: w32::HWND) -> Option<&'a mut ListsCtx> {
    let ptr = w32::GetWindowLongPtrW(hwnd, w32::GWLP_USERDATA) as *mut ListsCtx;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

unsafe extern "system" fn lists_wndproc(
    hwnd: w32::HWND,
    msg: u32,
    wp: w32::WPARAM,
    lp: w32::LPARAM,
) -> w32::LRESULT {
    if msg == w32::WM_CREATE {
        let cs = &*(lp as *const w32::CREATESTRUCTW);
        let ptr = cs.lpCreateParams as *mut ListsCtx;
        if ptr.is_null() {
            return -1;
        }
        w32::SetWindowLongPtrW(hwnd, w32::GWLP_USERDATA, ptr as isize);
        (*ptr).create_controls(hwnd, cs.hInstance);
        return 0;
    }

    // 窗口销毁：释放装箱上下文（无论以何种方式关闭）
    if msg == w32::WM_NCDESTROY {
        let ptr = w32::GetWindowLongPtrW(hwnd, w32::GWLP_USERDATA) as *mut ListsCtx;
        if !ptr.is_null() {
            w32::SetWindowLongPtrW(hwnd, w32::GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
        }
        return 0;
    }

    let Some(c) = lists_state(hwnd) else {
        return w32::DefWindowProcW(hwnd, msg, wp, lp);
    };

    match msg {
        w32::WM_COMMAND => {
            let id = wp & 0xFFFF;
            if id == ID_LISTS_OK {
                c.commit();
                c.close(hwnd);
                0
            } else if id == ID_LISTS_CANCEL {
                c.close(hwnd);
                0
            } else {
                0
            }
        }
        w32::WM_CLOSE => {
            c.close(hwnd);
            0
        }
        w32::WM_KEYDOWN => {
            if wp == w32::VK_ESCAPE {
                c.close(hwnd);
                0
            } else if wp == w32::VK_RETURN {
                c.commit();
                c.close(hwnd);
                0
            } else {
                w32::DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        _ => w32::DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

unsafe fn state<'a>(hwnd: w32::HWND) -> Option<&'a mut AppState> {
    let ptr = w32::GetWindowLongPtrW(hwnd, w32::GWLP_USERDATA) as *mut AppState;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

unsafe extern "system" fn wndproc(
    hwnd: w32::HWND,
    msg: u32,
    wp: w32::WPARAM,
    lp: w32::LPARAM,
) -> w32::LRESULT {
    if msg == w32::WM_CREATE {
        let cs = &*(lp as *const w32::CREATESTRUCTW);
        let ptr = cs.lpCreateParams as *mut AppState;
        if ptr.is_null() {
            return -1;
        }
        w32::SetWindowLongPtrW(hwnd, w32::GWLP_USERDATA, ptr as isize);
        let s = &mut *ptr;
        s.create_controls(hwnd, cs.hInstance);
        w32::SetTimer(hwnd, TIMER_ID, TIMER_MS, std::ptr::null());
        (*ptr).status =
            "就绪 —— 「智能优化」按压力策略执行；勾选进程后可单独释放内存或结束进程；右键查看更多操作。"
                .to_string();
        (*ptr).reload();
        return 0;
    }

    let Some(s) = state(hwnd) else {
        return w32::DefWindowProcW(hwnd, msg, wp, lp);
    };

    match msg {
        w32::WM_SIZE => {
            // 最小化时降低 UI 刷新频率，减少后台开销
            let interval = if wp == w32::SIZE_MINIMIZED {
                2000
            } else {
                TIMER_MS
            };
            w32::SetTimer(hwnd, TIMER_ID, interval, std::ptr::null());
            s.layout();
            w32::InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        w32::WM_PAINT => {
            s.on_paint();
            0
        }
        w32::WM_ERASEBKGND => 1,
        w32::WM_GETMINMAXINFO => {
            let mmi = &mut *(lp as *mut w32::MINMAXINFO);
            mmi.ptMinTrackSize.x = s.px(940);
            mmi.ptMinTrackSize.y = s.px(560);
            0
        }
        w32::WM_DPICHANGED => {
            let dpi = ((wp >> 16) & 0xFFFF) as u32;
            s.apply_dpi(dpi);
            let suggested = &*(lp as *const w32::RECT);
            w32::MoveWindow(
                hwnd,
                suggested.left,
                suggested.top,
                suggested.width(),
                suggested.height(),
                1,
            );
            s.layout();
            w32::InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        w32::WM_APP_AUTO => {
            s.pump_auto();
            w32::InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        w32::WM_CONTEXTMENU => {
            s.show_context_menu();
            w32::InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        w32::WM_CLOSE => {
            // 默认收进托盘（可在配置中关闭该行为）；退出走托盘菜单「退出 winmem」。
            // 托盘图标注册失败时绝不隐藏窗口，否则程序将无法再被操作。
            if s.config.minimize_to_tray && s.tray_added {
                s.hide_to_tray();
                0
            } else {
                w32::DestroyWindow(hwnd);
                0
            }
        }
        w32::WM_DESTROY => {
            s.remove_tray_icon();
            w32::PostQuitMessage(0);
            0
        }
        w32::WM_APP_TRAY => {
            match lp as u32 {
                w32::WM_LBUTTONDBLCLK | w32::WM_LBUTTONUP => {
                    s.restore_window();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                w32::WM_RBUTTONUP => s.show_tray_menu(),
                _ => {}
            }
            0
        }
        w32::WM_APP_LISTS => {
            // 名单对话框已关闭：恢复主窗口并应用结果
            w32::EnableWindow(hwnd, 1);
            s.apply_lists_result();
            w32::InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        w32::WM_TIMER => {
            if wp == TIMER_ID {
                s.pump_tasks();
                s.pump_auto();
                s.ticks = s.ticks.wrapping_add(1);
                if s.ticks % 5 == 0 {
                    s.refresh_memory();
                    // 只重绘顶部内存面板
                    let mut rc = w32::RECT::default();
                    w32::GetClientRect(hwnd, &mut rc);
                    let top = w32::RECT {
                        left: 0,
                        top: 0,
                        right: rc.right,
                        bottom: TOP_H,
                    };
                    w32::InvalidateRect(hwnd, &top, 0);
                }
            }
            0
        }
        w32::WM_COMMAND => {
            let id = wp & 0xFFFF;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            if s.busy
                && id != ID_BTN_REFRESH
                && id != ID_BTN_PREVIEW
                && id != ID_BTN_CANCEL
                && id != ID_CHK_AUTO
            {
                return 0;
            }
            match (id, code) {
                (ID_BTN_REFRESH, w32::BN_CLICKED) => {
                    s.reload();
                    s.set_status("列表已刷新。", 0);
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_OPTIMIZE, w32::BN_CLICKED) => {
                    s.op_optimize();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_PREVIEW, w32::BN_CLICKED) => {
                    s.op_preview();
                }
                (ID_CHK_AUTO, w32::BN_CLICKED) => {
                    s.op_toggle_auto();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_TRIM, w32::BN_CLICKED) => {
                    s.op_trim_selected();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_KILL, w32::BN_CLICKED) => {
                    s.op_kill_selected();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_SYSMEM_REFRESH, w32::BN_CLICKED) => {
                    s.start_sysmem_diag();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_PF_SETTINGS, w32::BN_CLICKED) => {
                    s.op_pagefile_settings();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_SYSMEM_RESTORE, w32::BN_CLICKED) => {
                    s.op_sysmem_restore();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_CANCEL, w32::BN_CLICKED) => {
                    s.op_cancel();
                }
                (ID_BTN_SELECT_ALL, w32::BN_CLICKED) => {
                    s.select_all();
                }
                (ID_BTN_CLEAR_SEL, w32::BN_CLICKED) => {
                    s.clear_selection();
                }
                (ID_EDIT_FILTER, w32::EN_CHANGE) => {
                    s.filter = w32::get_text(s.hfilter);
                    s.rebuild();
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                _ => {}
            }
            0
        }
        w32::WM_NOTIFY => {
            let hdr = &*(lp as *const w32::NMHDR);
            let code = hdr.code as i32;
            let from = hdr.hwndFrom;

            // 页面标签切换：进程 ↔ 系统内存
            if code == w32::TCN_SELCHANGE && from == s.htab {
                let idx = w32::tab_cur_sel(s.htab);
                s.switch_view(if idx == 1 { View::Sysmem } else { View::Procs });
                w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }

            if code == w32::LVN_COLUMNCLICK {
                let nmlv = &*(lp as *const w32::NMLISTVIEW);
                let sort = match nmlv.iSubItem {
                    0 => SortBy::Pid,
                    1 => SortBy::Name,
                    2 => SortBy::Mem,
                    3 => SortBy::Private,
                    _ => s.sort,
                };
                s.sort = sort;
                s.rebuild();
                w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }

            // 列表自绘：把系统进程 / 无权读取的行文字调淡，形成层次
            if code == w32::NM_CUSTOMDRAW && from == s.hlist {
                let cd = &mut *(lp as *mut w32::NMLVCUSTOMDRAW);
                return match cd.nmcd.dwDrawStage {
                    w32::CDDS_PREPAINT => w32::CDRF_NOTIFYITEMDRAW,
                    w32::CDDS_ITEMPREPAINT => {
                        if cd.nmcd.uItemState & w32::CDIS_SELECTED == 0 {
                            if let Some(p) = s.rows.get(cd.nmcd.dwItemSpec) {
                                // 非候选（仅手动 / 禁止操作 / 无权读取）的行淡显
                                if p.state != "可优化" {
                                    cd.clrText = s.palette.dim;
                                }
                            }
                        }
                        w32::CDRF_DODEFAULT
                    }
                    _ => w32::CDRF_DODEFAULT,
                };
            }
            0
        }
        _ => w32::DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------
// 自检：--bench
// ---------------------------------------------------------------------------

/// 解析 `--bench [秒数]`，返回自检时长（秒）；未指定则为 `None`。
fn bench_secs(args: &[String]) -> Option<u64> {
    let idx = args.iter().position(|a| a == "--bench")?;
    let secs = args
        .get(idx + 1)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10)
        .clamp(3, 600);
    Some(secs)
}

/// 自身进程的资源快照。`None` 表示该项读取失败（不伪装成 0）。
struct SelfSample {
    /// CPU 时间（100ns 单位），含内核与用户态
    cpu_time: Option<u64>,
    working_set: Option<u64>,
    private_bytes: Option<u64>,
}

fn sample_self() -> SelfSample {
    match wapi::open_handle(wapi::current_pid(), wapi::PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(h) => {
            let counters = wapi::memory_counters(h.raw()).ok();
            SelfSample {
                cpu_time: wapi::cpu_time_of(h.raw()).ok(),
                working_set: counters.as_ref().map(|c| c.WorkingSetSize as u64),
                private_bytes: counters.as_ref().map(|c| c.PrivateUsage as u64),
            }
        }
        Err(_) => SelfSample {
            cpu_time: None,
            working_set: None,
            private_bytes: None,
        },
    }
}

/// `--bench` 自检：实测启动耗时、空闲 CPU 与内存、刷新开销，并写报告。
///
/// 结果同时打印到 stdout 与当前目录的 `winmem-bench.txt`，方便在 CI 或本地复现。
/// **自检不会执行任何优化操作**（不裁剪、不清空待机列表、不改系统配置）。
fn run_bench(app: &mut AppState, secs: u64, startup_ms: f64, first_scan_ms: f64) {
    use std::time::{Duration, Instant};

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    let mut lines: Vec<String> = Vec::new();

    lines.push("winmem-gui 自检报告（--bench）".into());
    lines.push("数值为本次运行的实测值，受机器与当前负载影响；自检不执行任何优化操作。".into());
    lines.push(String::new());

    // ---- 1) 启动 ----
    lines.push(format!("启动到界面就绪        {:.1} ms", startup_ms));
    lines.push(format!(
        "  其中首次进程扫描    {:.1} ms（含 300ms 差分基线等待）",
        first_scan_ms
    ));

    // ---- 2) 空闲开销：保持消息泵运转但不做任何操作 ----
    let idle_start = Instant::now();
    let s0 = sample_self();
    while idle_start.elapsed() < Duration::from_secs(secs) {
        unsafe {
            let mut msg: w32::MSG = std::mem::zeroed();
            while w32::PeekMessageW(&mut msg, 0, 0, 0, w32::PM_REMOVE) > 0 {
                w32::TranslateMessage(&msg);
                w32::DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let idle_secs = idle_start.elapsed().as_secs_f64();
    let s1 = sample_self();

    lines.push(String::new());
    lines.push(format!("空闲观测时长          {:.1} s", idle_secs));
    // GetProcessTimes 的 CPU 时间按系统时钟节拍（约 15.6ms）更新：
    // 增加量为 0 时只能说「低于节拍精度」，不能报成「0%」。
    const CPU_TICK_SECS: f64 = 0.015625;
    match (s0.cpu_time, s1.cpu_time) {
        (Some(a), Some(b)) => {
            let cpu_secs = b.saturating_sub(a) as f64 / 1e7;
            if cpu_secs > 0.0 {
                lines.push(format!(
                    "空闲 CPU              {:.3}%（单核占比；整机 {:.3}%）",
                    cpu_secs / idle_secs * 100.0,
                    cpu_secs / idle_secs / cores * 100.0
                ));
            } else {
                lines.push(format!(
                    "空闲 CPU              < {:.3}%（{} 秒内不足一个时钟节拍 ≈{:.0}ms）",
                    CPU_TICK_SECS / idle_secs * 100.0,
                    idle_secs.round(),
                    CPU_TICK_SECS * 1000.0
                ));
            }
        }
        _ => lines.push("空闲 CPU              未知（读取失败）".into()),
    }
    lines.push(format!(
        "工作集（空闲态）      {}",
        s1.working_set
            .map(term::fmt_bytes)
            .unwrap_or_else(|| report::UNKNOWN.into())
    ));
    lines.push(format!(
        "私有提交（空闲态）    {}",
        s1.private_bytes
            .map(term::fmt_bytes)
            .unwrap_or_else(|| report::UNKNOWN.into())
    ));

    // ---- 3) 刷新开销：进程采集 + 列表重建 ----
    //
    // 常规刷新复用上一轮采样的差分基线，只做一遍全量枚举；
    // 首次刷新（或基线过期）需要额外一遍基线采集 + 等待。
    let cfg = app.config.clone();
    // 保证测的是「复用基线的一次枚举」这条常规路径（不受观测时长影响）
    app.sampler_at_ms = Some(metrics::monotonic_ms());
    let t = Instant::now();
    app.all = app.collect_now(&cfg);
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    let count = app.all.len();
    let t = Instant::now();
    app.rebuild();
    let rebuild_ms = t.elapsed().as_secs_f64() * 1000.0;

    lines.push(String::new());
    lines.push(format!("进程数                {}", count));
    lines.push(format!(
        "首次采集（含基线）    {:.1} ms（两遍枚举 + {}ms 等待）",
        first_scan_ms, BASELINE_GAP_MS
    ));
    lines.push(format!(
        "常规采集（复用基线）  {:.1} ms（一遍枚举）",
        scan_ms
    ));
    lines.push(format!("列表重建              {:.1} ms", rebuild_ms));
    lines.push(format!(
        "一次常规刷新合计      {:.1} ms",
        scan_ms + rebuild_ms
    ));

    // ---- 4) 系统内存诊断（含一次 PowerShell 查询）----
    let t = Instant::now();
    let _ = AppState::build_sysmem_report();
    let sysmem_ms = t.elapsed().as_secs_f64() * 1000.0;
    lines.push(String::new());
    lines.push(format!(
        "系统内存只读诊断      {:.1} ms（后台线程执行，不阻塞界面）",
        sysmem_ms
    ));

    lines.push(String::new());
    lines.push("提示：参数为经验初始值，请结合实际负载调整（见 README 第五、九节）。".into());

    let report = lines.join("\n");
    println!("{}", report);
    let path = std::env::current_dir()
        .map(|d| d.join("winmem-bench.txt"))
        .unwrap_or_else(|_| std::path::PathBuf::from("winmem-bench.txt"));
    match std::fs::write(&path, &report) {
        Ok(()) => println!("\n报告已写入：{}", path.display()),
        Err(e) => eprintln!("写入自检报告失败：{}", e),
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 是否处于 `--bench` 自检模式（决定初始化失败时是否弹窗）。
static BENCH_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 初始化失败时给出明确提示，而不是静默退出。
unsafe fn fatal(step: &str) {
    let code = wapi::last_error();
    let text = format!("winmem-gui 初始化失败：{}\nGetLastError = {}", step, code);
    eprintln!("{}", text);
    // 自检可能跑在无桌面会话的环境（CI）：此时弹窗会一直等待而卡死
    if BENCH_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let t = w32::w(&text);
    let cap = w32::w("winmem");
    w32::MessageBoxW(
        0,
        t.as_ptr(),
        cap.as_ptr(),
        w32::MB_OK | w32::MB_ICONWARNING,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bench = bench_secs(&args);
    if bench.is_some() {
        BENCH_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let t_start = std::time::Instant::now();

    unsafe {
        // 列表 + 标签页（系统原生公共控件）
        let icc = w32::INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<w32::INITCOMMONCONTROLSEX>() as u32,
            dwICC: w32::ICC_LISTVIEW_CLASSES | w32::ICC_TAB_CLASSES,
        };
        w32::InitCommonControlsEx(&icc);

        let hinst = w32::GetModuleHandleW(std::ptr::null());
        let class_name = w32::w("winmem_gui_wnd");
        let wc = w32::WNDCLASSW {
            style: w32::CS_HREDRAW | w32::CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: 0,
            hCursor: w32::LoadCursorW(0, w32::IDC_ARROW as *const u16),
            hbrBackground: 0,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if w32::RegisterClassW(&wc) == 0 {
            fatal("注册主窗口类失败");
            return;
        }

        // 名单编辑对话框窗口类
        let lists_class = w32::w(LISTS_CLASS);
        let wc_lists = w32::WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(lists_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: 0,
            hCursor: w32::LoadCursorW(0, w32::IDC_ARROW as *const u16),
            // 系统颜色画笔：索引 + 1（不是 RGB 值）
            hbrBackground: wapi::COLOR_BTNFACE as isize + 1,
            lpszMenuName: std::ptr::null(),
            lpszClassName: lists_class.as_ptr(),
        };
        if w32::RegisterClassW(&wc_lists) == 0 {
            fatal("注册名单对话框窗口类失败");
            return;
        }

        // 开机启动时带 --minimized：只驻留托盘，不弹窗口
        let start_minimized = args.iter().any(|a| a == "--minimized" || a == "/minimized");

        let app = Box::into_raw(Box::new(AppState::new()));
        // 记录启动参数：重启到新版本时原样带上
        (*app).start_args = args.clone();
        let title = w32::w("winmem — 内存优化工具");
        let scale = w32::system_dpi() as f32 / 96.0;
        let win_w = (1180.0 * scale).round() as i32;
        let win_h = (760.0 * scale).round() as i32;
        let hwnd = w32::CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            w32::WS_OVERLAPPEDWINDOW | w32::WS_CLIPCHILDREN,
            w32::CW_USEDEFAULT,
            w32::CW_USEDEFAULT,
            win_w,
            win_h,
            0,
            0,
            hinst,
            app as *mut c_void,
        );
        if hwnd == 0 {
            fatal("创建主窗口失败");
            return;
        }

        let startup_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        if let Some(secs) = bench {
            // 自检：窗口照常创建（因此测的是真实初始化路径），但不显示、不常驻。
            let s = &mut *app;
            s.remove_tray_icon();
            let first_scan_ms = LAST_COLLECT_MS.load(std::sync::atomic::Ordering::Relaxed) as f64;
            run_bench(s, secs, startup_ms, first_scan_ms);
            std::process::exit(0);
        }

        // 上次更新遗留的备份（*.old）在启动时清理；此刻旧进程通常已退出
        if let Some(exe) = winmem::update::self_exe() {
            winmem::update::cleanup_backups(&exe);
        }

        if start_minimized {
            // 只驻留托盘：不弹窗口、不抢焦点
            w32::ShowWindow(hwnd, w32::SW_HIDE);
            w32::SetTimer(hwnd, TIMER_ID, TIMER_MS_HIDDEN, std::ptr::null());
            let s = &mut *app;
            if s.config.auto_enabled {
                s.start_auto_session();
            }
            s.maybe_auto_check_update();
        } else {
            w32::ShowWindow(hwnd, w32::SW_SHOW);
            w32::UpdateWindow(hwnd);
            (*app).maybe_auto_check_update();
        }

        let mut msg: w32::MSG = std::mem::zeroed();
        while w32::GetMessageW(&mut msg, 0, 0, 0) > 0 {
            w32::TranslateMessage(&msg);
            w32::DispatchMessageW(&msg);
        }
    }
}
