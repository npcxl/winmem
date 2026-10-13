//! winmem —— 命令行入口。
//!
//! 用法概览（详细见 `winmem help`）：
//! ```text
//! winmem status                    查看系统内存状态与压力原因
//! winmem list [--top N]            列出进程指标
//! winmem optimize [--dry-run]      按统一策略执行一次压力缓解（先确认持续压力）
//! winmem trim --pid 1234           手动裁剪工作集（仍遵守硬保护）
//! winmem monitor [--auto]          监测模式（默认只观察）
//! winmem auto                      智能自动优化开关与名单
//! winmem standby                   手动清空系统待机列表（高级）
//! winmem kill --pid 1234           手动结束进程（需确认）
//! winmem sysmem                    系统内存配置只读诊断
//! winmem config [--init|--template] 查看/生成/打印配置文件
//! winmem history                   查看最近的操作记录
//! ```

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use winmem::cli::{self, AutoCmd, Change, Command, SysmemCmd};
use winmem::config::Config;
use winmem::engine::{AutoEvent, AutoStatus};
use winmem::engine::{CancelToken, Engine, RunOptions};
use winmem::metrics;
use winmem::policy::{self, Decision, Purpose};
use winmem::report;
use winmem::sysmem;
use winmem::term::{self, Column};
use winmem::update;
use winmem::win;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    term::enable_ansi();
    // 上次热更新遗留的备份（*.old）在启动时清理
    if let Some(exe) = update::self_exe() {
        update::cleanup_backups(&exe);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}参数错误：{}{}", term::RED, e, term::RESET);
            println!();
            println!("运行 `winmem help` 查看用法。");
            std::process::exit(2);
        }
    };
    dispatch(cmd);
}

fn dispatch(cmd: Command) {
    match cmd {
        Command::Status => cmd_status(),
        Command::List { top, all, filter } => cmd_list(top, all, filter),
        Command::Optimize {
            dry_run,
            budget,
            wait_secs,
            track,
        } => cmd_optimize(dry_run, budget, wait_secs, track),
        Command::Trim {
            pids,
            names,
            dry_run,
        } => cmd_trim(&pids, &names, dry_run),
        Command::Monitor { interval, auto } => cmd_monitor(interval, auto),
        Command::Auto(a) => cmd_auto(a),
        Command::Sysmem(s) => cmd_sysmem(s),
        Command::Standby => cmd_standby(),
        Command::Kill {
            pids,
            names,
            force,
            yes,
        } => cmd_kill(&pids, &names, force, yes),
        Command::Config { init, template } => cmd_config(init, template),
        Command::Update {
            check_only,
            yes,
            repo,
            restart,
        } => cmd_update(check_only, yes, repo, restart),
        Command::History => cmd_history(),
        Command::Help => print_help(),
        Command::Version => println!("winmem {}", VERSION),
    }
}

// ---------------------------------------------------------------------------
// 公共工具
// ---------------------------------------------------------------------------

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{feff}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2060}'
            )
        })
        .collect()
}

fn read_line() -> String {
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) => std::process::exit(0),
        Ok(_) => sanitize(&s),
        Err(_) => String::new(),
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{}{} [y/N]：{}", term::YELLOW, prompt, term::RESET);
    let _ = io::stdout().flush();
    let a = read_line().trim().to_ascii_lowercase();
    a == "y" || a == "yes" || a == "是"
}

fn load_engine() -> Engine {
    let cfg = Config::load();
    for w in &cfg.warnings {
        eprintln!("{}配置警告：{}{}", term::YELLOW, w, term::RESET);
    }
    Engine::new(cfg)
}

fn change_kind(c: Change) -> sysmem::ChangeKind {
    match c {
        Change::MemoryCompression => sysmem::ChangeKind::MemoryCompression,
        Change::PageCombining => sysmem::ChangeKind::PageCombining,
        Change::AutomaticPagefile => sysmem::ChangeKind::AutomaticPagefile,
    }
}

// ---------------------------------------------------------------------------
// status / list
// ---------------------------------------------------------------------------

fn cmd_status() {
    let mut engine = load_engine();
    let Some(snap) = engine.system_snapshot() else {
        eprintln!("{}无法读取系统内存状态{}", term::RED, term::RESET);
        return;
    };
    let cfg = &engine.config.policy;

    let used = snap.total_phys.saturating_sub(snap.avail_phys);
    let free_pct = if snap.total_phys > 0 {
        snap.avail_phys as f64 / snap.total_phys as f64 * 100.0
    } else {
        0.0
    };
    let commit_pct = if snap.commit_limit > 0 {
        snap.commit_total as f64 / snap.commit_limit as f64 * 100.0
    } else {
        0.0
    };

    println!(
        "{}winmem{} {}v{}  内存状态{}",
        term::CYAN,
        term::RESET,
        term::DIM,
        VERSION,
        term::RESET
    );
    println!();
    println!(
        "  物理内存   {}/{}     可用 {}{}{}",
        term::fmt_bytes(used),
        term::fmt_bytes(snap.total_phys),
        term::GREEN,
        term::fmt_bytes(snap.avail_phys),
        term::RESET
    );
    println!(
        "  {}  {:.1}% 可用",
        term::bar(100.0 - free_pct, 36),
        free_pct
    );
    println!(
        "  提交量     {}/{}   {:.1}%",
        term::fmt_bytes(snap.commit_total),
        term::fmt_bytes(snap.commit_limit),
        commit_pct
    );
    println!();

    // 压力判定（与优化流程使用同一套状态机与阈值）
    let mut tracker = policy::PressureTracker::new();
    let rep = tracker.update(&snap, cfg, metrics::monotonic_secs());
    println!("  压力判定：{:?}", rep.kind);
    println!(
        "  阈值：可用内存 < {:.0}% 持续 {} 秒进入压力；≥ {:.0}% 退出；提交量告警 {:.0}%",
        cfg.press_enter_free_ratio * 100.0,
        cfg.press_hold_secs,
        cfg.press_exit_free_ratio * 100.0,
        cfg.commit_warn_ratio * 100.0
    );
    println!(
        "  低内存通知接口：{}",
        if snap.low_memory_signal.is_some() {
            format!(
                "可用（当前{}）",
                if snap.low_memory_signal == Some(true) {
                    "低内存"
                } else {
                    "正常"
                }
            )
        } else {
            "不可用".to_string()
        }
    );
    for n in &rep.notes {
        println!("  · {}", n);
    }
    println!();
    println!(
        "{}提示：单次 status 只反映瞬时状态；持续压力需要 {}winmem optimize{}（会等待确认）或 {}winmem monitor{} 观察。{}",
        term::DIM,
        term::CYAN,
        term::RESET,
        term::CYAN,
        term::RESET,
        term::RESET
    );
}

