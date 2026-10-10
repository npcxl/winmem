//! winmem —— 命令行入口。
//!
//! 用法概览（详细见 `winmem help`）：
//! ```text
//! winmem status                    查看系统内存状态与压力原因
//! winmem list [--top N]            列出进程指标
//! winmem optimize [--dry-run]      按统一策略执行一次压力缓解
//! winmem trim --pid 1234           手动裁剪工作集（仍遵守硬保护）
//! winmem monitor [--auto]          监测模式（默认只观察）
//! winmem standby                   手动清空系统待机列表（高级）
//! winmem kill --pid 1234           手动结束进程（需确认）
//! winmem config [--init]           查看/生成配置文件
//! winmem history                   查看最近的操作记录
//! ```

use std::collections::HashMap;
use std::io::{self, Write};

use winmem::config::Config;
use winmem::engine::{CancelToken, Engine, RunOptions};
use winmem::metrics;
use winmem::policy::{self, Decision, Purpose};
use winmem::report;
use winmem::term::{self, Column};
use winmem::win;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const VALUED_OPTS: &[&str] = &["pid", "name", "top", "sort", "interval", "budget"];

fn main() {
    term::enable_ansi();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return;
    }
    let (pos, opts) = parse_args(&args);
    let cmd = pos
        .first()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let rest: Vec<String> = pos.iter().skip(1).cloned().collect();

    match cmd.as_str() {
        "status" | "st" => cmd_status(),
        "list" | "ls" | "ps" => cmd_list(&opts, &rest),
        "optimize" | "opt" => cmd_optimize(&opts),
        "trim" => cmd_trim(&opts, &rest),
        "monitor" | "mon" => cmd_monitor(&opts),
        "standby" | "purge" => cmd_standby(),
        "kill" | "close" => cmd_kill(&opts, &rest, false),
        "config" | "cfg" => cmd_config(&opts),
        "history" | "hist" => cmd_history(),
        "help" | "-h" | "--help" | "?" => print_help(),
        "version" | "-v" | "--version" => println!("winmem {}", VERSION),
        other => {
            eprintln!("{}未知命令：{}{}", term::RED, other, term::RESET);
            println!();
            print_help();
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

fn parse_args(args: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut pos = Vec::new();
    let mut opts = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let stripped = a.strip_prefix("--").or_else(|| {
            if a.len() > 1 {
                a.strip_prefix('-')
            } else {
                None
            }
        });
        let Some(rest) = stripped else {
            pos.push(a.clone());
            i += 1;
            continue;
        };
        let (key, inline) = match rest.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (rest.to_string(), None),
        };
        if let Some(v) = inline {
            opts.insert(key, v);
        } else if VALUED_OPTS.contains(&key.as_str())
            && args
                .get(i + 1)
                .map(|n| !n.starts_with('-'))
                .unwrap_or(false)
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

fn parse_pids(s: &str) -> Vec<u32> {
    s.split([',', ';', ' '])
        .filter_map(|x| x.trim().parse::<u32>().ok())
        .collect()
}

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
        "{}提示：单次 status 只反映瞬时状态；持续压力需要 {}winmem monitor{} 观察。{}",
        term::DIM,
        term::CYAN,
        term::RESET,
        term::RESET
    );
}

