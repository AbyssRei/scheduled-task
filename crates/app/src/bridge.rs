//! 应用与核心库的桥接层。
//!
//! 上行:daemon(std mpsc 事件)→ 转发线程 → async-channel → UI 在 `cx.spawn`
//! 中 `.await`。
//! 下行:UI 直接调 [`AppCore`] 方法(增删改查/运行/停止),配置变更后
//! `daemon.reload_preserving` 保持运行族状态。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use scheduled_task_core::daemon::{Daemon, DaemonEvent};
use scheduled_task_core::logger::{self, LogLevel};
use scheduled_task_core::model::{TaskConfig, TaskId};
use scheduled_task_core::state::TaskStatus;
use scheduled_task_core::store::{ConfigFile, TaskStore, ThemeSetting};

/// 应用共享核心:配置权威副本 + daemon + 持久化。
pub struct AppCore {
    daemon: Arc<Daemon>,
    store: TaskStore,
    tasks: Mutex<Vec<TaskConfig>>,
    next_id: AtomicU64,
    max_parallel: AtomicU64,
    theme: Mutex<ThemeSetting>,
    log_level: Mutex<LogLevel>,
}

impl AppCore {
    /// 从 `%APPDATA%\scheduled-task\tasks.ron` 加载(不存在则空配置起步)。
    pub fn init(daemon: Arc<Daemon>) -> Result<Arc<Self>, String> {
        let store = TaskStore::default_path().map_err(|e| e.to_string())?;
        let config: ConfigFile = store.load().map_err(|e| e.to_string())?;
        if let Err(error) = logger::set_log_level(config.log_level) {
            tracing::warn!("无法应用文件日志等级: {error}");
        }
        let mut tasks = config.tasks;
        // id 递增计数器:重启后取 max+1
        let mut next = 1_u64;
        for t in &mut tasks {
            if t.id.0 >= next {
                next = t.id.0 + 1;
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
            daemon,
            store,
            tasks: Mutex::new(tasks),
            next_id: AtomicU64::new(next),
            max_parallel: AtomicU64::new(config.max_parallel.max(1) as u64),
            theme: Mutex::new(theme),
            log_level: Mutex::new(config.log_level),
        }))
    }

    pub fn tasks(&self) -> Vec<TaskConfig> {
        self.tasks.lock().unwrap().clone()
    }

    /// 状态快照(显示用)。
    pub fn status_of(&self, id: TaskId) -> Option<TaskStatus> {
        self.daemon.states().get_status(id)
    }

    pub fn add(&self, mut task: TaskConfig) -> TaskId {
        task.id = TaskId(self.next_id.fetch_add(1, Ordering::SeqCst));
        self.tasks.lock().unwrap().push(task.clone());
        self.commit_task_changes();
        task.id
    }

    /// 编辑任务:运行族状态保留(daemon.reload_preserving 语义)。
    pub fn update(&self, task: TaskConfig) {
        let mut all = self.tasks.lock().unwrap();
        if let Some(slot) = all.iter_mut().find(|t| t.id == task.id) {
            *slot = task;
        }
        drop(all);
        self.commit_task_changes();
    }

    pub fn delete(&self, id: TaskId) {
        self.tasks.lock().unwrap().retain(|t| t.id != id);
        self.commit_task_changes();
    }

    /// 启用/禁用切换。运行族任务仅翻转 enabled 标记,运行状态保留
    /// (收尾时由 finalize_status 按 enabled 归一化)。
    pub fn toggle_enabled(&self, id: TaskId) {
        let mut all = self.tasks.lock().unwrap();
        if let Some(t) = all.iter_mut().find(|t| t.id == id) {
            t.enabled = !t.enabled;
        }
        drop(all);
        self.commit_task_changes();
    }

    pub fn run_now(&self, id: TaskId) {
        self.daemon.execute_now(id);
    }

    /// 全局并发上限,由配置文件持久化并立即作用于队列。
    pub fn max_parallel(&self) -> usize {
        self.max_parallel.load(Ordering::SeqCst) as usize
    }

    /// 更新全局并发上限并保存,正在执行的任务不中断。
    pub fn set_max_parallel(&self, value: usize) {
        let value = value.clamp(1, u32::MAX as usize);
        self.max_parallel.store(value as u64, Ordering::SeqCst);
        self.daemon.set_max_parallel(value);
        self.save();
        self.daemon.notify_config_changed();
    }

    /// 界面主题设置(启动时应用;未持久化过 = 跟随系统)。
    pub fn theme_setting(&self) -> ThemeSetting {
        *self.theme.lock().unwrap()
    }

    /// 更新界面主题设置并持久化。
    pub fn set_theme_setting(&self, setting: ThemeSetting) {
        *self.theme.lock().unwrap() = setting;
        self.store.set_theme_setting(Some(setting));
        self.save();
    }

    /// 当前管理器文件日志等级。
    pub fn log_level(&self) -> LogLevel {
        *self.log_level.lock().unwrap()
    }

    /// 立即调整文件日志并保存;保存失败时恢复之前的运行等级。
    pub fn set_log_level(&self, level: LogLevel) -> Result<(), String> {
        let mut config = self.config_snapshot();
        let previous = config.log_level;
        logger::set_log_level(level)?;
        config.log_level = level;
        if let Err(error) = self.store.save(&config) {
            return match logger::set_log_level(previous) {
                Ok(()) => Err(format!("保存日志等级失败: {error}")),
                Err(restore_error) => Err(format!(
                    "保存日志等级失败: {error}; 恢复运行等级失败: {restore_error}"
                )),
            };
        }
        *self.log_level.lock().unwrap() = level;
        Ok(())
    }

    pub fn stop(&self, id: TaskId) -> bool {
        self.daemon.stop_task(id)
    }

    /// 任务配置变更的统一收尾:保留运行状态重载、持久化并通知界面。
    fn commit_task_changes(&self) {
        self.daemon.reload_preserving(self.tasks());
        self.save();
        self.daemon.notify_config_changed();
    }
    fn config_snapshot(&self) -> ConfigFile {
        let tasks = self.tasks.lock().unwrap().clone();
        ConfigFile {
            config_version: scheduled_task_core::store::CONFIG_VERSION,
            theme: Some(*self.theme.lock().unwrap()),
            tasks,
            max_parallel: self.max_parallel() as u32,
            log_level: self.log_level(),
        }
    }

    fn save(&self) {
        if let Err(e) = self.store.save(&self.config_snapshot()) {
            tracing::error!("保存任务配置失败: {e}");
        }
    }
}

/// 事件转发:daemon std mpsc → async-channel(UI 侧 await)。
pub fn spawn_event_forward(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
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
