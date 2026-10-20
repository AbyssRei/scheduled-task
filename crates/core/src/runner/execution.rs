//! 执行生命周期：启动、输出回调、重试判定、超时停止和收尾。

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::ScriptSpec;

use super::command::resolve;
#[cfg(windows)]
use super::job;
use super::output;
use super::{RunOptions, RunOutcome, RunnerError, RunningProcess, global_registry};

/// 执行脚本并实时回报输出(阻塞直到进程结束/重试触发/超时)。
pub fn run(spec: &ScriptSpec, opts: RunOptions) -> Result<RunOutcome, RunnerError> {
    if opts
        .retry
        .as_ref()
        .is_some_and(|retry| retry.window == 0 || retry.window > 100)
    {
        return Err(RunnerError::Resolve("连续次数 应在 1–100 之间".into()));
    }
    let resolved = resolve(spec)?;
    if opts
        .cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Ok(RunOutcome {
            returncode: -1,
            output: String::new(),
            retry_triggered: false,
        });
    }

    let mut command = Command::new(&resolved.program);
    command
        .args(&resolved.args)
        .envs(&resolved.environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if resolved.feed_newline_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(dir) = &resolved.working_dir {
        command.current_dir(dir);
    }

    // Windows:CREATE_SUSPENDED → 入 Job → 恢复(树状终止 + 崩溃兜底)
    #[cfg(windows)]
    let (mut child, job_guard) =
        job::JobGuard::spawn_into_job(&mut command).map_err(|e| RunnerError::Job(e.to_string()))?;
    #[cfg(not(windows))]
    let mut child = command.spawn()?;

    // pause 应答:喂一个空行后关闭 stdin
    if resolved.feed_newline_stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        use std::io::Write as _;
        let _ = stdin.write_all(b"\r\n");
        drop(stdin);
    }

    let running = Arc::new(RunningProcess {
        child: Mutex::new(child),
        #[cfg(windows)]
        job: Mutex::new(Some(job_guard)),
        stopped_manually: AtomicBool::new(false),
    });

    if let Some(id) = opts.id {
        global_registry().register(id, running.clone());
    }

    let (line_rx, readers) = output::capture(&running, opts.charset);

    let outcome = drive_until_done(&running, &opts, line_rx);

    for reader in readers {
        let _ = reader.join();
    }

    if let Some(id) = opts.id {
        global_registry().unregister(id);
    }
    Ok(outcome)
}

/// 主循环:逐行回调、重试窗口、超时终止(每 0.1s 轮询)。
fn drive_until_done(
    running: &Arc<RunningProcess>,
    opts: &RunOptions,
    line_rx: Receiver<String>,
) -> RunOutcome {
    let mut output_lines: Vec<String> = Vec::new();
    let mut output_history: Vec<String> = Vec::new();
    let mut retry_triggered = false;
    let start = Instant::now();

    loop {
        if opts
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            running.stop();
            break;
        }
        // 超时检查
        if let Some(timeout) = opts.timeout
            && start.elapsed() > timeout
        {
            tracing::error!("任务执行超时,强制终止");
            running.stop();
            output_lines.push("[ERROR] 任务执行超时,已自动停止".into());
            break;
        }

        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let line = line.trim().to_string();
                handle_line(
                    &line,
                    running,
                    opts,
                    &mut output_lines,
                    &mut output_history,
                    &mut retry_triggered,
                );
                if retry_triggered {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // 两个读取线程均已结束(管道关闭):等进程真正退出
                let wait_timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
                if wait_child(running, wait_timeout, opts).is_none() {
                    tracing::error!("任务执行超时,强制终止");
                    running.stop();
                    output_lines.push("[ERROR] 任务执行超时,已自动停止".into());
                }
                break;
            }
        }
    }

    let returncode = returncode_of(running);
    let output = output_lines.join("\n");
    if returncode != 0 && !retry_triggered {
        tracing::error!("任务执行失败,返回码: {returncode}, 输出: {output}");
    }
    RunOutcome {
        returncode,
        output,
        retry_triggered,
    }
}

/// 单行处理:回调、历史、重试判定与终止。
fn handle_line(
    line: &str,
    running: &Arc<RunningProcess>,
    opts: &RunOptions,
    output_lines: &mut Vec<String>,
    output_history: &mut Vec<String>,
    retry_triggered: &mut bool,
) {
    // 空行也进入完整输出、逐行回调与重试窗口。
    output_lines.push(line.to_string());
    if let Some(cb) = &opts.on_line {
        cb(line);
    }
    if let Some(retry) = &opts.retry {
        output_history.push(line.to_string());
        if output_history.len() > retry.window as usize {
            output_history.remove(0);
        }
        // 判定窗口不含当前行(避免当前行出现两次,阈值提前一格)
        let history = &output_history[..output_history.len() - 1];
        if (retry.check)(line, history) {
            tracing::warn!("[自动重试] 检测到需要重试的条件,准备停止进程");
            running.stop();
            output_lines.push("[ERROR] 检测到重试条件,已停止进程准备重试".into());
            *retry_triggered = true;
        }
    }
}

fn wait_child(
    running: &Arc<RunningProcess>,
    timeout: Duration,
    opts: &RunOptions,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if opts
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            running.stop();
            return running.child.lock().unwrap().try_wait().ok().flatten();
        }
        match running.child.lock().unwrap().try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn returncode_of(running: &Arc<RunningProcess>) -> i32 {
    match running.child.lock().unwrap().try_wait() {
        Ok(Some(status)) => status.code().unwrap_or(-1),
        _ => -1,
    }
}
