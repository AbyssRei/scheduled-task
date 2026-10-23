//! 应用与核心库的桥接层。
//!
//! 上行:daemon(std mpsc 事件)→ 转发线程 → async-channel → UI 在 `cx.spawn`
//! 中 `.await`。
//! 下行:UI 直接调 [`AppCore`] 方法(增删改查/运行/停止),配置变更后
//! `daemon.reload_preserving` 保持运行族状态。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use scheduled_task_core::daemon::{Daemon, DaemonEvent};
use scheduled_task_core::logger::{self, LogLevel, LogRetention, LogTimeZone};
use scheduled_task_core::model::{TaskConfig, TaskId};
use scheduled_task_core::state::TaskStatus;
use scheduled_task_core::store::{ConfigFile, TaskStore, ThemeSetting};

/// 设置面板统一编辑和提交的应用配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    pub theme: ThemeSetting,
    pub max_parallel: u32,
    pub log_level: LogLevel,
    pub log_timezone: LogTimeZone,
    pub log_retention: LogRetention,
}

impl AppSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_parallel == 0 {
            return Err("最大同时执行任务数必须大于 0".into());
        }
        self.log_timezone.validate()?;
        self.log_retention.validate()
    }

    fn update_config(&self, config: &mut ConfigFile) {
        config.theme = Some(self.theme);
        config.max_parallel = self.max_parallel;
        config.log_level = self.log_level;
        config.log_timezone = self.log_timezone.clone();
        config.log_retention = self.log_retention;
    }
}

/// 应用共享核心:配置权威副本 + daemon + 持久化。
pub struct AppCore {
    change_gate: Mutex<()>,
    daemon: Arc<Daemon>,
    store: TaskStore,
    tasks: Mutex<Vec<TaskConfig>>,
    next_id: AtomicU64,
    max_parallel: AtomicU64,
    theme: Mutex<ThemeSetting>,
    log_level: Mutex<LogLevel>,
    log_retention: Mutex<LogRetention>,
    log_timezone: Mutex<LogTimeZone>,
}

impl AppCore {
    /// 从 `%APPDATA%\scheduled-task\tasks.ron` 加载(不存在则空配置起步)。
    pub fn init(daemon: Arc<Daemon>) -> Result<Arc<Self>, String> {
        let store = TaskStore::default_path().map_err(|e| e.to_string())?;
        let config: ConfigFile = store.load().map_err(|e| e.to_string())?;
        // 首次轮转和清理前应用保存的时区及存储限制。
        logger::init_logging_with_settings(
            std::path::Path::new("logs"),
            config.log_retention,
            config.log_timezone.clone(),
        )
        .map_err(|error| format!("初始化日志失败: {error}"))?;
        if let Err(error) = logger::set_log_level(config.log_level) {
            tracing::warn!("无法应用文件日志等级: {error}");
        }
        let mut tasks = config.tasks;
        // id 递增计数器:重启后取 max+1
        let mut next = 1_u64;
        for t in &mut tasks {
            if t.id.0 >= next {
                next = t.id.0.checked_add(1).ok_or("任务 ID 已耗尽")?;
            }
        }
        let theme = config.theme.unwrap_or_default();
        daemon.set_max_parallel(config.max_parallel as usize);
        daemon.load(tasks.clone());
        tracing::info!(
            "已从 {} 加载 {} 个任务",
            store.path().display(),
            tasks.len()
        );
        Ok(Arc::new(Self {
            change_gate: Mutex::new(()),
            daemon,
            store,
            tasks: Mutex::new(tasks),
            next_id: AtomicU64::new(next),
            max_parallel: AtomicU64::new(config.max_parallel.max(1) as u64),
            theme: Mutex::new(theme),
            log_level: Mutex::new(config.log_level),
            log_retention: Mutex::new(config.log_retention),
            log_timezone: Mutex::new(config.log_timezone),
        }))
    }

    pub fn tasks(&self) -> Vec<TaskConfig> {
        self.tasks.lock().unwrap().clone()
    }

    /// 状态快照(显示用)。
    pub fn status_of(&self, id: TaskId) -> Option<TaskStatus> {
        self.daemon.states().get_status(id)
    }

    pub fn last_run(&self, id: TaskId) -> Option<scheduled_task_core::daemon::ExecutionRecord> {
        self.daemon.last_run(id)
    }
    pub fn next_fire(&self, id: TaskId) -> Option<jiff::civil::DateTime> {
        self.daemon.next_fire(id)
    }
    pub fn queue_reason(&self, id: TaskId) -> Option<String> {
        self.daemon.queue_reason(id)
    }

