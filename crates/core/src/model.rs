//! 任务强类型数据模型。
//!
//! 采用强类型 schema,配置加载时校验版本与字段约束。
//! 可选字段通过 serde default 提供默认值。
//! 运行时状态(status)不在此模型中——由 [`crate::state::TaskStateManager`] 持有。

use std::collections::BTreeMap;
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

impl TaskConfig {
    /// 所有配置入口共享的领域约束，避免手工编辑绕过表单校验。
    pub fn validate(&self) -> Result<(), String> {
        self.script.validate_env_files()?;
        if self.name.trim().is_empty() {
            return Err("任务名称不能为空".into());
        }
        if self.schedule.times().is_empty() {
            return Err("至少需要一个触发时间(HH:MM:SS)".into());
        }
        match &self.script.source {
            ScriptSource::File(path) if path.trim().is_empty() => {
                return Err("脚本路径不能为空".into());
            }
            _ => {}
        }
        if let ScriptKind::Manual { command } = &self.script.kind
            && command.trim().is_empty()
        {
            return Err("手工命令不能为空".into());
        }
        if let Some(retry) = &self.retry {
            if !(1..=100).contains(&retry.window) {
                return Err("连续次数 应在 1–100 之间".into());
            }
            if !(1..=10).contains(&retry.max_retries) {
                return Err("最大重试次数 应在 1–10 之间".into());
            }
        }
        if self.timeout.is_some_and(|value| {
            value < Duration::from_secs(60)
                || value > Duration::from_secs(300 * 60)
                || value.as_secs() % 60 != 0
                || value.subsec_nanos() != 0
        }) {
            return Err("超时应为 1–300 的整数分钟".into());
        }
        if let ConcurrencyPolicy::Group(group) = &self.concurrency
            && group.trim().is_empty()
        {
            return Err("同组互斥需要填写组名".into());
        }
        Ok(())
    }
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
        minutes
            .map(|m| {
                m.checked_mul(60)
                    .map(Duration::from_secs)
                    .ok_or_else(|| serde::de::Error::custom("超时分钟数溢出"))
            })
            .transpose()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DayOfMonth(u8);

impl<'de> Deserialize<'de> for DayOfMonth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename = "DayOfMonth")]
        struct RawDay(u8);
        Self::new(RawDay::deserialize(deserializer)?.0).map_err(serde::de::Error::custom)
    }
}

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
    /// 传给脚本或手工命令的参数,按 token 保持顺序,不经过 shell 再解析。
    #[serde(default)]
    pub args: Vec<String>,
    /// 工作目录;相对路径按脚本所在目录解析,为空时使用脚本所在目录。
    #[serde(default)]
    pub working_dir: Option<String>,
    /// 显式环境变量，优先级高于所有 .env 文件，空值表示设置为空字符串。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 兼容旧配置的单文件入口，先于 env_files 加载；相对路径按最终工作目录解析。
    #[serde(default)]
    pub env_file: Option<String>,
    /// 按顺序加载，后面的文件覆盖前面的同名变量。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_files: Vec<String>,
}

impl ScriptSpec {
    pub fn env_file_paths(&self) -> impl Iterator<Item = &str> {
        self.env_file
            .iter()
            .map(String::as_str)
            .filter(|path| !path.trim().is_empty())
            .chain(self.env_files.iter().map(String::as_str))
    }

    pub fn validate_env_files(&self) -> Result<(), String> {
        for (index, path) in self.env_files.iter().enumerate() {
            if path.trim().is_empty() || path.contains('\0') {
                return Err(format!(
                    "第 {} 个 .env 文件路径为空或包含无效字符",
                    index + 1
                ));
            }
        }
        Ok(())
    }
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
