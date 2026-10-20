//! 全局日志。
//!
//! tracing + 后台写入与统一日志时区的日滚动日志,控制台与文件默认 INFO。
//! 文件日志等级支持运行时调整,控制台仍由 RUST_LOG 独立控制。
//! 文件按天滚动,默认保留 7 天,可额外限制日志总大小。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

mod daily;
mod timezone;

pub use timezone::{LogTimeZone, format_log_timestamp, set_log_timezone};

use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

static LOG_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);
static FILE_FILTER: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();
static FILE_WRITER: Mutex<Option<daily::SharedLogWriter>> = Mutex::new(None);
static INITIALIZATION: Mutex<()> = Mutex::new(());

/// 文件日志的保留策略,大小单位为 1024 × 1024 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogRetention {
    pub retention_days: u16,
    pub size_limit_enabled: bool,
    pub max_size_mb: u32,
}

impl Default for LogRetention {
    fn default() -> Self {
        Self {
            retention_days: 7,
            size_limit_enabled: false,
            max_size_mb: 1024,
        }
    }
}

impl LogRetention {
    pub fn validate(&self) -> Result<(), String> {
        if !(3..=365).contains(&self.retention_days) {
            return Err("日志存储保存天数必须在 3 到 365 天之间".to_owned());
        }
        if !(200..=5120).contains(&self.max_size_mb) {
            return Err("日志存储大小限制必须在 200 到 5120 MB 之间".to_owned());
        }
        Ok(())
    }
}

/// 即时应用日志保留策略并清理超过限制的历史文件。
pub fn set_log_retention(retention: LogRetention) -> Result<(), String> {
    retention.validate()?;
    FILE_WRITER
        .lock()
        .unwrap()
        .as_ref()
        .ok_or_else(|| "日志系统尚未初始化".to_owned())?
        .set_retention(retention);
    Ok(())
}

/// 文件日志等级,按严重程度保留当前等级及更高等级的消息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn filter(self) -> EnvFilter {
        let filter = match self {
            Self::Error => "error".to_owned(),
            Self::Warn => "warn,gpui_windows::events=warn".to_owned(),
            Self::Info => "info,gpui_windows::events=warn".to_owned(),
            Self::Debug => "debug,gpui_windows::events=warn".to_owned(),
            Self::Trace => "trace,gpui_windows::events=warn".to_owned(),
        };
        // GPUI 在 Windows 收到系统主题/设备通知时会记录 INFO,这些通知本身不表示任务或应用异常。
        EnvFilter::new(filter)
    }
}

/// 即时调整文件日志等级,不影响控制台的 RUST_LOG 过滤器。
pub fn set_log_level(level: LogLevel) -> Result<(), String> {
    FILE_FILTER
        .get()
        .ok_or_else(|| "日志系统尚未初始化".to_owned())?
        .reload(level.filter())
        .map_err(|error| format!("更新文件日志等级失败: {error}"))
}

/// 已验证的日志设置；配置成功落盘后才提交，历史日志清理始终最后执行。
pub struct PreparedLogSettings {
    filter: Option<(reload::Handle<EnvFilter, Registry>, EnvFilter)>,
    writer: Option<daily::SharedLogWriter>,
    timezone: Option<(LogTimeZone, jiff::tz::TimeZone)>,
    retention: Option<LogRetention>,
}

/// 只准备发生变化的设置，准备过程不改变运行状态或删除日志。
pub fn prepare_log_settings(
    level: Option<LogLevel>,
    timezone: Option<LogTimeZone>,
    retention: Option<LogRetention>,
) -> Result<PreparedLogSettings, String> {
    let filter = level
        .map(|level| {
            FILE_FILTER
                .get()
                .cloned()
                .map(|handle| (handle, level.filter()))
                .ok_or_else(|| "日志系统尚未初始化".to_owned())
        })
        .transpose()?;
    let timezone = timezone
        .map(|setting| {
            let zone = setting.resolve()?;
            Ok::<_, String>((setting, zone))
        })
        .transpose()?;
    if let Some(retention) = retention {
        retention.validate()?;
    }
    let writer = if timezone.is_some() || retention.is_some() {
        Some(
            FILE_WRITER
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| "日志系统尚未初始化".to_owned())?,
        )
    } else {
        None
    };
    Ok(PreparedLogSettings {
        filter,
        writer,
        timezone,
        retention,
    })
}

