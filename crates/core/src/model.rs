//! 任务强类型数据模型。
//!
//! 采用强类型 schema,配置加载时校验版本与字段约束。
//! 可选字段通过 serde default 提供默认值。
//! 运行时状态(status)不在此模型中——由 [`crate::state::TaskStateManager`] 持有。

use std::time::Duration;

use jiff::civil::Time;
use serde::{Deserialize, Serialize};

use thiserror::Error;

/// 任务唯一标识。
///
/// 进程与执行器注册表使用自增 ID 为键,确保同名任务相互独立。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskId(pub u64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 任务配置(强类型)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub id: TaskId,
    pub name: String,
    pub script: ScriptSpec,
    pub schedule: Schedule,
    /// 自动重试策略;None = 不自动重试
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
    /// 执行超时(分钟→秒);None = 不限
    #[serde(default, with = "timeout_secs_opt")]
    pub timeout: Option<Duration>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 并发策略;同一 TaskId 始终串行,避免覆盖进程登记和运行状态。
    #[serde(default)]
    pub concurrency: ConcurrencyPolicy,
}

fn default_true() -> bool {
    true
}

/// 任务并发策略。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    /// 可与其他任务并行,仍受全局上限和同任务单实例约束。
    #[default]
    Parallel,
    /// 同一任务同时最多运行一个实例,后续触发进入队列。
    Solo,
    /// 同组同时最多运行一个任务,组内按 FIFO 排队。
    Group(String),
}

/// Option<Duration> 以分钟为单位序列化(人读可编辑:timeout = 30 表示 30 分钟)。
mod timeout_secs_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            // 显式 Some(分钟):裸数字会破坏 Option 反序列化语义
            Some(d) => s.serialize_some(&(d.as_secs() / 60)),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let minutes: Option<u64> = Option::deserialize(d)?;
        Ok(minutes.map(|m| Duration::from_secs(m * 60)))
    }
}

/// 调度规则。
///
/// monthly 按真实月历触发:当月无指定日期则跳过(见 scheduler)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    Daily { times: Vec<Time> },
    Monthly { day: DayOfMonth, times: Vec<Time> },
    Custom { times: Vec<Time> },
}

impl Schedule {
    /// 本调度规则的全部触发时刻。
    pub fn times(&self) -> &[Time] {
        match self {
            Schedule::Daily { times }
            | Schedule::Monthly { times, .. }
            | Schedule::Custom { times } => times,
        }
    }

    /// monthly 的目标日(1–31);其余 None。
    pub fn monthly_day(&self) -> Option<DayOfMonth> {
        match self {
            Schedule::Monthly { day, .. } => Some(*day),
            _ => None,
        }
    }
}

/// 每月几号(1–31),加载时校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayOfMonth(u8);

impl DayOfMonth {
    pub fn new(day: u8) -> Result<Self, ModelError> {
        if (1..=31).contains(&day) {
            Ok(Self(day))
        } else {
            Err(ModelError::DayOutOfRange(day))
        }
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

/// 脚本规格:类型 + 来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptSpec {
    pub kind: ScriptKind,
    pub source: ScriptSource,
}

/// 脚本类型与解释器选择。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    /// 按扩展名自动推断
    Auto,
    Bash,
    Sh,
    Cmd,
    PowerShell,
    Pwsh,
    Python {
        launcher: PyLauncher,
    },
    /// 手工命令,`{script}` 占位符替换为脚本路径
    Manual {
        command: String,
    },
}

/// Python 启动方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PyLauncher {
    System,
    Uv {
        #[serde(default)]
        project: Option<String>,
    },
}

/// 脚本来源:文件路径或内联内容(内联写临时文件再执行)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptSource {
    File(String),
    Inline(String),
}

/// 自动重试策略。
///
/// pattern 每行一个候选(OR 语义,空白归一化子串匹配,
/// 空 = 窗口内核心内容一致);window 为连续次数;
/// max_retries 为最大重试次数;cooldown 为重试间隔,默认 2s。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default)]
    pub pattern: String,
    #[serde(default = "default_window")]
    pub window: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_cooldown", with = "cooldown_secs")]
    pub cooldown: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            window: default_window(),
            max_retries: default_max_retries(),
            cooldown: default_cooldown(),
        }
    }
}

fn default_window() -> u32 {
    5
}

fn default_max_retries() -> u32 {
    3
}

fn default_cooldown() -> Duration {
    Duration::from_secs(2)
}

mod cooldown_secs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(v: &Duration, s: S) -> Result<S::Ok, S::Error> {
        v.as_secs().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// 模型层校验错误(Display 中文)。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelError {
    #[error("每月日期超出范围: {0}(应为 1–31)")]
    DayOutOfRange(u8),
    #[error("触发时刻数量不足")]
    NoTimes,
    #[error("任务名为空")]
    EmptyName,
}
