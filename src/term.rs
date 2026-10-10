//! 终端渲染工具：ANSI 颜色、进度条、表格原语。
//!
//! 只负责“怎么显示”，不含任何业务判断。

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";

pub use crate::report::{fmt_bytes, fmt_delta};

/// 按字符数截断（中文按 1 个字符计，终端宽度会略有偏差）。
pub fn truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 按显示宽度补空格（粗算：CJK 记 2 宽）。
pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let mut out = s.to_string();
    out.push_str(&" ".repeat(width - w));
    out
}

/// 粗略显示宽度：CJK / 全角字符按 2 计。
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            let wide = matches!(cp,
                0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 |
                0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 |
                0xFFE0..=0xFFE6 | 0x20000..=0x3FFFD);
            if wide {
                2
            } else {
                1
            }
        })
        .sum()
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

/// 一列的定义：标题、宽度、是否右对齐。
pub struct Column<'a> {
    pub title: &'a str,
    pub width: usize,
    pub right: bool,
}

/// 渲染表格（含表头与分隔线）。
pub fn render_table(columns: &[Column<'_>], rows: &[Vec<String>]) -> String {
    let mut out = String::new();

    let mut header = String::new();
    for c in columns {
        let t = truncate(c.title, c.width);
        header.push_str(&if c.right {
            format!("{:>width$}", t, width = c.width)
        } else {
            pad(&t, c.width)
        });
        header.push_str("  ");
    }
    out.push_str(&format!("{}{}{}\n", BOLD, header.trim_end(), RESET));

    let total: usize = columns.iter().map(|c| c.width + 2).sum();
    out.push_str(&format!(
        "{}{}{}\n",
        DIM,
        "─".repeat(total.saturating_sub(2)),
        RESET
    ));

    for row in rows {
        let mut line = String::new();
        for (i, c) in columns.iter().enumerate() {
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            let t = truncate(cell, c.width);
            line.push_str(&if c.right {
                format!("{:>width$}", t, width = c.width)
            } else {
                pad(&t, c.width)
            });
            line.push_str("  ");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 启用 ANSI 转义（Windows 10+ 控制台）。
pub fn enable_ansi() {
    crate::win::enable_ansi();
}
