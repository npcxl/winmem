//! winmem —— Windows 内存优化与进程内存栈管理工具
//!
//! 用法：
//!   winmem status                 查看内存总览
//!   winmem list [--top N] [--sort mem|private|name|pid] [--filter 关键字]
//!   winmem trim (--pid 1234,5678 | --name chrome.exe | --top 10 | --all)
//!   winmem optimize               一键优化（清空待机列表 + 释放工作集）
//!   winmem kill (--pid 1234 | --name 记事本.exe) [--force] [--yes]
//!   winmem interactive            交互式：列出内存栈 → 选择性地关闭 / 释放

#[cfg(not(windows))]
compile_error!("winmem 仅支持 Windows 平台。");

use winmem::ops::{self, SortBy};
use winmem::term as ui;
use winmem::win;

use std::collections::HashMap;
use std::io::{self, Write};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    win::enable_ansi();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        cmd_status();
        return;
    }

    let (pos, opts) = parse_args(&raw);
    let cmd = pos.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let rest: Vec<String> = pos.iter().skip(1).cloned().collect();

    match cmd.as_str() {
        "status" | "st" | "stat" | "info" => cmd_status(),
        "list" | "ls" | "ps" | "top" => cmd_list(&opts, &rest),
        "trim" | "release" | "free" => cmd_trim(&opts),
        "optimize" | "opt" | "clean" | "boost" => cmd_optimize(),
        "kill" | "close" | "end" => cmd_kill(&opts, &rest),
        "interactive" | "i" | "shell" | "ui" => interactive(),
        "help" | "-h" | "--help" | "?" => print_help(),
        "version" | "-v" | "--version" => println!("winmem {}", VERSION),
        other => {
            eprintln!("{}未知命令：{}{}", ui::RED, other, ui::RESET);
            println!();
            print_help();
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

/// 需要跟值的选项名。
const VALUED_OPTS: &[&str] = &[
    "pid", "name", "top", "sort", "filter", "action",
];

fn parse_args(args: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut pos = Vec::new();
    let mut opts = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = if let Some(rest) = a.strip_prefix("--") {
            match rest.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (rest.to_string(), None),
            }
        } else if let Some(rest) = a.strip_prefix('-') {
            if rest.is_empty() {
                pos.push(a.clone());
                i += 1;
                continue;
            }
            match rest.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (rest.to_string(), None),
            }
        } else {
            pos.push(a.clone());
            i += 1;
            continue;
        };

        if let Some(v) = inline {
            opts.insert(key, v);
        } else if VALUED_OPTS.contains(&key.as_str())
            && args.get(i + 1).map(|n| !n.starts_with('-')).unwrap_or(false)
        {
            opts.insert(key, args[i + 1].clone());
            i += 1;
        } else {
            opts.insert(key, "true".to_string());
        }
        i += 1;
    }
    (pos, opts)
}

fn flag(opts: &HashMap<String, String>, key: &str) -> bool {
    opts.contains_key(key)
}

fn value<'a>(opts: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    opts.get(key).map(|s| s.as_str())
}

/// 解析逗号 / 空格分隔的 PID 列表。
fn parse_pids(s: &str) -> Vec<u32> {
    s.split([',', ';', ' '])
        .filter_map(|x| x.trim().parse::<u32>().ok())
        .collect()
}

// ---------------------------------------------------------------------------
// 命令实现
// ---------------------------------------------------------------------------

fn cmd_status() {
    print_status();
    println!();
    println!(
        "{}提示{}：想看内存栈明细用 {}winmem list{}，想一键优化用 {}winmem optimize{}。",
        ui::DIM,
        ui::RESET,
        ui::CYAN,
        ui::RESET,
        ui::CYAN,
        ui::RESET
    );
}

