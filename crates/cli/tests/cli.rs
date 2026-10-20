//! CLI 的真实隔离进程验证，不读取用户配置或启动计划任务。
#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Directory(PathBuf);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "scheduled-task-cli-{}-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn invoke(&self, arguments: &[&str]) -> i32 {
        use std::os::windows::process::CommandExt;
        let mut child = Command::new(env!("CARGO_BIN_EXE_scheduled-task-cli"))
            .args(arguments)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI 未及时退出：{arguments:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn config(&self, enabled: bool, code: i32) {
        std::fs::write(self.0.join("tasks.ron"), format!(r#"(config_version: 2, tasks: [(
            id: (1), name: "隔离验证", enabled: {enabled},
            script: (kind: cmd, source: inline("@echo off\r\necho CLI_FIXTURE\r\nexit /b {code}\r\n")),
            schedule: daily(times: ["23:59:59"])
        )])"#)).unwrap();
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn 返回码区分成功失败禁用及不存在的任务() {
    let directory = Directory::new();
    directory.config(true, 0);
    assert_eq!(directory.invoke(&["run", "tasks.ron", "--task", "1"]), 0);
    directory.config(true, 7);
    assert_eq!(directory.invoke(&["run", "tasks.ron", "--task", "1"]), 1);
    directory.config(false, 0);
    assert_eq!(directory.invoke(&["run", "tasks.ron", "--task", "1"]), 3);
    assert_eq!(
        directory.invoke(&["run", "tasks.ron", "--task", "missing"]),
        3
    );
    assert_eq!(directory.invoke(&["run", "missing.ron", "--task", "1"]), 3);
}

#[test]
fn 未知选项缺少参数及多余参数立即返回用法错误() {
    let directory = Directory::new();
    for arguments in [
        vec![],
        vec!["run"],
        vec!["run", "tasks.ron", "--task"],
        vec!["run", "tasks.ron", "--unknown", "1"],
        vec!["run", "tasks.ron", "--task", "1", "extra"],
    ] {
        assert_eq!(directory.invoke(&arguments), 2);
        assert!(!directory.0.join("logs").exists());
    }
}
