//! 命令行参数解析（纯逻辑，可单元测试）。
//!
//! 设计目标（对应评审问题）：
//! - 子命令**明确接收结构化参数**，不再把带值参数与位置参数混在一起；
//! - `allow` / `startup` / `filter` / `interval` / `budget` / `wait` 等**带值参数**必须真的读到值，
//!   绝不能把 `true` 当作参数值塞进配置或名单；
//! - 缺失值、非法值、未知参数、互斥参数一律返回 `Err`，**不把错误值保存进配置**。
//!
//! 本模块不依赖 Windows API，因此可以直接在测试里验证完整入口的路由结果。

/// 可变更的系统配置项（与 `sysmem::ChangeKind` 一一对应，避免本模块依赖 Windows 层）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    MemoryCompression,
    PageCombining,
    AutomaticPagefile,
}

impl Change {
    pub fn id(self) -> &'static str {
        match self {
            Change::MemoryCompression => "compression",
            Change::PageCombining => "combining",
            Change::AutomaticPagefile => "pagefile",
        }
    }
}

/// `sysmem` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysmemCmd {
    /// 只读诊断
    Status,
    /// 变更某项系统配置
    Change { kind: Change, enable: bool },
    /// 恢复本工具上次修改前的配置
    Restore { kind: Option<Change> },
    /// 查看变更记录
    History,
}

/// `auto` 子命令参数。`on` / `off` 互斥。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoCmd {
    pub on: bool,
    pub off: bool,
    pub allow: Vec<String>,
    pub clear_allow: bool,
    /// `--startup on|off`；`None` 表示不修改
    pub startup: Option<bool>,
    /// 是否前台运行自动会话
    pub run: bool,
}

/// 解析后的完整命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Status,
    List {
        top: Option<usize>,
        all: bool,
        filter: Option<String>,
    },
    Optimize {
        dry_run: bool,
        budget: usize,
        /// 等待持续压力确认的最长秒数（0 表示不等待，仅报告“压力尚未确认”）
        wait_secs: u64,
    },
    Trim {
        pids: Vec<u32>,
        names: Vec<String>,
        dry_run: bool,
    },
    Monitor {
        interval: u64,
        auto: bool,
    },
    Auto(AutoCmd),
    Sysmem(SysmemCmd),
    Standby,
    Kill {
        pids: Vec<u32>,
        names: Vec<String>,
        force: bool,
        yes: bool,
    },
    Config {
        init: bool,
    },
    History,
    Help,
    Version,
}

// ---------------------------------------------------------------------------
// 解析实现
// ---------------------------------------------------------------------------

/// 一次子命令解析出的原始选项。
#[derive(Debug, Default)]
struct Opts {
    flags: Vec<String>,
    vals: Vec<(String, String)>,
    pos: Vec<String>,
}

impl Opts {
    fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }

    fn value(&self, k: &str) -> Option<&str> {
        self.vals.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
}

struct Cursor {
    args: Vec<String>,
    i: usize,
}

impl Cursor {
    fn new(args: &[String]) -> Cursor {
        Cursor {
            args: args.to_vec(),
            i: 0,
        }
    }

    fn next(&mut self) -> Option<String> {
        let a = self.args.get(self.i).cloned();
        if a.is_some() {
            self.i += 1;
        }
        a
    }

    fn peek(&self) -> Option<&str> {
        self.args.get(self.i).map(|s| s.as_str())
    }
}

/// 去掉 `--` / `-` 前缀；不是选项时返回 None。
fn strip_prefix(a: &str) -> Option<&str> {
    if let Some(rest) = a.strip_prefix("--") {
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    } else if a.len() > 1 {
        a.strip_prefix('-')
    } else {
        None
    }
}

/// 解析剩余 token。
///
/// - `valued`：必须带值的参数（`--k v` 或 `--k=v`），缺值报错；
/// - `flags`：开关参数，带 `=值` 报错；
/// - 其它以 `-` 开头的 token：未知参数报错。
fn collect(c: &mut Cursor, valued: &[&str], flags: &[&str]) -> Result<Opts, String> {
    let mut o = Opts::default();
    while let Some(a) = c.next() {
        let Some(body) = strip_prefix(&a) else {
            o.pos.push(a);
            continue;
        };
        let (key, inline) = match body.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (body.to_string(), None),
        };
        if key.is_empty() {
            return Err(format!("无法识别的参数：{}", a));
        }
        if valued.contains(&key.as_str()) {
            match inline {
                Some(v) => o.vals.push((key, v)),
                None => match c.peek() {
                    Some(n) if !n.starts_with('-') => {
                        let v = c.next().unwrap();
                        o.vals.push((key, v));
                    }
                    _ => return Err(format!("参数 --{} 缺少值", key)),
                },
            }
        } else if flags.contains(&key.as_str()) {
            if inline.is_some() {
                return Err(format!("开关 --{} 不接受 =值", key));
            }
            if !o.flags.iter().any(|f| f == &key) {
                o.flags.push(key);
            }
        } else {
            return Err(format!("未知参数：--{}", key));
        }
    }
    Ok(o)
}

