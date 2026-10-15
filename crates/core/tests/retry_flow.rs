//! 全链路场景集成测试:正常完成 / 重试触发 / 超时 / 手动停止 / 禁用任务。
//!
//! 验证调度编排与进程执行的端到端行为,bat 场景仅在 Windows 跑。

use std::sync::mpsc::Receiver;
use std::time::Duration;

use scheduled_task_core::daemon::{DaemonEvent, execute_with_retry, spawn_channel};
use scheduled_task_core::model::{
    DayOfMonth, PyLauncher, RetryPolicy, Schedule, ScriptKind, ScriptSource, ScriptSpec,
    TaskConfig, TaskId,
};
use scheduled_task_core::runner::global_registry;
use scheduled_task_core::state::{TaskStateManager, TaskStatus};

fn temp_script(name: &str, content: &str) -> ScriptSpec {
    let dir = std::env::temp_dir().join("scheduled-task-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    ScriptSpec {
        kind: ScriptKind::Auto,
        source: ScriptSource::File(path.to_string_lossy().into_owned()),
    }
}

fn base_task(id: u64, name: &str, script: ScriptSpec) -> TaskConfig {
    TaskConfig {
        id: TaskId(id),
        name: name.into(),
        script,
        schedule: Schedule::Monthly {
            day: DayOfMonth::new(1).unwrap(),
            times: vec!["08:00:00".parse().unwrap()],
        },
        retry: None,
        timeout: None,
        enabled: true,
        concurrency: Default::default(),
    }
}

fn drain(rx: &Receiver<DaemonEvent>) -> Vec<DaemonEvent> {
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

#[cfg(windows)]
#[test]
fn scenario_normal_completion() {
    let script = temp_script(
        "ok.bat",
        "@echo off\r\necho step-one-ok\r\necho step-two-ok\r\nexit /b 0\r\n",
    );
    let task = base_task(101, "正常完成", script);
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();

    execute_with_retry(&task, &states, &tx);

    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
    let events = drain(&rx);
    assert!(events.iter().any(|e| matches!(
        e,
        DaemonEvent::Finished {
            returncode: 0,
            retry_triggered: false,
            ..
        }
    )));
    assert!(
        events.iter().any(
            |e| matches!(e, DaemonEvent::LogLine { line, .. } if line.contains("step-one-ok"))
        )
    );
}

#[cfg(windows)]
#[test]
fn scenario_retry_exhausted_lands_failed() {
    // 每次运行输出两条命中模式行,窗口=2 即触发;max_retries=1 → 第 2 次尝试
    // 后耗尽 → FAILED(终态,finalize 不覆盖)
    let script = temp_script(
        "flaky.bat",
        "@echo off\r\necho Curl error 7: Failed after 2040 ms\r\necho Curl error 7: Failed after 2051 ms\r\nexit /b 0\r\n",
    );
    let mut task = base_task(102, "重试耗尽", script);
    task.retry = Some(RetryPolicy {
        pattern: "Curl error 7".into(),
        window: 2,
        max_retries: 1,
        cooldown: Duration::from_millis(50),
    });
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();

    execute_with_retry(&task, &states, &tx);

    assert_eq!(
        states.get_status(task.id),
        Some(TaskStatus::Failed),
        "重试耗尽应落 FAILED 且不被 finalize 覆盖"
    );
    let events = drain(&rx);
    // 状态序列:Running → Retrying → Running → Failed
    let statuses: Vec<TaskStatus> = events
        .iter()
        .filter_map(|e| match e {
            DaemonEvent::Status { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            TaskStatus::Running,
            TaskStatus::Retrying,
            TaskStatus::Running,
            TaskStatus::Failed
        ],
        "重试循环应按策略转换状态,耗尽次数后进入失败状态"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        DaemonEvent::Finished {
            retry_triggered: true,
            ..
        }
    )));
}

