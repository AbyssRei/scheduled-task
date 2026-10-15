//! 在无控制台的隔离父进程中验证任务后台执行及输出捕获。

#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use scheduled_task_core::model::{ScriptKind, ScriptSource, ScriptSpec};
use scheduled_task_core::runner::{RunOptions, run};
use windows::Win32::System::Console::{GetConsoleCP, GetConsoleWindow};
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

const HELPER_ENV: &str = "SCHEDULED_TASK_HIDDEN_CONSOLE_TEST";

#[test]
fn scripts_run_without_console_windows_and_keep_output() {
    // 脱离测试终端，模拟桌面程序启动脚本；避免继承隐藏控制台导致假通过。
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "detached_runner", "--ignored", "--nocapture"])
        .env(HELPER_ENV, "1")
        .creation_flags(0x0000_0008) // DETACHED_PROCESS，不为隔离父进程创建窗口。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("后台脚本测试未按时结束");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "后台脚本测试失败：\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "仅由后台执行测试作为无控制台父进程启动"]
fn detached_runner() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    assert!(unsafe { GetConsoleWindow() }.0.is_null());
    let executable = std::env::current_exe().unwrap();
    let executable = executable.to_string_lossy();
    let arguments = "--exact console_probe --ignored --nocapture";
    let cmd_script =
        format!("@echo off\r\n\"{executable}\" {arguments}\r\nexit /b %errorlevel%\r\n");
    let ps_script = format!(
        "& '{}' {arguments}\nexit $LASTEXITCODE\n",
        executable.replace('\'', "''")
    );
    for (kind, content) in [
        (ScriptKind::Cmd, cmd_script),
        (ScriptKind::PowerShell, ps_script.clone()),
        (ScriptKind::Pwsh, ps_script),
    ] {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_lines = lines.clone();
        let result = run(
            &ScriptSpec {
                kind: kind.clone(),
                source: ScriptSource::Inline(content),
            },
            RunOptions {
                timeout: Some(Duration::from_secs(15)),
                on_line: Some(Box::new(move |line| {
                    captured_lines.lock().unwrap().push(line.to_owned());
                })),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.returncode, 0, "{kind:?}：{}", result.output);
        for marker in ["hidden-console-stdout-ok", "hidden-console-stderr-ok"] {
            assert!(result.output.contains(marker), "{kind:?} 未捕获 {marker}");
            assert!(
                lines
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line.contains(marker)),
                "{kind:?} 未回调 {marker}"
            );
        }
    }
}

#[test]
#[ignore = "仅作为被执行脚本的子进程检查窗口状态"]
fn console_probe() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let window = unsafe { GetConsoleWindow() };
    let visible = !window.0.is_null() && unsafe { IsWindowVisible(window) }.as_bool();
    assert!(!visible, "任务不应创建可见控制台窗口");
    // 伪控制台可返回不可见的消息窗口，仍可能对应可见终端，不能只检查可见性。
    assert!(window.0.is_null(), "任务不应关联终端窗口：{window:?}");
    // 无窗口控制台仍可能有代码页，不将 GetConsoleCP 非零误判为弹窗。
    println!("console-code-page={}", unsafe { GetConsoleCP() });
    println!("hidden-console-stdout-ok");
    eprintln!("hidden-console-stderr-ok");
}
