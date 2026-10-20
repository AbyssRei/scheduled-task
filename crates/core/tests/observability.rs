//! 最近执行记录与等待原因的真实执行场景。
#![cfg(windows)]

use scheduled_task_core::daemon::{Admission, Daemon, DaemonEvent, EventReceiver, spawn_channel};
use scheduled_task_core::model::{
    ConcurrencyPolicy, Schedule, ScriptKind, ScriptSource, ScriptSpec, TaskConfig, TaskId,
};
use scheduled_task_core::state::TaskStatus;
use std::time::{Duration, Instant};

fn task(id: u64, script: &str) -> TaskConfig {
    TaskConfig {
        id: TaskId(id),
        name: "执行记录验证".into(),
        script: ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline(script.into()),
            args: vec![],
            working_dir: None,
            env: Default::default(),
            env_file: None,
            env_files: Vec::new(),
        },
        schedule: Schedule::Daily {
            times: vec!["23:59:59".parse().unwrap()],
        },
        retry: None,
        timeout: Some(Duration::from_secs(5)),
        enabled: true,
        concurrency: ConcurrencyPolicy::Parallel,
    }
}

fn finished(rx: &EventReceiver, id: TaskId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "等待执行记录超时");
        if matches!(rx.recv_timeout(Duration::from_millis(100)), Ok(DaemonEvent::Finished { id: actual, .. }) if actual == id)
        {
            return;
        }
    }
}

#[test]
fn 最近结果区分成功失败和用户停止且删除会清理记录() {
    let (tx, rx) = spawn_channel();
    let daemon = Daemon::new(tx);
    for (index, code) in [0, 7].into_iter().enumerate() {
        let task = task(8100 + index as u64, &format!("@exit /b {code}\r\n"));
        daemon.load(vec![task.clone()]);
        daemon.execute_now(task.id);
        finished(&rx, task.id);
        let record = daemon.last_run(task.id).unwrap();
        assert_eq!(record.returncode, code);
        assert!(!record.stopped);
        assert!(record.duration > Duration::ZERO);
        assert_eq!(daemon.states().get_status(task.id), Some(TaskStatus::Ready));
    }
    let task = task(8102, "@echo off\r\nping -n 30 127.0.0.1 >nul\r\n");
    daemon.load(vec![task.clone()]);
    daemon.execute_now(task.id);
    daemon.stop_task(task.id);
    finished(&rx, task.id);
    assert!(daemon.last_run(task.id).unwrap().stopped);
    daemon.load(vec![]);
    assert!(daemon.last_run(task.id).is_none());
}

#[test]
fn 等待原因区分自身执行全局名额和互斥组() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 2);
    let mut first = task(8201, "exit /b 0");
    first.concurrency = ConcurrencyPolicy::Group("备份".into());
    let mut second = first.clone();
    second.id = TaskId(8202);
    let third = task(8203, "exit /b 0");
    daemon.load(vec![first.clone(), second.clone(), third.clone()]);
    assert_eq!(daemon.admit(&first), Admission::Allow);
    assert_eq!(daemon.admit(&first), Admission::Queue);
    assert!(daemon.queue_reason(first.id).unwrap().contains("当前执行"));
    assert_eq!(daemon.admit(&second), Admission::Queue);
    assert!(daemon.queue_reason(second.id).unwrap().contains("互斥组"));
    assert_eq!(daemon.admit(&third), Admission::Allow);
    assert!(daemon.queue_reason(second.id).unwrap().contains("执行名额"));
    daemon.shutdown();
    assert!(daemon.queue_reason(second.id).is_none());
}

#[test]
fn 重试耗尽即使进程已正常退出也不显示成功() {
    use scheduled_task_core::model::RetryPolicy;
    let (tx, rx) = spawn_channel();
    let daemon = Daemon::new(tx);
    let mut task = task(8301, "@echo retry-marker\r\nexit /b 0\r\n");
    task.retry = Some(RetryPolicy {
        pattern: "retry-marker".into(),
        window: 1,
        max_retries: 1,
        cooldown: Duration::ZERO,
    });
    daemon.load(vec![task.clone()]);
    daemon.execute_now(task.id);
    finished(&rx, task.id);
    let mut record = daemon.last_run(task.id).unwrap();
    assert!(record.retry_triggered);
    assert!(!record.succeeded());
    assert_eq!(
        daemon.states().get_status(task.id),
        Some(TaskStatus::Failed)
    );
    // 进程抢先自然退出时，退出码可能是 0；结果仍需遵守重试耗尽语义。
    record.returncode = 0;
    assert!(!record.succeeded());
}
