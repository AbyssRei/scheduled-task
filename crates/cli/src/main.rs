//! scheduled-task-cli:无头验证入口。
//!
//! 用法:
//! - `scheduled-task-cli run <config.ron>`:调度模式,1s tick 触发,Ctrl+C 退出
//! - `scheduled-task-cli run <config.ron> --task <id|名称>`:立即执行单个任务,
//!   执行完成后退出,不进入调度。

use std::time::Duration;

use scheduled_task_core::daemon::{Daemon, DaemonEvent, spawn_channel};
use scheduled_task_core::logger::init_logging;
use scheduled_task_core::store::TaskStore;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") if args.len() >= 2 => run(&args[1], args.get(3).map(String::as_str)),
        _ => {
            eprintln!("用法: scheduled-task-cli run <config.ron> [--task <id|名称>]");
            std::process::exit(2);
        }
    }
}

fn run(config_path: &str, only_task: Option<&str>) {
    if let Err(e) = init_logging(std::path::Path::new("logs")) {
        eprintln!("初始化日志失败: {e}");
        std::process::exit(1);
    }
    let store = TaskStore::new(config_path.into());
    let config = match store.load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("加载配置失败: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "已加载配置 {}(config_version {},{} 个任务)",
        config_path,
        config.config_version,
        config.tasks.len()
    );

    let (tx, rx) = spawn_channel();
    let daemon = Daemon::new(tx);
    daemon.set_max_parallel(config.max_parallel as usize);
    daemon.load(config.tasks);

    if let Some(key) = only_task {
        // 立即执行模式:按 id 或名称定位任务
        let tasks = daemon.tasks_snapshot();
        let found = tasks
            .iter()
            .find(|t| t.name == key || t.id.0.to_string() == key);
        match found {
            Some(t) => {
                daemon.execute_now(t.id);
                // 阻塞等待该任务执行结束
                while let Ok(event) = rx.recv() {
                    let done = matches!(&event, DaemonEvent::Finished { id, .. } | DaemonEvent::AdmittedRejected { id, .. } if *id == t.id);
                    print_event(event);
                    if done {
                        break;
                    }
                }
            }
            None => {
                eprintln!("未找到任务: {key}");
                std::process::exit(1);
            }
        }
        return;
    }

    // 调度模式
    println!("进入调度模式(1s tick),Ctrl+C 退出");
    loop {
        daemon.tick();
        drain_events(&rx);
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 每轮处理有界事件批次,持续输出不能阻塞下一次调度 tick。
fn drain_events(rx: &std::sync::mpsc::Receiver<DaemonEvent>) {
    for event in rx.try_iter().take(1000) {
        print_event(event);
    }
}

fn print_event(event: DaemonEvent) {
    match event {
        DaemonEvent::Status { id, status } => {
            println!("[事件] {id} 状态 -> {}", status.label());
        }
        DaemonEvent::LogLine { id, line } => {
            println!("[{id}] {line}");
        }
        DaemonEvent::Finished {
            id,
            returncode,
            retry_triggered,
        } => {
            println!("[事件] {id} 执行结束(返回码 {returncode},重试触发 {retry_triggered})");
        }
        DaemonEvent::AdmittedRejected { id, reason } => {
            println!("[事件] {id} 触发被拒: {reason}");
        }
        DaemonEvent::Queued { id } => {
            println!("[事件] {id} 已进入并发等待队列");
        }
        DaemonEvent::ConfigChanged => {
            println!("[事件] 任务配置已变更");
        }
    }
}
