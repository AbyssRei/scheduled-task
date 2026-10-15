//! scheduled-task-core:定时任务领域库(调度、状态机、重试判定、脚本执行、配置持久化)。
//!
//! 提供任务模型、调度、状态管理、脚本执行、重试判定与配置持久化。

pub mod daemon;
pub mod logger;
pub mod model;
pub mod retry;
pub mod runner;
pub mod scheduler;
pub mod state;
pub mod store;
