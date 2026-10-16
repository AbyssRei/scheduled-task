//! 配置持久化。
//!
//! RON 格式 `%APPDATA%\scheduled-task\tasks.ron`(directories::ProjectDirs),
//! 文件头包含 config_version,可选字段通过 serde default 提供默认值。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::logger::LogLevel;
use crate::model::TaskConfig;

/// 配置文件顶层结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub config_version: u32,
    #[serde(default)]
    pub theme: Option<ThemeSetting>,
    /// 文件日志等级,未设置时为 INFO。
    #[serde(default)]
    pub log_level: LogLevel,
    pub tasks: Vec<TaskConfig>,
    /// 全局同时运行的最大任务数,至少为 1。
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
}

fn default_max_parallel() -> u32 {
    1
}

/// 当前配置版本。
pub const CONFIG_VERSION: u32 = 2;

/// 界面主题设置(应用级,持久化到配置文件)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSetting {
    /// 跟随系统深浅色(默认)
    #[default]
    FollowSystem,
    /// 固定浅色
    Light,
    /// 固定深色
    Dark,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            theme: None,
            log_level: LogLevel::default(),
            tasks: Vec::new(),
            max_parallel: default_max_parallel(),
        }
    }
}

/// store 错误(Display 中文)。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("配置无效: {0}")]
    InvalidConfig(String),
    #[error("RON 解析失败: {0}")]
    Parse(String),
    #[error("RON 序列化失败: {0}")]
    Emit(String),
    #[error("读写配置文件失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("无法定位配置目录")]
    NoConfigDir,
    #[error("配置版本不受支持: {0}")]
    UnsupportedVersion(u32),
}

/// 配置存储:RON 读写。
pub struct TaskStore {
    path: PathBuf,
    theme_setting: Mutex<Option<ThemeSetting>>,
}

