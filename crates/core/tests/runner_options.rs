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
            env_file: Some(".env".into()), env_order: Vec::new(), env_files: Vec::new(),
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
            env_order: Vec::new(),
            env_files: Vec::new(),
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
fn 显式变量按保存顺序覆盖大小写同名变量() {
    for (order, expected) in [
        (
            vec!["scheduled_task_env_order", "SCHEDULED_TASK_ENV_ORDER"],
            "upper",
        ),
        (
            vec!["SCHEDULED_TASK_ENV_ORDER", "scheduled_task_env_order"],
            "lower",
        ),
    ] {
        let result = run(
            &ScriptSpec {
                kind: ScriptKind::Cmd,
                source: ScriptSource::Inline(
                    "@echo off\r\necho ORDER=%SCHEDULED_TASK_ENV_ORDER%\r\n".into(),
                ),
                args: Vec::new(),
                working_dir: None,
                env: [
                    ("SCHEDULED_TASK_ENV_ORDER".into(), "upper".into()),
                    ("scheduled_task_env_order".into(), "lower".into()),
                ]
                .into_iter()
                .collect(),
                env_order: order.into_iter().map(str::to_owned).collect(),
                env_file: None,
                env_files: Vec::new(),
            },
            RunOptions {
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.returncode, 0, "{}", result.output);
        assert!(
            result.output.contains(&format!("ORDER={expected}")),
            "{}",
            result.output
        );
    }
}

#[test]
fn 多环境文件按最终工作目录解析并依序覆盖且显式变量优先() {
    let directory = TempDirectory::new();
    let scripts = directory.path().join("scripts");
    let working = directory.path().join("数据 工作");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(working.join("配置")).unwrap();
    let script = scripts.join("print.cmd");
    std::fs::write(&script, "@echo off\r\necho LEGACY_ONLY=%LEGACY_ONLY%\r\necho BASE_ONLY=%BASE_ONLY%\r\necho LAST_ONLY=%LAST_ONLY%\r\necho ORDER=%ORDER%\r\necho PRIORITY=%PRIORITY%\r\necho MIXED_NAME=%MIXED_NAME%\r\necho CASEKEY=%CASEKEY%\r\necho EMPTY=[%EMPTY%]\r\necho COMSPEC=%COMSPEC%\r\n").unwrap();
    std::fs::write(
        working.join("legacy.env"),
        "LEGACY_ONLY=legacy\nORDER=legacy\nPRIORITY=legacy\nMIXED_NAME=legacy\nCaseKey=legacy\n",
    )
    .unwrap();
    std::fs::write(
        working.join("配置/base.env"),
        "BASE_ONLY=base\nORDER=base\nPRIORITY=base\nmixed_name=base\nCaseKey=base\nEMPTY=base\n",
    )
    .unwrap();
    let last_file = directory.path().join("额外 配置.env");
    std::fs::write(
        &last_file,
        "LAST_ONLY=last\nORDER=last\nPRIORITY=last\nMIXED_NAME=last\nCASEKEY=last\nEMPTY=\n",
    )
    .unwrap();
    let last_file = last_file.to_string_lossy().into_owned();
    for (files, expected_order, expected_empty) in [
        (vec!["配置/base.env".into(), last_file.clone()], "last", ""),
        (
            vec![last_file.clone(), "配置/base.env".into()],
            "base",
            "base",
        ),
        (
            vec!["配置/base.env".into(), last_file, "配置/base.env".into()],
            "base",
            "base",
        ),
    ] {
        let result = run(
            &ScriptSpec {
                kind: ScriptKind::Cmd,
                source: ScriptSource::File(script.to_string_lossy().into_owned()),
                args: vec![],
                working_dir: Some("../数据 工作".into()),
                env: [
                    ("PRIORITY".into(), "explicit".into()),
                    ("casekey".into(), "explicit-case".into()),
                ]
                .into_iter()
                .collect(),
                env_file: Some("legacy.env".into()),
                env_order: Vec::new(),
                env_files: files,
            },
            RunOptions {
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.returncode, 0, "{}", result.output);
        for expected in [
            "LEGACY_ONLY=legacy".to_owned(),
            "BASE_ONLY=base".into(),
            "LAST_ONLY=last".into(),
            "PRIORITY=explicit".into(),
            "CASEKEY=explicit-case".into(),
            format!("ORDER={expected_order}"),
            format!("MIXED_NAME={expected_order}"),
            format!("EMPTY=[{expected_empty}]"),
            format!("COMSPEC={}", std::env::var("COMSPEC").unwrap()),
        ] {
            assert!(
                result.output.lines().any(|line| line == expected),
                "缺少 {expected}: {}",
                result.output
            );
        }
    }
}

#[test]
fn 任一环境文件缺失或格式无效时均不启动脚本() {
    let directory = TempDirectory::new();
    std::fs::write(directory.path().join("good.env"), "VALID=1\n").unwrap();
    std::fs::write(directory.path().join("invalid.env"), "BROKEN_LINE\n").unwrap();
    for (files, expected) in [
        (vec!["missing.env", "good.env"], "missing.env"),
        (vec!["good.env", "missing.env"], "missing.env"),
        (vec!["invalid.env", "good.env"], "invalid.env"),
        (vec!["good.env", "invalid.env"], "invalid.env"),
    ] {
        let error = run(
            &ScriptSpec {
                kind: ScriptKind::Cmd,
                source: ScriptSource::Inline("@echo started>started.marker\r\n".into()),
                args: vec![],
                working_dir: Some(directory.path().to_string_lossy().into_owned()),
                env: Default::default(),
                env_file: None,
                env_order: Vec::new(),
                env_files: files.into_iter().map(str::to_owned).collect(),
            },
            RunOptions {
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, RunnerError::Resolve(message) if message.contains(expected)));
        assert!(!directory.path().join("started.marker").exists());
    }
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
                env_order: Vec::new(),
                env_files: Vec::new(),
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

#[test]
fn 大输出保持逐段回调且捕获内存有界() {
    use scheduled_task_core::runner::{Charset, OutputCapture};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let bytes = Arc::new(AtomicUsize::new(0));
    let captured = bytes.clone();
    let result = run(&ScriptSpec {
        kind: ScriptKind::Pwsh,
        source: ScriptSource::Inline("[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); [Console]::Out.Write(('字' * 700000))".into()),
        args: vec![], working_dir: None, env: Default::default(), env_file: None, env_order: Vec::new(), env_files: Vec::new(),
    }, RunOptions {
        capture: OutputCapture::Tail(8192), charset: Charset::Utf8,
        timeout: Some(Duration::from_secs(15)),
        on_line: Some(Box::new(move |line| {
            assert!(line.len() <= 64 * 1024);
            assert!(line.chars().all(|character| character == '字'));
            captured.fetch_add(line.len(), Ordering::SeqCst);
        })), ..Default::default()
    }).unwrap();
    assert_eq!(result.returncode, 0);
    assert_eq!(bytes.load(Ordering::SeqCst), 700000 * 3);
    assert!(result.output.len() <= 8192);
    assert!(result.output_truncated);
}

#[test]
fn 输出管道积压时取消仍能结束读取线程() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let callback_cancel = cancel.clone();
    let result = run(
        &ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline(
                "@echo off\r\nfor /L %%i in (1,1,100000) do echo %%i\r\n".into(),
            ),
            args: vec![],
            working_dir: None,
            env: Default::default(),
            env_file: None,
            env_order: Vec::new(),
            env_files: Vec::new(),
        },
        RunOptions {
            cancel: Some(cancel),
            timeout: Some(Duration::from_secs(5)),
            on_line: Some(Box::new(move |_| {
                std::thread::sleep(Duration::from_millis(100));
                callback_cancel.store(true, Ordering::SeqCst);
            })),
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(result.returncode, 0);
}
