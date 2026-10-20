//! 并发准入、排队与真实执行生命周期行为测试。

use scheduled_task_core::daemon::{Admission, Daemon, spawn_channel};
use scheduled_task_core::model::{
    ConcurrencyPolicy, Schedule, ScriptKind, ScriptSource, ScriptSpec, TaskConfig, TaskId,
};

fn task(id: u64, policy: ConcurrencyPolicy) -> TaskConfig {
    TaskConfig {
        id: TaskId(id),
        name: format!("任务{id}"),
        script: ScriptSpec {
            kind: ScriptKind::Auto,
            source: ScriptSource::Inline("".into()),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            env_file: None,
            env_order: Vec::new(),
            env_files: Vec::new(),
        },
        schedule: Schedule::Custom { times: vec![] },
        retry: None,
        timeout: None,
        enabled: true,
        concurrency: policy,
    }
}

#[test]
fn global_limit_queues_fifo_and_rejects_duplicate() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 1);
    let first = task(10001, ConcurrencyPolicy::Parallel);
    let second = task(10002, ConcurrencyPolicy::Parallel);
    daemon.load(vec![first.clone(), second.clone()]);

    assert_eq!(daemon.admit(&first), Admission::Allow);
    assert_eq!(daemon.admit(&second), Admission::Queue);
    assert_eq!(daemon.queued_count(), 1);
    assert!(matches!(daemon.admit(&second), Admission::Reject { .. }));
    assert_eq!(daemon.queued_count(), 1);
}

#[test]
fn solo_policy_queues_repeated_trigger_for_same_task() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 4);
    let repeated = task(10003, ConcurrencyPolicy::Solo);
    daemon.load(vec![repeated.clone()]);

    assert_eq!(daemon.admit(&repeated), Admission::Allow);
    assert_eq!(daemon.admit(&repeated), Admission::Queue);
    assert_eq!(daemon.queued_count(), 1);
}

#[test]
fn group_policy_serializes_tasks_with_same_group() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 4);
    let first = task(10004, ConcurrencyPolicy::Group("daily".into()));
    let second = task(10005, ConcurrencyPolicy::Group("daily".into()));
    let unrelated = task(10006, ConcurrencyPolicy::Group("other".into()));
    daemon.load(vec![first.clone(), second.clone(), unrelated.clone()]);

    assert_eq!(daemon.admit(&first), Admission::Allow);
    assert_eq!(daemon.admit(&second), Admission::Queue);
    assert_eq!(daemon.admit(&unrelated), Admission::Allow);
}

#[test]
fn queue_limit_rejects_overflow_without_growing_queue() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 1);
    daemon.set_max_queue_size(1);
    let tasks: Vec<_> = (10007..10010)
        .map(|id| task(id, ConcurrencyPolicy::Solo))
        .collect();
    daemon.load(tasks.clone());
    assert_eq!(daemon.admit(&tasks[0]), Admission::Allow);
    assert_eq!(daemon.admit(&tasks[1]), Admission::Queue);
    assert!(matches!(daemon.admit(&tasks[2]), Admission::Reject { .. }));
    assert_eq!(daemon.queued_count(), 1);
}

#[test]
fn editing_active_policy_preserves_group_queue_order() {
    let (tx, _rx) = spawn_channel();
    let daemon = Daemon::with_max_parallel(tx, 3);
    let mut first = task(10020, ConcurrencyPolicy::Parallel);
    let second = task(10021, ConcurrencyPolicy::Group("串行组".into()));
    daemon.load(vec![first.clone(), second.clone()]);
    assert_eq!(daemon.admit(&first), Admission::Allow);
    first.concurrency = second.concurrency.clone();
    daemon.reload_preserving(vec![first.clone(), second.clone()]);
    assert_eq!(daemon.admit(&first), Admission::Queue);
    // 首项被同 ID 的旧执行占用阻挡,同组后项仍须等待它。
    assert_eq!(daemon.admit(&second), Admission::Queue);
    assert_eq!(daemon.queued_count(), 2);
}

#[cfg(windows)]
mod lifecycle {
    use super::*;
    use scheduled_task_core::daemon::DaemonEvent;
    use scheduled_task_core::daemon::EventReceiver as Receiver;
    use scheduled_task_core::state::TaskStatus;
    use std::time::{Duration, Instant};

