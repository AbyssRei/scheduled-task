//! 全局日志。
//!
//! tracing + tracing-appender 滚动日志,控制台与文件默认 INFO。
//! 文件日志等级支持运行时调整,控制台仍由 RUST_LOG 独立控制。
//! 文件按天滚动,最多保留 5 个文件;单个文件不设大小上限。

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();
static FILE_FILTER: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

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
        EnvFilter::new(match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        })
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

/// 初始化全局日志。返回前进程内全局生效;WorkerGuard 常驻
/// (非阻塞 appender 的刷写依赖其存活)。
///
/// - 控制台:INFO(带彩色,供 CLI/开发用)
/// - 文件:`{log_dir}/scheduled_task.YYYY-MM-DD.log`,默认 INFO,按天滚动并保留 5 个文件
pub fn init_logging(log_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(log_dir)?;

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("scheduled_task")
        .filename_suffix("log")
        .max_log_files(5)
        .build(log_dir)
        .map_err(std::io::Error::other)?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);

    let (file_filter, handle) = reload::Layer::new(LogLevel::default().filter());
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_filter(file_filter);

    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(console_layer)
        .init();
    let _ = FILE_FILTER.set(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

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