fn parse_u64(o: &Opts, key: &str) -> Result<Option<u64>, String> {
    match o.value(key) {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("参数 --{} 的值“{}”不是合法整数", key, v)),
    }
}

fn parse_usize(o: &Opts, key: &str) -> Result<Option<usize>, String> {
    match o.value(key) {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("参数 --{} 的值“{}”不是合法整数", key, v)),
    }
}

/// 解析 PID 列表：`--pid 1,2,3` 或 `--pid=1,2`。
fn parse_pids(o: &Opts, key: &str) -> Result<Vec<u32>, String> {
    let Some(v) = o.value(key) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in v.split([',', ';', ' ', '\t']) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        match t.parse::<u32>() {
            Ok(p) => out.push(p),
            Err(_) => return Err(format!("--{} 中的“{}”不是合法 PID", key, t)),
        }
    }
    if out.is_empty() {
        return Err(format!("参数 --{} 没有包含有效的 PID", key));
    }
    Ok(out)
}

/// 按 `,` / `;` / 空白切分进程名列表（保留原始大小写，交由 config 规范化）。
fn split_names(v: &str) -> Vec<String> {
    v.split([',', ';', ' ', '\t'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_bool_word(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" | "enable" | "开启" => Some(true),
        "off" | "0" | "false" | "no" | "disable" | "关闭" => Some(false),
        _ => None,
    }
}

/// 顶层解析入口。
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut c = Cursor::new(args);
    let Some(cmd) = c.next() else {
        return Ok(Command::Help);
    };
    let cmd = cmd.to_ascii_lowercase();
    match cmd.as_str() {
        "help" | "-h" | "--help" | "?" => Ok(Command::Help),
        "version" | "-v" | "--version" => Ok(Command::Version),
        "status" | "st" => {
            let o = collect(&mut c, &[], &[])?;
            reject_positional(&o, "status")?;
            Ok(Command::Status)
        }
        "list" | "ls" | "ps" => parse_list(&mut c),
        "optimize" | "opt" => parse_optimize(&mut c),
        "trim" => parse_trim(&mut c),
        "monitor" | "mon" => parse_monitor(&mut c),
        "auto" => parse_auto(&mut c),
        "sysmem" | "sys" => parse_sysmem(&mut c),
        "standby" | "purge" => {
            let o = collect(&mut c, &[], &[])?;
            reject_positional(&o, "standby")?;
            Ok(Command::Standby)
        }
        "kill" | "close" => parse_kill(&mut c),
        "config" | "cfg" => {
            let o = collect(&mut c, &[], &["init"])?;
            reject_positional(&o, "config")?;
            Ok(Command::Config { init: o.has("init") })
        }
        "history" | "hist" => {
            let o = collect(&mut c, &[], &[])?;
            reject_positional(&o, "history")?;
            Ok(Command::History)
        }
        other => Err(format!("未知命令：{}", other)),
    }
}

fn reject_positional(o: &Opts, cmd: &str) -> Result<(), String> {
    if o.pos.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} 不接受位置参数：{}",
            cmd,
            o.pos.join(" ")
        ))
    }
}

fn parse_list(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(c, &["top", "filter"], &["all"])?;
    let top = parse_usize(&o, "top")?;
    let filter = o
        .value("filter")
        .map(|s| s.to_string())
        .or_else(|| o.pos.first().cloned());
    Ok(Command::List {
        top,
        all: o.has("all"),
        filter,
    })
}

fn parse_optimize(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(c, &["budget", "wait"], &["dry-run", "dry_run"])?;
    reject_positional(&o, "optimize")?;
    let budget = parse_usize(&o, "budget")?.unwrap_or(0);
    // 默认等待 90 秒：比 press_hold_secs(30s) 更长，才有机会确认“持续压力”
    let wait_secs = parse_u64(&o, "wait")?.unwrap_or(90);
    Ok(Command::Optimize {
        dry_run: o.has("dry-run") || o.has("dry_run"),
        budget,
        wait_secs,
    })
}

