//! winmem —— Windows 内存优化与进程内存栈管理工具（核心库）
//!
//! 该 crate 同时提供：
//! - 命令行程序 `winmem`
//! - 图形界面程序 `winmem-gui`
//!
//! 模块划分：
//! - [`win`]：Windows 原生 API 绑定（进程枚举、内存计数、特权、待机列表）
//! - [`ops`]：业务层（内存栈枚举、排序过滤、内存释放、一键优化）
//! - [`term`]：终端输出（仅命令行程序使用）

#[cfg(not(windows))]
compile_error!("winmem 仅支持 Windows 平台。");

pub mod ops;
pub mod term;
pub mod win;
