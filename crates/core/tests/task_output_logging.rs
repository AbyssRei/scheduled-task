//! 真实脚本输出同时进入文件日志和界面事件，且遵守日志等级。
#![cfg(windows)]

use scheduled_task_core::daemon::EventSender as Sender;
use std::path::{Path, PathBuf};
use std::time::Duration;

use scheduled_task_core::daemon::{DaemonEvent, execute_with_retry, spawn_channel};
use scheduled_task_core::model::{
    DayOfMonth, RetryPolicy, Schedule, ScriptKind, ScriptSource, ScriptSpec, TaskConfig, TaskId,
};
use scheduled_task_core::state::{TaskStateManager, TaskStatus};

struct LogFile {
    directory: PathBuf,
}

impl LogFile {
    fn new(id: u64) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "scheduled-task-output-log-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("output.log"), "").unwrap();
        Self { directory }
    }

    fn path(&self) -> PathBuf {
        self.directory.join("output.log")
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.path())
            .unwrap()
            .lines()
            .filter(|line| line.contains("scheduled_task_core::task_output"))
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for LogFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn task(id: u64, kind: ScriptKind, script: String) -> TaskConfig {
    TaskConfig {
        id: TaskId(id),
        name: "输出日志测试".into(),
        script: ScriptSpec {
            kind,
            source: ScriptSource::Inline(script),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            env_file: None,
        },
        schedule: Schedule::Monthly {
            day: DayOfMonth::new(1).unwrap(),
            times: vec!["08:00:00".parse().unwrap()],
        },
        retry: None,
        timeout: Some(Duration::from_secs(15)),
        enabled: true,
        concurrency: Default::default(),
    }
}

fn run_logged(task: &TaskConfig, path: &Path, level: tracing::Level, tx: &Sender) {
    let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(level)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let states = TaskStateManager::new();
        states.reset(task.id, true);
        execute_with_retry(task, &states, tx);
        assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
    });
}

#[test]
fn 慢界面省略输出仍完整记录文件并保留完成事件() {
    let log = LogFile::new(7199);
    let task = task(
        7199,
        ScriptKind::Cmd,
        "@echo off\r\nfor /L %%i in (1,1,5000) do echo flood-%%i\r\n".into(),
    );
    let (tx, rx) = spawn_channel();
    run_logged(&task, &log.path(), tracing::Level::INFO, &tx);
    assert_eq!(log.lines().len(), 5000);
    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DaemonEvent::LogsDropped { count } if *count > 0))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DaemonEvent::Finished { returncode: 0, .. }))
    );
}

#[test]
fn info_records_live_stdout_stderr_and_unicode_once() {
    let log = LogFile::new(7101);
    let release = log.directory.join("continue");
    // 匹配运行器默认解码；此处只验证记录链路，不改变终端的编码策略。
    let codepage = if unsafe { windows::Win32::Globalization::GetACP() } == 936 {
        936
    } else {
        65001
    };
    let script = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::GetEncoding({codepage})\n\
         [Console]::Out.WriteLine('标准输出日志')\n\
         while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 20 }}\n\
         [Console]::Error.WriteLine('标准错误日志')\n",
        release.to_string_lossy().replace('\'', "''")
    );
    let task = task(7101, ScriptKind::Pwsh, script);
    let path = log.path();
    let (tx, rx) = spawn_channel();
    let worker = std::thread::spawn(move || run_logged(&task, &path, tracing::Level::INFO, &tx));
    let mut outputs = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            DaemonEvent::LogLine { id, line } => {
                assert_eq!(id, TaskId(7101));
                if line == "标准输出日志" {
                    // 脚本尚在等待继续信号，首行已经落盘，不能等结束后才写日志。
                    let lines = log.lines();
                    assert_eq!(lines.len(), 1);
                    assert!(lines[0].contains("标准输出日志"));
                    std::fs::write(&release, "continue").unwrap();
                }
                outputs.push(line);
            }
            DaemonEvent::Finished { returncode, .. } => {
                assert_eq!(returncode, 0);
                break;
            }
            _ => {}
        }
    }
    worker.join().unwrap();
    assert_eq!(outputs, ["标准输出日志", "标准错误日志"]);
    let lines = log.lines();
    assert_eq!(lines.len(), 2);
    for (line, output) in lines.iter().zip(outputs) {
        assert!(line.contains(&output));
        assert!(line.contains("task_id=7101"));
        assert!(line.contains("task_name=输出日志测试"));
        assert!(line.contains("attempt=1"));
        assert!(line.contains("INFO"));
    }
}

#[test]
fn warning_filter_keeps_task_output_events() {
    let log = LogFile::new(7102);
    let task = task(
        7102,
        ScriptKind::Cmd,
        "@echo off\r\necho visible-in-ui\r\n".into(),
    );
    let (tx, rx) = spawn_channel();
    run_logged(&task, &log.path(), tracing::Level::WARN, &tx);
    assert!(log.lines().is_empty());
    let outputs: Vec<_> = rx
        .try_iter()
        .filter_map(|event| match event {
            DaemonEvent::LogLine { line, .. } => Some(line),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, ["visible-in-ui"]);
}

#[test]
fn disconnected_ui_still_records_output() {
    let log = LogFile::new(7103);
    let task = task(
        7103,
        ScriptKind::Cmd,
        "@echo off\r\necho saved-without-ui\r\n".into(),
    );
    let (tx, rx) = spawn_channel();
    drop(rx);
    run_logged(&task, &log.path(), tracing::Level::INFO, &tx);
    let lines = log.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("saved-without-ui"));
}

#[test]
fn retry_output_has_one_based_attempt_numbers() {
    let log = LogFile::new(7104);
    let task = task(
        7104,
        ScriptKind::Cmd,
        "@echo off\r\necho retry-output\r\nexit /b 1\r\n".into(),
    );
    let task = TaskConfig {
        retry: Some(RetryPolicy {
            pattern: "unused-pattern".into(),
            window: 2,
            max_retries: 1,
            cooldown: Duration::ZERO,
        }),
        ..task
    };
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(log.path())
        .unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();
    tracing::subscriber::with_default(subscriber, || execute_with_retry(&task, &states, &tx));
    assert_eq!(states.get_status(task.id), Some(TaskStatus::Failed));
    let lines = log.lines();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("attempt=1"));
    assert!(lines[1].contains("attempt=2"));
    assert!(lines.iter().all(|line| line.contains("retry-output")));
    assert_eq!(
        rx.try_iter()
            .filter(|event| matches!(event, DaemonEvent::LogLine { .. }))
            .count(),
        2
    );
}