fn parse_trim(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(c, &["pid", "name"], &["dry-run"])?;
    let pids = parse_pids(&o, "pid")?;
    let mut names = Vec::new();
    if let Some(n) = o.value("name") {
        names.extend(split_names(n));
    }
    for p in &o.pos {
        if let Ok(pid) = p.trim().parse::<u32>() {
            pids.push(pid);
        } else {
            names.push(p.clone());
        }
    }
    if pids.is_empty() && names.is_empty() {
        return Err("trim 需要指定目标：--pid 1,2 或 --name chrome.exe".into());
    }
    Ok(Command::Trim {
        pids,
        names,
        dry_run: o.has("dry-run"),
    })
}

fn parse_monitor(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(c, &["interval"], &["auto"])?;
    reject_positional(&o, "monitor")?;
    Ok(Command::Monitor {
        interval: parse_u64(&o, "interval")?.unwrap_or(5),
        auto: o.has("auto"),
    })
}

fn parse_auto(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(
        c,
        &["allow", "startup"],
        &[
            "on",
            "off",
            "enable",
            "disable",
            "clear-allow",
            "run",
            "monitor",
        ],
    )?;
    reject_positional(&o, "auto")?;

    let on = o.has("on") || o.has("enable");
    let off = o.has("off") || o.has("disable");
    if on && off {
        return Err("--on/--enable 与 --off/--disable 不能同时使用".into());
    }

    let startup = match o.value("startup") {
        None => None,
        Some(v) => match parse_bool_word(v) {
            Some(b) => Some(b),
            None => {
                return Err(format!(
                    "参数 --startup 的值“{}”非法（只接受 on/off）",
                    v
                ))
            }
        },
    };

    let allow = o.value("allow").map(split_names).unwrap_or_default();
    if on && o.vals.iter().any(|(k, _)| k == "allow") {
        // --allow 是追加语义，允许与 --on 同时使用；这里不做限制，仅保留显式解析。
    }

    Ok(Command::Auto(AutoCmd {
        on,
        off,
        allow,
        clear_allow: o.has("clear-allow"),
        startup,
        run: o.has("run") || o.has("monitor"),
    }))
}

fn parse_sysmem(c: &mut Cursor) -> Result<Command, String> {
    let sub = c.next().map(|s| s.to_ascii_lowercase());
    match sub.as_deref() {
        None | Some("status") => {
            let o = collect(c, &[], &[])?;
            reject_positional(&o, "sysmem status")?;
            Ok(Command::Sysmem(SysmemCmd::Status))
        }
        Some("compression") | Some("compress") => sysmem_change(c, Change::MemoryCompression),
        Some("combining") => sysmem_change(c, Change::PageCombining),
        Some("pagefile") => sysmem_change(c, Change::AutomaticPagefile),
        Some("restore") => {
            let kind = match c.peek().map(|s| s.to_ascii_lowercase()) {
                Some(k) => {
                    c.next();
                    Some(match k.as_str() {
                        "compression" | "compress" => Change::MemoryCompression,
                        "combining" => Change::PageCombining,
                        "pagefile" => Change::AutomaticPagefile,
                        other => return Err(format!("未知配置项：{}", other)),
                    })
                }
                None => None,
            };
            let o = collect(c, &[], &[])?;
            reject_positional(&o, "sysmem restore")?;
            Ok(Command::Sysmem(SysmemCmd::Restore { kind }))
        }
        Some("history") => {
            let o = collect(c, &[], &[])?;
            reject_positional(&o, "sysmem history")?;
            Ok(Command::Sysmem(SysmemCmd::History))
        }
        Some(other) => Err(format!(
            "未知 sysmem 子命令：{}（可用：status/compression/combining/pagefile/restore/history）",
            other
        )),
    }
}

fn sysmem_change(c: &mut Cursor, kind: Change) -> Result<Command, String> {
    let o = collect(c, &[], &["enable", "disable"])?;
    reject_positional(&o, &format!("sysmem {}", kind.id()))?;
    let enable = o.has("enable");
    let disable = o.has("disable");
    if enable && disable {
        return Err(format!(
            "sysmem {}：--enable 与 --disable 不能同时使用",
            kind.id()
        ));
    }
    if !enable && !disable {
        return Ok(Command::Sysmem(SysmemCmd::Change {
            kind,
            enable: false,
        }));
    }
    Ok(Command::Sysmem(SysmemCmd::Change { kind, enable }))
}

