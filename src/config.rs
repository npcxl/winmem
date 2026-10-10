//! 用户配置：策略阈值、保护名单、自动优化允许名单。
//!
//! 配置文件为扁平 `key = value` 文本（手写解析，无需第三方 crate），
//! 默认位于 `%APPDATA%\winmem\config.ini`。所有策略参数集中在此，
//! 避免魔法数字散落在各处。

use std::collections::BTreeMap;
use std::path::PathBuf;

/// 策略参数。初始值均为经验值，需按实际负载实测调整。
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyConfig {
    /// 进入低内存压力的可用内存比例（低于此值才考虑优化）
    pub press_enter_free_ratio: f64,
    /// 退出低内存压力的可用内存比例（滞回，避免反复进出）
    pub press_exit_free_ratio: f64,
    /// 需要持续多久的低内存才真正进入压力处理（秒）
    pub press_hold_secs: u64,
    /// 提交量提示阈值（占比）
    pub commit_warn_ratio: f64,
    /// 提交量告警阈值（占比）
    pub commit_alert_ratio: f64,
    /// 每个批次处理的候选数
    pub batch_size: usize,
    /// 一次运行的最大处理进程数（执行预算）
    pub run_budget: usize,
    /// 单个进程裁剪后的冷却时长（秒）
    pub cooldown_secs: u64,
    /// 单批执行后的观察时长（秒）
    pub observe_secs: u64,
    /// 延迟跟踪节点（秒）：默认 1 分钟、5 分钟
    pub track_secs: Vec<u64>,
    /// 单批最小收益（字节）；低于此值视为收益不足并延长冷却
    pub min_gain_bytes: u64,
    /// 近期前台活动保护窗口（秒）
    pub recent_foreground_secs: u64,
    /// 候选最小工作集（字节），过小不值得处理
    pub min_candidate_ws: u64,
    /// 视为“活跃计算”的 CPU 占用阈值（本进程区间内的百分比）
    pub busy_cpu_percent: f64,
    /// 是否允许自动处理未在允许名单中的进程（默认 false）
    pub auto_allow_unlisted: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            press_enter_free_ratio: 0.10,
            press_exit_free_ratio: 0.15,
            press_hold_secs: 30,
            commit_warn_ratio: 0.85,
            commit_alert_ratio: 0.90,
            batch_size: 2,
            run_budget: 12,
            cooldown_secs: 20 * 60,
            observe_secs: 30,
            track_secs: vec![60, 300],
            min_gain_bytes: 64 * 1024 * 1024,
            recent_foreground_secs: 600,
            min_candidate_ws: 128 * 1024 * 1024,
            busy_cpu_percent: 5.0,
            auto_allow_unlisted: false,
        }
    }
}

/// 应用级配置。
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub policy: PolicyConfig,
    /// 用户保护名单（进程名，忽略大小写与 .exe 后缀）
    pub protect: Vec<String>,
    /// 自动优化允许名单；自动模式默认只处理这里的进程
    pub allow: Vec<String>,
    /// 用户是否显式开启自动执行
    pub auto_enabled: bool,
    /// 配置文件路径（加载失败时仍为空）
    pub path: Option<PathBuf>,
    /// 加载过程中的告警（未知键、值非法等）
    pub warnings: Vec<String>,
}

