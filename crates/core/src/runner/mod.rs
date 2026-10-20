//! 脚本执行器。
//!
//! 支持多种脚本解释器,Windows 优先。统一处理 stdout/stderr 逐行实时回调、
//! 超时终止与自动重试窗口。
//!
//! 进程树管理:Windows 用 Job Object(见 [`job`]),子进程 CREATE_SUSPENDED
//! 挂起 → 入 Job → 恢复,防孙进程在入 Job 前逃逸;停止时终止整个进程树,
//! 管理器崩溃时由操作系统终止作业内全部进程。
//!
//! 行结构保证:stdout/stderr 各由独立线程按行解码后送入同一通道——
//! 行内不串流,两路行间顺序不保证。

mod command;
mod execution;
mod output;
mod registry;

#[cfg(windows)]
pub mod job;

pub use execution::run;
pub use registry::{ProcessRegistry, RunningProcess, global_registry};

use std::time::Duration;

use thiserror::Error;

use crate::model::TaskId;

/// 输出字符集选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    /// 系统 ANSI 代码页(中文 Windows 为 GBK)
    #[default]
    Default,
    Utf8,
    Gbk,
}

/// 逐行实时回调类型。
pub type LineCallback = Box<dyn Fn(&str) + Send>;
/// 自动重试判定回调类型(当前行,不含当前行的历史窗口)。
pub type RetryCheckFn = Box<dyn Fn(&str, &[String]) -> bool + Send>;

/// 运行期选项。
pub struct RunOptions {
    /// 默认只累计最近 256 KiB；逐行回调始终接收全部输出。
    pub capture: OutputCapture,
    /// 托管执行的取消令牌,覆盖创建进程之前及注册间隙。
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 注册键(TaskId),None 表示不登记(不可被外部 stop)
    pub id: Option<TaskId>,
    /// 执行超时;None 表示不限
    pub timeout: Option<Duration>,
    pub charset: Charset,
    /// 逐行实时回调(已 strip 的行)
    pub on_line: Option<LineCallback>,
    /// 自动重试判定:window = 连续次数(含当前行)。check 收到
    /// (当前行, 不含当前行的历史窗口);历史截断到 window 条由本模块负责。
    pub retry: Option<RetryCheck>,
}

pub struct RetryCheck {
    pub window: u32,
    pub check: RetryCheckFn,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            capture: OutputCapture::default(),
            cancel: None,
            id: None,
            timeout: None,
            charset: Charset::Default,
            on_line: None,
            retry: None,
        }
    }
}

/// 结果捕获策略；完整捕获只用于明确接受内存成本的独立调用。
#[derive(Debug, Clone, Copy)]
pub enum OutputCapture {
    Tail(usize),
    Full,
}

impl Default for OutputCapture {
    fn default() -> Self {
        Self::Tail(256 * 1024)
    }
}

/// 执行结果:退出码、捕获输出及截断与重试标记。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub returncode: i32,
    pub output: String,
    pub output_truncated: bool,
    pub retry_triggered: bool,
}

/// runner 错误(Display 中文)。
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("脚本解析命令失败: {0}")]
    Resolve(String),
    #[error("创建进程失败: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("内联脚本写临时文件失败: {0}")]
    TempFile(String),
    #[error("Job Object 初始化失败: {0}")]
    Job(String),
}
