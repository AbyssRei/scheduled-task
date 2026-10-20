//! 任务执行选项的真实脚本测试:参数、工作目录、环境变量及 dotenv。

#![cfg(windows)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use scheduled_task_core::model::{ScriptKind, ScriptSource, ScriptSpec};
use scheduled_task_core::runner::{RunOptions, RunnerError, run};

struct TempDirectory(std::path::PathBuf);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

impl TempDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "scheduled-task-runner-options-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn command_options_reach_script_with_expected_precedence() {
    let directory = TempDirectory::new();
    std::fs::write(
        directory.path().join(".env"),
        "DOTENV_ONLY=from-dotenv\nOVERRIDE=from-dotenv\n",
    )
    .unwrap();

    let mut env = BTreeMap::new();
    env.insert("OVERRIDE".into(), "from-explicit".into());
    let result = run(
        &ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline(
                "@echo off\r\necho ARG1=%~1\r\necho ARG2=%~2\r\necho CWD=%CD%\r\necho DOTENV_ONLY=%DOTENV_ONLY%\r\necho OVERRIDE=%OVERRIDE%\r\n"
                    .into(),
            ),
            args: vec!["first value".into(), "second".into()],
            working_dir: Some(directory.path().to_string_lossy().into_owned()),
            env,
            env_file: Some(".env".into()),
        },
        RunOptions {
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(result.returncode, 0, "脚本输出: {}", result.output);
    assert!(result.output.lines().any(|line| line == "ARG1=first value"));
    assert!(result.output.lines().any(|line| line == "ARG2=second"));
    assert!(
        result
            .output
            .lines()
            .any(|line| line == format!("CWD={}", directory.path().display()))
    );
    assert!(
        result
            .output
            .lines()
            .any(|line| line == "DOTENV_ONLY=from-dotenv")
    );
    assert!(
        result
            .output
            .lines()
            .any(|line| line == "OVERRIDE=from-explicit")
    );
}

#[test]
fn invalid_dotenv_blocks_process_start() {
    let directory = TempDirectory::new();
    std::fs::write(directory.path().join("invalid.env"), "BROKEN_LINE\n").unwrap();

    let error = run(
        &ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline("@echo off\r\necho should-not-run\r\n".into()),
            args: Vec::new(),
            working_dir: Some(directory.path().to_string_lossy().into_owned()),
            env: BTreeMap::new(),
            env_file: Some("invalid.env".into()),
        },
        RunOptions {
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RunnerError::Resolve(message) if message.contains(".env") && message.contains("缺少")
    ));
}

#[test]
fn cmd_and_manual_commands_preserve_literal_arguments() {
    let directory = TempDirectory::new();
    let executable = std::env::current_exe().unwrap();
    let probe = format!(
        "\"{}\" --exact argument_probe --ignored --nocapture --",
        executable.display()
    );
    let args = [
        "带空格的参数",
        "alpha&echo SHOULD_NOT_EXECUTE",
        "%COMSPEC%",
        "!PATH!",
        "a|b<c>d^e(f)",
        "",
        "a\"b",
        "quoted\"&echo SHOULD_NOT_EXECUTE",
        "尾部\\",
    ];
    for kind in [
        ScriptKind::Cmd,
        ScriptKind::Manual {
            command: probe.clone(),
        },
    ] {
        let mut environment = BTreeMap::new();
        environment.insert("SCHEDULED_TASK_ARGUMENT_PROBE".into(), "1".into());
        for (index, value) in args.iter().enumerate() {
            environment.insert(format!("EXPECTED_ARGUMENT_{index}"), (*value).into());
        }
        let result = run(
            &ScriptSpec {
                kind: kind.clone(),
                source: ScriptSource::Inline(format!(
                    "@echo off\r\n{probe} %*\r\nexit /b %errorlevel%\r\n"
                )),
                args: args.iter().map(|value| (*value).into()).collect(),
                working_dir: Some(directory.path().to_string_lossy().into_owned()),
                env: environment,
                env_file: None,
            },
            RunOptions {
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.returncode, 0, "{kind:?}: {}", result.output);
        assert!(
            result.output.contains("ARGUMENT_PROBE_OK"),
            "{}",
            result.output
        );
    }
}

#[test]
#[ignore = "仅作为参数测试的隔离子进程入口"]
fn argument_probe() {
    if std::env::var_os("SCHEDULED_TASK_ARGUMENT_PROBE").is_none() {
        return;
    }
    let arguments: Vec<_> = std::env::args()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect();
    let expected: Vec<_> = (0..9)
        .map(|index| std::env::var(format!("EXPECTED_ARGUMENT_{index}")).unwrap())
        .collect();
    assert_eq!(arguments, expected);
    println!("ARGUMENT_PROBE_OK");
}