fn print_status() {
    let Some(ms) = win::memory_status() else {
        eprintln!("{}无法获取内存状态{}", ui::RED, ui::RESET);
        return;
    };
    let total = ms.ullTotalPhys;
    let avail = ms.ullAvailPhys;
    let used = total.saturating_sub(avail);
    let used_pct = if total > 0 {
        used as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    println!("{}winmem{} {}v{}   Windows 内存优化工具{}", ui::CYAN, ui::RESET, ui::DIM, VERSION, ui::RESET);
    println!();
    println!(
        "  物理内存   总共 {}{}{}   已用 {}{}{}   可用 {}{}{}",
        ui::DIM, ui::fmt_bytes(total), ui::RESET,
        ui::DIM, ui::fmt_bytes(used), ui::RESET,
        ui::GREEN, ui::fmt_bytes(avail), ui::RESET
    );
    println!("  {}  {:.1}%", ui::bar(used_pct, 36), used_pct);

    if let Some(pi) = win::performance_info() {
        let page_size = pi.PageSize.max(1) as u64;
        let commit_total = pi.CommitTotal as u64 * page_size;
        let commit_limit = pi.CommitLimit as u64 * page_size;
        let cache = pi.SystemCache as u64 * page_size;
        let kernel_total = (pi.KernelPaged as u64 + pi.KernelNonpaged as u64) * page_size;
        let commit_pct = if commit_limit > 0 {
            commit_total as f64 / commit_limit as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  提交内存   {} / {}   {:.1}%",
            ui::fmt_bytes(commit_total),
            ui::fmt_bytes(commit_limit),
            commit_pct
        );
        println!("  系统缓存   {}", ui::fmt_bytes(cache));
        println!("  内核占用   {}", ui::fmt_bytes(kernel_total));
        println!("  进程/线程  {} / {}", pi.ProcessCount, pi.ThreadCount);
    }

    let mem_load = ms.dwMemoryLoad;
    if mem_load >= 90 {
        println!();
        println!(
            "  {}{}内存压力很高（{}%），建议执行 {}winmem optimize{} 释放内存。{}",
            ui::BOLD, ui::RED, mem_load, ui::CYAN, ui::RESET, ui::RESET
        );
    }
}

fn cmd_list(opts: &HashMap<String, String>, rest: &[String]) {
    let sort = value(opts, "sort")
        .and_then(SortBy::parse)
        .unwrap_or(SortBy::Mem);
    let filter = value(opts, "filter");
    let top = value(opts, "top").and_then(|v| v.parse::<usize>().ok());
    let show_all = flag(opts, "all");

    // 位置参数作为过滤关键字
    let filter = filter
        .map(|s| s.to_string())
        .or_else(|| rest.first().cloned());

    let top = if show_all { None } else { top.or(Some(30)) };

    let list = ops::refine(ops::list_processes(), sort, filter.as_deref(), top);
    println!();
    println!(
        "{}进程内存栈{}（按 {}{}{} 排序，共显示 {}{}{} 条）",
        ui::BOLD,
        ui::RESET,
        ui::CYAN,
        sort_label(sort),
        ui::RESET,
        ui::CYAN,
        list.len(),
        ui::RESET
    );
    println!();
    ui::print_table(&list, false);
    println!();
    let sum: u64 = list.iter().map(|p| p.working_set).sum();
    println!("  {}以上进程工作集合计：{}{}", ui::DIM, ui::fmt_bytes(sum), ui::RESET);
    println!(
        "  {}关闭进程：{}winmem kill --pid <PID>{}   {}查看全部：{}winmem list --all{}",
        ui::DIM, ui::CYAN, ui::RESET, ui::DIM, ui::CYAN, ui::RESET
    );
    println!(
        "  {}释放内存：{}winmem trim --pid <PID>{}   一键优化：{}winmem optimize{}   {}交互模式：{}winmem interactive{}",
        ui::DIM, ui::CYAN, ui::RESET, ui::CYAN, ui::RESET, ui::DIM, ui::CYAN, ui::RESET
    );
}

fn sort_label(s: SortBy) -> &'static str {
    match s {
        SortBy::Mem => "工作集",
        SortBy::Private => "私有内存",
        SortBy::Name => "名称",
        SortBy::Pid => "PID",
    }
}