fn process_rows(cfg: &Config, top: Option<usize>, filter: Option<&str>) -> Vec<Vec<String>> {
    let mut sampler = metrics::ProcessSampler::new();
    // 先采一次以建立差分基线
    let _ = sampler.collect(false);
    std::thread::sleep(std::time::Duration::from_millis(700));
    let mut list = sampler.collect(true);

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

fn cmd_list(opts: &HashMap<String, String>, rest: &[String]) {
    let cfg = Config::load();
    let top = if flag(opts, "all") {
        None
    } else {
        value(opts, "top").and_then(|v| v.parse().ok()).or(Some(40))
    };
    let filter = value(opts, "filter")
        .map(|s| s.to_string())
        .or_else(|| rest.first().cloned());

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
    println!("      --dry-run                    只显示候选与跳过原因，不执行");
    println!("      --budget N                   本次最多处理的进程数");
    println!("  winmem trim --pid 1,2 | --name chrome");
    println!("                                   手动裁剪工作集（仍遵守硬保护）");
    println!("  winmem monitor [--interval 5] [--auto]");
    println!("                                   监测模式；默认只观察，--auto 才执行");
    println!("  winmem standby                   手动清空系统待机列表（高级诊断）");
    println!("  winmem kill --pid 1,2 | --name x 手动结束进程（永不进入自动流程）");
    println!("      --force                      允许处理默认保护但非致命的进程");
    println!("      --yes                        跳过确认（脚本用）");
    println!("  winmem config [--init]           查看配置；--init 生成模板");
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

fn cmd_optimize(opts: &HashMap<String, String>) {
    let mut engine = load_engine();
    let dry = flag(opts, "dry-run") || flag(opts, "dry_run");
    let budget = value(opts, "budget")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let run_opts = RunOptions {
        dry_run: dry,
        budget,
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

    let mut history = report::History::default();
    history.push(&report::History::summarize(&outcome));
    history.flush();

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

fn resolve_targets(opts: &HashMap<String, String>, rest: &[String], allow_name: bool) -> Vec<u32> {
    if let Some(p) = value(opts, "pid") {
        return parse_pids(p);
    }
    let name = value(opts, "name").map(|s| s.to_string()).or_else(|| {
        if allow_name {
            rest.first().cloned()
        } else {
            None
        }
    });
    if let Some(n) = name {
        let mut sampler = metrics::ProcessSampler::new();
        let list = sampler.collect(false);
        let want = policy::normalize_name(&n);
        return list
            .iter()
            .filter(|m| policy::normalize_name(&m.name) == want)
            .map(|m| m.pid)
            .collect();
    }
    if let Some(pid) = rest.first().and_then(|s| s.parse::<u32>().ok()) {
        return vec![pid];
    }
    Vec::new()
}

fn cmd_trim(opts: &HashMap<String, String>, rest: &[String]) {
    let targets = resolve_targets(opts, rest, true);
    if targets.is_empty() {
        eprintln!(
            "{}没有匹配到进程。用法：winmem trim --pid 1234 或 --name chrome.exe{}",
            term::YELLOW,
            term::RESET
        );
        return;
    }

    let mut engine = load_engine();
    let run_opts = RunOptions {
        dry_run: flag(opts, "dry-run"),
        manual_targets: targets.clone(),
        force_run: true,
        budget: 0,
    };
    let cancel = CancelToken::new();

    println!();
    let outcome = engine.run(&run_opts, &cancel, &mut |p| {
        print_progress(&p.message);
    });
    println!("\r{: <74}", "");

    if run_opts.dry_run {
        print!("{}", report::format_plan(&outcome.plan));
        return;
    }

    println!("{}", report::format_outcome(&outcome));

    let mut history = report::History::default();
    history.push(&format!(
        "手动 trim：{}",
        report::History::summarize(&outcome)
    ));
    history.flush();

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

fn cmd_monitor(opts: &HashMap<String, String>) {
    let mut engine = load_engine();
    let interval = value(opts, "interval")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5)
        .max(2);
    let auto = flag(opts, "auto");

    if auto && !engine.config.auto_enabled {
        eprintln!(
            "{}自动执行未在配置中开启（auto_enabled = false），已降级为只观察。{}",
            term::YELLOW,
            term::RESET
        );
        eprintln!("如需开启，请编辑配置文件后重试。");
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
    loop {
        let Some(snap) = engine.system_snapshot() else {
            eprintln!("{}采样失败，跳过本轮{}", term::YELLOW, term::RESET);
            std::thread::sleep(std::time::Duration::from_secs(interval));
            continue;
        };
        let cfg = engine.config.policy.clone();
        let mut tracker = policy::PressureTracker::new();
        let rep = tracker.update(&snap, &cfg, metrics::monotonic_secs());
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
            let mut h = report::History::default();
            h.push(&report::History::summarize(&outcome));
            h.flush();
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

fn cmd_kill(opts: &HashMap<String, String>, rest: &[String], _dummy: bool) {
    let force = flag(opts, "force");
    let skip_confirm = flag(opts, "yes") || flag(opts, "y");
    let targets = resolve_targets(opts, rest, true);
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
    let list = sampler.collect(false);
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

fn cmd_config(opts: &HashMap<String, String>) {
    let path = Config::default_path();
    if flag(opts, "init") {
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
    for w in &cfg.warnings {
        println!("  {}警告：{}{}", term::YELLOW, w, term::RESET);
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
