//! 真实进程反复启动、显式刷写及立即退出的文件日志回归测试。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use scheduled_task_core::logger::{init_logging, shutdown_logging};

const LOG_DIR_ENV: &str = "SCHEDULED_TASK_LOG_RESTART_TEST_DIR";
const BATCH_ENV: &str = "SCHEDULED_TASK_LOG_RESTART_TEST_BATCH";
const RECORDS_PER_BATCH: usize = 1000;

struct TempLogDir(PathBuf);

impl TempLogDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "scheduled-task-log-restart-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempLogDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// 超时或断言失败也必须回收子进程,随后才能删除临时日志目录。
struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_batch(log_dir: &Path, batch: usize) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "log_writer_child", "--nocapture"])
        .env(LOG_DIR_ENV, log_dir)
        .env(BATCH_ENV, batch.to_string())
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW,测试不弹出终端。
    }
    let mut child = TestChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "第 {batch} 批日志写入失败: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "第 {batch} 批日志写入超时");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_daily_logs(log_dir: &Path) -> String {
    let mut output = String::new();
    for entry in std::fs::read_dir(log_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        let Some(date) = name
            .strip_prefix("scheduled_task.")
            .and_then(|name| name.strip_suffix(".log"))
        else {
            continue;
        };
        if date.parse::<jiff::civil::Date>().is_ok() {
            output.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    output
}

#[test]
fn repeated_process_start_keeps_previous_logs_and_flushes_tail() {
    let log_dir = TempLogDir::new();
    for batch in 0..3 {
        run_batch(&log_dir.0, batch);
        // 合并规范日期文件,允许测试恰好跨过轮转时间,不依赖固定日期。
        let output = read_daily_logs(&log_dir.0);
        for previous_batch in 0..=batch {
            for line in 0..RECORDS_PER_BATCH {
                let marker = format!("restart-probe batch={previous_batch} line={line:04}");
                assert_eq!(
                    output
                        .lines()
                        .filter(|record| record.ends_with(&marker))
                        .count(),
                    1,
                    "第 {batch} 次重启后日志缺失或重复: {marker}"
                );
            }
            let marker = format!("restart-tail batch={previous_batch}");
            assert_eq!(
                output
                    .lines()
                    .filter(|record| record.ends_with(&marker))
                    .count(),
                1,
                "第 {batch} 次重启后日志尾部缺失或重复: {marker}"
            );
        }
    }
}

#[test]
#[ignore = "由日志重启测试作为独立子进程调用"]
fn log_writer_child() {
    let Some(log_dir) = std::env::var_os(LOG_DIR_ENV) else {
        return;
    };
    let batch: usize = std::env::var(BATCH_ENV).unwrap().parse().unwrap();
    init_logging(Path::new(&log_dir)).unwrap();
    for line in 0..RECORDS_PER_BATCH {
        tracing::info!("restart-probe batch={batch} line={line:04}");
    }
    tracing::info!("restart-tail batch={batch}");
    shutdown_logging();
    shutdown_logging();
    // 模拟桌面框架完成退出回调后立即结束进程,不能依赖栈析构或额外等待。
    std::process::exit(0);
}