fn cmd_trim(opts: &HashMap<String, String>) {
    let targets: Vec<ops::ProcInfo> = if let Some(p) = value(opts, "pid") {
        let pids = parse_pids(p);
        let all = ops::list_processes();
        all.into_iter().filter(|i| pids.contains(&i.pid)).collect()
    } else if let Some(n) = value(opts, "name") {
        ops::find(None, Some(n))
    } else if let Some(t) = value(opts, "top").and_then(|v| v.parse::<usize>().ok()) {
        ops::refine(ops::list_processes(), SortBy::Mem, None, Some(t))
    } else {
        // 默认：内存占用最高的 15 个非关键进程
        ops::refine(ops::list_processes(), SortBy::Mem, None, None)
            .into_iter()
            .filter(|p| p.working_set > 0 && !ops::is_critical(p.pid, &p.name))
            .take(15)
            .collect()
    };

    if targets.is_empty() {
        println!("{}没有匹配到可优化的进程。{}", ui::YELLOW, ui::RESET);
        return;
    }

    let with_ws: Vec<_> = targets.iter().filter(|p| p.working_set > 0).collect();
    if with_ws.is_empty() {
        println!("{}匹配到的进程工作集均为 0，无需优化。{}", ui::YELLOW, ui::RESET);
        return;
    }

    println!();
    println!("{}开始释放 {} 个进程的工作集…{}\n", ui::CYAN, with_ws.len(), ui::RESET);

    let mut total = 0u64;
    let mut ok = 0usize;
    let mut fail = 0usize;
    for p in &with_ws {
        let r = ops::trim_process(p.pid, &p.name);
        match r.error {
            None => {
                ok += 1;
                total += r.freed;
                println!(
                    "  {}✓{} {:<28} {} 释放 {}",
                    ui::GREEN, ui::RESET, trunc(&r.name), pid_fmt(r.pid), ui::fmt_bytes(r.freed)
                );
            }
            Some(e) => {
                fail += 1;
                println!(
                    "  {}✗{} {:<28} {} {}",
                    ui::RED, ui::RESET, trunc(&r.name), pid_fmt(r.pid), e
                );
            }
        }
    }

    println!();
    println!(
        "{}完成{}：成功 {}{}{} 个，失败 {}{}{} 个，共释放 {}{}{} 工作集。",
        ui::BOLD, ui::RESET,
        ui::GREEN, ok, ui::RESET,
        ui::YELLOW, fail, ui::RESET,
        ui::GREEN, ui::fmt_bytes(total), ui::RESET
    );
    if !win::is_elevated() {
        println!(
            "  {}提示：以管理员身份运行可优化更多系统进程。{}",
            ui::DIM, ui::RESET
        );
    }
}

fn cmd_optimize() {
    println!();
    println!("{}正在执行一键内存优化…{}", ui::CYAN, ui::RESET);
    println!(
        "  {}（清空系统待机列表 + 释放全部可操作进程的工作集）{}",
        ui::DIM, ui::RESET
    );
    println!();

    let report = ops::optimize();

    match &report.standby_ok {
        Some(Ok(())) => println!("  {}✓{} 已清空系统待机列表（Standby List）", ui::GREEN, ui::RESET),
        Some(Err(code)) => println!(
            "  {}✗{} 清空待机列表失败（NTSTATUS 0x{:08X}），请以管理员身份运行",
            ui::YELLOW, ui::RESET, *code as u32
        ),
        None => {}
    }
    println!(
        "  {}✓{} 已释放 {}{}{} 个进程的工作集，失败 {}{}{} 个（跳过 {} 个系统关键进程）",
        ui::GREEN, ui::RESET,
        ui::GREEN, report.trimmed, ui::RESET,
        ui::YELLOW, report.failed, ui::RESET,
        report.skipped_critical
    );

    println!();
    let gained = report.avail_after.saturating_sub(report.avail_before);
    println!(
        "  可用物理内存：{} → {}{}{}  （增加 {}{}{}）",
        ui::fmt_bytes(report.avail_before),
        ui::BOLD, ui::fmt_bytes(report.avail_after), ui::RESET,
        ui::GREEN, ui::fmt_bytes(gained), ui::RESET
    );
    println!(
        "  累计释放工作集：{}{}{}（该部分会转入待机列表，需要时仍可被系统复用）",
        ui::GREEN, ui::fmt_bytes(report.freed_total), ui::RESET
    );
    if !win::is_elevated() {
        println!();
        println!(
            "  {}提示：以管理员身份运行效果更佳（可操作系统进程与待机列表）。{}",
            ui::DIM, ui::RESET
        );
    }
}