fn process_rows(cfg: &Config, top: Option<usize>, filter: Option<&str>) -> Vec<Vec<String>> {
    let mut sampler = metrics::ProcessSampler::new();
    // 先采一次以建立差分基线（CPU / I/O 需要两次采样）
    let _ = sampler.collect();
    std::thread::sleep(std::time::Duration::from_millis(700));
    let mut list = sampler.collect();

    if let Some(f) = filter {
        let f = f.to_ascii_lowercase();
        list.retain(|m| m.name.to_ascii_lowercase().contains(&f) || m.pid.to_string() == f);
    }
    list.sort_by_key(|m| std::cmp::Reverse(m.working_set.unwrap_or(0)));
    if let Some(n) = top {
        list.truncate(n);
    }

    let self_pid = win::current_pid();
    let self_session = win::current_session();
    let fg = win::foreground_pid();

    list.iter()
        .map(|m| {
            let activity = match (m.cpu_percent, m.io_active) {
                (Some(c), Some(true)) => Some(c.max(100.0)),
                (Some(c), _) => Some(c),
                (None, Some(true)) => Some(100.0),
                (None, _) => None,
            };
            let verdict = policy::decide(&policy::PolicyInput {
                facts: &policy::ProcessFacts {
                    pid: m.pid,
                    name: m.name.clone(),
                    session: m.session,
                    working_set: m.working_set.unwrap_or(0),
                    private_bytes: m.private_bytes.unwrap_or(0),
                    os_critical: m.os_critical,
                    last_foreground_secs: None,
                    cpu_percent: activity,
                    cooling: false,
                    is_self: m.pid == self_pid,
                },
                config: cfg,
                self_session,
                foreground_pid: fg,
                purpose: Purpose::Auto,
            });
            let state = match verdict.decision {
                Decision::Candidate => "可优化",
                Decision::Skip(_) => "仅手动",
                Decision::Blocked(_) => "禁止操作",
            };
            vec![
                m.pid.to_string(),
                m.name.clone(),
                m.working_set
                    .map(term::fmt_bytes)
                    .unwrap_or_else(|| report::UNKNOWN.into()),
                m.private_bytes
                    .map(term::fmt_bytes)
                    .unwrap_or_else(|| report::UNKNOWN.into()),
                m.session
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into()),
                m.cpu_percent
                    .map(|c| format!("{:.1}%", c))
                    .unwrap_or_else(|| "-".into()),
                state.to_string(),
            ]
        })
        .collect()
}

fn cmd_list(top: Option<usize>, all: bool, filter: Option<String>) {
    let cfg = Config::load();
    let top = if all { None } else { top.or(Some(40)) };

    let rows = process_rows(&cfg, top, filter.as_deref());
    let cols = [
        Column {
            title: "PID",
            width: 7,
            right: true,
        },
        Column {
            title: "进程名",
            width: 28,
            right: false,
        },
        Column {
            title: "工作集",
            width: 10,
            right: true,
        },
        Column {
            title: "私有提交",
            width: 10,
            right: true,
        },
        Column {
            title: "会话",
            width: 5,
            right: true,
        },
        Column {
            title: "CPU",
            width: 6,
            right: true,
        },
        Column {
            title: "状态",
            width: 8,
            right: false,
        },
    ];
    println!();
    print!("{}", term::render_table(&cols, &rows));
    println!();
    println!(
        "{}说明：工作集是常驻物理内存的部分，私有提交是进程已提交的私有虚拟内存；{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}      两者含义不同，不能相加，也不代表“内存泄漏”。{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}      CPU 为相对单个逻辑处理器的百分比，多核上可能超过 100%。{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}提示：默认只处理自动优化允许名单内的进程，详见 winmem config{}",
        term::DIM,
        term::RESET
    );
}

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------

fn print_help() {
    println!(
        "{}{}winmem {} —— Windows 内存优化工具{}",
        term::BOLD,
        term::CYAN,
        VERSION,
        term::RESET
    );
    println!();
    println!("{}命令：{}", term::BOLD, term::RESET);
    println!("  winmem status                    查看内存状态与压力原因");
    println!("  winmem list [--top N] [--all] [--filter 名]");
    println!("                                   查看进程指标（工作集/私有提交/CPU）");
    println!("  winmem optimize [--dry-run]      按统一策略执行一次压力缓解");
    println!("      --wait N                     等待持续压力确认的最长秒数（默认 90）");
    println!("      --track                      执行后做 1 分钟 / 5 分钟效果跟踪");
    println!("      --budget N                   本次最多处理的进程数");
    println!("  winmem trim --pid 1,2 | --name chrome");
    println!("                                   手动裁剪工作集（仍遵守硬保护）");
    println!("  winmem monitor [--interval 5] [--auto]");
    println!("                                   监测模式；默认只观察，--auto 才执行");
    println!("  winmem standby                   手动清空系统待机列表（高级诊断）");
    println!("  winmem kill --pid 1,2 | --name x 手动结束进程（永不进入自动流程）");
    println!("      --force                      允许处理默认保护但非致命的进程");
    println!("      --yes                        跳过确认（脚本用）");
    println!("  winmem auto [--on|--off] [--allow 名称] [--clear-allow]");
    println!("             [--startup on|off] [--run]");
    println!("                                   智能自动优化（默认关闭；默认只处理允许名单）");
    println!(
        "  winmem sysmem [status]           系统内存配置只读诊断（压缩/页面合并/分页文件/提交量）"
    );
    println!("  winmem sysmem compression|combining [--enable|--disable]");
    println!("  winmem sysmem pagefile [--enable|--disable]");
    println!("                                   系统配置变更（需管理员，可能需重启）");
    println!("  winmem sysmem restore [项目]      恢复本工具上次修改前的配置");
    println!("  winmem sysmem history            查看系统配置变更记录");
    println!("  winmem config [--init|--template]");
    println!("                                   查看配置；--init 生成模板；--template 打印模板到标准输出");
    println!("  winmem update [--check] [--yes] [--restart] [--repo owner/name]");
    println!("                                   热更新：检查 → 下载 → 校验 SHA-256 → 替换自身");
    println!("                                   --check 只看不装；--restart 装完自动重启");
    println!("  winmem history                   查看最近操作记录");
    println!();
    println!("{}重要说明：{}", term::BOLD, term::RESET);
    println!("  · 自动优化默认关闭，且只处理配置中 allow 名单内的进程；");
    println!("  · 工作集裁剪不会减少应用私有提交需求，也无法修复内存泄漏；");
    println!("  · 被换出的页面进入待机列表，需要时会被系统重新读回，可能有轻微延迟；");
    println!("  · 参数为经验初始值，请结合实际负载调整，详见 README。");
    println!();
    println!(
        "{}配置文件：{}",
        term::DIM,
        Config::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（无法定位 %APPDATA%）".into())
    );
}

