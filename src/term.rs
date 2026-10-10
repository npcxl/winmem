//! 终端输出：颜色、单位格式化、表格与进度条。

use crate::ops::ProcInfo;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";

/// 人类可读的容量。
pub fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let v = n as f64;
    if v >= GB {
        format!("{:.2} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else if v >= KB {
        format!("{:.0} KB", v / KB)
    } else {
        format!("{:.0} B", v)
    }
}

/// 进度条。
pub fn bar(pct: f64, width: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let color = if pct >= 90.0 {
        RED
    } else if pct >= 70.0 {
        YELLOW
    } else {
        GREEN
    };
    format!(
        "{}{}{}{}{}",
        color,
        "█".repeat(filled),
        DIM,
        "░".repeat(width.saturating_sub(filled)),
        RESET
    )
}

/// 按字符数截断并补齐（进程名基本为 ASCII）。
fn fit(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(width - count));
        out
    }
}

/// 打印进程内存栈表格。
pub fn print_table(list: &[ProcInfo], show_index: bool) {
    let header = if show_index {
        format!(
            "{}{:>3}  {:>7}  {:<28} {:>11} {:>11} {:>7}  状态{}",
            BOLD, "编号", "PID", "进程名", "工作集", "私有内存", "线程", RESET
        )
    } else {
        format!(
            "{}{:>7}  {:<28} {:>11} {:>11} {:>7}  状态{}",
            BOLD, "PID", "进程名", "工作集", "私有内存", "线程", RESET
        )
    };
    let rule_len = if show_index { 92 } else { 88 };
    println!("{}", header);
    println!("{}", format!("{}{}{}", DIM, "─".repeat(rule_len), RESET));

    for (i, p) in list.iter().enumerate() {
        let status = if !p.readable {
            format!("{}无权读取{}", DIM, RESET)
        } else if crate::ops::is_critical(p.pid, &p.name) {
            format!("{}系统{}", DIM, RESET)
        } else {
            format!("{}可操作{}", GREEN, RESET)
        };

        let name_cell = if p.readable {
            format!("{:<28}", fit(&p.name, 28))
        } else {
            format!("{}{:<28}{}", DIM, fit(&p.name, 28), RESET)
        };

        if show_index {
            println!(
                "{:>3}  {:>7}  {} {:>11} {:>11} {:>7}  {}",
                i + 1,
                p.pid,
                name_cell,
                fmt_bytes(p.working_set),
                fmt_bytes(p.private_bytes),
                p.threads,
                status
            );
        } else {
            println!(
                "{:>7}  {} {:>11} {:>11} {:>7}  {}",
                p.pid,
                name_cell,
                fmt_bytes(p.working_set),
                fmt_bytes(p.private_bytes),
                p.threads,
                status
            );
        }
    }
}