impl TaskStore {
    /// 用默认标准目录构造:`%APPDATA%\scheduled-task\tasks.ron`。
    ///
    /// 用 BaseDirs(而非 ProjectDirs):后者在 Windows 会多一层 `config`
    /// 子目录,与默认配置路径不符。
    pub fn default_path() -> Result<Self, StoreError> {
        let dirs = directories::BaseDirs::new().ok_or(StoreError::NoConfigDir)?;
        Ok(Self::new(
            dirs.config_dir().join("scheduled-task").join("tasks.ron"),
        ))
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            theme_setting: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 界面主题设置(启动时应用;未设置 = 跟随系统)。
    pub fn theme_setting(&self) -> Option<ThemeSetting> {
        *self.theme_setting.lock().unwrap()
    }

    pub fn set_theme_setting(&self, setting: Option<ThemeSetting>) {
        *self.theme_setting.lock().unwrap() = setting;
    }

    /// 加载配置;文件不存在返回默认(空)配置。
    pub fn load(&self) -> Result<ConfigFile, StoreError> {
        if !self.path.exists() {
            return Ok(ConfigFile::default());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let config: ConfigFile = ron::Options::default()
            .from_str(&text)
            .map_err(|e| StoreError::Parse(e.to_string()))?;
        if config.config_version > CONFIG_VERSION {
            return Err(StoreError::UnsupportedVersion(config.config_version));
        }
        validate_config(&config)?;
        *self.theme_setting.lock().unwrap() = config.theme;
        Ok(config)
    }

    /// 保存配置(父目录不存在则创建)。
    pub fn save(&self, config: &ConfigFile) -> Result<(), StoreError> {
        validate_config(config)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = config.clone();
        out.config_version = CONFIG_VERSION;
        out.theme = *self.theme_setting.lock().unwrap();
        let text = ron::ser::to_string_pretty(&out, ron::ser::PrettyConfig::default())
            .map_err(|e| StoreError::Emit(e.to_string()))?;
        std::fs::write(&self.path, text)?;
        tracing::info!("任务配置已保存到 {}", self.path.display());
        Ok(())
    }
}

/// 从配置构造任务时的运行时初始化:按 enabled 归一化(防上次异常退出锁死状态)。
/// 状态落位由 TaskStateManager.reset 完成,这里只做加载侧校验的占位类型引用。
pub type LoadedTasks = Vec<TaskConfig>;

/// 配置入口校验并发域与任务标识,防止手工编辑绕过表单约束。
fn validate_config(config: &ConfigFile) -> Result<(), StoreError> {
    if config.max_parallel == 0 {
        return Err(StoreError::InvalidConfig("最大并行数必须大于 0".into()));
    }
    let mut ids = std::collections::HashSet::new();
    for task in &config.tasks {
        if !ids.insert(task.id) {
            return Err(StoreError::InvalidConfig(format!(
                "任务 ID 重复: {}",
                task.id
            )));
        }
        if let crate::model::ConcurrencyPolicy::Group(group) = &task.concurrency
            && group.trim().is_empty()
        {
            return Err(StoreError::InvalidConfig(format!(
                "任务 {} 的互斥组名不能为空",
                task.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{PyLauncher, Schedule, ScriptKind, ScriptSource, ScriptSpec};

    #[test]
    fn ron_roundtrip_preserves_enum_shapes() {
        let config = ConfigFile {
            config_version: CONFIG_VERSION,
            max_parallel: 3,
            theme: None,
            log_level: LogLevel::Debug,
            tasks: vec![TaskConfig {
                id: crate::model::TaskId(1),
                name: "每日同步".into(),
                script: ScriptSpec {
                    kind: ScriptKind::Python {
                        launcher: PyLauncher::Uv {
                            project: Some("demo".into()),
                        },
                    },
                    source: ScriptSource::File("scripts/sync.py".into()),
                },
                schedule: Schedule::Monthly {
                    day: crate::model::DayOfMonth::new(1).unwrap(),
                    times: vec!["08:00:00".parse().unwrap()],
                },
                retry: Some(crate::model::RetryPolicy::default()),
                timeout: Some(Duration::from_secs(30 * 60)),
                enabled: true,
                concurrency: crate::model::ConcurrencyPolicy::Parallel,
            }],
        };

        let text = ron::ser::to_string_pretty(&config, Default::default()).unwrap();
        // enum 原生表达 + 人读可编辑
        assert!(text.contains("python("));
        assert!(text.contains("launcher: uv("));
        assert!(text.contains("project: Some(\"demo\")"));
        assert!(text.contains("monthly("));
        assert!(text.contains("config_version: 2"));
        assert!(text.contains("log_level: debug"));
        let back: ConfigFile = ron::Options::default().from_str(&text).unwrap();
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.max_parallel, 3);
        assert_eq!(back.log_level, LogLevel::Debug);
        assert_eq!(back.tasks[0].name, "每日同步");
        assert_eq!(back.tasks[0].timeout, Some(Duration::from_secs(30 * 60)));
    }

    #[test]
    fn missing_fields_use_defaults() {
        // window/max_retries/cooldown/enabled 缺省值;timeout 以分钟表达
        let text = r#"
(
    config_version: 1,
    tasks: [
        (
            id: (7),
            name: "最少字段",
            script: (
                kind: auto,
                source: file("t.bat"),
            ),
            schedule: daily(times: ["07:30:00"]),
        ),
    ],
)
"#;
        let back: ConfigFile = ron::Options::default().from_str(text).unwrap();
        let t = &back.tasks[0];
        assert_eq!(t.id, crate::model::TaskId(7));
        assert_eq!(back.max_parallel, 1);
        assert_eq!(back.log_level, LogLevel::Info);
        assert!(t.enabled);
        assert!(t.retry.is_none());
        assert!(t.timeout.is_none());
        // 未指定策略时使用 Parallel。
        assert_eq!(t.concurrency, crate::model::ConcurrencyPolicy::Parallel);
    }

    #[test]
    fn log_levels_roundtrip_and_reject_unknown_values() {
        for (level, value) in [
            (LogLevel::Error, "error"),
            (LogLevel::Warn, "warn"),
            (LogLevel::Info, "info"),
            (LogLevel::Debug, "debug"),
            (LogLevel::Trace, "trace"),
        ] {
            let source = format!("(config_version: 2, tasks: [], log_level: {value})");
            let config: ConfigFile = ron::from_str(&source).unwrap();
            assert_eq!(config.log_level, level);
            let encoded = ron::ser::to_string(&config).unwrap();
            let decoded: ConfigFile = ron::from_str(&encoded).unwrap();
            assert_eq!(decoded.log_level, level);
        }
        assert_eq!(ConfigFile::default().log_level, LogLevel::Info);
        assert!(
            ron::from_str::<ConfigFile>("(config_version: 2, tasks: [], log_level: verbose)")
                .is_err()
        );
    }

    #[test]
    fn ron_roundtrip_preserves_concurrency_policy() {
        use crate::model::ConcurrencyPolicy;

        let source = r#"
(
    config_version: 1,
    tasks: [
        (
            id: (1), name: "并行", concurrency: parallel,
            script: (kind: auto, source: inline("")),
            schedule: daily(times: ["07:30:00"]),
        ),
        (
            id: (2), name: "串行", concurrency: solo,
            script: (kind: auto, source: inline("")),
            schedule: daily(times: ["07:30:00"]),
        ),
        (
            id: (3), name: "分组", concurrency: group("批处理"),
            script: (kind: auto, source: inline("")),
            schedule: daily(times: ["07:30:00"]),
        ),
    ],
)
"#;
        let config: ConfigFile = ron::Options::default().from_str(source).unwrap();
        assert_eq!(config.tasks[0].concurrency, ConcurrencyPolicy::Parallel);
        assert_eq!(config.tasks[1].concurrency, ConcurrencyPolicy::Solo);
        assert_eq!(
            config.tasks[2].concurrency,
            ConcurrencyPolicy::Group("批处理".into())
        );

        let encoded = ron::ser::to_string(&config).unwrap();
        let decoded: ConfigFile = ron::Options::default().from_str(&encoded).unwrap();
        assert_eq!(decoded.tasks[2].concurrency, config.tasks[2].concurrency);
    }

    #[test]
    fn rejects_zero_limit_duplicate_ids_and_blank_groups() {
        let mut config = ConfigFile {
            max_parallel: 0,
            ..Default::default()
        };
        assert!(validate_config(&config).is_err());
        config.max_parallel = 1;
        let task: TaskConfig = ron::from_str(
            r#"(
            id: (7), name: "校验", script: (kind: cmd, source: inline("exit /b 0")),
            schedule: daily(times: ["07:30:00"]), concurrency: group(" ")
        )"#,
        )
        .unwrap();
        config.tasks.push(task);
        assert!(validate_config(&config).is_err());
        config.tasks[0].concurrency = crate::model::ConcurrencyPolicy::Solo;
        assert!(validate_config(&config).is_ok());
        config.tasks.push(config.tasks[0].clone());
        assert!(validate_config(&config).is_err());
    }
}