// ---------------------------------------------------------------------------
// optimize / trim
// ---------------------------------------------------------------------------

fn print_progress(msg: &str) {
    let m = term::truncate(msg, 72);
    print!("\r{:<74}", m);
    let _ = io::stdout().flush();
}

fn cmd_optimize(dry: bool, budget: usize, wait_secs: u64, track: bool) {
    let mut engine = load_engine();

    let run_opts = RunOptions {
        dry_run: dry,
        budget,
        confirm_secs: wait_secs,
        track,
        ..Default::default()
    };
    let cancel = CancelToken::new();

    println!();
    println!(
        "{}策略参数：低于 {:.0}% 可用内存持续 {} 秒才处理；每批 {} 个；冷却 {} 秒{}",
        term::DIM,
        engine.config.policy.press_enter_free_ratio * 100.0,
        engine.config.policy.press_hold_secs,
        engine.config.policy.batch_size,
        engine.config.policy.cooldown_secs,
        term::RESET
    );
    if !dry {
        println!(
            "{}将先等待持续压力确认（最长 {} 秒）；确认不了会明确说明，不会声称“可用内存充足”。{}",
            term::DIM,
            wait_secs,
            term::RESET
        );
    }
    println!();

    let outcome = engine.run(&run_opts, &cancel, &mut |p| {
        let msg = if p.total > 0 {
            format!("{}（{}/{}）", p.message, p.done, p.total)
        } else {
            p.message.clone()
        };
        print_progress(&msg);
    });
    println!("\r{: <74}", "");

    if dry {
        print!("{}", report::format_plan(&outcome.plan));
        println!();
        println!("{}{}{}", term::YELLOW, outcome.finished, term::RESET);
        return;
    }

    println!("{}", report::format_outcome(&outcome));

    if outcome.observation.trimmed_count > 0 {
        let mut history = report::History::default();
        history.push(&report::History::summarize(&outcome));
        history.flush();
    }

    // 效果跟踪（默认关闭，--track 开启）
    if track && outcome.observation.trimmed_count > 0 {
        println!();
        println!(
            "{}开始效果跟踪（{} 秒节点）；等待期间可 Ctrl+C 中断。{}",
            term::CYAN,
            engine
                .config
                .policy
                .track_secs
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" / "),
            term::RESET
        );
        let cancel = CancelToken::new();
        let points = engine.run_tracking(&outcome, &cancel, &mut |m| print_progress(m));
        println!("\r{: <74}", "");
        for p in &points {
            println!(
                "  跟踪 {} 秒：可用内存 {}，提交量 {}",
                p.at_secs,
                p.avail_phys
                    .map(term::fmt_bytes)
                    .unwrap_or_else(|| report::UNKNOWN.into()),
                p.commit_total
                    .map(term::fmt_bytes)
                    .unwrap_or_else(|| report::UNKNOWN.into())
            );
            for proc in &p.procs {
                let state = if proc.alive { "存活" } else { "已退出" };
                println!(
                    "    {:>7}  {:<24} {}  工作集 {}{}",
                    proc.pid,
                    term::truncate(&proc.name, 24),
                    state,
                    proc.working_set
                        .map(term::fmt_bytes)
                        .unwrap_or_else(|| report::UNKNOWN.into()),
                    match proc.regrowth_ratio {
                        Some(r) => format!("（回涨 {:.0}%）", r * 100.0),
                        None => "（回涨未知）".to_string(),
                    }
                );
            }
        }
    } else if !track && outcome.observation.trimmed_count > 0 {
        println!(
            "{}提示：未进行效果跟踪；可用 --track 查看 1 分钟 / 5 分钟的工作集回涨。{}",
            term::DIM,
            term::RESET
        );
    }

    // 提交压力只提示，不用裁剪掩盖
    if let Some(p) = &outcome.plan.pressure {
        if matches!(
            p.kind,
            policy::PressureKind::Commit | policy::PressureKind::Both
        ) {
            println!();
            println!(
                "{}提交量已接近上限：裁剪工作集无法解决该问题，请检查异常增长的程序、{}",
                term::YELLOW,
                term::RESET
            );
            println!("{}必要时调整分页文件大小。{}", term::YELLOW, term::RESET);
        }
    }
}