#[cfg(windows)]
#[test]
fn scenario_timeout_kills_process() {
    // ping 约 3 秒,超时 0.5 秒:进程被终止,输出含超时标记
    let script = temp_script(
        "slow.bat",
        "@echo off\r\necho slow-start\r\nping -n 3 127.0.0.1 >nul\r\n",
    );
    let mut task = base_task(103, "超时终止", script);
    task.timeout = Some(Duration::from_millis(500));
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();

    execute_with_retry(&task, &states, &tx);

    let events = drain(&rx);
    let finished = events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::Finished { returncode, .. } => Some(*returncode),
            _ => None,
        })
        .expect("应有 Finished 事件");
    assert_ne!(finished, 0, "超时被终止,返回码非 0");
    // 无重试配置:不置 FAILED,收尾归一化回 READY
    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
}

#[cfg(windows)]
#[test]
fn scenario_manual_stop() {
    // 长任务 + 400ms 后外部停止:提前终止,收尾归一化 READY
    let script = temp_script(
        "long.bat",
        "@echo off\r\necho long-start\r\nping -n 10 127.0.0.1 >nul\r\n",
    );
    let task = base_task(104, "手动停止", script);
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, _rx) = spawn_channel();

    let stop_id = task.id;
    let stopper = std::thread::spawn(move || {
        // 等待进程完成注册(并行测试下线程调度有抖动,固定延时可能早于注册),
        // 最多等 5s 后做停止请求——语义即"运行中手动停止"
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !global_registry().is_running(stop_id) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        global_registry().stop(stop_id)
    });

    let started = std::time::Instant::now();
    execute_with_retry(&task, &states, &tx);
    let elapsed = started.elapsed();

    assert!(stopper.join().unwrap(), "停止请求应成功");
    assert!(
        elapsed < Duration::from_secs(5),
        "应被提前终止而非跑满 9 秒"
    );
    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
}

#[test]
fn scenario_disabled_task_never_starts() {
    // 禁用任务:DISABLED -> RUNNING 被拒,不起进程,状态不动
    let script = temp_script("disabled.bat", "@echo off\r\necho should-not-run\r\n");
    let mut task = base_task(105, "禁用任务", script);
    task.enabled = false;
    let states = TaskStateManager::new();
    states.reset(task.id, false);
    let (tx, rx) = spawn_channel();

    execute_with_retry(&task, &states, &tx);

    assert_eq!(states.get_status(task.id), Some(TaskStatus::Disabled));
    assert!(!global_registry().is_running(task.id), "禁用任务不应起进程");
    assert!(drain(&rx).is_empty(), "中止执行不应产生任何事件");
}

#[cfg(windows)]
#[test]
fn scenario_external_kill_finalizes_normally() {
    // 第三方杀死进程:执行线程应自行收尾归一化,
    // 之后的 reconcile 对账不得二次改动状态
    use scheduled_task_core::daemon::Daemon;
    use scheduled_task_core::runner::global_registry;

    let script = temp_script(
        "monitor_long.bat",
        "@echo off
echo m-start
ping -n 10 127.0.0.1 >nul
",
    );
    let mut task = base_task(201, "外部杀死", script);
    task.schedule = Schedule::Daily { times: vec![] };
    let states = std::sync::Arc::new(TaskStateManager::new());
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();

    // 执行放在独立线程(模拟生产中的执行线程)
    let worker_task = task.clone();
    let worker_states = states.clone();
    let worker_tx = tx.clone();
    let worker = std::thread::spawn(move || {
        execute_with_retry(&worker_task, &worker_states, &worker_tx);
    });

    // 等待注册
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !global_registry().is_running(task.id) {
        assert!(std::time::Instant::now() < deadline, "任务未按时注册");
        std::thread::sleep(Duration::from_millis(20));
    }
    // 第三方杀死整树
    let pid = global_registry().pid(task.id).expect("应能取到 pid");
    std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    // 等执行线程收尾(最多 10s)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut finished = false;
    while std::time::Instant::now() < deadline {
        if let Ok(DaemonEvent::Finished { returncode, .. }) = rx.try_recv() {
            finished = true;
            assert_ne!(returncode, 0, "被外部杀死的任务返回码非 0");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(finished, "执行线程应在进程死亡后收尾");
    worker.join().unwrap();
    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));

    // 对账对已完成任务是无害空操作
    let daemon = Daemon::new(tx);
    daemon.load(vec![task.clone()]);
    daemon.reconcile();
    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
}