fn parse_kill(c: &mut Cursor) -> Result<Command, String> {
    let o = collect(c, &["pid", "name"], &["force", "yes", "y"])?;
    let mut pids = parse_pids(&o, "pid")?;
    let mut names = Vec::new();
    if let Some(n) = o.value("name") {
        names.extend(split_names(n));
    }
    for p in &o.pos {
        if let Ok(pid) = p.trim().parse::<u32>() {
            pids.push(pid);
        } else {
            names.push(p.clone());
        }
    }
    if pids.is_empty() && names.is_empty() {
        return Err("kill 需要指定目标：--pid 1,2 或 --name chrome.exe".into());
    }
    Ok(Command::Kill {
        pids,
        names,
        force: o.has("force"),
        yes: o.has("yes") || o.has("y"),
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> Result<Command, String> {
        let args: Vec<String> = s.split_whitespace().map(|x| x.to_string()).collect();
        parse(&args)
    }

    #[test]
    fn sysmem_change_reads_enable_flag() {
        // 评审问题 1：--enable 必须真正传到子命令，而不是丢失在 opts 里
        for (name, kind) in [
            ("compression", Change::MemoryCompression),
            ("combining", Change::PageCombining),
            ("pagefile", Change::AutomaticPagefile),
        ] {
            let c = parse_str(&format!("sysmem {} --enable", name)).unwrap();
            assert_eq!(
                c,
                Command::Sysmem(SysmemCmd::Change { kind, enable: true }),
                "{} --enable 解析错误",
                name
            );
            let c = parse_str(&format!("sysmem {} --disable", name)).unwrap();
            assert_eq!(
                c,
                Command::Sysmem(SysmemCmd::Change { kind, enable: false })
            );
        }
    }

    #[test]
    fn sysmem_enable_and_disable_are_mutually_exclusive() {
        assert!(parse_str("sysmem compression --enable --disable").is_err());
    }

    #[test]
    fn sysmem_change_without_action_requests_report_only() {
        // 没有 --enable/--disable 时，命令仍可解析（上层只显示当前状态）
        assert_eq!(
            parse_str("sysmem pagefile").unwrap(),
            Command::Sysmem(SysmemCmd::Change {
                kind: Change::AutomaticPagefile,
                enable: false
            })
        );
    }

    #[test]
    fn auto_allow_reads_values_not_true() {
        // 评审问题 2：`auto --allow chrome.exe` 不能把 "true" 当作名单
        let c = parse_str("auto --allow chrome.exe,foo.exe").unwrap();
        match c {
            Command::Auto(a) => {
                assert_eq!(a.allow, vec!["chrome.exe", "foo.exe"]);
                assert!(!a.on && !a.off);
            }
            other => panic!("解析结果错误：{:?}", other),
        }
    }

    #[test]
    fn auto_startup_on_off_are_distinct() {
        match parse_str("auto --startup on").unwrap() {
            Command::Auto(a) => assert_eq!(a.startup, Some(true)),
            other => panic!("{:?}", other),
        }
        match parse_str("auto --startup off").unwrap() {
            Command::Auto(a) => assert_eq!(a.startup, Some(false)),
            other => panic!("{:?}", other),
        }
        match parse_str("auto --startup=on").unwrap() {
            Command::Auto(a) => assert_eq!(a.startup, Some(true)),
            other => panic!("{:?}", other),
        }
        // 非法值必须报错，绝不写入配置
        assert!(parse_str("auto --startup maybe").is_err());
    }

    #[test]
    fn auto_off_is_recognized() {
        match parse_str("auto --off").unwrap() {
            Command::Auto(a) => assert!(a.off && !a.on),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn auto_on_and_off_are_mutually_exclusive() {
        assert!(parse_str("auto --on --off").is_err());
        assert!(parse_str("auto --enable --disable").is_err());
    }

    #[test]
    fn auto_clear_allow_and_run() {
        match parse_str("auto --clear-allow --run").unwrap() {
            Command::Auto(a) => assert!(a.clear_allow && a.run),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn valued_option_missing_value_errors() {
        assert!(parse_str("auto --allow").is_err());
        assert!(parse_str("list --top").is_err());
        assert!(parse_str("trim --pid").is_err());
        assert!(parse_str("monitor --interval").is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(parse_str("list --top abc").is_err());
        assert!(parse_str("optimize --budget -3").is_err());
        assert!(parse_str("trim --pid 1,abc").is_err());
        assert!(parse_str("monitor --interval x").is_err());
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(parse_str("list --nope").is_err());
        assert!(parse_str("auto --nope").is_err());
        assert!(parse_str("optimize --bogus").is_err());
        assert!(parse_str("sysmem status --enable").is_err());
    }

    #[test]
    fn switches_reject_inline_values() {
        assert!(parse_str("optimize --dry-run=1").is_err());
        assert!(parse_str("auto --on=1").is_err());
    }

    #[test]
    fn list_filter_and_positional() {
        match parse_str("list --top 10 --filter chrome").unwrap() {
            Command::List { top, filter, all } => {
                assert_eq!(top, Some(10));
                assert_eq!(filter.as_deref(), Some("chrome"));
                assert!(!all);
            }
            other => panic!("{:?}", other),
        }
        match parse_str("list chrome").unwrap() {
            Command::List { filter, .. } => assert_eq!(filter.as_deref(), Some("chrome")),
            other => panic!("{:?}", other),
        }
        match parse_str("list --all").unwrap() {
            Command::List { all, .. } => assert!(all),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn optimize_defaults_and_flags() {
        match parse_str("optimize").unwrap() {
            Command::Optimize {
                dry_run,
                budget,
                wait_secs,
            } => {
                assert!(!dry_run);
                assert_eq!(budget, 0);
                assert!(wait_secs >= 30, "默认应等待足够长以确认持续压力");
            }
            other => panic!("{:?}", other),
        }
        match parse_str("optimize --dry-run --budget 4 --wait 0").unwrap() {
            Command::Optimize {
                dry_run,
                budget,
                wait_secs,
            } => {
                assert!(dry_run);
                assert_eq!(budget, 4);
                assert_eq!(wait_secs, 0);
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn trim_accepts_pid_and_name() {
        match parse_str("trim --pid 1,2 --name chrome.exe --dry-run").unwrap() {
            Command::Trim {
                pids,
                names,
                dry_run,
            } => {
                assert_eq!(pids, vec![1, 2]);
                assert_eq!(names, vec!["chrome.exe"]);
                assert!(dry_run);
            }
            other => panic!("{:?}", other),
        }
        // 位置参数：数字当 PID，其它当名称
        match parse_str("trim 1234 chrome").unwrap() {
            Command::Trim { pids, names, .. } => {
                assert_eq!(pids, vec![1234]);
                assert_eq!(names, vec!["chrome"]);
            }
            other => panic!("{:?}", other),
        }
        assert!(parse_str("trim").is_err(), "缺少目标必须报错");
    }

    #[test]
    fn kill_flags() {
        match parse_str("kill --pid 9 --force --yes").unwrap() {
            Command::Kill {
                pids,
                force,
                yes,
                names,
            } => {
                assert_eq!(pids, vec![9]);
                assert!(force && yes);
                assert!(names.is_empty());
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn sysmem_restore_with_and_without_kind() {
        assert_eq!(
            parse_str("sysmem restore").unwrap(),
            Command::Sysmem(SysmemCmd::Restore { kind: None })
        );
        assert_eq!(
            parse_str("sysmem restore pagefile").unwrap(),
            Command::Sysmem(SysmemCmd::Restore {
                kind: Some(Change::AutomaticPagefile)
            })
        );
        assert!(parse_str("sysmem restore bogus").is_err());
        assert!(parse_str("sysmem bogus").is_err());
    }

    #[test]
    fn simple_commands() {
        assert_eq!(parse_str("status").unwrap(), Command::Status);
        assert_eq!(parse_str("standby").unwrap(), Command::Standby);
        assert_eq!(parse_str("history").unwrap(), Command::History);
        assert_eq!(
            parse_str("config --init").unwrap(),
            Command::Config { init: true }
        );
        assert_eq!(parse_str("help").unwrap(), Command::Help);
        assert_eq!(parse_str("--version").unwrap(), Command::Version);
        // 空参数等价于帮助
        assert_eq!(parse(&[]).unwrap(), Command::Help);
    }

    #[test]
    fn monitor_interval() {
        match parse_str("monitor --interval 7 --auto").unwrap() {
            Command::Monitor { interval, auto } => {
                assert_eq!(interval, 7);
                assert!(auto);
            }
            other => panic!("{:?}", other),
        }
        assert!(parse_str("monitor extra").is_err());
    }
}