    pub fn add(&self, mut task: TaskConfig) -> Result<TaskId, String> {
        task.id = TaskId(
            self.next_id
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
                .map_err(|_| "任务 ID 已耗尽")?,
        );
        let id = task.id;
        self.commit_task_changes(|tasks| {
            tasks.push(task);
            Ok(())
        })?;
        Ok(id)
    }

    /// 编辑任务:运行族状态保留(daemon.reload_preserving 语义)。
    pub fn update(&self, task: TaskConfig) -> Result<(), String> {
        self.commit_task_changes(|tasks| {
            let slot = tasks
                .iter_mut()
                .find(|t| t.id == task.id)
                .ok_or("任务不存在")?;
            *slot = task;
            Ok(())
        })
    }

    pub fn delete(&self, id: TaskId) -> Result<(), String> {
        self.commit_task_changes(|tasks| {
            tasks.retain(|t| t.id != id);
            Ok(())
        })
    }

    /// 启用/禁用切换。运行族任务仅翻转 enabled 标记,运行状态保留
    /// (收尾时由 finalize_status 按 enabled 归一化)。
    pub fn toggle_enabled(&self, id: TaskId) -> Result<(), String> {
        self.commit_task_changes(|tasks| {
            let task = tasks.iter_mut().find(|t| t.id == id).ok_or("任务不存在")?;
            task.enabled = !task.enabled;
            Ok(())
        })
    }

    pub fn run_now(&self, id: TaskId) {
        self.daemon.execute_now(id);
    }

    /// 全局并发上限,由配置文件持久化并立即作用于队列。
    pub fn max_parallel(&self) -> usize {
        self.max_parallel.load(Ordering::SeqCst) as usize
    }

    /// 界面主题设置(启动时应用;未持久化过 = 跟随系统)。
    pub fn theme_setting(&self) -> ThemeSetting {
        *self.theme.lock().unwrap()
    }

    /// 当前管理器文件日志等级。
    pub fn log_level(&self) -> LogLevel {
        *self.log_level.lock().unwrap()
    }

    pub fn stop(&self, id: TaskId) -> bool {
        self.daemon.stop_task(id)
    }

    /// 当前文件日志存储限制。
    pub fn log_retention(&self) -> LogRetention {
        *self.log_retention.lock().unwrap()
    }

    /// GUI 与文件日志共用的时区设置。
    pub fn log_timezone(&self) -> LogTimeZone {
        self.log_timezone.lock().unwrap().clone()
    }

    pub fn settings(&self) -> AppSettings {
        AppSettings {
            theme: self.theme_setting(),
            max_parallel: self.max_parallel() as u32,
            log_level: self.log_level(),
            log_timezone: self.log_timezone(),
            log_retention: self.log_retention(),
        }
    }

    /// 全页校验、完整配置保存一次、统一生效；失败时不提前清理历史日志。
    pub fn save_settings(&self, settings: AppSettings) -> Result<(), String> {
        self.save_settings_with(settings, |previous, settings| {
            let prepared = logger::prepare_log_settings(
                (previous.log_level != settings.log_level).then_some(settings.log_level),
                (previous.log_timezone != settings.log_timezone)
                    .then(|| settings.log_timezone.clone()),
                (previous.log_retention != settings.log_retention)
                    .then_some(settings.log_retention),
            )?;
            Ok(move || prepared.apply())
        })
    }

    fn save_settings_with<F>(
        &self,
        settings: AppSettings,
        prepare_logs: impl FnOnce(&AppSettings, &AppSettings) -> Result<F, String>,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let _guard = self.change_gate.lock().unwrap();
        settings.validate()?;
        let previous_settings = self.settings();
        if settings == previous_settings {
            return Ok(());
        }
        let apply_logs = prepare_logs(&previous_settings, &settings)?;
        let previous = self.config_snapshot();
        let mut config = previous.clone();
        settings.update_config(&mut config);
        self.store
            .save(&config)
            .map_err(|error| format!("保存设置失败，所有更改均未应用: {error}"))?;
        if let Err(error) = apply_logs() {
            return match self.store.save(&previous) {
                Ok(()) => Err(format!("应用设置失败，已恢复原配置: {error}")),
                Err(restore_error) => Err(format!(
                    "应用设置失败，运行设置未变: {error}；恢复原配置文件失败: {restore_error}"
                )),
            };
        }
        *self.theme.lock().unwrap() = settings.theme;
        *self.log_level.lock().unwrap() = settings.log_level;
        *self.log_timezone.lock().unwrap() = settings.log_timezone;
        *self.log_retention.lock().unwrap() = settings.log_retention;
        self.max_parallel
            .store(u64::from(settings.max_parallel), Ordering::SeqCst);
        self.daemon.set_max_parallel(settings.max_parallel as usize);
        self.daemon.notify_config_changed();
        Ok(())
    }