impl Config {
    /// 配置文件默认路径：`%APPDATA%\winmem\config.ini`。
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")?;
        let mut p = PathBuf::from(base);
        p.push("winmem");
        p.push("config.ini");
        Some(p)
    }

    /// 从默认路径加载；文件不存在时返回默认配置（并在 `path` 字段记录位置）。
    pub fn load() -> Config {
        let path = Self::default_path();
        let mut cfg = match &path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(text) => Self::parse(&text),
                Err(_) => Config::default(),
            },
            None => Config::default(),
        };
        cfg.path = path;
        cfg
    }

    /// 解析配置文本。未知键与非法值会记录到 `warnings`，不会导致失败。
    pub fn parse(text: &str) -> Config {
        let policy = PolicyConfig::default();
        let mut cfg = Config {
            policy,
            ..Default::default()
        };
        let mut map: BTreeMap<String, String> = BTreeMap::new();

        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                cfg.warnings
                    .push(format!("第 {} 行缺少 '='，已忽略", idx + 1));
                continue;
            };
            map.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }

        for (k, v) in &map {
            let p = &mut cfg.policy;
            match k.as_str() {
                "press_enter_free_ratio" => {
                    set_f64(&mut p.press_enter_free_ratio, v, k, &mut cfg.warnings)
                }
                "press_exit_free_ratio" => {
                    set_f64(&mut p.press_exit_free_ratio, v, k, &mut cfg.warnings)
                }
                "press_hold_secs" => set_u64(&mut p.press_hold_secs, v, k, &mut cfg.warnings),
                "commit_warn_ratio" => set_f64(&mut p.commit_warn_ratio, v, k, &mut cfg.warnings),
                "commit_alert_ratio" => set_f64(&mut p.commit_alert_ratio, v, k, &mut cfg.warnings),
                "batch_size" => set_usize(&mut p.batch_size, v, k, &mut cfg.warnings),
                "run_budget" => set_usize(&mut p.run_budget, v, k, &mut cfg.warnings),
                "cooldown_secs" => set_u64(&mut p.cooldown_secs, v, k, &mut cfg.warnings),
                "observe_secs" => set_u64(&mut p.observe_secs, v, k, &mut cfg.warnings),
                "min_gain_mb" => set_mb(&mut p.min_gain_bytes, v, k, &mut cfg.warnings),
                "recent_foreground_secs" => {
                    set_u64(&mut p.recent_foreground_secs, v, k, &mut cfg.warnings)
                }
                "min_candidate_ws_mb" => set_mb(&mut p.min_candidate_ws, v, k, &mut cfg.warnings),
                "busy_cpu_percent" => set_f64(&mut p.busy_cpu_percent, v, k, &mut cfg.warnings),
                "auto_allow_unlisted" => {
                    set_bool(&mut p.auto_allow_unlisted, v, k, &mut cfg.warnings)
                }
                "auto_enabled" => set_bool(&mut cfg.auto_enabled, v, k, &mut cfg.warnings),
                "protect" => cfg.protect = split_list(v),
                "allow" => cfg.allow = split_list(v),
                other => cfg.warnings.push(format!("未知配置项：{}", other)),
            }
        }

        // 基本一致性校验：退出阈值必须不小于进入阈值，否则滞回失效
        if cfg.policy.press_exit_free_ratio < cfg.policy.press_enter_free_ratio {
            cfg.warnings.push(format!(
                "press_exit_free_ratio({}) 小于 press_enter_free_ratio({})，已自动纠正",
                cfg.policy.press_exit_free_ratio, cfg.policy.press_enter_free_ratio
            ));
            cfg.policy.press_exit_free_ratio = cfg.policy.press_enter_free_ratio;
        }
        if cfg.policy.batch_size == 0 {
            cfg.warnings.push("batch_size 不能为 0，已改为 1".into());
            cfg.policy.batch_size = 1;
        }
        cfg
    }

    /// 生成默认配置文件内容（供 `--init` 或文档使用）。
    pub fn template() -> String {
        let d = PolicyConfig::default();
        format!(
            "\
# winmem 配置。所有阈值均为经验值，需要按实际负载实测调整。
# 修改后重启程序生效。

# ---- 保护名单：永不自动处理这些进程（逗号分隔，可省略 .exe）----
protect = explorer.exe, dwm.exe

# ---- 自动优化允许名单：自动模式只处理这里的进程 ----
allow =

# 是否允许自动执行优化（默认关闭，monitor 必须显式开启）
auto_enabled = false
# 是否允许处理未列入 allow 的进程（默认 false，风险自负）
auto_allow_unlisted = false

# ---- 压力判定 ----
press_enter_free_ratio = {}
press_exit_free_ratio = {}
press_hold_secs = {}
commit_warn_ratio = {}
commit_alert_ratio = {}

# ---- 执行 ----
batch_size = {}
run_budget = {}
cooldown_secs = {}
observe_secs = {}
min_gain_mb = {}
min_candidate_ws_mb = {}
recent_foreground_secs = {}
busy_cpu_percent = {}
",
            d.press_enter_free_ratio,
            d.press_exit_free_ratio,
            d.press_hold_secs,
            d.commit_warn_ratio,
            d.commit_alert_ratio,
            d.batch_size,
            d.run_budget,
            d.cooldown_secs,
            d.observe_secs,
            d.min_gain_bytes / (1024 * 1024),
            d.min_candidate_ws / (1024 * 1024),
            d.recent_foreground_secs,
            d.busy_cpu_percent
        )
    }
}

