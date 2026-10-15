//! 全局日志。
//!
//! tracing + tracing-appender 滚动日志,控制台 INFO / 文件 DEBUG。
//! 文件按天滚动,最多保留 5 个文件;单个文件不设大小上限。

use std::path::Path;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// 初始化全局日志。返回前进程内全局生效;WorkerGuard 常驻
/// (非阻塞 appender 的刷写依赖其存活)。
///
/// - 控制台:INFO(带彩色,供 CLI/开发用)
/// - 文件:`{log_dir}/scheduled_task.YYYY-MM-DD.log`,DEBUG,按天滚动并保留 5 个文件
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

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_filter(EnvFilter::try_new("debug").unwrap_or_default());

    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(console_layer)
        .init();
    Ok(())
}
