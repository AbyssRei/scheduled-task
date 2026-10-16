//! 调度、原子准入、并发队列与执行生命周期编排。
//! 状态按 TaskId 管理;所有触发共用准入锁,重试冷却也占用执行槽。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::{ConcurrencyPolicy, TaskConfig, TaskId};
use crate::retry::check_retry_condition;
use crate::runner::{ProcessRegistry, RetryCheck, RunOptions};
use crate::scheduler::{FireTracker, local_now};
use crate::state::{StateError, TaskStateManager, TaskStatus, is_running_family};

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Status {
        id: TaskId,
        status: TaskStatus,
    },
    LogLine {
        id: TaskId,
        line: String,
    },
    Finished {
        id: TaskId,
        returncode: i32,
        retry_triggered: bool,
    },
    AdmittedRejected {
        id: TaskId,
        reason: String,
    },
    Queued {
        id: TaskId,
    },
    ConfigChanged,
}

/// 准入同时预留槽位或入队,不是只读查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Allow,
    Queue,
    Reject { reason: &'static str },
}

struct ActiveRun {
    /// 运行中的策略保持启动时快照,编辑只影响下一次执行。
    policy: ConcurrencyPolicy,
    cancel: Arc<AtomicBool>,
}

struct AdmissionState {
    queue: VecDeque<TaskId>,
    active: HashMap<TaskId, ActiveRun>,
    max_parallel: usize,
    max_queue_size: usize,
}

/// 克隆仅共享同一个调度器,执行线程收尾可立即唤醒队列。
#[derive(Clone)]
pub struct Daemon {
    states: Arc<TaskStateManager>,
    tasks: Arc<Mutex<Vec<TaskConfig>>>,
    tracker: Arc<Mutex<FireTracker>>,
    event_tx: std::sync::mpsc::Sender<DaemonEvent>,
    shutdown: Arc<AtomicBool>,
    dead_since: Arc<Mutex<HashMap<TaskId, Instant>>>,
    tick_count: Arc<AtomicU64>,
    /// 锁顺序始终为 admission → tasks → tracker/状态机。
    admission: Arc<Mutex<AdmissionState>>,
}

pub fn spawn_channel() -> (
    std::sync::mpsc::Sender<DaemonEvent>,
    std::sync::mpsc::Receiver<DaemonEvent>,
) {
    std::sync::mpsc::channel()
}

impl Daemon {
    pub fn new(event_tx: std::sync::mpsc::Sender<DaemonEvent>) -> Self {
        Self {
            states: Arc::new(TaskStateManager::new()),
            tasks: Arc::new(Mutex::new(Vec::new())),
            tracker: Arc::new(Mutex::new(FireTracker::new())),
            dead_since: Arc::new(Mutex::new(HashMap::new())),
            tick_count: Arc::new(AtomicU64::new(0)),
            event_tx,
            shutdown: Arc::new(AtomicBool::new(false)),
            admission: Arc::new(Mutex::new(AdmissionState {
                queue: VecDeque::new(),
                active: HashMap::new(),
                max_parallel: 1,
                max_queue_size: 1024,
            })),
        }
    }

    pub fn with_max_parallel(
        event_tx: std::sync::mpsc::Sender<DaemonEvent>,
        max_parallel: usize,
    ) -> Self {
        let daemon = Self::new(event_tx);
        daemon.set_max_parallel(max_parallel);
        daemon
    }

    /// 降低上限不终止在途执行;升高上限立即唤醒排队任务。
    pub fn set_max_parallel(&self, value: usize) {
        let mut admission = self.admission.lock().unwrap();
        admission.max_parallel = value.max(1);
        self.pump_locked(&mut admission);
    }

    pub fn max_parallel(&self) -> usize {
        self.admission.lock().unwrap().max_parallel
    }

    /// 降低队列上限仅影响新触发,已接收的队列项继续保留。
    pub fn set_max_queue_size(&self, value: usize) {
        self.admission.lock().unwrap().max_queue_size = value;
    }

    pub fn queued_count(&self) -> usize {
        self.admission.lock().unwrap().queue.len()
    }