impl PreparedLogSettings {
    /// 等级提交失败时尚未修改任何设置；其余步骤已准备完成且不会返回失败。
    pub fn apply(self) -> Result<(), String> {
        if let Some((handle, filter)) = self.filter {
            handle
                .reload(filter)
                .map_err(|error| format!("更新文件日志等级失败: {error}"))?;
        }
        if let Some(writer) = self.writer {
            writer.apply_settings(self.timezone, self.retention);
        }
        Ok(())
    }
}

/// 正常退出时停止文件写入并刷新已经排队的日志;重复调用无副作用。
/// 强制结束进程或断电无法执行此清理。
pub fn shutdown_logging() {
    let guard = LOG_GUARD.lock().unwrap().take();
    if guard.is_some() {
        if let Some(filter) = FILE_FILTER.get() {
            let _ = filter.reload(EnvFilter::new("off"));
        }
        drop(guard);
        FILE_WRITER.lock().unwrap().take();
    }
}

/// 初始化全局日志。返回前进程内全局生效,退出时调用 shutdown_logging
/// 释放 WorkerGuard,通知后台刷写并有限等待。
///
/// - 控制台:INFO(带彩色,供 CLI/开发用)
/// - 文件:`{log_dir}/scheduled_task.YYYY-MM-DD.log`,默认 INFO,按天滚动并保留 7 天
pub fn init_logging(log_dir: &Path) -> std::io::Result<()> {
    init_logging_with_retention(log_dir, LogRetention::default())
}

/// 使用持久化的保留策略初始化全局日志,首次清理前即应用该策略。
pub fn init_logging_with_retention(log_dir: &Path, retention: LogRetention) -> std::io::Result<()> {
    init_logging_with_settings(log_dir, retention, LogTimeZone::default())
}

