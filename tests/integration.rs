//! Windows 集成测试。
//!
//! 原则：**只操作自己启动的测试子进程**，不对真实系统进程执行任何危险操作。

#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::time::Duration;

use winmem::engine::{Engine, TrimExecutor};
use winmem::win::{self, ProcessIdentity, WinError};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 启动一个存活约 60 秒的后台子进程。
fn spawn_victim() -> Child {
    Command::new("cmd")
        .args(["/C", "ping -n 60 127.0.0.1 > nul"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("无法启动测试子进程")
}

fn wait_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn identity_is_stable_and_detects_pid_reuse() {
    let mut child = spawn_victim();
    let pid = child.id();

    let a = ProcessIdentity::query(pid).expect("应能读取子进程身份");
    let b = ProcessIdentity::query(pid).expect("应能再次读取子进程身份");
    assert_eq!(a, b, "同一进程的创建时间必须一致");

    // 伪造一个不同的创建时间，模拟 PID 被复用后的旧身份
    let forged = ProcessIdentity {
        pid,
        creation_time: a.creation_time.wrapping_add(1),
    };
    let h = win::open_handle(pid, win::PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
    assert!(!forged.matches(h.raw()), "伪造身份必须被识别为不匹配");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn stale_identity_cannot_kill_process() {
    let mut child = spawn_victim();
    let pid = child.id();
    let real = ProcessIdentity::query(pid).expect("应能读取子进程身份");

    // 用错误的创建时间去结束进程：必须被拒绝
    let stale = ProcessIdentity {
        pid,
        creation_time: real.creation_time.wrapping_add(12345),
    };
    let engine = Engine::new(Default::default());
    match engine.kill_verified(&stale) {
        Err(WinError::ProcessGone(_)) | Err(WinError::AccessDenied(_)) | Err(WinError::Os(_)) => {}
        other => panic!("过期身份不应被接受，实际结果：{:?}", other),
    }

    // 进程必须仍然存活
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("查询子进程状态失败").is_none(),
        "身份校验失败时绝不能结束目标进程"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn trim_reduces_working_set_without_killing_process() {
    let mut child = spawn_victim();
    let pid = child.id();
    let identity = ProcessIdentity::query(pid).expect("应能读取子进程身份");

    let before = win::process_memory(pid).expect("应能读取内存计数");
    let before_ws = before.WorkingSetSize as u64;

    let executor = TrimExecutor::new(50);
    let outcome = executor.trim_one(&identity, "cmd.exe");

    assert!(
        outcome.error.is_none(),
        "裁剪自身启动的子进程不应失败：{:?}",
        outcome.error
    );
    assert!(
        outcome.skipped.is_none(),
        "不应被跳过：{:?}",
        outcome.skipped
    );

    // 进程必须仍然存活：工作集裁剪不是结束进程
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("查询子进程状态失败").is_none(),
        "工作集裁剪不得结束进程"
    );

    // 结果要么是明确的减少量，要么是“未知”（None），不能伪装成 0
    if let Ok(after) = win::process_memory(pid) {
        let after_ws = after.WorkingSetSize as u64;
        if let Some(reduction) = outcome.working_set_reduction {
            assert!(
                reduction <= before_ws,
                "减少量不应大于原工作集：{} > {}",
                reduction,
                before_ws
            );
            let _ = after_ws;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn verified_identity_can_kill_and_process_dies() {
    let mut child = spawn_victim();
    let pid = child.id();
    let identity = ProcessIdentity::query(pid).expect("应能读取子进程身份");

    let engine = Engine::new(Default::default());
    engine
        .kill_verified(&identity)
        .expect("身份正确时应能结束进程");

    assert!(
        wait_exit(&mut child, Duration::from_secs(5)),
        "进程应在结束请求后退出"
    );
    let _ = child.wait();
}

#[test]
fn auto_session_stops_promptly_when_cancelled() {
    use std::sync::{Arc, Mutex};
    use winmem::engine::{run_auto_session, AutoStatus, CancelToken};

    let cfg = winmem::config::Config::default();
    let mut engine = Engine::new(cfg);
    let cancel = CancelToken::new();
    let status = Arc::new(Mutex::new(AutoStatus::default()));
    let c2 = cancel.clone();

    let handle = std::thread::spawn(move || {
        run_auto_session(&mut engine, &c2, &status, &mut |_| {});
    });

    // 等会话真正启动
    std::thread::sleep(Duration::from_millis(400));
    cancel.cancel();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(handle.is_finished(), "取消后自动会话必须尽快退出");
    let _ = handle.join();
}

#[test]
fn auto_session_with_empty_allow_list_never_trims() {
    use std::sync::{Arc, Mutex};
    use winmem::engine::{run_auto_session, AutoStatus, CancelToken};

    // 开关已打开，但允许名单为空 ⇒ 只监测，不执行
    let cfg = winmem::config::Config {
        auto_enabled: true,
        allow: Vec::new(),
        ..Default::default()
    };
    let mut engine = Engine::new(cfg);
    let cancel = CancelToken::new();
    let status = Arc::new(Mutex::new(AutoStatus::default()));
    let c2 = cancel.clone();
    let s2 = status.clone();

    let handle = std::thread::spawn(move || {
        run_auto_session(&mut engine, &c2, &s2, &mut |_| {});
    });
    std::thread::sleep(Duration::from_secs(3));
    cancel.cancel();
    let _ = handle.join();

    let st = status.lock().unwrap();
    assert_eq!(st.batches, 0, "允许名单为空时不应执行任何批次");
    assert_eq!(st.trimmed, 0, "允许名单为空时不应处理任何进程");
    assert!(!st.running, "会话结束后应标记为已停止");
}

#[test]
fn system_snapshot_reports_real_values() {
    let mut engine = Engine::new(Default::default());
    let snap = engine.system_snapshot().expect("应能读取系统内存状态");
    assert!(snap.total_phys > 0, "物理内存总量必须为正");
    assert!(snap.avail_phys <= snap.total_phys, "可用内存不应超过总量");
}