    pub fn states(&self) -> &TaskStateManager {
        &self.states
    }
    pub fn registry(&self) -> &'static ProcessRegistry {
        crate::runner::global_registry()
    }

    /// 启动加载;若已有在途任务则使用保留式重载。
    pub fn load(&self, tasks: Vec<TaskConfig>) {
        self.reload_preserving(tasks);
    }

    /// 编辑/禁用/删除与准入串行化。队列只存 ID,出队读取最新配置。
    pub fn reload_preserving(&self, tasks: Vec<TaskConfig>) {
        let mut admission = self.admission.lock().unwrap();
        let old = std::mem::replace(&mut *self.tasks.lock().unwrap(), tasks.clone());
        admission
            .queue
            .retain(|id| tasks.iter().any(|t| t.id == *id && t.enabled));
        for task in old {
            if !tasks.iter().any(|t| t.id == task.id) {
                // 删除运行任务只取消后续触发,本次继续运行并占槽直至收尾。
                if !admission.active.contains_key(&task.id) {
                    self.states.forget(task.id);
                }
            }
        }
        for task in &tasks {
            if admission.active.contains_key(&task.id)
                || self
                    .states
                    .get_status(task.id)
                    .is_some_and(is_running_family)
            {
                continue;
            }
            if self.states.get_status(task.id).is_none() {
                self.states.reset(task.id, task.enabled);
            } else if admission.queue.contains(&task.id) {
                self.change_status(task.id, TaskStatus::Queued, "保留队列位置");
            } else {
                let target = if task.enabled {
                    TaskStatus::Ready
                } else {
                    TaskStatus::Disabled
                };
                self.change_status(task.id, target, "配置变更");
            }
        }
        let mut tracker = self.tracker.lock().unwrap();
        tracker.retain_config(&tasks);
        tracker.seed_passed(&tasks, &local_now(jiff::Timestamp::now()));
        drop(tracker);
        self.pump_locked(&mut admission);
    }

    /// 停止同时取消该任务全部等待项;取消令牌覆盖启动间隙与重试冷却。
    pub fn stop_task(&self, id: TaskId) -> bool {
        let mut admission = self.admission.lock().unwrap();
        admission.queue.retain(|queued| *queued != id);
        if let Some(active) = admission.active.get(&id) {
            active.cancel.store(true, Ordering::SeqCst);
            self.change_status(id, TaskStatus::Stopping, "用户请求停止");
            // 在持锁期间停止当前注册进程,避免误杀刚接续的下一次执行。
            if self.registry().is_running(id) {
                self.registry().stop(id);
            }
        } else {
            if self.states.get_status(id).is_some_and(is_running_family) {
                self.change_status(id, TaskStatus::Stopping, "用户请求停止");
                self.registry().stop(id);
            }
            let enabled = self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.enabled);
            if let Some(enabled) = enabled {
                self.states
                    .finalize_status(id, enabled, "取消排队或停止完成");
                self.emit_status(id);
            }
        }
        self.pump_locked(&mut admission);
        true
    }

    pub fn tasks_snapshot(&self) -> Vec<TaskConfig> {
        self.tasks.lock().unwrap().clone()
    }

    pub fn execute_now(&self, id: TaskId) {
        let task = self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .cloned();
        if let Some(task) = task {
            self.trigger(&task);
        } else {
            self.reject(id, "任务不存在");
        }
    }

    pub fn notify_config_changed(&self) {
        let _ = self.event_tx.send(DaemonEvent::ConfigChanged);
    }

    /// 停止调度并取消等待项,在途执行自行收尾,不再拉起队列。
    pub fn shutdown(&self) {
        let mut admission = self.admission.lock().unwrap();
        self.shutdown.store(true, Ordering::SeqCst);
        for id in admission.queue.drain(..).collect::<Vec<_>>() {
            if !admission.active.contains_key(&id) {
                let enabled = self
                    .tasks
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|t| t.id == id && t.enabled);
                self.states
                    .finalize_status(id, enabled, "关闭调度,取消排队");
                self.emit_status(id);
            }
        }
    }
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// 托管执行以执行线程生命周期为准,避免启动/收尾间隙错误释放并发槽。
    pub fn reconcile(&self) {
        let admission = self.admission.lock().unwrap();
        for task in self.tasks.lock().unwrap().iter() {
            let status = self.states.get_status(task.id);
            if admission.active.contains_key(&task.id)
                || status != Some(TaskStatus::Running)
                || self.registry().is_alive(task.id)
            {
                self.dead_since.lock().unwrap().remove(&task.id);
                continue;
            }
            let elapsed = self
                .dead_since
                .lock()
                .unwrap()
                .entry(task.id)
                .or_insert_with(Instant::now)
                .elapsed();
            if elapsed > Duration::from_secs(5) {
                self.states
                    .finalize_status(task.id, task.enabled, "监测到进程已结束,自动重置");
                self.dead_since.lock().unwrap().remove(&task.id);
                self.emit_status(task.id);
            }
        }
    }

    /// 单一准入点,预留全局槽和任务/组锁;重复等待项合并为一次。
    pub fn admit(&self, task: &TaskConfig) -> Admission {
        let mut admission = self.admission.lock().unwrap();
        self.admit_locked(task.id, &mut admission)
    }

    fn admit_locked(&self, id: TaskId, admission: &mut AdmissionState) -> Admission {
        if self.is_shutdown() {
            return Admission::Reject {
                reason: "调度器已关闭",
            };
        }
        let tasks = self.tasks.lock().unwrap().clone();
        let Some(task) = tasks.iter().find(|t| t.id == id) else {
            return Admission::Reject {
                reason: "任务不存在",
            };
        };
        if !task.enabled {
            return Admission::Reject {
                reason: "任务已禁用",
            };
        }
        if admission.queue.contains(&id) {
            return Admission::Reject {
                reason: "任务已有等待项",
            };
        }
        if Self::can_start(task, admission, &tasks) {
            self.reserve(task, admission);
            return Admission::Allow;
        }
        if admission.queue.len() >= admission.max_queue_size {
            return Admission::Reject {
                reason: "等待队列已满",
            };
        }
        admission.queue.push_back(id);
        // 同任务仍有实例运行时,保留其运行状态,另发入队事件。
        if !admission.active.contains_key(&id) {
            self.change_status(id, TaskStatus::Queued, "并发资源忙,进入队列");
        }
        let _ = self.event_tx.send(DaemonEvent::Queued { id });
        Admission::Queue
    }

    fn can_start(task: &TaskConfig, admission: &AdmissionState, tasks: &[TaskConfig]) -> bool {
        admission.active.len() < admission.max_parallel
            && !admission.active.contains_key(&task.id)
            && !admission.active.values().any(|active| match (&task.concurrency, &active.policy) {
                (ConcurrencyPolicy::Group(a), ConcurrencyPolicy::Group(b)) => a == b,
                _ => false,
            })
            // 即使前项因同 ID 在途而等待,同组后项也不能越过它。
            && !admission.queue.iter().take_while(|id| **id != task.id).any(|id| {
                tasks.iter().find(|queued| queued.id == *id).is_some_and(|queued| {
                    matches!((&task.concurrency, &queued.concurrency),
                        (ConcurrencyPolicy::Group(a), ConcurrencyPolicy::Group(b)) if a == b)
                })
            })
    }

    fn reserve(&self, task: &TaskConfig, admission: &mut AdmissionState) {
        admission.active.insert(
            task.id,
            ActiveRun {
                policy: task.concurrency.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        self.change_status(task.id, TaskStatus::Running, "获得并发槽");
    }

    /// 按等待顺序选取首个可运行项;被占用的任务/组不阻塞无关任务。
    fn pump_locked(&self, admission: &mut AdmissionState) {
        if self.is_shutdown() {
            return;
        }
        loop {
            let next = {
                let tasks = self.tasks.lock().unwrap();
                admission.queue.iter().enumerate().find_map(|(ix, id)| {
                    tasks
                        .iter()
                        .find(|t| t.id == *id && t.enabled && Self::can_start(t, admission, &tasks))
                        .cloned()
                        .map(|task| (ix, task))
                })
            };
            let Some((ix, task)) = next else {
                return;
            };
            admission.queue.remove(ix);
            self.reserve(&task, admission);
            self.spawn_allowed(task, admission);
        }
    }

    pub fn tick(&self) -> usize {
        if self.is_shutdown() {
            return 0;
        }
        {
            let mut admission = self.admission.lock().unwrap();
            self.pump_locked(&mut admission);
        }
        if self.tick_count.fetch_add(1, Ordering::Relaxed) % 2 == 1 {
            self.reconcile();
        }
        let now = local_now(jiff::Timestamp::now());
        let due: Vec<TaskConfig> = {
            let tasks = self.tasks.lock().unwrap();
            let mut tracker = self.tracker.lock().unwrap();
            tasks
                .iter()
                .filter(|t| tracker.should_fire(t, &now))
                .cloned()
                .collect()
        };
        for task in &due {
            self.trigger(task);
        }
        self.tracker.lock().unwrap().gc(now.date());
        due.len()
    }

    fn trigger(&self, task: &TaskConfig) {
        let mut admission = self.admission.lock().unwrap();
        self.pump_locked(&mut admission);
        match self.admit_locked(task.id, &mut admission) {
            Admission::Allow => {
                let current = self
                    .tasks
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|t| t.id == task.id)
                    .cloned()
                    .unwrap();
                self.spawn_allowed(current, &mut admission);
            }
            Admission::Queue => {}
            Admission::Reject { reason } => self.reject(task.id, reason),
        }
    }

    fn reject(&self, id: TaskId, reason: &str) {
        tracing::warn!("任务 {id} 触发被拒: {reason}");
        let _ = self.event_tx.send(DaemonEvent::AdmittedRejected {
            id,
            reason: reason.into(),
        });
    }

    fn change_status(&self, id: TaskId, status: TaskStatus, reason: &str) {
        if self.states.transition(id, status, reason).is_ok() {
            self.emit_status(id);
        }
    }
    fn emit_status(&self, id: TaskId) {
        if let Some(status) = self.states.get_status(id) {
            let _ = self.event_tx.send(DaemonEvent::Status { id, status });
        }
    }

    fn spawn_allowed(&self, task: TaskConfig, admission: &mut AdmissionState) {
        let daemon = self.clone();
        let id = task.id;
        let cancel = admission.active[&id].cancel.clone();
        let result = std::thread::Builder::new()
            .name(format!("exec-{}", task.name))
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_attempts(
                        &task,
                        &daemon.states,
                        &daemon.event_tx,
                        Some(cancel),
                        Some(&daemon.admission),
                    )
                }));
                let (returncode, retry_triggered) = outcome.unwrap_or_else(|_| {
                    tracing::error!("任务 {id} 执行线程异常退出");
                    if daemon.registry().is_running(id) {
                        daemon.registry().stop(id);
                    }
                    (-1, false)
                });
                let mut admission = daemon.admission.lock().unwrap();
                daemon.finish_locked(id, returncode, retry_triggered, &mut admission);
                daemon.pump_locked(&mut admission);
            });
        if let Err(error) = result {
            tracing::error!("任务 {id} 执行线程启动失败: {error}");
            self.finish_locked(id, -1, false, admission);
        }
    }

    fn finish_locked(
        &self,
        id: TaskId,
        returncode: i32,
        retry_triggered: bool,
        admission: &mut AdmissionState,
    ) {
        admission.active.remove(&id);
        let current = self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .cloned();
        if let Some(task) = current {
            self.states.finalize_status(id, task.enabled, "执行结束");
            if admission.queue.contains(&id) {
                self.change_status(id, TaskStatus::Queued, "等待下一次执行");
            } else if self.states.get_status(id) != Some(TaskStatus::Failed) {
                self.emit_status(id);
            }
        } else {
            self.states.forget(id);
        }
        let _ = self.event_tx.send(DaemonEvent::Finished {
            id,
            returncode,
            retry_triggered,
        });
    }
}