fn cmd_kill(opts: &HashMap<String, String>, rest: &[String]) {
    let force = flag(opts, "force");
    let skip_confirm = flag(opts, "yes") || flag(opts, "y");

    let mut targets: Vec<ops::ProcInfo> = Vec::new();
    if let Some(p) = value(opts, "pid") {
        let pids = parse_pids(p);
        targets = ops::list_processes()
            .into_iter()
            .filter(|i| pids.contains(&i.pid))
            .collect();
    } else if let Some(n) = value(opts, "name") {
        targets = ops::find(None, Some(n));
    } else if let Some(pid) = rest.first().and_then(|s| s.parse::<u32>().ok()) {
        targets = ops::find(Some(pid), None);
    } else if let Some(name) = rest.first() {
        targets = ops::find(None, Some(name));
    }

    if targets.is_empty() {
        println!("{}没有匹配到任何进程。{}", ui::YELLOW, ui::RESET);
        println!(
            "  用法：{}winmem kill --pid 1234{} 或 {}winmem kill --name chrome.exe{}",
            ui::CYAN, ui::RESET, ui::CYAN, ui::RESET
        );
        return;
    }

    let blocked: Vec<&ops::ProcInfo> = targets
        .iter()
        .filter(|p| ops::is_critical(p.pid, &p.name))
        .collect();
    let killable: Vec<ops::ProcInfo> = targets
        .iter()
        .filter(|p| !ops::is_critical(p.pid, &p.name) || (force && p.pid > 4 && p.pid != win::current_pid()))
        .cloned()
        .collect();

    println!();
    println!("{}准备结束以下进程：{}", ui::BOLD, ui::RESET);
    println!();
    ui::print_table(&killable, false);

    if !blocked.is_empty() {
        println!();
        println!(
            "  {}{}已自动跳过 {} 个系统关键进程（如需强制请加 --force，但可能导致系统不稳定）：{}",
            ui::YELLOW,
            ui::RESET,
            blocked.len(),
            blocked
                .iter()
                .map(|p| format!("{}({})", p.name, p.pid))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if killable.is_empty() {
        println!();
        println!("{}没有可以安全结束的进程。{}", ui::YELLOW, ui::RESET);
        return;
    }

    let total_mem: u64 = killable.iter().map(|p| p.private_bytes).sum();
    println!();
    println!(
        "  将释放约 {}{}{} 私有内存。",
        ui::GREEN, ui::fmt_bytes(total_mem), ui::RESET
    );

    if !skip_confirm && !confirm("确认结束以上进程？未保存的数据可能丢失") {
        println!("{}已取消。{}", ui::DIM, ui::RESET);
        return;
    }

    println!();
    let mut ok = 0usize;
    for p in &killable {
        match win::terminate_process(p.pid) {
            Ok(()) => {
                ok += 1;
                println!("  {}✓{} 已结束 {:<28} {}", ui::GREEN, ui::RESET, trunc(&p.name), pid_fmt(p.pid));
            }
            Err(code) => println!(
                "  {}✗{} 无法结束 {:<28} {} （错误码 {}）",
                ui::RED, ui::RESET, trunc(&p.name), pid_fmt(p.pid), code
            ),
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    let avail = win::memory_status().map(|m| m.ullAvailPhys).unwrap_or(0);
    println!();
    println!(
        "{}完成{}：成功结束 {}{}{} 个进程，当前可用物理内存 {}{}{}。",
        ui::BOLD, ui::RESET, ui::GREEN, ok, ui::RESET, ui::GREEN, ui::fmt_bytes(avail), ui::RESET
    );
}

// ---------------------------------------------------------------------------
// 交互模式
// ---------------------------------------------------------------------------

fn interactive() {
    println!();
    println!("{}进入交互模式{}", ui::BOLD, ui::CYAN);
    println!(
        "  {}输入编号选择进程（支持 1,3,5-8），再选择操作；输入 t 做一键优化，q 退出。{}",
        ui::DIM, ui::RESET
    );

    loop {
        println!();
        print_status();
        println!();

        let list = ops::refine(ops::list_processes(), SortBy::Mem, None, Some(40));
        if list.is_empty() {
            println!("{}未枚举到任何进程。{}", ui::YELLOW, ui::RESET);
            return;
        }
        println!("{}内存占用最高的 {} 个进程：{}", ui::BOLD, list.len(), ui::RESET);
        println!();
        ui::print_table(&list, true);

        println!();
        print!("{}请选择进程编号（如 1,3,5-8）／ t 一键优化 ／ q 退出：{}", ui::CYAN, ui::RESET);
        let _ = io::stdout().flush();
        let input = read_line();
        let input = input.trim().to_ascii_lowercase();

        if input.is_empty() {
            continue;
        }
        if input == "q" || input == "quit" || input == "exit" {
            println!("{}再见。{}", ui::DIM, ui::RESET);
            return;
        }
        if input == "t" || input == "opt" {
            cmd_optimize();
            continue;
        }

        let idx = parse_selection(&input, list.len());
        if idx.is_empty() {
            println!("{}输入无效，请重新选择。{}", ui::RED, ui::RESET);
            continue;
        }

        let picked: Vec<ops::ProcInfo> = idx.iter().map(|&i| list[i].clone()).collect();
        println!();
        println!("{}已选择 {} 个进程：{}", ui::BOLD, picked.len(), ui::RESET);
        println!();
        ui::print_table(&picked, false);

        println!();
        print!(
            "{}操作：{}[{}k{}] 结束进程   [{}t{}] 释放内存(不结束)   [{}c{}] 取消 ：{}",
            ui::CYAN, ui::RESET,
            ui::RED, ui::RESET,
            ui::GREEN, ui::RESET,
            ui::DIM, ui::RESET,
            ui::RESET
        );
        let _ = io::stdout().flush();
        let action = read_line().trim().to_ascii_lowercase();

        match action.as_str() {
            "k" | "kill" | "close" => {
                let killable: Vec<ops::ProcInfo> = picked
                    .iter()
                    .filter(|p| !ops::is_critical(p.pid, &p.name))
                    .cloned()
                    .collect();
                let skipped = picked.len() - killable.len();
                if skipped > 0 {
                    println!(
                        "  {}{} 个系统关键进程已自动跳过。{}",
                        ui::YELLOW, skipped, ui::RESET
                    );
                }
                if killable.is_empty() {
                    println!("{}没有可结束的进程。{}", ui::YELLOW, ui::RESET);
                    continue;
                }
                if !confirm("确认结束？未保存的数据可能丢失") {
                    println!("{}已取消。{}", ui::DIM, ui::RESET);
                    continue;
                }
                println!();
                for p in &killable {
                    match win::terminate_process(p.pid) {
                        Ok(()) => {
                            println!("  {}✓{} 已结束 {:<28} {}", ui::GREEN, ui::RESET, trunc(&p.name), pid_fmt(p.pid))
                        }
                        Err(code) => println!(
                            "  {}✗{} 无法结束 {:<28} {} （错误码 {}）",
                            ui::RED, ui::RESET, trunc(&p.name), pid_fmt(p.pid), code
                        ),
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            "t" | "trim" | "free" => {
                println!();
                let mut total = 0u64;
                for p in &picked {
                    if p.working_set == 0 {
                        continue;
                    }
                    let r = ops::trim_process(p.pid, &p.name);
                    match r.error {
                        None => {
                            total += r.freed;
                            println!(
                                "  {}✓{} {:<28} 释放 {}",
                                ui::GREEN, ui::RESET, trunc(&r.name), ui::fmt_bytes(r.freed)
                            );
                        }
                        Some(e) => println!(
                            "  {}✗{} {:<28} {}",
                            ui::RED, ui::RESET, trunc(&r.name), e
                        ),
                    }
                }
                println!();
                println!("  {}共释放 {}{}", ui::DIM, ui::fmt_bytes(total), ui::RESET);
            }
            _ => println!("{}已取消。{}", ui::DIM, ui::RESET),
        }
    }
}

/// 解析 "1,3,5-8" 之类的选择，返回 0 基下标。
fn parse_selection(s: &str, max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split([',', '，', ' ']).map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                let (a, b) = (a.min(b), a.max(b));
                for n in a..=b.min(max) {
                    if n >= 1 {
                        out.push(n - 1);
                    }
                }
            }
        } else if let Ok(n) = part.parse::<usize>() {
            if n >= 1 && n <= max {
                out.push(n - 1);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn read_line() -> String {
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) => std::process::exit(0),
        Ok(_) => sanitize(&s),
        Err(_) => String::new(),
    }
}

/// 清洗输入：去掉 BOM、零宽字符等不可见字符（PowerShell 管道常会带上）。
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c, '\u{feff}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2060}'))
        .collect()
}

fn confirm(prompt: &str) -> bool {
    print!("{}{} [y/N]：{}", ui::YELLOW, prompt, ui::RESET);
    let _ = io::stdout().flush();
    let ans = read_line().trim().to_ascii_lowercase();
    ans == "y" || ans == "yes" || ans == "是"
}

fn trunc(s: &str) -> String {
    let mut out: String = s.chars().take(26).collect();
    if s.chars().count() > 26 {
        out.push('…');
    }
    out
}

fn pid_fmt(pid: u32) -> String {
    format!("{}PID {:<6}{}", ui::DIM, pid, ui::RESET)
}

fn print_help() {
    println!(
        "{}{}winmem {} —— Windows 内存优化与进程内存栈管理工具{}",
        ui::BOLD, ui::CYAN, VERSION, ui::RESET
    );
    println!();
    println!("{}用法：{}", ui::BOLD, ui::RESET);
    println!("  winmem                           查看内存总览");
    println!("  winmem status                    查看内存总览");
    println!("  winmem list [选项]               列出进程内存栈（默认按工作集降序取前 30）");
    println!("      --top N                      只显示前 N 条");
    println!("      --all                        显示全部进程");
    println!("      --sort mem|private|name|pid  排序方式（默认 mem，工作集）");
    println!("      --filter 关键字               按进程名过滤");
    println!("  winmem trim [选项]               释放指定进程的工作集（不结束进程）");
    println!("      --pid 1234,5678              指定进程 PID");
    println!("      --name chrome.exe            指定进程名");
    println!("      --top N                      释放内存占用最高的 N 个进程");
    println!("      （不带参数时释放内存占用最高的 15 个非关键进程）");
    println!("  winmem optimize                  一键优化：清空待机列表 + 释放所有可操作进程工作集");
    println!("  winmem kill [选项]               结束进程（默认需二次确认）");
    println!("      --pid 1234,5678              按 PID 结束");
    println!("      --name chrome.exe            按进程名结束（可匹配多个）");
    println!("      --force                      允许结束非致命但重要的进程");
    println!("      --yes                        跳过确认（脚本用）");
    println!("  winmem interactive               交互模式：列出内存栈，选择性关闭/释放");
    println!("  winmem help                      显示本帮助");
    println!();
    println!("{}示例：{}", ui::BOLD, ui::RESET);
    println!("  winmem list --top 20 --sort private");
    println!("  winmem trim --name chrome.exe");
    println!("  winmem kill --pid 12345");
    println!("  winmem interactive");
    println!();
    println!(
        "{}说明：{}内存优化采用“释放工作集 + 清空系统待机列表”的方式，不会结束你的程序；",
        ui::DIM, ui::RESET
    );
    println!("      只有 kill / 交互模式下选择 k 才会真正关闭进程；建议以管理员身份运行以获得最佳效果。");
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_range() {
        assert_eq!(parse_selection("1-3", 40), vec![0, 1, 2]);
    }

    #[test]
    fn selection_mixed() {
        assert_eq!(parse_selection("1,3,5-8", 10), vec![0, 2, 4, 5, 6, 7]);
    }

    #[test]
    fn selection_spaces_and_cjk_comma() {
        assert_eq!(parse_selection(" 2 ， 4 ", 10), vec![1, 3]);
    }

    #[test]
    fn selection_invalid() {
        assert!(parse_selection("abc", 10).is_empty());
        assert!(parse_selection("99", 10).is_empty());
        assert!(parse_selection("", 10).is_empty());
    }

    #[test]
    fn pid_list_parsing() {
        assert_eq!(parse_pids("12,34;56"), vec![12, 34, 56]);
        assert_eq!(parse_pids(" 7 "), vec![7]);
    }

    #[test]
    fn critical_process_protection() {
        assert!(ops::is_critical(4, "System"));
        assert!(ops::is_critical(100, "csrss.exe"));
        assert!(ops::is_critical(200, "lsass.exe"));
        assert!(!ops::is_critical(5000, "chrome.exe"));
        assert!(!ops::is_critical(6000, "notepad.exe"));
    }

    #[test]
    fn bytes_formatting() {
        assert_eq!(ui::fmt_bytes(0), "0 B");
        assert_eq!(ui::fmt_bytes(1024), "1 KB");
        assert_eq!(ui::fmt_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(ui::fmt_bytes(1024 * 1024 * 1024), "1.00 GB");
    }
}
