//! 按任务 ID 登记运行进程，并统一处理存活查询与外部停止请求。

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::{Duration, Instant};

use crate::model::TaskId;

#[cfg(windows)]
use super::job;

/// 进程注册表:按 TaskId 键隔离各任务,允许任务名称相同。
#[derive(Default)]
pub struct ProcessRegistry {
    inner: Mutex<std::collections::HashMap<TaskId, Arc<RunningProcess>>>,
}

pub struct RunningProcess {
    pub(super) child: Mutex<Child>,
    /// Windows:与子进程同生命周期的作业守护(树状终止 + 崩溃兜底)
    #[cfg(windows)]
    pub(super) job: Mutex<Option<job::JobGuard>>,
    pub(super) stopped_manually: AtomicBool,
}

impl RunningProcess {
    /// 停止:Windows 直接终止整个作业及其进程树;
    /// Unix 使用 SIGTERM→5s→SIGKILL 降级。
    pub(super) fn stop(&self) -> bool {
        #[cfg(windows)]
        {
            if let Some(job) = self.job.lock().unwrap().as_ref() {
                tracing::info!("终止作业内全部进程(树状)");
                job.terminate();
            }
            // 等待子进程退出(KILL_ON_JOB_CLOSE 下应立即)
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.child.lock().unwrap().try_wait() {
                    Ok(Some(_)) => {
                        tracing::info!("任务已成功停止");
                        return true;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("停止任务失败: {e}");
                        return false;
                    }
                }
                if Instant::now() >= deadline {
                    tracing::error!("作业终止后子进程仍未退出");
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        #[cfg(not(windows))]
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
            match child.wait() {
                Ok(_) => {
                    tracing::info!("任务已成功停止");
                    true
                }
                Err(e) => {
                    tracing::error!("停止任务失败: {e}");
                    false
                }
            }
        }
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn register(&self, id: TaskId, process: Arc<RunningProcess>) {
        // 同 id 旧注册先移除(理论不可达:状态机拦截重复执行)
        self.inner.lock().unwrap().insert(id, process);
    }

    pub(super) fn unregister(&self, id: TaskId) {
        self.inner.lock().unwrap().remove(&id);
    }

    pub fn is_running(&self, id: TaskId) -> bool {
        self.inner.lock().unwrap().contains_key(&id)
    }

    /// 外部停止请求。返回 false 表示任务未在运行。
    pub fn stop(&self, id: TaskId) -> bool {
        let entry = self.inner.lock().unwrap().get(&id).cloned();
        match entry {
            Some(proc) => {
                proc.stopped_manually.store(true, Ordering::SeqCst);
                proc.stop()
            }
            None => {
                tracing::warn!("任务未在运行: {id}");
                false
            }
        }
    }

    /// 任务进程是否真实存活(注册存在且子进程尚未退出)。
    pub fn is_alive(&self, id: TaskId) -> bool {
        let entry = self.inner.lock().unwrap().get(&id).cloned();
        match entry {
            Some(p) => p
                .child
                .lock()
                .unwrap()
                .try_wait()
                .map(|status| status.is_none())
                .unwrap_or(false),
            None => false,
        }
    }

    /// 任务子进程 PID(诊断/外部终止用);未注册时返回 None。
    pub fn pid(&self, id: TaskId) -> Option<u32> {
        let entry = self.inner.lock().unwrap().get(&id).cloned();
        entry.map(|p| p.child.lock().unwrap().id())
    }

    /// 任务是否被手动停止过(重试循环据此不再重试)。
    pub fn was_manually_stopped(&self, id: TaskId) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.stopped_manually.load(Ordering::SeqCst))
            .unwrap_or(false)
    }
}

/// 全局进程注册表(按 TaskId 键;daemon/CLI/测试共用同一实例,
/// 保证 stop 与运行登记落在同一张表上)。
pub fn global_registry() -> &'static ProcessRegistry {
    static REGISTRY: std::sync::OnceLock<ProcessRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(ProcessRegistry::new)
}