    fn executable(id: u64) -> TaskConfig {
        let mut task = task(id, ConcurrencyPolicy::Solo);
        task.script = ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline(
                "@echo off\r\necho started\r\nping 127.0.0.1 -n 2 >nul\r\nexit /b 0\r\n".into(),
            ),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            env_file: None,
            env_order: Vec::new(),
            env_files: Vec::new(),
        };
        task
    }

    fn receive_until(
        rx: &Receiver,
        events: &mut Vec<DaemonEvent>,
        predicate: impl Fn(&DaemonEvent) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("等待任务事件超时");
            let done = predicate(&event);
            events.push(event);
            if done {
                return;
            }
        }
    }

    fn started(event: &DaemonEvent, id: TaskId) -> bool {
        matches!(event, DaemonEvent::Status { id: event_id, status: TaskStatus::Running } if *event_id == id)
    }

    fn finished(event: &DaemonEvent, id: TaskId) -> bool {
        matches!(event, DaemonEvent::Finished { id: event_id, .. } if *event_id == id)
    }

    fn wait_for_finishes(rx: &Receiver, events: &mut Vec<DaemonEvent>, ids: &[TaskId]) {
        while !ids
            .iter()
            .all(|id| events.iter().any(|event| finished(event, *id)))
        {
            receive_until(rx, events, |event| {
                matches!(event, DaemonEvent::Finished { .. })
            });
        }
    }

    fn delay(task: &mut TaskConfig, seconds: u32) {
        task.script.source = ScriptSource::Inline(format!(
            "@echo off\r\necho started\r\nping 127.0.0.1 -n {} >nul\r\nexit /b 0\r\n",
            seconds + 1
        ));
    }

    #[test]
    fn completion_starts_fifo_head_without_scheduler_tick() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let first = executable(10100);
        let second = executable(10101);
        let third = executable(10102);
        daemon.load(vec![first.clone(), second.clone(), third.clone()]);
        daemon.execute_now(first.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |e| started(e, first.id));
        daemon.execute_now(second.id);
        daemon.execute_now(third.id);
        assert_eq!(
            daemon.states().get_status(second.id),
            Some(TaskStatus::Queued)
        );
        assert_eq!(daemon.queued_count(), 2);
        receive_until(&rx, &mut events, |e| finished(e, third.id));
        let order: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DaemonEvent::Status {
                    id,
                    status: TaskStatus::Running,
                } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(order, vec![first.id, second.id, third.id]);
        assert_eq!(daemon.queued_count(), 0);
        assert!(
            events.iter().position(|e| finished(e, first.id)).unwrap()
                < events.iter().position(|e| started(e, second.id)).unwrap()
        );
        assert!(
            events.iter().position(|e| finished(e, second.id)).unwrap()
                < events.iter().position(|e| started(e, third.id)).unwrap()
        );
    }

    #[test]
    fn stopping_queued_task_cancels_it() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let first = executable(10200);
        let second = executable(10201);
        daemon.load(vec![first.clone(), second.clone()]);
        daemon.execute_now(first.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |e| started(e, first.id));
        daemon.execute_now(second.id);
        assert_eq!(daemon.queued_count(), 1);
        assert!(daemon.stop_task(second.id));
        assert_eq!(daemon.queued_count(), 0);
        assert_eq!(
            daemon.states().get_status(second.id),
            Some(TaskStatus::Ready)
        );
        receive_until(&rx, &mut events, |e| finished(e, first.id));
        events.extend(rx.try_iter());
        assert!(!events.iter().any(|e| started(e, second.id)));
    }

    #[test]
    fn disabling_or_deleting_queued_tasks_cancels_them() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let first = executable(10300);
        let mut disabled = executable(10301);
        let deleted = executable(10302);
        daemon.load(vec![first.clone(), disabled.clone(), deleted.clone()]);
        daemon.execute_now(first.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |e| started(e, first.id));
        daemon.execute_now(disabled.id);
        daemon.execute_now(deleted.id);
        assert_eq!(daemon.queued_count(), 2);
        disabled.enabled = false;
        daemon.reload_preserving(vec![first.clone(), disabled.clone()]);
        assert_eq!(daemon.queued_count(), 0);
        assert_eq!(
            daemon.states().get_status(disabled.id),
            Some(TaskStatus::Disabled)
        );
        assert_eq!(daemon.states().get_status(deleted.id), None);
        receive_until(&rx, &mut events, |e| finished(e, first.id));
        events.extend(rx.try_iter());
        assert!(
            !events
                .iter()
                .any(|e| started(e, disabled.id) || started(e, deleted.id))
        );
    }

    #[test]
    fn repeated_task_execution_never_overlaps() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 4);
        // 即使选 Parallel,相同 TaskId 也必须保持注册表/状态机的一实例约束。
        let mut repeated = executable(10400);
        repeated.concurrency = ConcurrencyPolicy::Parallel;
        daemon.load(vec![repeated.clone()]);
        daemon.execute_now(repeated.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |e| started(e, repeated.id));
        daemon.execute_now(repeated.id);
        assert_eq!(daemon.queued_count(), 1);
        receive_until(&rx, &mut events, |e| finished(e, repeated.id));
        receive_until(&rx, &mut events, |e| finished(e, repeated.id));
        let phases: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if started(e, repeated.id) {
                    Some("开始")
                } else if finished(e, repeated.id) {
                    Some("完成")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(phases, vec!["开始", "完成", "开始", "完成"]);
        assert_eq!(daemon.queued_count(), 0);
    }

    #[test]
    fn stop_during_retry_cooldown_releases_slot_without_retrying() {
        use scheduled_task_core::model::RetryPolicy;
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let mut retrying = executable(10500);
        retrying.script.source = ScriptSource::Inline("@echo off\r\nexit /b 1\r\n".into());
        retrying.retry = Some(RetryPolicy {
            pattern: "retry".into(),
            window: 2,
            max_retries: 3,
            cooldown: Duration::from_secs(30),
        });
        let queued = executable(10501);
        daemon.load(vec![retrying.clone(), queued.clone()]);
        daemon.execute_now(retrying.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |event| {
            matches!(event,
            DaemonEvent::Status { id, status: TaskStatus::Retrying } if *id == retrying.id)
        });
        daemon.execute_now(queued.id);
        assert_eq!(daemon.queued_count(), 1);
        let stopped_at = Instant::now();
        daemon.stop_task(retrying.id);
        receive_until(&rx, &mut events, |event| started(event, queued.id));
        assert!(
            stopped_at.elapsed() < Duration::from_secs(3),
            "停止不应等待完整重试冷却"
        );
        wait_for_finishes(&rx, &mut events, &[retrying.id, queued.id]);
        assert_eq!(events.iter().filter(|e| started(e, retrying.id)).count(), 1);
        assert_eq!(
            daemon.states().get_status(retrying.id),
            Some(TaskStatus::Ready)
        );
    }

    #[test]
    fn immediate_stop_covers_worker_startup_gap() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        // 多次覆盖 execute_now 返回后、进程登记前的停止窗口。
        let tasks: Vec<_> = (10600..10608)
            .map(|id| {
                let mut task = executable(id);
                delay(&mut task, 10);
                task
            })
            .collect();
        daemon.load(tasks.clone());
        let mut events = Vec::new();
        for task in &tasks {
            let stopped_at = Instant::now();
            daemon.execute_now(task.id);
            assert!(daemon.stop_task(task.id));
            receive_until(&rx, &mut events, |event| finished(event, task.id));
            assert!(
                stopped_at.elapsed() < Duration::from_secs(3),
                "启动间隙停止不应遗漏进程"
            );
            assert!(!daemon.registry().is_running(task.id));
            assert_eq!(daemon.states().get_status(task.id), Some(TaskStatus::Ready));
        }
    }

    #[test]
    fn raising_parallel_limit_starts_waiter_immediately() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let mut first = executable(10700);
        delay(&mut first, 3);
        let second = executable(10701);
        daemon.load(vec![first.clone(), second.clone()]);
        daemon.execute_now(first.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |event| started(event, first.id));
        daemon.execute_now(second.id);
        assert_eq!(daemon.queued_count(), 1);
        daemon.set_max_parallel(2);
        receive_until(&rx, &mut events, |event| started(event, second.id));
        assert!(!events.iter().any(|event| finished(event, first.id)));
        assert_eq!(daemon.queued_count(), 0);
        wait_for_finishes(&rx, &mut events, &[first.id, second.id]);
    }

    #[test]
    fn lowering_parallel_limit_preserves_running_tasks_and_limits_new_runs() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 2);
        let first = executable(10800);
        let mut second = executable(10801);
        delay(&mut second, 2);
        let third = executable(10802);
        daemon.load(vec![first.clone(), second.clone(), third.clone()]);
        daemon.execute_now(first.id);
        daemon.execute_now(second.id);
        daemon.set_max_parallel(1);
        daemon.execute_now(third.id);
        assert_eq!(daemon.queued_count(), 1);
        let mut events = Vec::new();
        wait_for_finishes(&rx, &mut events, &[first.id, second.id, third.id]);
        let third_started = events
            .iter()
            .position(|event| started(event, third.id))
            .unwrap();
        for task in [first, second] {
            let done = events
                .iter()
                .position(|event| finished(event, task.id))
                .unwrap();
            assert!(done < third_started);
            assert!(
                matches!(events[done], DaemonEvent::Finished { returncode: 0, .. }),
                "降低并发上限不应终止在途进程"
            );
        }
    }

    #[test]
    fn disabling_running_task_finishes_as_disabled() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let mut running = executable(10900);
        daemon.load(vec![running.clone()]);
        daemon.execute_now(running.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |event| started(event, running.id));
        running.enabled = false;
        daemon.reload_preserving(vec![running.clone()]);
        assert_eq!(
            daemon.states().get_status(running.id),
            Some(TaskStatus::Running)
        );
        receive_until(&rx, &mut events, |event| finished(event, running.id));
        assert_eq!(
            daemon.states().get_status(running.id),
            Some(TaskStatus::Disabled)
        );
        assert!(events.iter().any(|event| matches!(event,
            DaemonEvent::Finished { id, returncode: 0, .. } if *id == running.id)));
    }

    #[test]
    fn shutdown_cancels_queue_and_rejects_new_triggers() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 1);
        let first = executable(11000);
        let queued = executable(11001);
        daemon.load(vec![first.clone(), queued.clone()]);
        daemon.execute_now(first.id);
        daemon.execute_now(queued.id);
        assert_eq!(daemon.queued_count(), 1);
        daemon.shutdown();
        assert!(daemon.is_shutdown());
        assert_eq!(daemon.queued_count(), 0);
        assert_eq!(
            daemon.states().get_status(queued.id),
            Some(TaskStatus::Ready)
        );
        daemon.execute_now(queued.id);
        let mut events = Vec::new();
        receive_until(&rx, &mut events, |event| finished(event, first.id));
        events.extend(rx.try_iter());
        assert!(!events.iter().any(|event| started(event, queued.id)));
        assert!(events.iter().any(|event| matches!(event,
            DaemonEvent::AdmittedRejected { id, .. } if *id == queued.id)));
    }

    #[test]
    fn same_group_never_overlaps_while_other_group_runs() {
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 3);
        let mut first = executable(11100);
        delay(&mut first, 2);
        let mut second = executable(11101);
        let mut unrelated = executable(11102);
        first.concurrency = ConcurrencyPolicy::Group("独占组".into());
        second.concurrency = first.concurrency.clone();
        unrelated.concurrency = ConcurrencyPolicy::Group("其他组".into());
        daemon.load(vec![first.clone(), second.clone(), unrelated.clone()]);
        daemon.execute_now(first.id);
        daemon.execute_now(second.id);
        daemon.execute_now(unrelated.id);
        let mut events = Vec::new();
        wait_for_finishes(&rx, &mut events, &[first.id, second.id, unrelated.id]);
        let first_done = events
            .iter()
            .position(|event| finished(event, first.id))
            .unwrap();
        assert!(
            events
                .iter()
                .position(|event| started(event, second.id))
                .unwrap()
                > first_done
        );
        assert!(
            events
                .iter()
                .position(|event| started(event, unrelated.id))
                .unwrap()
                < first_done
        );
    }

    #[test]
    fn simultaneous_triggers_respect_limit_and_preserve_each_inline_script() {
        use std::sync::{Arc, Barrier};
        let (tx, rx) = spawn_channel();
        let daemon = Daemon::with_max_parallel(tx, 2);
        let tasks: Vec<_> = (11200..11208)
            .map(|id| {
                let mut task = executable(id);
                task.script.source = ScriptSource::Inline(format!(
                    "@echo off\r\necho token-{id}\r\nping 127.0.0.1 -n 2 >nul\r\nexit /b 0\r\n"
                ));
                task
            })
            .collect();
        daemon.load(tasks.clone());
        let barrier = Arc::new(Barrier::new(tasks.len() + 1));
        let handles: Vec<_> = tasks
            .iter()
            .map(|task| {
                let daemon = daemon.clone();
                let barrier = barrier.clone();
                let id = task.id;
                std::thread::spawn(move || {
                    barrier.wait();
                    daemon.execute_now(id);
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let mut events = Vec::new();
        wait_for_finishes(
            &rx,
            &mut events,
            &tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
        );
        let mut running = 0;
        let mut peak = 0;
        for event in &events {
            match event {
                DaemonEvent::Status {
                    status: TaskStatus::Running,
                    ..
                } => {
                    running += 1;
                    peak = peak.max(running);
                    assert!(running <= 2, "并发触发不应超出两执行槽");
                }
                DaemonEvent::Finished { returncode, .. } => {
                    assert_eq!(*returncode, 0);
                    assert!(running > 0);
                    running -= 1;
                }
                DaemonEvent::LogLine { id, line } => {
                    assert_eq!(
                        line,
                        &format!("token-{}", id.0),
                        "内联脚本内容不得被其他任务覆盖"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(peak, 2);
        assert_eq!(running, 0);
        for task in tasks {
            assert!(events.iter().any(|event| matches!(event,
                DaemonEvent::LogLine { id, line } if *id == task.id && line == &format!("token-{}", task.id.0))));
        }
    }
}
