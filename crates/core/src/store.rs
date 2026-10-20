//! 配置持久化。
//!
//! RON 格式 `%APPDATA%\scheduled-task\tasks.ron`(directories::ProjectDirs),
//! 文件头包含 config_version,可选字段通过 serde default 提供默认值。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::logger::{LogLevel, LogRetention, LogTimeZone};
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
    /// 文件日志存储限制,旧配置默认保留 7 天且不限制总大小。
    #[serde(default)]
    pub log_retention: LogRetention,
    /// GUI 与文件日志共用时区,旧配置默认同步系统。
    #[serde(default)]
    pub log_timezone: LogTimeZone,
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
            log_retention: LogRetention::default(),
            log_timezone: LogTimeZone::default(),
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
        let text = ron::ser::to_string_pretty(&out, ron::ser::PrettyConfig::default())
            .map_err(|e| StoreError::Emit(e.to_string()))?;
        atomic_write(&self.path, text.as_bytes())?;
        *self.theme_setting.lock().unwrap() = out.theme;
        tracing::info!("任务配置已保存到 {}", self.path.display());
        Ok(())
    }
}

/// 先完整写入同目录临时文件再替换，写入失败不会截断已有配置。
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let temporary = path.with_file_name(format!(
        ".scheduled-task-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // std::fs::rename 在 Windows 使用允许替换已有文件的重命名操作。
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// 从配置构造任务时的运行时初始化:按 enabled 归一化(防上次异常退出锁死状态)。
/// 状态落位由 TaskStateManager.reset 完成,这里只做加载侧校验的占位类型引用。
pub type LoadedTasks = Vec<TaskConfig>;

/// 配置入口校验并发域与任务标识,防止手工编辑绕过表单约束。
fn validate_config(config: &ConfigFile) -> Result<(), StoreError> {
    config
        .log_timezone
        .validate()
        .map_err(StoreError::InvalidConfig)?;
    config
        .log_retention
        .validate()
        .map_err(StoreError::InvalidConfig)?;
    if config.max_parallel == 0 {
        return Err(StoreError::InvalidConfig("最大并行数必须大于 0".into()));
    }
    let mut ids = std::collections::HashSet::new();
    for task in &config.tasks {
        task.validate()
            .map_err(|message| StoreError::InvalidConfig(format!("任务 {}: {message}", task.id)))?;
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
    fn 手工配置与保存共享任务边界且极端超时不触发异常() {
        let store = temporary_store();
        let source = r#"(config_version: 2, tasks: [(
            id: (1), name: "边界验证", script: (kind: cmd, source: inline("exit /b 0")),
            schedule: monthly(day: (1), times: ["08:00:00"]),
            retry: Some((window: 1, max_retries: 1)), timeout: Some(1)
        )])"#;
        for invalid in [
            source.replace("day: (1)", "day: (0)"),
            source.replace("day: (1)", "day: (32)"),
            source.replace("[\"08:00:00\"]", "[]"),
            source.replace("window: 1", "window: 0"),
            source.replace("window: 1", "window: 101"),
            source.replace("max_retries: 1", "max_retries: 0"),
            source.replace("max_retries: 1", "max_retries: 11"),
            source.replace("Some(1)", "Some(0)"),
            source.replace("Some(1)", "Some(301)"),
            source.replace("Some(1)", "Some(18446744073709551615)"),
            source.replace("边界验证", " "),
        ] {
            std::fs::write(store.path(), &invalid).unwrap();
            assert!(store.load().is_err(), "不应加载：{invalid}");
            if let Ok(config) = ron::from_str::<ConfigFile>(&invalid) {
                assert!(store.save(&config).is_err(), "不应保存：{invalid}");
            }
        }
        for valid in [
            source.to_string(),
            source
                .replace("Some(1)", "Some(300)")
                .replace("window: 1", "window: 100")
                .replace("max_retries: 1", "max_retries: 10"),
        ] {
            std::fs::write(store.path(), valid).unwrap();
            let config = store.load().unwrap();
            store.save(&config).unwrap();
            assert_eq!(
                store.load().unwrap().tasks[0]
                    .schedule
                    .monthly_day()
                    .unwrap()
                    .get(),
                1
            );
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn 显式变量顺序往返兼容旧映射且忽略重复和失效顺序项() {
        let store = temporary_store();
        for (order, expected) in [
            ("", vec!["A", "B", "Z"]),
            (r#"env_order: ["Z", "A", "B"],"#, vec!["Z", "A", "B"]),
            (r#"env_order: ["Z", "不存在", "Z"],"#, vec!["Z", "A", "B"]),
        ] {
            let source = format!(
                r#"(config_version: 2, tasks: [(
                id: (1), name: "变量排序", script: (kind: cmd, source: inline("exit /b 0"),
                    env: {{"A": "", "B": "  原始空格  ", "Z": "多\n行"}}, {order}),
                schedule: daily(times: ["08:00:00"])
            )])"#
            );
            std::fs::write(store.path(), source).unwrap();
            let config = store.load().unwrap();
            assert_eq!(
                config.tasks[0]
                    .script
                    .ordered_env()
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            store.save(&config).unwrap();
            let saved = store.load().unwrap();
            assert_eq!(
                saved.tasks[0]
                    .script
                    .ordered_env()
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(saved.tasks[0].script.env, config.tasks[0].script.env);
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn 环境文件列表兼容旧配置并保留顺序且校验空路径() {
        let store = temporary_store();
        for (fields, expected) in [
            ("", vec![]),
            (r#"env_file: Some("legacy.env"),"#, vec!["legacy.env"]),
            (
                r#"env_files: ["base.env", "local.env", "base.env"],"#,
                vec!["base.env", "local.env", "base.env"],
            ),
            (
                r#"env_file: Some("legacy.env"), env_files: ["base.env", "local.env"],"#,
                vec!["legacy.env", "base.env", "local.env"],
            ),
        ] {
            let source = format!(
                r#"(config_version: 2, tasks: [(
                id: (1), name: "环境文件验证", script: (kind: cmd, source: inline("exit /b 0"), {fields}),
                schedule: daily(times: ["08:00:00"])
            )])"#
            );
            std::fs::write(store.path(), source).unwrap();
            let config = store.load().unwrap();
            assert_eq!(
                config.tasks[0].script.env_file_paths().collect::<Vec<_>>(),
                expected
            );
            store.save(&config).unwrap();
            assert_eq!(
                store.load().unwrap().tasks[0]
                    .script
                    .env_file_paths()
                    .collect::<Vec<_>>(),
                expected
            );
            for invalid in ["", " \t", "nul\0path"] {
                let mut config = config.clone();
                config.tasks[0].script.env_files.push(invalid.into());
                assert!(store.save(&config).is_err());
                std::fs::write(store.path(), ron::ser::to_string(&config).unwrap()).unwrap();
                assert!(store.load().is_err());
            }
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn ron_roundtrip_preserves_enum_shapes() {
        let config = ConfigFile {
            config_version: CONFIG_VERSION,
            max_parallel: 3,
            theme: None,
            log_level: LogLevel::Debug,
            log_timezone: LogTimeZone::Custom("Asia/Shanghai".into()),
            log_retention: LogRetention {
                retention_days: 365,
                size_limit_enabled: true,
                max_size_mb: 5120,
            },
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
                    args: vec!["--profile".into(), "nightly".into()],
                    working_dir: Some("data".into()),
                    env: [("MODE".into(), "full".into())].into_iter().collect(),
                    env_file: Some(".env".into()),
                    env_order: Vec::new(),
                    env_files: vec![".env.local".into(), "config/.env.secrets".into()],
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
        assert_eq!(back.log_retention, config.log_retention);
        assert_eq!(back.log_timezone, config.log_timezone);
        assert_eq!(back.tasks[0].name, "每日同步");
        assert_eq!(back.tasks[0].timeout, Some(Duration::from_secs(30 * 60)));
        assert_eq!(back.tasks[0].script.args, ["--profile", "nightly"]);
        assert_eq!(back.tasks[0].script.working_dir.as_deref(), Some("data"));
        assert_eq!(back.tasks[0].script.env.get("MODE"), Some(&"full".into()));
        assert_eq!(back.tasks[0].script.env_file.as_deref(), Some(".env"));
        assert_eq!(
            back.tasks[0].script.env_files,
            vec![".env.local", "config/.env.secrets"]
        );
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
        assert_eq!(back.log_retention, LogRetention::default());
        assert_eq!(back.log_timezone, LogTimeZone::System);
        assert!(t.enabled);
        assert!(t.retry.is_none());
        assert!(t.timeout.is_none());
        assert!(t.script.args.is_empty());
        assert!(t.script.working_dir.is_none());
        assert!(t.script.env.is_empty());
        assert!(t.script.env_file.is_none());
        assert!(t.script.env_files.is_empty());
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

    fn temporary_store() -> TaskStore {
        static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        TaskStore::new(std::env::temp_dir().join(format!(
            "scheduled-task-store-{}-{}.ron",
            std::process::id(),
            NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }

    #[test]
    fn 保存以完整配置主题为准且校验失败保留原文件与缓存() {
        let store = temporary_store();
        let mut config = ConfigFile {
            theme: Some(ThemeSetting::Dark),
            ..Default::default()
        };
        store.save(&config).unwrap();
        assert_eq!(store.theme_setting(), Some(ThemeSetting::Dark));
        config.theme = Some(ThemeSetting::Light);
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap().theme, Some(ThemeSetting::Light));
        let original = std::fs::read(store.path()).unwrap();
        config.theme = Some(ThemeSetting::Dark);
        config.max_parallel = 0;
        assert!(store.save(&config).is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), original);
        assert_eq!(store.theme_setting(), Some(ThemeSetting::Light));
        std::fs::remove_file(store.path()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn 替换失败不会截断原配置或提前更新主题缓存() {
        use std::os::windows::fs::OpenOptionsExt;

        let store = temporary_store();
        let mut config = ConfigFile {
            theme: Some(ThemeSetting::Light),
            ..Default::default()
        };
        store.save(&config).unwrap();
        let original = std::fs::read(store.path()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(store.path())
            .unwrap();
        config.theme = Some(ThemeSetting::Dark);
        config.max_parallel = 5;
        assert!(store.save(&config).is_err());
        assert_eq!(store.theme_setting(), Some(ThemeSetting::Light));
        drop(lock);
        assert_eq!(std::fs::read(store.path()).unwrap(), original);
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn saved_retention_survives_other_configuration_changes() {
        let store = temporary_store();
        let retention = LogRetention {
            retention_days: 30,
            size_limit_enabled: true,
            max_size_mb: 2048,
        };
        let config = ConfigFile {
            log_retention: retention,
            log_timezone: LogTimeZone::Custom("America/New_York".into()),
            ..Default::default()
        };
        store.save(&config).unwrap();
        let mut loaded = store.load().unwrap();
        assert_eq!(loaded.log_retention, retention);
        loaded.log_level = LogLevel::Debug;
        loaded.max_parallel = 4;
        loaded.theme = Some(ThemeSetting::Dark);
        store.save(&loaded).unwrap();
        let updated = store.load().unwrap();
        assert_eq!(updated.log_retention, retention);
        assert_eq!(updated.log_timezone, config.log_timezone);
        assert_eq!(updated.theme, Some(ThemeSetting::Dark));
        assert_eq!(updated.log_level, LogLevel::Debug);
        assert_eq!(updated.max_parallel, 4);
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn 日志时区配置往返并保持其他设置() {
        let store = temporary_store();
        let mut config = ConfigFile {
            log_level: LogLevel::Trace,
            log_retention: LogRetention {
                retention_days: 90,
                size_limit_enabled: true,
                max_size_mb: 4096,
            },
            max_parallel: 3,
            ..Default::default()
        };
        config.theme = Some(ThemeSetting::Light);
        for timezone in [
            LogTimeZone::Custom("Asia/Shanghai".into()),
            LogTimeZone::Custom("America/New_York".into()),
            LogTimeZone::System,
        ] {
            config.log_timezone = timezone.clone();
            store.save(&config).unwrap();
            let loaded = store.load().unwrap();
            assert_eq!(loaded.log_timezone, timezone);
            assert_eq!(loaded.log_level, config.log_level);
            assert_eq!(loaded.log_retention, config.log_retention);
            assert_eq!(loaded.max_parallel, config.max_parallel);
            assert_eq!(loaded.theme, Some(ThemeSetting::Light));
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn 日志时区在加载及保存时拒绝非法值() {
        let store = temporary_store();
        for name in ["", "Invalid/Timezone", "Asia/Not_A_Zone"] {
            let config = ConfigFile {
                log_timezone: LogTimeZone::Custom(name.into()),
                ..Default::default()
            };
            assert!(matches!(
                store.save(&config),
                Err(StoreError::InvalidConfig(_))
            ));
            // 手工编辑配置也不能绕过时区校验。
            std::fs::write(store.path(), ron::ser::to_string(&config).unwrap()).unwrap();
            assert!(matches!(store.load(), Err(StoreError::InvalidConfig(_))));
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn retention_limits_are_validated_on_load_and_save() {
        let store = temporary_store();
        for (days, size, valid) in [
            (3, 200, true),
            (365, 5120, true),
            (2, 1024, false),
            (366, 1024, false),
            (7, 199, false),
            (7, 5121, false),
        ] {
            let config = ConfigFile {
                log_retention: LogRetention {
                    retention_days: days,
                    size_limit_enabled: false,
                    max_size_mb: size,
                },
                ..Default::default()
            };
            assert_eq!(store.save(&config).is_ok(), valid);
            // 手工修改 RON 也必须通过相同边界校验,未启用大小限制不放宽存储值范围。
            std::fs::write(store.path(), ron::ser::to_string(&config).unwrap()).unwrap();
            assert_eq!(store.load().is_ok(), valid);
        }
        std::fs::remove_file(store.path()).unwrap();
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
