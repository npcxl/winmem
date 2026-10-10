//! winmem —— Windows 内存优化与进程内存栈管理工具（核心库）
//!
//! 该 crate 同时提供命令行程序 `winmem` 与图形界面程序 `winmem-gui`。
//!
//! 分层：
//! - [`win`]：Windows API 封装（句柄 RAII、进程身份、错误分类）
//! - [`metrics`]：系统 / 进程指标采集（读取失败一律为未知，不伪装成 0）
//! - [`policy`]：保护规则、压力判定、冷却（纯逻辑，可单测）
//! - [`engine`]：统一执行引擎（采样 → 判定 → 小批执行 → 观察 → 冷却）
//! - [`config`]：用户配置、保护名单与自动优化允许名单
//! - [`report`]：报告格式化与历史记录
//! - [`term`]：终端渲染

#[cfg(not(windows))]
compile_error!("winmem 仅支持 Windows 平台。");

pub mod config;
pub mod engine;
pub mod json;
pub mod metrics;
pub mod policy;
pub mod report;
pub mod sysmem;
pub mod term;
pub mod win;