fn resolve_targets(pids: &[u32], names: &[String]) -> Vec<u32> {
    let mut out: Vec<u32> = pids.to_vec();
    if !names.is_empty() {
        let mut sampler = metrics::ProcessSampler::new();
        let list = sampler.collect();
        for n in names {
            let want = policy::normalize_name(n);
            if want.is_empty() {
                continue;
            }
            out.extend(
                list.iter()
                    .filter(|m| policy::normalize_name(&m.name) == want)
                    .map(|m| m.pid),
            );
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn cmd_trim(pids: &[u32], names: &[String], dry: bool) {
    let targets = resolve_targets(pids, names);
    if targets.is_empty() {
        eprintln!(
            "{}没有匹配到进程。用法：winmem trim --pid 1234 或 --name chrome.exe{}",
            term::YELLOW,
            term::RESET
        );
        return;
    }

    let mut engine = load_engine();
    // 手动裁剪：明确告知会绕过软冷却（这是显式的人工操作）
    println!();
    println!(
        "{}手动操作：将按统一策略释放选中进程的内存；手动操作会绕过“冷却/名单”等软保护，{}",
        term::YELLOW,
        term::RESET
    );
    println!(
        "{}      但核心系统进程、其它会话、身份未知等硬/关键保护仍会阻止。{}",
        term::YELLOW,
        term::RESET
    );
    let run_opts = RunOptions {
        dry_run: dry,
        manual_targets: targets.clone(),
        force_run: true,
        budget: 0,
        confirm_secs: 0,
        track: false,
    };
    let cancel = CancelToken::new();

    let outcome = engine.run(&run_opts, &cancel, &mut |p| {
        print_progress(&p.message);
    });
    println!("\r{: <74}", "");

    if dry {
        print!("{}", report::format_plan(&outcome.plan));
        return;
    }

    println!("{}", report::format_outcome(&outcome));

    if outcome.observation.trimmed_count > 0 {
        let mut history = report::History::default();
        history.push(&format!(
            "手动 trim：{}",
            report::History::summarize(&outcome)
        ));
        history.flush();
    }

    println!(
        "{}注意：EmptyWorkingSet 可能一次移除该进程大量工作集页面，{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}      这与“只释放指定大小”不同，被换出的页面需要时会重新读回。{}",
        term::DIM,
        term::RESET
    );
}

// ---------------------------------------------------------------------------
// monitor / standby
// ---------------------------------------------------------------------------

fn cmd_monitor(interval: u64, auto: bool) {
    let mut engine = load_engine();
    let interval = interval.max(2);

    if auto && !engine.config.auto_enabled {
        eprintln!(
            "{}自动执行未在配置中开启（auto_enabled = false），已降级为只观察。{}",
            term::YELLOW,
            term::RESET
        );
        eprintln!("如需开启，请使用 `winmem auto --on` 后重试。");
        return;
    }

    println!();
    println!(
        "{}监测模式{}：每 {} 秒采样一次，{}。Ctrl+C 退出。",
        term::CYAN,
        term::RESET,
        interval,
        if auto {
            "满足条件时按策略自动执行"
        } else {
            "只观察，不执行任何操作"
        }
    );
    println!();

    let cancel = CancelToken::new();
    let mut round: u64 = 0;
    engine.establish_baseline();
    loop {
        engine.poll_foreground();
        let Some(snap) = engine.system_snapshot() else {
            eprintln!("{}采样失败，跳过本轮{}", term::YELLOW, term::RESET);
            std::thread::sleep(std::time::Duration::from_secs(interval));
            continue;
        };
        let rep = engine.update_pressure(&snap, metrics::monotonic_secs());
        round += 1;

        let free_pct = if snap.total_phys > 0 {
            snap.avail_phys as f64 / snap.total_phys as f64 * 100.0
        } else {
            100.0
        };
        println!(
            "[{:>3}] 可用 {:.1}% ({})  提交 {:.1}%  压力={:?}",
            round,
            free_pct,
            term::fmt_bytes(snap.avail_phys),
            rep.commit_ratio * 100.0,
            rep.kind
        );
        for n in &rep.notes {
            println!("      · {}", n);
        }

        if auto && rep.should_act {
            println!("      → 满足压力条件，按统一策略执行一次优化");
            let outcome = engine.run(&RunOptions::default(), &cancel, &mut |p| {
                print_progress(&p.message);
            });
            println!("\r{: <74}", "");
            println!("      {}", report::History::summarize(&outcome));
            if outcome.observation.trimmed_count > 0 {
                let mut h = report::History::default();
                h.push(&report::History::summarize(&outcome));
                h.flush();
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn cmd_standby() {
    println!();
    println!(
        "{}待机列表清空是高级诊断操作{}：它会把缓存页直接释放为空闲内存，",
        term::YELLOW,
        term::RESET
    );
    println!("之后需要重新从磁盘读取，可能造成短时卡顿。默认优化流程**不会**执行它。");
    println!();
    if !confirm("确认清空系统待机列表？") {
        println!("{}已取消。{}", term::DIM, term::RESET);
        return;
    }

    // 按需启用特权，操作结束后由 RAII 恢复原状态
    let _priv = win::PrivilegeGuard::enable("SeProfileSingleProcessPrivilege");
    let had_priv = _priv.is_some();

    match win::purge_standby_list() {
        Ok(()) => {
            println!("{}已清空系统待机列表。{}", term::GREEN, term::RESET);
        }
        Err(status) => {
            println!(
                "{}失败：NTSTATUS 0x{:08X}{}",
                term::RED,
                status as u32,
                term::RESET
            );
            let hint = match status as u32 {
                0xC0000061 => {
                    "STATUS_PRIVILEGE_NOT_HELD：当前进程未持有必要特权，请以管理员身份运行"
                }
                0xC000000D => "STATUS_INVALID_PARAMETER：系统不支持该命令",
                0xC0000002 => "STATUS_NOT_IMPLEMENTED：系统不支持该命令",
                _ => "请检查系统版本与权限",
            };
            println!("  {}", hint);
            if !had_priv {
                println!("  （SeProfileSingleProcessPrivilege 未成功启用）");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// kill / config / history
// ---------------------------------------------------------------------------

fn cmd_kill(pids: &[u32], names: &[String], force: bool, skip_confirm: bool) {
    let targets = resolve_targets(pids, names);
    if targets.is_empty() {
        eprintln!(
            "{}没有匹配到进程。用法：winmem kill --pid 1234{}",
            term::YELLOW,
            term::RESET
        );
        return;
    }

    let cfg = Config::load();
    let mut sampler = metrics::ProcessSampler::new();
    let list = sampler.collect();
    let self_session = win::current_session();
    let fg = win::foreground_pid();
    let self_pid = win::current_pid();

    let mut blocked = Vec::new();
    let mut warned: Vec<(u32, String, String)> = Vec::new();
    let mut ready: Vec<(u32, String, u64)> = Vec::new();

    for pid in &targets {
        let Some(m) = list.iter().find(|m| m.pid == *pid) else {
            continue;
        };
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
            config: &cfg,
            self_session,
            foreground_pid: fg,
            purpose: Purpose::ManualKill,
        });

        match verdict.decision {
            Decision::Blocked(r) => blocked.push((m.pid, m.name.clone(), r.describe().to_string())),
            _ => {
                let warns: Vec<&str> = verdict.warnings.iter().map(|r| r.describe()).collect();
                if !warns.is_empty() && !force {
                    blocked.push((
                        m.pid,
                        m.name.clone(),
                        format!("{}（如确需处理请加 --force）", warns.join("、")),
                    ));
                } else {
                    if !warns.is_empty() {
                        warned.push((m.pid, m.name.clone(), warns.join("、")));
                    }
                    ready.push((m.pid, m.name.clone(), m.creation_time.unwrap_or(0)));
                }
            }
        }
    }

    println!();
    if !blocked.is_empty() {
        println!("{}已跳过：{}", term::YELLOW, term::RESET);
        for (pid, name, why) in &blocked {
            println!("  {:>7}  {:<28} {}", pid, term::truncate(name, 28), why);
        }
    }
    if !warned.is_empty() {
        println!("{}强制处理（原有保护原因）：{}", term::RED, term::RESET);
        for (pid, name, why) in &warned {
            println!("  {:>7}  {:<28} {}", pid, term::truncate(name, 28), why);
        }
    }
    if ready.is_empty() {
        println!();
        println!("{}没有可以结束的进程。{}", term::YELLOW, term::RESET);
        return;
    }

    println!();
    println!("{}准备结束以下进程：{}", term::BOLD, term::RESET);
    for (pid, name, _) in &ready {
        println!("  {:>7}  {}", pid, name);
    }
    if !skip_confirm && !confirm("确认结束？未保存的数据会丢失") {
        println!("{}已取消。{}", term::DIM, term::RESET);
        return;
    }

    let engine = load_engine();
    println!();
    let mut ok = 0usize;
    for (pid, name, creation) in &ready {
        if *creation == 0 {
            println!(
                "  {}✗{} {:<28} 无法确认进程身份，已跳过",
                term::RED,
                term::RESET,
                term::truncate(name, 28)
            );
            continue;
        }
        let identity = win::ProcessIdentity {
            pid: *pid,
            creation_time: *creation,
        };
        match engine.kill_verified(&identity) {
            Ok(()) => {
                ok += 1;
                println!(
                    "  {}✓{} {:<28} PID {}",
                    term::GREEN,
                    term::RESET,
                    term::truncate(name, 28),
                    pid
                );
            }
            Err(e) => println!(
                "  {}✗{} {:<28} {}",
                term::RED,
                term::RESET,
                term::truncate(name, 28),
                e
            ),
        }
    }
    println!();
    println!(
        "{}完成：成功结束 {} 个进程。{}",
        term::BOLD,
        ok,
        term::RESET
    );
}

// ---------------------------------------------------------------------------
// update：热更新（检查 → 下载 → 校验 → 替换 → 可选重启）
// ---------------------------------------------------------------------------

/// 版本清单里的链接**只用于展示**，不下载任何东西。
fn print_release_summary(info: &update::ReleaseInfo, self_name: &str) {
    println!(
        "{}发现新版本{} {}（当前 {}）",
        term::GREEN,
        term::RESET,
        info.version,
        update::current_version()
    );
    println!("{}  标签    {}{}", term::DIM, info.tag, term::RESET);
    if let Some(a) = info.asset(self_name) {
        let size = a
            .size
            .map(|s| format!("{:.1} MB", s as f64 / 1024.0 / 1024.0))
            .unwrap_or_else(|| "大小未知".into());
        println!(
            "{}  更新包  {}（{}）{}",
            term::DIM,
            a.name,
            size,
            term::RESET
        );
        let sha = if a.sha256.len() >= 16 {
            &a.sha256[..16]
        } else {
            &a.sha256
        };
        println!(
            "{}  SHA-256 {}…（下载后校验）{}",
            term::DIM,
            sha,
            term::RESET
        );
    } else {
        println!(
            "{}  注意：本次发布未提供 {}，无法自动更新{}",
            term::YELLOW,
            self_name,
            term::RESET
        );
    }
    if !info.notes.is_empty() {
        println!("{}  说明    {}{}", term::DIM, info.notes, term::RESET);
    }
}

fn cmd_update(check_only: bool, yes: bool, repo: Option<String>, restart: bool) {
    let cfg = Config::load();
    let repo = repo.unwrap_or_else(|| cfg.update_repo.clone());

    let self_exe = match update::self_exe() {
        Some(p) => p,
        None => {
            eprintln!("{}无法确定当前程序路径，已取消。{}", term::RED, term::RESET);
            std::process::exit(1);
        }
    };
    let self_name = self_exe
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    println!();
    println!(
        "{}当前版本{} winmem {}",
        term::BOLD,
        term::RESET,
        update::current_version()
    );
    println!(
        "{}更新源  {}（公开 Release 清单）{}",
        term::DIM,
        repo,
        term::RESET
    );
    println!("{}说明    {}{}", term::DIM, update::NOTICE, term::RESET);

    // 清理上次更新遗留的备份（旧进程可能还在退出，失败也不影响本次）
    let cleaned = update::cleanup_backups(&self_exe);
    if cleaned > 0 {
        println!(
            "{}已清理上次更新遗留的 {} 个备份文件。{}",
            term::DIM,
            cleaned,
            term::RESET
        );
    }
    println!();

    match update::check(&repo) {
        update::CheckResult::Failed(e) => {
            eprintln!("{}检查更新失败：{}", term::RED, e);
            eprintln!("{}", term::RESET);
            std::process::exit(1);
        }
        update::CheckResult::UpToDate { current, latest } => {
            println!(
                "{}已是最新版本{}（本地 {}，远端最新 {}）。",
                term::GREEN,
                term::RESET,
                current,
                latest
            );
        }
        update::CheckResult::Available(info) => {
            print_release_summary(&info, &self_name);
            if check_only {
                println!(
                    "{}仅检查（--check），未下载任何内容。{}",
                    term::DIM,
                    term::RESET
                );
                return;
            }
            if info.asset(&self_name).is_none() {
                std::process::exit(1);
            }
            println!();
            if !yes && !confirm("是否下载并替换当前程序？") {
                println!("{}已取消。{}", term::DIM, term::RESET);
                return;
            }

            let sibling = update::sibling_name(&self_name);
            let report = update::install(&info, &self_exe, &[sibling]);
            println!("{}安装结果：{}", term::BOLD, term::RESET);
            for f in &report.files {
                let mark = if f.ok { "✔" } else { "✖" };
                let color = if f.ok { term::GREEN } else { term::RED };
                let extra = f
                    .backup
                    .as_ref()
                    .map(|b| {
                        format!(
                            "（原文件备份为 {}）",
                            b.file_name().unwrap_or_default().to_string_lossy()
                        )
                    })
                    .unwrap_or_default();
                println!(
                    "  {}{} {}  {}{}{}",
                    color,
                    mark,
                    f.path.file_name().unwrap_or_default().to_string_lossy(),
                    f.message,
                    extra,
                    term::RESET
                );
            }

            if !report.all_ok {
                // 部分文件没更新成功：可能是目录不可写或本次发布缺少该文件
                eprintln!(
                    "\n{}部分文件未更新成功。若目录不可写，请以管理员身份重试。{}",
                    term::YELLOW,
                    term::RESET
                );
                std::process::exit(1);
            }

            println!(
                "\n{}更新已就位，重启程序后生效。{}（备份文件会在下次启动时自动清理）",
                term::GREEN,
                term::RESET
            );
            if restart {
                match update::restart(&self_exe, &[]) {
                    Ok(()) => {
                        println!("已启动新版本。");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        eprintln!(
                            "{}启动新版本失败：{}（请手动重启）{}",
                            term::YELLOW,
                            e,
                            term::RESET
                        );
                    }
                }
            }
        }
    }
}

fn cmd_config(init: bool, template: bool) {
    let path = Config::default_path();

    // 只打印模板：不读写任何文件，便于打包、生成示例配置与文档
    if template {
        print!("{}", Config::template());
        return;
    }

    if init {
        let Some(p) = &path else {
            eprintln!("{}无法定位 %APPDATA%{}", term::RED, term::RESET);
            return;
        };
        if p.exists() && !confirm(&format!("{} 已存在，覆盖？", p.display())) {
            println!("{}已取消。{}", term::DIM, term::RESET);
            return;
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(p, Config::template()) {
            Ok(()) => println!(
                "{}已生成配置模板：{}{}",
                term::GREEN,
                p.display(),
                term::RESET
            ),
            Err(e) => eprintln!("{}写入失败：{}{}", term::RED, e, term::RESET),
        }
        return;
    }

    let cfg = Config::load();
    println!();
    println!(
        "{}配置文件：{}",
        term::BOLD,
        path.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "{}存在：{}",
        term::DIM,
        if path.as_ref().map(|p| p.exists()).unwrap_or(false) {
            "是"
        } else {
            "否（使用默认值）"
        }
    );
    println!("{}", term::RESET);
    println!("{}", Config::template());
    println!("{}当前生效值：{}", term::BOLD, term::RESET);
    println!("  保护名单：{:?}", cfg.protect);
    println!("  允许名单：{:?}", cfg.allow);
    println!("  auto_enabled = {}", cfg.auto_enabled);
    println!("  auto_allow_unlisted = {}", cfg.policy.auto_allow_unlisted);
    match win::autostart_enabled() {
        Ok(true) => println!("  开机启动 = 已注册（与实际注册表一致）"),
        Ok(false) => println!("  开机启动 = 未注册"),
        Err(e) => println!("  开机启动 = 查询失败（{}）", e),
    }
    for w in &cfg.warnings {
        println!("  {}警告：{}{}", term::YELLOW, w, term::RESET);
    }
}

// ---------------------------------------------------------------------------
// auto：智能自动优化开关
// ---------------------------------------------------------------------------

fn print_auto_intro() {
    println!();
    println!("{}关于「智能自动优化」{}", term::BOLD, term::RESET);
    println!("  · 开启后程序会在后台监测内存，仅在可用物理内存低于 10% 并持续 30 秒时，");
    println!("    对你的**允许名单**中的闲置后台应用执行工作集裁剪；");
    println!("  · 允许名单为空时只监测和提示，不做任何操作；");
    println!("  · 不会结束进程、不会清空待机列表、不会修改任何系统配置；");
    println!("  · 工作集裁剪只是把不活跃页面换出，不能修复内存泄漏，也不保证「无影响」；");
    println!("  · 阈值是经验值，可在配置文件中调整，需按实际负载实测。");
    println!();
}

fn cmd_auto(a: AutoCmd) {
    let mut cfg = Config::load();
    let mut dirty = false;

    if a.on {
        if !cfg.auto_intro_shown {
            print_auto_intro();
            cfg.auto_intro_shown = true;
            dirty = true;
        }
        if !cfg.auto_enabled {
            cfg.auto_enabled = true;
            dirty = true;
            println!("{}已开启智能自动优化。{}", term::GREEN, term::RESET);
            if cfg.allow.is_empty() {
                println!(
                    "{}注意：允许名单为空，只会监测与提示。用 winmem auto --allow 名称 添加。{}",
                    term::YELLOW,
                    term::RESET
                );
            }
        } else {
            println!("{}自动优化已经是开启状态。{}", term::DIM, term::RESET);
        }
    }
    if a.off {
        if cfg.auto_enabled {
            cfg.auto_enabled = false;
            dirty = true;
        }
        println!(
            "{}已关闭智能自动优化。{}（该开关与系统配置相互独立，不会改动系统设置）",
            term::GREEN,
            term::RESET
        );
    }
    if !a.allow.is_empty() {
        for name in &a.allow {
            let n = policy::normalize_name(name);
            if !n.is_empty() && !cfg.allow.contains(&n) {
                cfg.allow.push(n);
            }
        }
        dirty = true;
        println!("{}允许名单已更新：{}", term::GREEN, cfg.allow.join(", "));
    }
    if a.clear_allow {
        cfg.allow.clear();
        dirty = true;
        println!(
            "{}允许名单已清空（此后只监测不执行）。{}",
            term::GREEN,
            term::RESET
        );
    }
    if let Some(v) = a.startup {
        cfg.start_with_windows = v;
        dirty = true;
        // 实际注册 / 撤销当前用户的开机启动，并读回验证
        match win::set_autostart(v) {
            Ok(()) => println!(
                "{}开机启动已{}，并已读回验证。{}",
                term::GREEN,
                if v { "注册" } else { "撤销" },
                term::RESET
            ),
            Err(e) => eprintln!(
                "{}开机启动变更失败：{}（配置中的开关未生效，请检查权限或手动设置）{}",
                term::RED,
                e,
                term::RESET
            ),
        }
    }

    if dirty {
        match cfg.save() {
            Ok(p) => println!("配置已保存：{}", p.display()),
            Err(e) => eprintln!("{}保存配置失败：{}{}", term::RED, e, term::RESET),
        }
    }

    if a.run {
        println!();
        println!(
            "{}前台运行自动会话（Ctrl+C 退出）…{}",
            term::CYAN,
            term::RESET
        );
        let mut engine = Engine::new(cfg.clone());
        let cancel = CancelToken::new();
        let status = Arc::new(Mutex::new(AutoStatus::default()));
        winmem::engine::run_auto_session(&mut engine, &cancel, &status, &mut |e| match e {
            AutoEvent::State(s) => println!("[状态] {}", s.label()),
            AutoEvent::Notice(m) => println!("[提示] {}", m),
            AutoEvent::Finished(o) => println!("[完成] {}", report::History::summarize(&o)),
        });
        return;
    }

    // 仅保存配置 / 查看状态
    if a.on && !a.off {
        println!();
        println!(
            "{}说明：`winmem auto --on` 只保存配置（记录你希望开启），{}",
            term::YELLOW,
            term::RESET
        );
        println!(
            "{}      它不会在命令行进程外持续运行。实际启动后台监测请使用：{}",
            term::YELLOW,
            term::RESET
        );
        println!("        · winmem auto --run        （前台运行自动会话）");
        println!("        · 运行 winmem-gui.exe       （图形界面，勾选「智能自动优化」）");
        if win::autostart_enabled().unwrap_or(false) {
            println!("        · 已注册开机启动：登录后会由 winmem-gui.exe 自动开启监测");
        }
    }

    println!();
    println!("{}智能自动优化{}", term::BOLD, term::RESET);
    println!(
        "  开关：{}",
        if cfg.auto_enabled {
            "已开启"
        } else {
            "已关闭（默认）"
        }
    );
    println!(
        "  允许名单：{}",
        if cfg.allow.is_empty() {
            "（空，只监测不执行）".to_string()
        } else {
            cfg.allow.join(", ")
        }
    );
    println!(
        "  保护名单：{}",
        if cfg.protect.is_empty() {
            "（空）".to_string()
        } else {
            cfg.protect.join(", ")
        }
    );
    println!(
        "  处理未列入允许名单的进程：{}",
        if cfg.policy.auto_allow_unlisted {
            "是（风险自负）"
        } else {
            "否（默认）"
        }
    );
    println!(
        "  开机自动启动：{}",
        match win::autostart_enabled() {
            Ok(true) => "已注册（实际状态）".to_string(),
            Ok(false) => "未注册（默认）".to_string(),
            Err(e) => format!("查询失败（{}）", e),
        }
    );
    println!("  采样间隔：{} 秒", cfg.policy.sample_interval_secs);
    println!(
        "  触发条件：可用内存 < {:.0}% 持续 {} 秒（回升到 {:.0}% 退出）",
        cfg.policy.press_enter_free_ratio * 100.0,
        cfg.policy.press_hold_secs,
        cfg.policy.press_exit_free_ratio * 100.0
    );
    println!(
        "  每批 {} 个（单次上限 {} 个，会话预算 {} 个），单进程冷却 {} 秒",
        cfg.policy.batch_size,
        cfg.policy.run_budget,
        cfg.policy.run_budget.saturating_mul(3),
        cfg.policy.cooldown_secs
    );
    println!("  效果跟踪节点：{:?} 秒", cfg.policy.track_secs);
    println!();
    println!("{}用法：{}", term::DIM, term::RESET);
    println!("  winmem auto --on                    开启（仅保存配置）");
    println!("  winmem auto --off                   关闭（立即取消运行中的会话）");
    println!("  winmem auto --allow a.exe,b.exe     追加允许名单");
    println!("  winmem auto --clear-allow           清空允许名单（只监测）");
    println!("  winmem auto --startup on|off        注册/撤销开机启动");
    println!("  winmem auto --run                   前台运行自动会话");
}

// ---------------------------------------------------------------------------
// sysmem：系统内存配置检查与变更
// ---------------------------------------------------------------------------

fn fmt_opt(v: Option<u64>) -> String {
    v.map(term::fmt_bytes)
        .unwrap_or_else(|| report::UNKNOWN.into())
}

/// 分页文件「自动管理」开关的展示文案。
///
/// 只有明确的布尔值才翻译成「系统自动管理 / 手动配置」；
/// 不支持 / 不可用 / 查询失败保持原语义，绝不当成「已关闭」。
fn automatic_label(s: &sysmem::Snapshot) -> String {
    match s {
        sysmem::Snapshot::Bool(true) => "系统自动管理".into(),
        sysmem::Snapshot::Bool(false) => "手动配置".into(),
        other => other.describe(),
    }
}

fn cmd_sysmem(cmd: SysmemCmd) {
    match cmd {
        SysmemCmd::Status => sysmem_report(),
        SysmemCmd::Change { kind, action } => sysmem_change(change_kind(kind), action),
        SysmemCmd::Restore { kind } => sysmem_restore(kind),
        SysmemCmd::History => sysmem_history(),
    }
}

fn sysmem_report() {
    println!();
    println!(
        "{}正在查询系统内存配置（只读，不会修改任何设置）…{}",
        term::DIM,
        term::RESET
    );

    let mem = win::memory_status();
    let commit = sysmem::commit_info();
    let comp = sysmem::probe_compression();
    let pf = sysmem::probe_page_files();

    println!();
    println!("{}系统内存{}", term::BOLD, term::RESET);

    if let Some(m) = mem {
        let used = m.ullTotalPhys.saturating_sub(m.ullAvailPhys);
        println!(
            "  物理内存   {}/{}（可用 {}）",
            term::fmt_bytes(used),
            term::fmt_bytes(m.ullTotalPhys),
            term::fmt_bytes(m.ullAvailPhys)
        );
    } else {
        println!("  物理内存   {}", report::UNKNOWN);
    }

    match commit {
        Some(c) => println!(
            "  提交量     {}/{}（{:.1}%，剩余 {}）",
            term::fmt_bytes(c.total),
            term::fmt_bytes(c.limit),
            c.ratio().unwrap_or(0.0) * 100.0,
            fmt_opt(c.headroom())
        ),
        None => println!("  提交量     {}", report::UNKNOWN),
    }

    println!("  内存压缩   {}", comp.memory_compression.describe());
    println!("     说明：部分页面压缩后留在物理内存，可减少磁盘换入，但需要 CPU 处理。");
    println!("  页面合并   {}", comp.page_combining.describe());
    println!(
        "     说明：Windows 可合并内容相同的物理页面，收益取决于重复页面与负载，存在处理开销。"
    );
    println!("  分页文件   {}", automatic_label(&pf.automatic_managed));
    println!(
        "     自动管理查询：{}；配置项：{}；使用量：{}；卷：{}",
        automatic_label(&pf.automatic_managed),
        pf.settings_status.describe(),
        pf.usage_status.describe(),
        pf.volume_status.describe()
    );
    for e in &pf.entries {
        println!(
            "    {}   已分配 {}   当前使用 {}   峰值 {}",
            e.path,
            e.allocated_mb
                .map(|v| format!("{} MB", v))
                .unwrap_or_else(|| report::UNKNOWN.into()),
            e.current_usage_mb
                .map(|v| format!("{} MB", v))
                .unwrap_or_else(|| report::UNKNOWN.into()),
            e.peak_usage_mb
                .map(|v| format!("{} MB", v))
                .unwrap_or_else(|| report::UNKNOWN.into())
        );
        println!("      （注意：分页文件使用量不是实际写入的字节数，也不是磁盘分页吞吐）");
    }
    for v in &pf.volumes {
        if let (Some(free), Some(total)) = (v.free_bytes, v.total_bytes) {
            println!(
                "    {} 卷可用 {}/{}",
                v.device,
                term::fmt_bytes(free),
                term::fmt_bytes(total)
            );
        }
    }

    println!();
    println!("{}诊断{}", term::BOLD, term::RESET);
    let diags = sysmem::diagnose(&pf, commit);
    if diags.is_empty() {
        println!("  （没有需要提示的项）");
    }
    for d in diags {
        let color = match d.severity {
            sysmem::Severity::Info => term::DIM,
            sysmem::Severity::Warn => term::YELLOW,
            sysmem::Severity::Critical => term::RED,
        };
        println!(
            "  {}{}[{}]{} {}",
            color,
            term::BOLD,
            d.severity.label(),
            term::RESET,
            d.text
        );
    }

    if !sysmem::is_elevated() {
        println!();
        println!(
            "{}提示：当前进程不是管理员。内存压缩 / 页面合并的查询与全部配置变更需要管理员权限；{}",
            term::DIM,
            term::RESET
        );
        println!(
            "{}      普通权限下本页仍可完成分页文件与提交量诊断。{}",
            term::DIM,
            term::RESET
        );
    }

    println!();
    println!(
        "{}说明：本页是只读诊断；参数配置与「智能自动优化」开关是两个独立的东西。{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}      winmem sysmem compression --enable   启用内存压缩（需管理员，可能需重启）{}",
        term::DIM,
        term::RESET
    );
    println!(
        "{}      winmem sysmem restore                  恢复本工具上次修改前的配置{}",
        term::DIM,
        term::RESET
    );
}

/// 变更某项系统配置。
///
/// `action = None` 表示用户没有给出 `--enable/--disable`：只显示当前状态与用法，不修改任何设置。
fn sysmem_change(kind: sysmem::ChangeKind, action: Option<bool>) {
    let before = sysmem::snapshot_of(kind);

    // 无 --enable/--disable：只报告，不修改
    let Some(action) = action else {
        println!();
        println!("  {}：{}", kind.title(), before.describe());
        println!(
            "  用法：winmem sysmem {} --enable | --disable",
            match kind {
                sysmem::ChangeKind::MemoryCompression => "compression",
                sysmem::ChangeKind::PageCombining => "combining",
                sysmem::ChangeKind::AutomaticPagefile => "pagefile",
            }
        );
        return;
    };

    println!();
    println!("{}即将变更系统配置{}", term::BOLD, term::RESET);
    println!("  项目：{}", kind.title());
    println!("  当前：{}", before.describe());
    println!("  目标：{}", if action { "开启" } else { "关闭" });
    println!("  权限：需要管理员权限（本程序只在实际变更时请求，不会自动提权）");
    println!("  重启：{}", kind.reboot_state().label());
    if !before.is_known() {
        println!(
            "{}  注意：当前原状态无法可靠读取，变更后将**无法自动恢复**，请自行记录。{}",
            term::YELLOW,
            term::RESET
        );
    }
    println!(
        "{}  注意：配置改变 ≠ 运行效果已确认；本工具不会宣称一定能改善性能。{}",
        term::YELLOW,
        term::RESET
    );
    println!();
    if !confirm("确认执行该变更吗？") {
        println!("{}已取消。{}", term::DIM, term::RESET);
        return;
    }

    let outcome = sysmem::apply_change(kind, action);
    println!();
    if outcome.ok {
        println!("{}{}{}", term::GREEN, outcome.message, term::RESET);
    } else {
        println!("{}{}{}", term::RED, outcome.message, term::RESET);
    }
    println!("  验证程度：{}", outcome.verify.label());
    println!("  重启状态：{}", outcome.reboot.label());
    println!("  变更后重新查询：{}", outcome.verified);
    if !sysmem::is_elevated() {
        println!(
            "{}  当前进程不是管理员，系统配置变更会失败或需要提权。{}",
            term::DIM,
            term::RESET
        );
    }
}

fn sysmem_restore(kind: Option<Change>) {
    let kinds: Vec<sysmem::ChangeKind> = match kind {
        Some(c) => vec![change_kind(c)],
        None => vec![
            sysmem::ChangeKind::MemoryCompression,
            sysmem::ChangeKind::PageCombining,
            sysmem::ChangeKind::AutomaticPagefile,
        ],
    };
    for k in kinds {
        let outcome = sysmem::restore_last(k);
        println!();
        println!("{}恢复 {}：{}", term::BOLD, k.title(), term::RESET);
        if outcome.ok {
            println!("{}{}{}", term::GREEN, outcome.message, term::RESET);
        } else {
            println!("{}{}{}", term::YELLOW, outcome.message, term::RESET);
        }
        println!("  验证程度：{}", outcome.verify.label());
        println!("  当前状态：{}", outcome.verified);

        // 分页文件无法可靠恢复时，直接打开系统设置，并明确说明限制
        if !outcome.ok && k == sysmem::ChangeKind::AutomaticPagefile {
            println!(
                "{}正在打开系统「虚拟内存」设置；本工具不会替你覆盖无法确认的配置。{}",
                term::DIM,
                term::RESET
            );
            if let Err(e) = sysmem::open_pagefile_settings() {
                println!("  （无法打开系统设置：{}）", e);
            }
        }
    }
}

fn sysmem_history() {
    let changes = sysmem::read_changes();
    if changes.is_empty() {
        println!("{}暂无系统配置变更记录。{}", term::DIM, term::RESET);
        return;
    }
    println!();
    for c in changes {
        println!("{}{}{}", term::DIM, c.at, term::RESET);
        println!(
            "  {}：{} → {}（结果 {}，验证：{}{}）",
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
        );
        if !c.target_entries.is_empty() {
            for e in &c.target_entries {
                println!(
                    "    分页文件项：{}（初始 {} / 最大 {}）",
                    e.path,
                    e.initial_mb
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "系统决定".into()),
                    e.maximum_mb
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "系统决定".into())
                );
            }
        }
    }
    if let Some(p) = sysmem::history_path() {
        println!();
        println!("{}记录文件：{}{}", term::DIM, p.display(), term::RESET);
    }
}

fn cmd_history() {
    let h = report::History::default();
    if h.is_empty() {
        println!("{}暂无历史记录。{}", term::DIM, term::RESET);
        return;
    }
    println!();
    for e in h.entries() {
        println!("{}  {}", term::DIM, e.at);
        println!("   {}", e.line);
    }
    if let Some(p) = h.path() {
        println!();
        println!("{}记录文件：{}{}", term::DIM, p.display(), term::RESET);
    }
}
