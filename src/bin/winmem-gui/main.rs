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

/// 采集进程并按统一策略标注状态。
fn collect_rows(cfg: &Config) -> Vec<Row> {
    let mut sampler = metrics::ProcessSampler::new();
    // 先建差分基线
    let _ = sampler.collect(false);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let list = sampler.collect(true);

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
const ID_MENU_TRIM: usize = 2001;
const ID_MENU_KILL: usize = 2002;
const ID_MENU_PREVIEW: usize = 2003;
const ID_MENU_CONFIG: usize = 2004;
const ID_MENU_STANDBY: usize = 2005;
const ID_BTN_SELECT_ALL: usize = 1005;
const ID_BTN_CLEAR_SEL: usize = 1006;
const ID_EDIT_FILTER: usize = 1007;
const ID_LISTVIEW: usize = 1008;

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 400;

/// 列定义：(标题, 宽度, 右对齐)
const COLUMNS: [(&str, i32, bool); 6] = [
    ("PID", 72, true),
    ("进程名", 262, false),
    ("工作集", 110, true),
    ("私有提交", 110, true),
    ("线程", 64, true),
    ("状态", 100, false),
];

const BUTTON_WIDTHS: [i32; 7] = [78, 106, 96, 134, 134, 68, 88];

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
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct AppState {
    hwnd: w32::HWND,
    hlist: w32::HWND,
    hfilter: w32::HWND,
    buttons: Vec<w32::HWND>,

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
            (ID_BTN_SELECT_ALL, "全选"),
            (ID_BTN_CLEAR_SEL, "取消选择"),
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

        let ly = self.px(TOP_H) + self.px(TOOLBAR_H);
        let lh = (h - ly - self.px(STATUS_H) - self.px(10)).max(self.px(80));
        w32::MoveWindow(
            self.hlist,
            self.px(12),
            ly,
            (w - self.px(24)).max(self.px(200)),
            lh,
            1,
        );
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
        self.all = collect_rows(&cfg);
        self.rebuild();
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
            w32::lv_ensure_visible(self.hlist, top_index);
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
            for b in &self.buttons {
                w32::EnableWindow(*b, if busy { 0 } else { 1 });
            }
        }
    }

    fn spawn_task<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(Sender<Task>) + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.set_status(label, 0);
        self.progress = Some((0, 0));
        self.set_busy(true);
        let tx = self.tx.clone();
        std::thread::spawn(move || f(tx));
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
                    self.set_busy(false);
                    self.set_status(&text, kind);
                    if refresh {
                        self.reload();
                    }
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
            move |tx| {
                let mut engine = Engine::new(cfg);
                let cancel = CancelToken::new();
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

        // 启动后台会话
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
        self.set_status("智能自动优化已开启（后台监测中，不阻塞界面）。", 1);
    }

    /// 系统内存只读诊断（内存压缩 / 页面合并 / 分页文件 / 提交量）。
    fn op_sysmem(&mut self) {
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
                winmem::sysmem::Probe::Value(true) => "系统自动管理".to_string(),
                winmem::sysmem::Probe::Value(false) => "手动配置".to_string(),
                other => other.label().to_string(),
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
        s.push_str("\n诊断：\n");
        for d in winmem::sysmem::diagnose(&pf, commit) {
            s.push_str(&format!("  [{}] {}\n", d.severity.label(), d.text));
        }
        if !winmem::sysmem::is_elevated() {
            s.push_str("\n提示：内存压缩/页面合并的查询与全部配置变更需要管理员权限；\n分页文件与提交量诊断在普通权限下可用。\n");
        }
        s.push_str("\n配置变更请在命令行执行：winmem sysmem compression --enable");

        unsafe {
            let t = w32::w(&s);
            let cap = w32::w("系统内存诊断");
            w32::MessageBoxW(self.hwnd, t.as_ptr(), cap.as_ptr(), w32::MB_OK);
        }
        self.set_status("系统内存诊断完成（只读，未修改任何设置）。", 0);
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
        let items: [(usize, &str); 8] = [
            (ID_MENU_TRIM, "释放选中进程的内存"),
            (ID_MENU_KILL, "结束选中进程…"),
            (ID_MENU_PREVIEW, "预览优化候选与跳过原因…"),
            (0, ""),
            (ID_MENU_SYSMEM, "系统内存诊断（压缩 / 分页文件 / 提交量）…"),
            (ID_MENU_CONFIG, "打开配置文件（保护名单 / 允许名单）…"),
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
            ID_MENU_PREVIEW => self.op_preview(),
            ID_MENU_SYSMEM => self.op_sysmem(),
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
            move |tx| {
                let mut engine = Engine::new(cfg);
                let cancel = CancelToken::new();
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

        self.spawn_task("正在结束选中进程…", move |tx| {
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

        // ---------- 工具栏：筛选标签 ----------
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
            "就绪 —— 点「一键优化」可立即回收内存；勾选进程后可单独释放内存或结束进程。"
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
            w32::DestroyWindow(hwnd);
            0
        }
        w32::WM_DESTROY => {
            w32::PostQuitMessage(0);
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
            if s.busy && id != ID_BTN_REFRESH && id != ID_BTN_PREVIEW && id != ID_CHK_AUTO {
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
                (ID_BTN_SELECT_ALL, w32::BN_CLICKED) => {
                    let n = s.rows.len() as i32;
                    for i in 0..n {
                        w32::lv_select_row(s.hlist, i, true);
                    }
                    s.set_status(&format!("已选中 {} 个进程。", n), 0);
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                (ID_BTN_CLEAR_SEL, w32::BN_CLICKED) => {
                    w32::lv_clear_selection(s.hlist);
                    s.set_status("已取消选择。", 0);
                    w32::InvalidateRect(hwnd, std::ptr::null(), 0);
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
// 入口
// ---------------------------------------------------------------------------

fn main() {
    unsafe {
        let icc = w32::INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<w32::INITCOMMONCONTROLSEX>() as u32,
            dwICC: w32::ICC_LISTVIEW_CLASSES,
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
            return;
        }

        let app = Box::into_raw(Box::new(AppState::new()));
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
            return;
        }

        w32::ShowWindow(hwnd, w32::SW_SHOW);
        w32::UpdateWindow(hwnd);

        let mut msg: w32::MSG = std::mem::zeroed();
        while w32::GetMessageW(&mut msg, 0, 0, 0) > 0 {
            w32::TranslateMessage(&msg);
            w32::DispatchMessageW(&msg);
        }
    }
}
