//! 真实进程反复启动、显式刷写及立即退出的文件日志回归测试。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use scheduled_task_core::logger::{
    LogLevel, LogRetention, LogTimeZone, format_log_timestamp, init_logging,
    init_logging_with_settings, prepare_log_settings, set_log_timezone, shutdown_logging,
};

const LOG_DIR_ENV: &str = "SCHEDULED_TASK_LOG_RESTART_TEST_DIR";
const BATCH_ENV: &str = "SCHEDULED_TASK_LOG_RESTART_TEST_BATCH";
const SCENARIO_ENV: &str = "SCHEDULED_TASK_LOG_SETTINGS_TEST_SCENARIO";
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

fn run_batch(log_dir: &Path, batch: usize, scenario: &str) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "log_writer_child", "--nocapture"])
        .env(LOG_DIR_ENV, log_dir)
        .env(BATCH_ENV, batch.to_string())
        .env(SCENARIO_ENV, scenario)
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
        run_batch(&log_dir.0, batch, "restart");
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
fn queued_records_keep_their_timezone_and_matching_daily_file() {
    let log_dir = TempLogDir::new();
    run_batch(&log_dir.0, 0, "timezone");
    let mut counts = [0; 2];
    let mut dates = std::collections::HashSet::new();
    for entry in std::fs::read_dir(&log_dir.0).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let Some(date) = name
            .strip_prefix("scheduled_task.")
            .and_then(|s| s.strip_suffix(".log"))
        else {
            continue;
        };
        let output = std::fs::read_to_string(entry.path()).unwrap();
        for line in output
            .lines()
            .filter(|line| line.contains("timezone-probe"))
        {
            assert_eq!(
                &line[..10],
                date,
                "后台队列不得把记录写入其他日期文件: {line}"
            );
            let prefix = line.split_whitespace().next().unwrap();
            let _: jiff::Timestamp = prefix.parse().unwrap();
            let index = if line.contains("phase=east") { 0 } else { 1 };
            assert!(prefix.ends_with(if index == 0 { "+14:00" } else { "-12:00" }));
            counts[index] += 1;
            dates.insert(date.to_owned());
        }
    }
    assert_eq!(counts, [RECORDS_PER_BATCH; 2]);
    // 两个时区相差 26 小时,即使恰逢午夜也必定使用不同日期。
    assert!(dates.len() >= 2);
}

#[test]
fn prepared_settings_only_change_logging_when_committed() {
    let log_dir = TempLogDir::new();
    run_batch(&log_dir.0, 0, "settings");
    let output = read_daily_logs(&log_dir.0);
    assert!(output.contains("settings-before-commit"));
    assert!(!output.contains("settings-filtered-after-commit"));
    let line = output
        .lines()
        .find(|line| line.contains("settings-after-commit"))
        .unwrap();
    assert!(line.split_whitespace().next().unwrap().ends_with("+08:00"));
}

fn check_prepared_settings(log_dir: &Path) {
    init_logging_with_settings(
        log_dir,
        LogRetention {
            retention_days: 365,
            ..LogRetention::default()
        },
        LogTimeZone::Custom("UTC".into()),
    )
    .unwrap();
    let old_date = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .checked_sub(jiff::Span::new().days(10))
        .unwrap();
    let old_file = log_dir.join(format!("scheduled_task.{old_date}.log"));
    std::fs::write(&old_file, "历史日志").unwrap();
    let instant: jiff::Timestamp = "2026-10-19T00:00:00Z".parse().unwrap();
    let original_timestamp = format_log_timestamp(instant);
    let retention = LogRetention {
        retention_days: 3,
        ..LogRetention::default()
    };
    let prepared = prepare_log_settings(
        Some(LogLevel::Warn),
        Some(LogTimeZone::Custom("+08:00".into())),
        Some(retention),
    )
    .unwrap();
    assert_eq!(format_log_timestamp(instant), original_timestamp);
    assert!(old_file.exists(), "准备设置时不得清理历史日志");
    drop(prepared);
    assert!(
        prepare_log_settings(
            Some(LogLevel::Error),
            Some(LogTimeZone::Custom("invalid/timezone".into())),
            Some(retention),
        )
        .is_err()
    );
    assert_eq!(format_log_timestamp(instant), original_timestamp);
    assert!(old_file.exists(), "无效设置不得清理历史日志");
    tracing::info!("settings-before-commit");
    prepare_log_settings(
        Some(LogLevel::Warn),
        Some(LogTimeZone::Custom("+08:00".into())),
        Some(retention),
    )
    .unwrap()
    .apply()
    .unwrap();
    assert_eq!(
        format_log_timestamp(instant),
        "2026-10-19T08:00:00.000+08:00"
    );
    assert!(!old_file.exists(), "提交成功后清理过期历史日志");
    tracing::info!("settings-filtered-after-commit");
    tracing::warn!("settings-after-commit");
}

#[test]
#[ignore = "由日志重启测试作为独立子进程调用"]
fn log_writer_child() {
    let Some(log_dir) = std::env::var_os(LOG_DIR_ENV) else {
        return;
    };
    let batch: usize = std::env::var(BATCH_ENV).unwrap().parse().unwrap();
    let scenario = std::env::var(SCENARIO_ENV).unwrap();
    if scenario == "settings" {
        check_prepared_settings(Path::new(&log_dir));
    } else if scenario == "timezone" {
        init_logging_with_settings(
            Path::new(&log_dir),
            LogRetention::default(),
            LogTimeZone::Custom("+14:00".into()),
        )
        .unwrap();
        let instant: jiff::Timestamp = "2026-10-19T00:00:00Z".parse().unwrap();
        assert!(
            init_logging_with_settings(
                Path::new(&log_dir),
                LogRetention::default(),
                LogTimeZone::Custom("UTC".into()),
            )
            .is_err()
        );
        assert_eq!(
            format_log_timestamp(instant),
            "2026-10-19T14:00:00.000+14:00"
        );
        for line in 0..RECORDS_PER_BATCH {
            tracing::info!("timezone-probe phase=east line={line:04}");
        }
        set_log_timezone(LogTimeZone::Custom("-12:00".into())).unwrap();
        assert_eq!(
            format_log_timestamp(instant),
            "2026-10-18T12:00:00.000-12:00"
        );
        assert!(set_log_timezone(LogTimeZone::Custom("invalid/timezone".into())).is_err());
        assert_eq!(
            format_log_timestamp(instant),
            "2026-10-18T12:00:00.000-12:00"
        );
        for line in 0..RECORDS_PER_BATCH {
            tracing::info!("timezone-probe phase=west line={line:04}");
        }
    } else {
        init_logging(Path::new(&log_dir)).unwrap();
        for line in 0..RECORDS_PER_BATCH {
            tracing::info!("restart-probe batch={batch} line={line:04}");
        }
        tracing::info!("restart-tail batch={batch}");
    }
    shutdown_logging();
    shutdown_logging();
    // 模拟桌面框架完成退出回调后立即结束进程,不能依赖栈析构或额外等待。
    std::process::exit(0);
}
