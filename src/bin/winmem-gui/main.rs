//! winmem-gui —— winmem 的 Windows 原生图形界面。
//!
//! 纯 Win32 + GDI 手写界面，零第三方依赖：
//! - 顶部：内存总览（使用率进度条 + 提交/缓存/进程统计）
//! - 中部：进程内存栈列表（可点列头排序、可筛选、可多选）
//! - 工具栏：刷新 / 一键优化 / 释放选中内存 / 结束选中进程 / 全选 / 取消选择
//! - 底部：状态栏（操作结果 + 进度 + 权限提示）

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![cfg_attr(
    not(target_pointer_width = "64"),
    allow(unused)
)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("winmem-gui 仅支持 64 位 Windows。");

mod w32;

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use winmem::ops::{self, SortBy};
use winmem::term;
use winmem::win as wapi;

// ---------------------------------------------------------------------------
// 外观
// ---------------------------------------------------------------------------

const C_BG: u32 = w32::rgb(244, 246, 248);
const C_PANEL: u32 = w32::rgb(255, 255, 255);
const C_TEXT: u32 = w32::rgb(30, 34, 40);
const C_DIM: u32 = w32::rgb(122, 130, 140);
const C_BORDER: u32 = w32::rgb(220, 224, 229);
const C_TRACK: u32 = w32::rgb(228, 231, 236);
const C_GREEN: u32 = w32::rgb(46, 160, 82);
const C_YELLOW: u32 = w32::rgb(226, 160, 20);
const C_RED: u32 = w32::rgb(214, 62, 51);
const C_BLUE: u32 = w32::rgb(56, 116, 214);

// 布局
const TOP_H: i32 = 96;
const TOOLBAR_H: i32 = 44;
const STATUS_H: i32 = 58;

// 控件 ID
const ID_BTN_REFRESH: usize = 1001;
const ID_BTN_OPTIMIZE: usize = 1002;
const ID_BTN_TRIM: usize = 1003;
const ID_BTN_KILL: usize = 1004;
const ID_BTN_SELECT_ALL: usize = 1005;
const ID_BTN_CLEAR_SEL: usize = 1006;
const ID_EDIT_FILTER: usize = 1007;
const ID_LISTVIEW: usize = 1008;

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 400;

/// 列定义：(标题, 宽度, 右对齐)
const COLUMNS: [(&str, i32, bool); 7] = [
    ("PID", 72, true),
    ("进程名", 250, false),
    ("工作集", 108, true),
    ("私有内存", 108, true),
    ("页面文件", 108, true),
    ("线程", 64, true),
    ("状态", 96, false),
];

const BUTTON_WIDTHS: [i32; 6] = [78, 106, 134, 134, 68, 88];

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

    /// 全部进程（未过滤）
    all: Vec<ops::ProcInfo>,
    /// 当前显示的顺序
    rows: Vec<ops::ProcInfo>,
    sort: SortBy,
    filter: String,

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
            all: Vec::new(),
            rows: Vec::new(),
            sort: SortBy::Mem,
            filter: String::new(),
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

    fn set_status(&mut self, text: &str, kind: u8) {
        self.status = text.to_string();
        self.status_kind = kind;
    }

    fn status_color(&self) -> u32 {
        match self.status_kind {
            1 => C_GREEN,
            2 => C_YELLOW,
            3 => C_RED,
            _ => C_TEXT,
        }
    }

    // -- 创建控件 ---------------------------------------------------------

    unsafe fn create_controls(&mut self, hwnd: w32::HWND, hinst: w32::HINSTANCE) {
        self.hwnd = hwnd;
        self.font_ui = w32::make_font(-14, w32::FW_NORMAL);
        self.font_title = w32::make_font(-21, w32::FW_SEMIBOLD);
        self.font_small = w32::make_font(-12, w32::FW_NORMAL);

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
            w32::lv_add_column(lv, i as i32, title, *width, *right);
        }
        self.hlist = lv;

        // 工具栏按钮
        let class_btn = w32::w("BUTTON");
        let labels = [
            (ID_BTN_REFRESH, "刷新"),
            (ID_BTN_OPTIMIZE, "一键优化"),
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
        w32::SendMessageW(
            edit,
            w32::EM_SETCUEBANNER,
            1,
            cue.as_ptr() as w32::LPARAM,
        );
        self.hfilter = edit;
    }

    // -- 布局 -------------------------------------------------------------

    unsafe fn layout(&self) {
        let mut rc = w32::RECT::default();
        w32::GetClientRect(self.hwnd, &mut rc);
        let w = rc.width();
        let h = rc.height();

        let by = TOP_H + 8;
        let bh = 28;
        let mut x = 16;
        for (i, btn) in self.buttons.iter().enumerate() {
            let bw = BUTTON_WIDTHS[i];
            w32::MoveWindow(*btn, x, by, bw, bh, 1);
            x += bw + 8;
        }
        w32::MoveWindow(self.hfilter, w - 240, by + 1, 224, 26, 1);

        let ly = TOP_H + TOOLBAR_H;
        let lh = (h - ly - STATUS_H - 10).max(80);
        w32::MoveWindow(self.hlist, 12, ly, (w - 24).max(200), lh, 1);
    }

    // -- 数据刷新 ---------------------------------------------------------

    fn refresh_memory(&mut self) {
        self.mem = wapi::memory_status();
        self.perf = wapi::performance_info();
    }

    /// 重新枚举进程并刷新列表（不影响第一行操作结果）。
    fn reload(&mut self) {
        self.refresh_memory();
        self.all = ops::list_processes();
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
        self.rows = ops::refine(self.all.clone(), self.sort, filter.as_deref(), None);

        let me = wapi::current_pid();
        unsafe {
            w32::lv_clear(self.hlist);
            for (i, p) in self.rows.iter().enumerate() {
                let state = if !p.readable {
                    "无权读取"
                } else if p.pid == me {
                    "本程序"
                } else if ops::is_critical(p.pid, &p.name) {
                    "系统"
                } else {
                    "可操作"
                };
                let cells = vec![
                    p.pid.to_string(),
                    p.name.clone(),
                    term::fmt_bytes(p.working_set),
                    term::fmt_bytes(p.private_bytes),
                    term::fmt_bytes(p.pagefile),
                    p.threads.to_string(),
                    state.to_string(),
                ];
                w32::lv_add_row(self.hlist, i as i32, &cells);
            }
            for pid in selected {
                if let Some(idx) = self.rows.iter().position(|r| r.pid == pid) {
                    w32::lv_select_row(self.hlist, idx as i32, true);
                }
            }
        }

        let sum: u64 = self.rows.iter().map(|p| p.working_set).sum();
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

    fn selected_procs(&self) -> Vec<ops::ProcInfo> {
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
        self.spawn_task("正在一键优化：清空待机列表并释放各进程工作集…", |tx| {
            let before = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let standby_ok = wapi::purge_standby_list().is_ok();
            let me = wapi::current_pid();

            let targets: Vec<ops::ProcInfo> = ops::list_processes()
                .into_iter()
                .filter(|p| p.working_set > 0 && p.pid != me && !ops::is_critical(p.pid, &p.name))
                .collect();

            let total = targets.len();
            let mut freed = 0u64;
            let mut fail = 0usize;
            for (i, p) in targets.iter().enumerate() {
                let r = ops::trim_process_with(p.pid, &p.name, 5);
                if r.error.is_none() {
                    freed += r.freed;
                } else {
                    fail += 1;
                }
                let _ = tx.send(Task::Progress {
                    done: i + 1,
                    total,
                });
            }

            let after = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let text = format!(
                "优化完成：处理 {} 个进程（失败 {}），释放工作集 {}{}；可用内存 {} → {}（+{}）",
                total,
                fail,
                term::fmt_bytes(freed),
                if standby_ok {
                    "，已清空待机列表"
                } else {
                    "，待机列表需管理员权限"
                },
                term::fmt_bytes(before),
                term::fmt_bytes(after),
                term::fmt_bytes(after.saturating_sub(before))
            );
            let _ = tx.send(Task::Done {
                text,
                kind: if fail > 0 { 2 } else { 1 },
                refresh: true,
            });
        });
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
        self.spawn_task("正在释放选中进程的内存…", move |tx| {
            let before = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let total = procs.len();
            let mut freed = 0u64;
            let mut fail = 0usize;
            for (i, p) in procs.iter().enumerate() {
                let r = ops::trim_process_with(p.pid, &p.name, 8);
                if r.error.is_none() {
                    freed += r.freed;
                } else {
                    fail += 1;
                }
                let _ = tx.send(Task::Progress {
                    done: i + 1,
                    total,
                });
            }
            let after = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let text = format!(
                "已释放 {} 个进程的内存：回收工作集 {}（失败 {}），可用内存 {} → {}（+{}）",
                total,
                term::fmt_bytes(freed),
                fail,
                term::fmt_bytes(before),
                term::fmt_bytes(after),
                term::fmt_bytes(after.saturating_sub(before))
            );
            let _ = tx.send(Task::Done {
                text,
                kind: if fail > 0 { 2 } else { 1 },
                refresh: true,
            });
        });
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

        let (blocked, killable): (Vec<_>, Vec<_>) = procs
            .into_iter()
            .partition(|p| ops::is_critical(p.pid, &p.name));

        unsafe {
            if !blocked.is_empty() {
                let names: Vec<String> = blocked
                    .iter()
                    .take(10)
                    .map(|p| format!("  {}（PID {}）", p.name, p.pid))
                    .collect();
                let msg = format!(
                    "以下 {} 个进程属于系统关键进程，已自动跳过：\n\n{}{}",
                    blocked.len(),
                    names.join("\n"),
                    if blocked.len() > 10 { "\n  …" } else { "" }
                );
                let text = w32::w(&msg);
                let cap = w32::w("已跳过系统关键进程");
                w32::MessageBoxW(self.hwnd, text.as_ptr(), cap.as_ptr(), w32::MB_OK | w32::MB_ICONINFORMATION);
            }
        }

        if killable.is_empty() {
            self.set_status("选中的进程全部是系统关键进程，已阻止操作。", 3);
            unsafe {
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
            }
            return;
        }

        unsafe {
            let names: Vec<String> = killable
                .iter()
                .take(12)
                .map(|p| format!("  {}（PID {}，私有内存 {}）", p.name, p.pid, term::fmt_bytes(p.private_bytes)))
                .collect();
            let total: u64 = killable.iter().map(|p| p.private_bytes).sum();
            let msg = format!(
                "确定要结束以下 {} 个进程吗？\n\n{}{}\n\n预计释放私有内存约 {}。\n未保存的数据将会丢失！",
                killable.len(),
                names.join("\n"),
                if killable.len() > 12 { "\n  …" } else { "" },
                term::fmt_bytes(total)
            );
            let text = w32::w(&msg);
            let cap = w32::w("确认结束进程");
            if w32::MessageBoxW(self.hwnd, text.as_ptr(), cap.as_ptr(), w32::MB_YESNO | w32::MB_ICONWARNING) != w32::IDYES {
                self.set_status("已取消结束进程。", 0);
                w32::InvalidateRect(self.hwnd, std::ptr::null(), 0);
                return;
            }
        }

        self.spawn_task("正在结束选中进程…", move |tx| {
            let before = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let total = killable.len();
            let mut ok = 0usize;
            let mut fail = 0usize;
            for (i, p) in killable.iter().enumerate() {
                match wapi::terminate_process(p.pid) {
                    Ok(()) => ok += 1,
                    Err(_) => fail += 1,
                }
                let _ = tx.send(Task::Progress {
                    done: i + 1,
                    total,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
            let after = wapi::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
            let text = format!(
                "已结束 {} 个进程（失败 {}），可用内存 {} → {}（+{}）",
                ok,
                fail,
                term::fmt_bytes(before),
                term::fmt_bytes(after),
                term::fmt_bytes(after.saturating_sub(before))
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

        // 整体背景
        w32::fill_rect(hdc, &rc, C_BG);

        // ---------- 顶部：内存总览 ----------
        let top = w32::RECT {
            left: 0,
            top: 0,
            right: rc.right,
            bottom: TOP_H,
        };
        w32::fill_rect(hdc, &top, C_PANEL);
        let top_line = w32::RECT {
            left: 0,
            top: TOP_H - 1,
            right: rc.right,
            bottom: TOP_H,
        };
        w32::fill_rect(hdc, &top_line, C_BORDER);

        let title = w32::RECT {
            left: 18,
            top: 12,
            right: rc.right - 260,
            bottom: 40,
        };
        w32::draw_text(
            hdc,
            "winmem · 内存优化",
            title,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE,
            C_TEXT,
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
            let color = pressure_color(pct);

            let ur = w32::RECT {
                left: rc.right - 260,
                top: 14,
                right: rc.right - 18,
                bottom: 38,
            };
            w32::draw_text(
                hdc,
                &format!("占用 {:.1}%", pct),
                ur,
                w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
                color,
                self.font_title,
            );

            // 进度条
            let bar = w32::RECT {
                left: 18,
                top: 48,
                right: rc.right - 18,
                bottom: 62,
            };
            w32::fill_rect(hdc, &bar, C_TRACK);
            let full = bar.width().max(1);
            let fill_w = ((full as f64) * (pct / 100.0)).round() as i32;
            if fill_w > 0 {
                let fill = w32::RECT {
                    left: bar.left,
                    top: bar.top,
                    right: bar.left + fill_w.min(full),
                    bottom: bar.bottom,
                };
                w32::fill_rect(hdc, &fill, color);
            }

            // 统计行
            let mut stats = format!(
                "物理内存 {}/{}    可用 {}",
                term::fmt_bytes(used),
                term::fmt_bytes(total),
                term::fmt_bytes(avail)
            );
            if let Some(pi) = &self.perf {
                let page = pi.PageSize.max(1) as u64;
                stats.push_str(&format!(
                    "    提交 {}/{}    系统缓存 {}    进程 {} / 线程 {}",
                    term::fmt_bytes(pi.CommitTotal as u64 * page),
                    term::fmt_bytes(pi.CommitLimit as u64 * page),
                    term::fmt_bytes(pi.SystemCache as u64 * page),
                    pi.ProcessCount,
                    pi.ThreadCount
                ));
            }
            let sr = w32::RECT {
                left: 18,
                top: 68,
                right: rc.right - 18,
                bottom: 90,
            };
            w32::draw_text(
                hdc,
                &stats,
                sr,
                w32::DT_LEFT
                    | w32::DT_VCENTER
                    | w32::DT_SINGLELINE
                    | w32::DT_END_ELLIPSIS,
                C_DIM,
                self.font_small,
            );
        }

        // ---------- 工具栏：筛选标签 ----------
        let label = w32::RECT {
            left: rc.right - 310,
            top: TOP_H + 8,
            right: rc.right - 244,
            bottom: TOP_H + 36,
        };
        w32::draw_text(
            hdc,
            "筛选",
            label,
            w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
            C_TEXT,
            self.font_ui,
        );

        // ---------- 底部：状态栏 ----------
        let sbar = w32::RECT {
            left: 0,
            top: rc.bottom - STATUS_H,
            right: rc.right,
            bottom: rc.bottom,
        };
        w32::fill_rect(hdc, &sbar, C_PANEL);
        let sline = w32::RECT {
            left: 0,
            top: rc.bottom - STATUS_H,
            right: rc.right,
            bottom: rc.bottom - STATUS_H + 1,
        };
        w32::fill_rect(hdc, &sline, C_BORDER);

        let text = if self.status.is_empty() {
            "就绪".to_string()
        } else {
            self.status.clone()
        };
        let tr = w32::RECT {
            left: 18,
            top: rc.bottom - STATUS_H + 8,
            right: rc.right - 200,
            bottom: rc.bottom - STATUS_H + 32,
        };
        w32::draw_text(
            hdc,
            &text,
            tr,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE | w32::DT_END_ELLIPSIS,
            self.status_color(),
            self.font_ui,
        );

        if let Some((done, total)) = self.progress {
            if total > 0 {
                let pr = w32::RECT {
                    left: rc.right - 200,
                    top: rc.bottom - STATUS_H + 8,
                    right: rc.right - 18,
                    bottom: rc.bottom - STATUS_H + 32,
                };
                w32::draw_text(
                    hdc,
                    &format!("{}/{}", done, total),
                    pr,
                    w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
                    C_BLUE,
                    self.font_ui,
                );
            }
        }

        // 第二行左侧：列表统计
        let sr2 = w32::RECT {
            left: 18,
            top: rc.bottom - STATUS_H + 30,
            right: (rc.right - 270).max(200),
            bottom: rc.bottom - STATUS_H + 52,
        };
        w32::draw_text(
            hdc,
            &self.stats,
            sr2,
            w32::DT_LEFT | w32::DT_VCENTER | w32::DT_SINGLELINE | w32::DT_END_ELLIPSIS,
            C_DIM,
            self.font_small,
        );

        // 第二行右侧：权限提示
        let perm = if wapi::is_elevated() {
            "管理员权限：可操作系统进程与待机列表"
        } else {
            "普通权限：部分系统进程不可操作"
        };
        let pr2 = w32::RECT {
            left: (rc.right - 270).max(200),
            top: rc.bottom - STATUS_H + 30,
            right: rc.right - 18,
            bottom: rc.bottom - STATUS_H + 52,
        };
        w32::draw_text(
            hdc,
            perm,
            pr2,
            w32::DT_RIGHT | w32::DT_VCENTER | w32::DT_SINGLELINE,
            C_DIM,
            self.font_small,
        );

        w32::EndPaint(self.hwnd, &ps);
    }
}

fn pressure_color(pct: f64) -> u32 {
    if pct >= 90.0 {
        C_RED
    } else if pct >= 70.0 {
        C_YELLOW
    } else {
        C_GREEN
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
        (*ptr).status = "就绪 —— 点「一键优化」可立即回收内存；勾选进程后可单独释放内存或结束进程。".to_string();
        (*ptr).reload();
        return 0;
    }

    let Some(s) = state(hwnd) else {
        return w32::DefWindowProcW(hwnd, msg, wp, lp);
    };

    match msg {
        w32::WM_SIZE => {
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
            mmi.ptMinTrackSize.x = 940;
            mmi.ptMinTrackSize.y = 560;
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
            let id = (wp & 0xFFFF) as usize;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            if s.busy && id != ID_BTN_REFRESH {
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
            if hdr.code as i32 == w32::LVN_COLUMNCLICK {
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
        let hwnd = w32::CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            w32::WS_OVERLAPPEDWINDOW | w32::WS_CLIPCHILDREN,
            w32::CW_USEDEFAULT,
            w32::CW_USEDEFAULT,
            1180,
            760,
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