    /// 候选配置先完整保存，成功后再发布给界面及调度器。
    fn commit_task_changes(
        &self,
        change: impl FnOnce(&mut Vec<TaskConfig>) -> Result<(), String>,
    ) -> Result<(), String> {
        let _guard = self.change_gate.lock().unwrap();
        let mut config = self.config_snapshot();
        change(&mut config.tasks)?;
        self.store
            .save(&config)
            .map_err(|error| format!("保存任务失败，更改未应用：{error}"))?;
        *self.tasks.lock().unwrap() = config.tasks.clone();
        self.daemon.reload_preserving(config.tasks);
        self.daemon.notify_config_changed();
        Ok(())
    }
    fn config_snapshot(&self) -> ConfigFile {
        let tasks = self.tasks.lock().unwrap().clone();
        ConfigFile {
            config_version: scheduled_task_core::store::CONFIG_VERSION,
            theme: Some(*self.theme.lock().unwrap()),
            tasks,
            max_parallel: self.max_parallel() as u32,
            log_level: self.log_level(),
            log_retention: self.log_retention(),
            log_timezone: self.log_timezone(),
        }
    }
}

/// 事件转发:daemon std mpsc → async-channel(UI 侧 await)。
pub fn spawn_event_forward(
    rx: scheduled_task_core::daemon::EventReceiver,
    tx: async_channel::Sender<DaemonEvent>,
) {
    std::thread::Builder::new()
        .name("event-forward".into())
        .spawn(move || {
            while let Ok(ev) = rx.recv() {
                if tx.send_blocking(ev).is_err() {
                    break; // UI 已退出
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use scheduled_task_core::model::{
        ConcurrencyPolicy, Schedule, ScriptKind, ScriptSource, ScriptSpec,
    };

    struct TestCore(Arc<AppCore>, std::path::PathBuf);

    impl TestCore {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "scheduled-task-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let (tx, _) = scheduled_task_core::daemon::spawn_channel();
            let core = AppCore {
                change_gate: Mutex::new(()),
                daemon: Arc::new(Daemon::new(tx)),
                store: TaskStore::new(directory.join("tasks.ron")),
                tasks: Mutex::new(vec![TaskConfig {
                    id: TaskId(42),
                    name: "仅保存，不运行".into(),
                    script: ScriptSpec {
                        kind: ScriptKind::Cmd,
                        source: ScriptSource::Inline("exit /b 0".into()),
                        args: vec![],
                        working_dir: None,
                        env: Default::default(),
                        env_file: None,
                        env_order: Vec::new(),
                        env_files: Vec::new(),
                    },
                    schedule: Schedule::Daily {
                        times: vec!["23:59:59".parse().unwrap()],
                    },
                    retry: None,
                    timeout: None,
                    enabled: false,
                    concurrency: ConcurrencyPolicy::Parallel,
                }]),
                next_id: AtomicU64::new(43),
                max_parallel: AtomicU64::new(1),
                theme: Mutex::new(ThemeSetting::Light),
                log_level: Mutex::new(LogLevel::Info),
                log_retention: Mutex::new(LogRetention::default()),
                log_timezone: Mutex::new(LogTimeZone::Custom("UTC".into())),
            };
            core.store.save(&core.config_snapshot()).unwrap();
            core.daemon.load(core.tasks());
            Self(Arc::new(core), directory)
        }

        fn changed(&self) -> AppSettings {
            AppSettings {
                theme: ThemeSetting::Dark,
                max_parallel: 4,
                log_level: LogLevel::Debug,
                log_timezone: LogTimeZone::Custom("+08:00".into()),
                log_retention: LogRetention {
                    retention_days: 30,
                    size_limit_enabled: true,
                    max_size_mb: 2048,
                },
            }
        }
    }

    impl Drop for TestCore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    /// 真实桌面回归入口：仅加载临时配置，不启动调度线程、托盘或保活。
    #[test]
    #[ignore = "需要真实桌面交互，关闭验收窗口后结束"]
    fn 环境条目拖动桌面验收() {
        use gpui_kit::{AppContext as _, WindowBounds, WindowOptions, px, size};

        let test = TestCore::new();
        let core = test.0.clone();
        let mut task = core.tasks()[0].clone();
        task.script.env_files = vec![
            ".env".into(),
            "config/.env.local".into(),
            "config/中文.env".into(),
        ];
        for (key, value) in [
            ("FIRST", "首条变量"),
            ("SECOND", "第一行\n第二行\n第三行"),
            ("THIRD", "  保留空格  "),
            ("LAST", ""),
        ] {
            task.script.env.insert(key.into(), value.into());
            task.script.env_order.push(key.into());
        }
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                gpui_kit::open_window(
                    WindowOptions {
                        titlebar: Some(gpui_kit::TitlebarOptions {
                            title: Some("环境条目拖动验收（临时配置）".into()),
                            ..Default::default()
                        }),
                        window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds::centered(
                            None,
                            size(px(1080.), px(800.)),
                            cx,
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        window.on_window_should_close(cx, |_, cx| {
                            cx.quit();
                            true
                        });
                        let edit_core = core.clone();
                        window.on_next_frame(move |window, cx| {
                            crate::task_dialog::TaskDialogView::open_edit(
                                edit_core, task, window, cx,
                            );
                        });
                        cx.new(|cx| crate::main_window::TaskListView::new(core.clone(), window, cx))
                    },
                )
                .expect("打开拖动验收窗口失败");
            });
    }

    #[test]
    #[cfg(windows)]
    fn 任务增删改及禁用保存失败均保留文件内存和调度状态() {
        use std::os::windows::fs::OpenOptionsExt;
        let test = TestCore::new();
        let core = &test.0;
        let original = std::fs::read(core.store.path()).unwrap();
        // 禁止替换已打开的目标文件，模拟真实的 Windows 保存失败。
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(core.store.path())
            .unwrap();
        let mut changed = core.tasks()[0].clone();
        changed.name = "更改后的任务".into();
        for result in [
            core.add(changed.clone()).map(|_| ()),
            core.update(changed.clone()),
            core.delete(TaskId(42)),
            core.toggle_enabled(TaskId(42)),
        ] {
            assert!(result.is_err());
            assert_eq!(std::fs::read(core.store.path()).unwrap(), original);
            assert_eq!(core.tasks().len(), 1);
            assert_eq!(core.tasks()[0].name, "仅保存，不运行");
            assert!(!core.tasks()[0].enabled);
            assert_eq!(core.daemon.tasks_snapshot()[0].name, "仅保存，不运行");
            assert_eq!(core.status_of(TaskId(42)), Some(TaskStatus::Disabled));
        }
        drop(handle);
        core.update(changed).unwrap();
        assert_eq!(core.tasks()[0].name, "更改后的任务");
        assert_eq!(core.daemon.tasks_snapshot()[0].name, "更改后的任务");
        assert_eq!(core.store.load().unwrap().tasks[0].name, "更改后的任务");
    }

    #[test]
    fn 并发提交任务和设置不覆盖彼此且无效任务不发布() {
        let test = TestCore::new();
        let core = &test.0;
        let mut invalid = core.tasks()[0].clone();
        invalid.name.clear();
        assert!(core.update(invalid).is_err());
        std::thread::scope(|scope| {
            scope.spawn(|| core.toggle_enabled(TaskId(42)).unwrap());
            scope.spawn(|| {
                core.save_settings_with(test.changed(), |_, _| Ok(|| Ok(())))
                    .unwrap()
            });
        });
        let saved = core.store.load().unwrap();
        assert!(saved.tasks[0].enabled);
        assert_eq!(saved.max_parallel, test.changed().max_parallel);
        assert_eq!(core.tasks()[0].enabled, saved.tasks[0].enabled);
    }

    #[test]
    fn 任务标识耗尽后拒绝新增且不回绕或修改配置() {
        let test = TestCore::new();
        let core = &test.0;
        let task = core.tasks()[0].clone();
        core.next_id.store(u64::MAX - 1, Ordering::SeqCst);
        assert_eq!(core.add(task.clone()).unwrap(), TaskId(u64::MAX - 1));
        let saved = std::fs::read(core.store.path()).unwrap();
        let tasks = core.tasks();
        for _ in 0..2 {
            assert_eq!(core.add(task.clone()).unwrap_err(), "任务 ID 已耗尽");
            assert_eq!(core.next_id.load(Ordering::SeqCst), u64::MAX);
            assert_eq!(core.tasks(), tasks);
            assert_eq!(core.daemon.tasks_snapshot(), tasks);
            assert_eq!(std::fs::read(core.store.path()).unwrap(), saved);
        }
    }

    #[test]
    fn 并发新增任务标识唯一且完整持久化() {
        let test = TestCore::new();
        let core = &test.0;
        let template = core.tasks()[0].clone();
        let mut ids = std::thread::scope(|scope| {
            (0..8)
                .map(|index| {
                    let mut task = template.clone();
                    task.name = format!("并发新增-{index}");
                    scope.spawn(move || core.add(task).unwrap().0)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        ids.sort_unstable();
        assert_eq!(ids, (43..51).collect::<Vec<_>>());
        let tasks = core.tasks();
        assert_eq!(tasks.len(), 9);
        assert_eq!(core.store.load().unwrap().tasks, tasks);
        assert_eq!(core.daemon.tasks_snapshot(), tasks);
    }

    #[test]
    fn 所有设置完整落盘后才提交并保留原任务() {
        let test = TestCore::new();
        let core = &test.0;
        let settings = test.changed();
        let previous = core.settings();
        core.save_settings_with(settings.clone(), |_, _| {
            assert_eq!(core.store.load().unwrap().theme, Some(previous.theme));
            Ok(|| {
                let saved = core.store.load().unwrap();
                assert_eq!(saved.theme, Some(settings.theme));
                assert_eq!(saved.max_parallel, settings.max_parallel);
                assert_eq!(saved.log_level, settings.log_level);
                assert_eq!(saved.log_timezone, settings.log_timezone);
                assert_eq!(saved.log_retention, settings.log_retention);
                assert_eq!(saved.tasks.len(), 1);
                assert_eq!(saved.tasks[0].id, TaskId(42));
                assert_eq!(saved.tasks[0].name, "仅保存，不运行");
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(core.settings(), settings);
        assert_eq!(core.daemon.max_parallel(), 4);
    }

    #[test]
    fn 无效字段和准备失败均不落盘或修改运行设置() {
        let test = TestCore::new();
        let core = &test.0;
        let original = std::fs::read(core.store.path()).unwrap();
        let previous = core.settings();
        for invalid in 0..3 {
            let mut settings = test.changed();
            match invalid {
                0 => settings.max_parallel = 0,
                1 => settings.log_retention.retention_days = 2,
                _ => settings.log_timezone = LogTimeZone::Custom("invalid/timezone".into()),
            }
            assert!(
                core.save_settings_with(
                    settings,
                    |_, _| -> Result<fn() -> Result<(), String>, String> {
                        panic!("无效设置不应进入日志准备阶段");
                    }
                )
                .is_err()
            );
        }
        assert!(
            core.save_settings_with(test.changed(), |_, _| {
                Err::<fn() -> Result<(), String>, _>("模拟日志尚未初始化".into())
            })
            .is_err()
        );
        assert_eq!(std::fs::read(core.store.path()).unwrap(), original);
        assert_eq!(core.settings(), previous);
        assert_eq!(core.daemon.max_parallel(), 1);
    }

    #[test]
    fn 保存失败不提交运行设置或执行清理() {
        let test = TestCore::new();
        let core = &test.0;
        let previous = core.settings();
        std::fs::remove_file(core.store.path()).unwrap();
        std::fs::create_dir(core.store.path()).unwrap();
        let result = core.save_settings_with(test.changed(), |_, _| {
            Ok(|| panic!("持久化失败不应提交日志或清理"))
        });
        assert!(result.unwrap_err().contains("保存设置失败"));
        assert_eq!(core.settings(), previous);
        assert_eq!(core.daemon.max_parallel(), 1);
        assert_eq!(std::fs::read_dir(&test.1).unwrap().count(), 1);
    }

    #[test]
    fn 日志提交失败恢复原配置且不修改其他运行设置() {
        let test = TestCore::new();
        let core = &test.0;
        let original = std::fs::read(core.store.path()).unwrap();
        let previous = core.settings();
        let error = core
            .save_settings_with(test.changed(), |_, _| Ok(|| Err("模拟日志提交失败".into())))
            .unwrap_err();
        assert!(error.contains("已恢复原配置"));
        assert_eq!(std::fs::read(core.store.path()).unwrap(), original);
        assert_eq!(core.store.theme_setting(), Some(previous.theme));
        assert_eq!(core.settings(), previous);
        assert_eq!(core.daemon.max_parallel(), 1);
    }

    #[test]
    fn 设置未改动不重复保存或准备日志() {
        let test = TestCore::new();
        let core = &test.0;
        // 移除文件后再次提交相同设置仍不新建文件，且不访问全局日志。
        std::fs::remove_file(core.store.path()).unwrap();
        core.save_settings(core.settings()).unwrap();
        assert!(!core.store.path().exists());
    }
}