/// 独立执行入口,供无调度器的验证场景使用。
pub fn execute_with_retry(
    task: &TaskConfig,
    states: &TaskStateManager,
    event_tx: &std::sync::mpsc::Sender<DaemonEvent>,
) {
    if states
        .transition(task.id, TaskStatus::Running, "开始执行")
        .is_err()
    {
        return;
    }
    let _ = event_tx.send(DaemonEvent::Status {
        id: task.id,
        status: TaskStatus::Running,
    });
    let (returncode, retry_triggered) = execute_attempts(task, states, event_tx, None, None);
    states.finalize_status(task.id, task.enabled, "执行结束");
    if let Some(status) = states.get_status(task.id)
        && status != TaskStatus::Failed
    {
        let _ = event_tx.send(DaemonEvent::Status {
            id: task.id,
            status,
        });
    }
    let _ = event_tx.send(DaemonEvent::Finished {
        id: task.id,
        returncode,
        retry_triggered,
    });
}

/// 执行尝试与重试,收尾交由调用方在准入锁下完成。
fn execute_attempts(
    task: &TaskConfig,
    states: &TaskStateManager,
    event_tx: &std::sync::mpsc::Sender<DaemonEvent>,
    cancel: Option<Arc<AtomicBool>>,
    admission: Option<&Mutex<AdmissionState>>,
) -> (i32, bool) {
    // 状态与事件在同一准入锁内发布,避免停止事件之后晚到的运行/重试事件。
    let transition = |status, reason| {
        let _guard = admission.map(|gate| gate.lock().unwrap());
        if states.transition(task.id, status, reason).is_err() {
            return false;
        }
        let _ = event_tx.send(DaemonEvent::Status {
            id: task.id,
            status,
        });
        true
    };
    let stopped = || {
        cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
            || states.get_status(task.id) == Some(TaskStatus::Stopping)
    };
    let max_retries = task.retry.as_ref().map_or(0, |r| r.max_retries);
    let mut last_returncode = -1;
    let mut last_retry_triggered = false;
    for attempt in 0..=max_retries {
        if stopped() {
            break;
        }
        if attempt > 0 && !transition(TaskStatus::Running, "开始重试") {
            break;
        }
        let retry_check = task.retry.as_ref().map(|r| {
            let pattern = r.pattern.clone();
            let window = r.window as usize;
            RetryCheck {
                window: r.window,
                check: Box::new(move |line, history| {
                    check_retry_condition(line, &pattern, history, window)
                }),
            }
        });
        let result = crate::runner::run(
            &task.script,
            RunOptions {
                id: Some(task.id),
                timeout: task.timeout,
                cancel: cancel.clone(),
                charset: Default::default(),
                on_line: Some(Box::new({
                    let id = task.id;
                    let name = task.name.clone();
                    let tx = event_tx.clone();
                    move |line| {
                        // 输出随执行逐行记录，界面关闭或事件接收端断开也不影响留存。
                        tracing::info!(
                            target: "scheduled_task_core::task_output",
                            task_id = id.0,
                            task_name = %name,
                            attempt = u64::from(attempt) + 1,
                            "{line}"
                        );
                        let _ = tx.send(DaemonEvent::LogLine {
                            id,
                            line: line.to_string(),
                        });
                    }
                })),
                retry: retry_check,
            },
        );
        match result {
            Ok(outcome) => {
                last_returncode = outcome.returncode;
                last_retry_triggered = outcome.retry_triggered;
                if last_returncode == 0 && !last_retry_triggered {
                    break;
                }
            }
            Err(error) => {
                tracing::error!("任务 {} 执行失败: {error}", task.id);
                last_returncode = -1;
                last_retry_triggered = false;
            }
        }
        if stopped() {
            break;
        }
        if attempt == max_retries {
            if task.retry.is_some() {
                transition(TaskStatus::Failed, "重试耗尽");
            }
            break;
        }
        if !transition(TaskStatus::Retrying, "等待重试") {
            break;
        }
        let cooldown = task
            .retry
            .as_ref()
            .map_or(Duration::from_secs(2), |r| r.cooldown);
        let start = Instant::now();
        while start.elapsed() < cooldown && !stopped() {
            std::thread::sleep(
                cooldown
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_millis(50)),
            );
        }
    }
    (last_returncode, last_retry_triggered)
}

/// daemon 状态机操作错误别名(GUI/CLI 匹配用)。
pub type StateResult<T> = Result<T, StateError>;