/// 首次写入和清理前应用持久化时区及保留策略，初始化失败不会切换已生效时区。
pub fn init_logging_with_settings(
    log_dir: &Path,
    retention: LogRetention,
    timezone: LogTimeZone,
) -> std::io::Result<()> {
    let _initialization = INITIALIZATION.lock().unwrap();
    retention
        .validate()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let timezone = timezone::SharedTimeZone::new(timezone)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if FILE_FILTER.get().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "日志系统已经初始化",
        ));
    }
    std::fs::create_dir_all(log_dir)?;

    let file_appender = daily::SharedLogWriter::new(log_dir, retention, timezone.clone())?;
    // 文件队列有界并施加背压，界面丢弃展示日志时仍保留文件记录。
    let (file_writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(1024)
        .lossy(false)
        .finish(file_appender.clone());

    let (file_filter, handle) = reload::Layer::new(LogLevel::default().filter());
    let file_layer = tracing_subscriber::fmt::layer()
        .with_timer(timezone.clone())
        .with_writer(file_writer)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_filter(file_filter);

    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive(
            "gpui_windows::events=warn"
                .parse()
                .expect("有效的 GPUI 日志过滤规则"),
        );
    let console_layer = tracing_subscriber::fmt::layer()
        .with_timer(timezone.clone())
        .with_writer(std::io::stdout)
        .with_filter(console_filter);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .map_err(std::io::Error::other)?;
    timezone::publish(timezone);
    // 只有全局订阅器安装成功后才清理，重复或失败初始化不应删除历史日志。
    file_appender.prune();
    *LOG_GUARD.lock().unwrap() = Some(guard);
    let _ = FILE_FILTER.set(handle);
    *FILE_WRITER.lock().unwrap() = Some(file_appender);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn 存储限制默认值兼容旧配置() {
        assert_eq!(
            ron::from_str::<LogRetention>("()").unwrap(),
            LogRetention::default()
        );
        let partial: LogRetention = ron::from_str("(retention_days:30)").unwrap();
        assert_eq!(partial.retention_days, 30);
        assert!(!partial.size_limit_enabled);
        assert_eq!(partial.max_size_mb, 1024);
        assert_eq!(LogRetention::default().retention_days, 7);
    }

    #[test]
    fn 存储限制严格校验含边界() {
        for days in [3, 365] {
            for size in [200, 5120] {
                assert!(
                    LogRetention {
                        retention_days: days,
                        max_size_mb: size,
                        ..LogRetention::default()
                    }
                    .validate()
                    .is_ok()
                );
            }
        }
        for days in [0, 2, 366, u16::MAX] {
            assert!(
                LogRetention {
                    retention_days: days,
                    ..LogRetention::default()
                }
                .validate()
                .is_err()
            );
        }
        for size in [0, 199, 5121, u32::MAX] {
            assert!(
                LogRetention {
                    max_size_mb: size,
                    ..LogRetention::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[derive(Clone, Default)]
    struct MemoryWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for MemoryWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MemoryWriter {
        fn take_output(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    fn emit_messages() {
        tracing::trace!(target: "gpui::window", "trace-probe");
        tracing::debug!(target: "gpui::window", "Sending a11y tree update: 1 nodes");
        tracing::debug!(target: "scheduled_task_core::runner", "task-debug-probe");
        tracing::info!(target: "scheduled_task_core", "info-probe");
        tracing::warn!(target: "gpui::window", "warn-probe");
        tracing::error!(target: "gpui::window", "error-probe");
        tracing::info!(target: "gpui_windows::events", "system-settings-probe");
        tracing::warn!(target: "gpui_windows::events", "system-settings-warning-probe");
    }

    #[test]
    fn 文件等级即时切换且不影响控制台() {
        let file = MemoryWriter::default();
        let console = MemoryWriter::default();
        let file_writer = file.clone();
        let console_writer = console.clone();
        let (filter, handle) = reload::Layer::new(LogLevel::default().filter());
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || file_writer.clone())
                    .with_filter(filter),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || console_writer.clone())
                    .with_filter(EnvFilter::new("info")),
            );

        tracing::subscriber::with_default(subscriber, || {
            // 复用同一批事件调用点,验证切换后缓存不会保留之前的等级。
            let cases = [
                (LogLevel::Info, false, false, true, true),
                (LogLevel::Debug, false, true, true, true),
                (LogLevel::Info, false, false, true, true),
                (LogLevel::Trace, true, true, true, true),
                (LogLevel::Warn, false, false, false, true),
                (LogLevel::Error, false, false, false, false),
            ];
            for (level, trace, debug, info, warn) in cases {
                handle.reload(level.filter()).unwrap();
                emit_messages();
                let output = file.take_output();
                assert_eq!(output.contains("trace-probe"), trace, "{level:?}");
                assert_eq!(
                    output.contains("Sending a11y tree update"),
                    debug,
                    "{level:?}"
                );
                assert_eq!(output.contains("task-debug-probe"), debug, "{level:?}");
                assert_eq!(output.contains("info-probe"), info, "{level:?}");
                assert_eq!(output.contains("warn-probe"), warn, "{level:?}");
                assert!(output.contains("error-probe"), "{level:?}");
                assert!(!output.contains("system-settings-probe"), "{level:?}");
                assert_eq!(
                    output.contains("system-settings-warning-probe"),
                    !matches!(level, LogLevel::Error),
                    "{level:?}"
                );

                let output = console.take_output();
                assert!(!output.contains("trace-probe"), "{level:?}");
                assert!(!output.contains("Sending a11y tree update"), "{level:?}");
                assert!(!output.contains("task-debug-probe"), "{level:?}");
                for message in ["info-probe", "warn-probe", "error-probe"] {
                    assert!(output.contains(message), "{level:?}: {message}");
                }
            }
        });
    }
}