#[test]
fn scenario_reconcile_resets_stuck_running_after_debounce() {
    // 执行线程卡死的安全网:状态停在 RUNNING 而进程不存在,
    // 对账去抖 5 秒后自动 finalize(保留禁用语义)
    use scheduled_task_core::daemon::Daemon;

    let script = ScriptSpec {
        kind: ScriptKind::Auto,
        source: ScriptSource::File("never-executed.bat".into()),
    };
    let task = base_task(202, "卡死重置", script.clone());
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::new(tx);
    daemon.load(vec![task.clone()]);
    // 模拟执行线程卡死:状态停在 RUNNING,但注册表中无进程
    daemon.states().set(task.id, TaskStatus::Running);

    // 第一轮对账:仅启动去抖计时,不重置
    daemon.reconcile();
    assert_eq!(
        daemon.states().get_status(task.id),
        Some(TaskStatus::Running),
        "去抖期内不应重置"
    );

    // 去抖 5 秒后:自动 finalize → 就绪
    std::thread::sleep(Duration::from_millis(5100));
    daemon.reconcile();
    assert_eq!(
        daemon.states().get_status(task.id),
        Some(TaskStatus::Ready),
        "去抖超时后应自动重置"
    );

    // 禁用语义:enabled=false 时应落 DISABLED
    let mut disabled = base_task(203, "卡死重置禁用", script);
    disabled.enabled = false;
    daemon.load(vec![disabled.clone()]);
    daemon.states().set(disabled.id, TaskStatus::Running);
    daemon.reconcile(); // 第一轮对账:启动去抖计时
    std::thread::sleep(Duration::from_millis(5100));
    daemon.reconcile(); // 去抖超时:重置为 DISABLED
    assert_eq!(
        daemon.states().get_status(disabled.id),
        Some(TaskStatus::Disabled),
        "禁用任务的自动重置应落 DISABLED"
    );
}

#[test]
fn scenario_stop_when_executor_gone_finalizes() {
    // 执行线程已结束(注册表无进程)时的停止:状态从 RUNNING 走
    // STOPPING → 就地归一化 → 按启用语义落位
    use scheduled_task_core::daemon::Daemon;

    let script = ScriptSpec {
        kind: ScriptKind::Auto,
        source: ScriptSource::File("never-executed.bat".into()),
    };
    let task = base_task(204, "停止兜底", script.clone());
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::new(tx);
    daemon.load(vec![task.clone()]);
    daemon.states().set(task.id, TaskStatus::Running);

    assert!(daemon.stop_task(task.id), "进程已不存在时停止应视为成功");
    assert_eq!(
        daemon.states().get_status(task.id),
        Some(TaskStatus::Ready),
        "就地归一化应落就绪"
    );

    // 禁用语义:enabled=false 时应落 DISABLED
    let mut disabled = base_task(205, "停止兜底禁用", script);
    disabled.enabled = false;
    daemon.load(vec![disabled.clone()]);
    daemon.states().set(disabled.id, TaskStatus::Running);
    assert!(daemon.stop_task(disabled.id));
    assert_eq!(
        daemon.states().get_status(disabled.id),
        Some(TaskStatus::Disabled)
    );
}

#[test]
fn scenario_python_uv_launcher_resolves() {
    // Python(Uv) 启动:uv run 拉起(冒烟,输出校验)
    let script = temp_script("example_uv.py", "print('uv-python-ok')\n");
    let script = ScriptSpec {
        kind: ScriptKind::Python {
            launcher: PyLauncher::Uv { project: None },
        },
        source: script.source,
    };
    let task = base_task(106, "uv拉起", script);
    let states = TaskStateManager::new();
    states.reset(task.id, true);
    let (tx, rx) = spawn_channel();

    execute_with_retry(&task, &states, &tx);

    assert_eq!(states.get_status(task.id), Some(TaskStatus::Ready));
    let events = drain(&rx);
    assert!(
        events.iter().any(
            |e| matches!(e, DaemonEvent::LogLine { line, .. } if line.contains("uv-python-ok"))
        )
    );
}