fn split_list(v: &str) -> Vec<String> {
    v.split([',', ';', ' ', '\t'])
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn set_bool(slot: &mut bool, v: &str, key: &str, warn: &mut Vec<String>) {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => *slot = true,
        "0" | "false" | "no" | "off" => *slot = false,
        _ => warn.push(format!("{} 的值 '{}' 不是布尔值，已保留默认", key, v)),
    }
}

fn set_f64(slot: &mut f64, v: &str, key: &str, warn: &mut Vec<String>) {
    match v.parse::<f64>() {
        Ok(x) if x.is_finite() && x >= 0.0 => *slot = x,
        _ => warn.push(format!("{} 的值 '{}' 不是合法数字，已保留默认", key, v)),
    }
}

fn set_u64(slot: &mut u64, v: &str, key: &str, warn: &mut Vec<String>) {
    match v.parse::<u64>() {
        Ok(x) => *slot = x,
        Err(_) => warn.push(format!("{} 的值 '{}' 不是合法整数，已保留默认", key, v)),
    }
}

fn set_usize(slot: &mut usize, v: &str, key: &str, warn: &mut Vec<String>) {
    match v.parse::<usize>() {
        Ok(x) => *slot = x,
        Err(_) => warn.push(format!("{} 的值 '{}' 不是合法整数，已保留默认", key, v)),
    }
}

fn set_mb(slot: &mut u64, v: &str, key: &str, warn: &mut Vec<String>) {
    match v.parse::<u64>() {
        Ok(x) => *slot = x * 1024 * 1024,
        Err(_) => warn.push(format!("{} 的值 '{}' 不是合法整数(MB)，已保留默认", key, v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let d = PolicyConfig::default();
        assert!(d.press_exit_free_ratio >= d.press_enter_free_ratio);
        assert!(d.batch_size >= 1);
        assert!(d.cooldown_secs >= 60);
        assert!(
            !d.auto_allow_unlisted,
            "默认不应自动处理未列入允许名单的进程"
        );
    }

    #[test]
    fn parses_values_and_lists() {
        let c = Config::parse(
            "\
# 注释
press_hold_secs = 45
batch_size = 3
min_gain_mb = 32
protect = explorer.exe, Dwm
allow = game.exe ; render.exe
auto_enabled = yes
",
        );
        assert_eq!(c.policy.press_hold_secs, 45);
        assert_eq!(c.policy.batch_size, 3);
        assert_eq!(c.policy.min_gain_bytes, 32 * 1024 * 1024);
        assert_eq!(c.protect, vec!["explorer.exe", "dwm"]);
        assert_eq!(c.allow, vec!["game.exe", "render.exe"]);
        assert!(c.auto_enabled);
        assert!(c.warnings.is_empty(), "不应产生告警：{:?}", c.warnings);
    }

    #[test]
    fn invalid_values_keep_defaults_and_warn() {
        let d = PolicyConfig::default();
        let c = Config::parse("press_hold_secs = abc\nbatch_size = -1\nunknown_key = 1\n");
        assert_eq!(c.policy.press_hold_secs, d.press_hold_secs);
        assert_eq!(c.policy.batch_size, d.batch_size);
        assert!(c.warnings.len() >= 3, "应记录全部问题：{:?}", c.warnings);
        assert!(c.warnings.iter().any(|w| w.contains("未知配置项")));
    }

    #[test]
    fn corrects_invalid_hysteresis() {
        let c = Config::parse("press_enter_free_ratio = 0.20\npress_exit_free_ratio = 0.05\n");
        assert!(c.policy.press_exit_free_ratio >= c.policy.press_enter_free_ratio);
        assert!(c.warnings.iter().any(|w| w.contains("已自动纠正")));
    }

    #[test]
    fn zero_batch_size_is_rejected() {
        let c = Config::parse("batch_size = 0\n");
        assert_eq!(c.policy.batch_size, 1);
    }

    #[test]
    fn template_is_parseable() {
        let c = Config::parse(&Config::template());
        // 模板里的 protect 示例不应产生告警
        assert!(
            !c.warnings.iter().any(|w| w.contains("未知配置项")),
            "模板包含未知键：{:?}",
            c.warnings
        );
    }
}
