//! 任务状态机。
//!
//! 包含 Queued 在内的 7 种状态;数据驱动转移表、同状态视为成功、非法转移拒绝并记 warning、
//! `finalize_status` 的 FAILED 终态保留与两步归一化。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::TaskId;

/// 任务状态枚举(7 态)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    /// 禁用
    Disabled,
    /// 准备就绪
    Ready,
    /// 等待并发资源,尚未启动进程
    Queued,
    /// 正在运行
    Running,
    /// 重试中(两次尝试之间)
    Retrying,
    /// 停止中(用户请求停止,等待进程退出)
    Stopping,
    /// 失败(重试耗尽)
    Failed,
}

impl TaskStatus {
    /// 状态的中文显示标签。
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Disabled => "禁用",
            TaskStatus::Ready => "准备就绪",
            TaskStatus::Queued => "排队中",
            TaskStatus::Running => "正在运行",
            TaskStatus::Retrying => "重试中",
            TaskStatus::Stopping => "停止中",
            TaskStatus::Failed => "失败",
        }
    }
}

/// 运行族:这些状态下任务被认为"正在运行",受存活监测线程对账。
pub const RUNNING_FAMILY: [TaskStatus; 3] = [
    TaskStatus::Running,
    TaskStatus::Retrying,
    TaskStatus::Stopping,
];

pub fn is_running_family(status: TaskStatus) -> bool {
    RUNNING_FAMILY.contains(&status)
}

/// 允许的状态转移表(包含排队、取消和失败后重新入队)。
///
/// 运行族不可直接跳到 DISABLED——必须先回到 READY,再由调用方按 enabled
/// 归一化(避免出现"禁用中却在跑"的矛盾)。
pub const ALLOWED_TRANSITIONS: &[(TaskStatus, &[TaskStatus])] = &[
    (TaskStatus::Disabled, &[TaskStatus::Ready]),
    (
        TaskStatus::Ready,
        &[
            TaskStatus::Running,
            TaskStatus::Queued,
            TaskStatus::Disabled,
        ],
    ),
    (
        TaskStatus::Queued,
        &[TaskStatus::Running, TaskStatus::Ready, TaskStatus::Disabled],
    ),
    (
        TaskStatus::Running,
        &[
            TaskStatus::Retrying,
            TaskStatus::Stopping,
            TaskStatus::Ready,
            TaskStatus::Failed,
        ],
    ),
    (
        TaskStatus::Retrying,
        &[
            TaskStatus::Running,
            TaskStatus::Stopping,
            TaskStatus::Ready,
            TaskStatus::Failed,
        ],
    ),
    (TaskStatus::Stopping, &[TaskStatus::Ready]),
    (
        TaskStatus::Failed,
        &[
            TaskStatus::Running,
            TaskStatus::Queued,
            TaskStatus::Ready,
            TaskStatus::Disabled,
        ],
    ),
];

pub fn allowed_transitions(from: TaskStatus) -> &'static [TaskStatus] {
    ALLOWED_TRANSITIONS
        .iter()
        .find(|(s, _)| *s == from)
        .map(|(_, targets)| *targets)
        .unwrap_or(&[])
}

/// 状态机错误(Display 为中文,GUI 据此弹窗)。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    #[error("任务 {0} 不存在")]
    UnknownTask(TaskId),
    #[error("非法状态转移: 任务 {task} {from:?} -> {to:?} ({reason})")]
    Transition {
        task: TaskId,
        from: TaskStatus,
        to: TaskStatus,
        reason: String,
    },
}

/// 任务状态管理器:统一校验并执行状态转移,线程安全。
///
/// 所有状态变更必须经过本管理器,以保证状态机的纪律与并发安全。
/// 状态由管理器持有(运行时态,不持久化),按 TaskId 键。
pub struct TaskStateManager {
    statuses: Mutex<HashMap<TaskId, TaskStatus>>,
}

impl TaskStateManager {
    pub fn new() -> Self {
        Self {
            statuses: Mutex::new(HashMap::new()),
        }
    }

    /// 加载配置时重置运行时状态:enabled→READY / disabled→DISABLED
    /// (防上次异常退出锁死状态)。
    pub fn reset(&self, id: TaskId, enabled: bool) {
        let status = if enabled {
            TaskStatus::Ready
        } else {
            TaskStatus::Disabled
        };
        self.statuses.lock().unwrap().insert(id, status);
    }

    pub fn forget(&self, id: TaskId) {
        self.statuses.lock().unwrap().remove(&id);
    }

    /// 直接写入状态(不经转移表校验)。
    ///
    /// 仅限编辑任务时保留运行族状态;
    /// 常规状态变更必须走 transition/finalize_status。
    pub fn set(&self, id: TaskId, status: TaskStatus) {
        self.statuses.lock().unwrap().insert(id, status);
    }

    /// 获取任务当前状态(线程安全);任务未注册时返回 None。
    pub fn get_status(&self, id: TaskId) -> Option<TaskStatus> {
        self.statuses.lock().unwrap().get(&id).copied()
    }

    fn current_of(
        &self,
        statuses: &mut HashMap<TaskId, TaskStatus>,
        id: TaskId,
    ) -> Result<TaskStatus, StateError> {
        statuses
            .get(&id)
            .copied()
            .ok_or(StateError::UnknownTask(id))
    }

    /// 尝试将任务转移到 new_status。
    ///
    /// 同状态视为成功;非法转移拒绝返回 Err 并记 warning,不抛 panic。
    /// 返回转移后的实际状态。
    pub fn transition(
        &self,
        id: TaskId,
        new_status: TaskStatus,
        reason: &str,
    ) -> Result<TaskStatus, StateError> {
        let mut statuses = self.statuses.lock().unwrap();
        let current = self.current_of(&mut statuses, id)?;
        if current == new_status {
            return Ok(current);
        }
        if !allowed_transitions(current).contains(&new_status) {
            tracing::warn!(
                "[状态机] 非法转移被拒绝: {id} {current:?} -> {new_status:?} ({reason})"
            );
            return Err(StateError::Transition {
                task: id,
                from: current,
                to: new_status,
                reason: reason.to_string(),
            });
        }
        statuses.insert(id, new_status);
        tracing::info!("[状态机] {id}: {current:?} -> {new_status:?} ({reason})");
        Ok(new_status)
    }

    /// 任务执行收尾的集中归一化:把状态落到 DISABLED 或 READY。
    ///
    /// 规则:
    /// - FAILED:保留失败状态,不覆盖。
    /// - 运行族(RUNNING/RETRYING/STOPPING):先转 READY,再按 enabled 决定
    ///   是否转 DISABLED;若已在 READY 则跳过第一步。
    /// - READY/DISABLED:按 enabled 归一化(enabled→READY,disabled→DISABLED)。
    ///
    /// 所有转移均检查结果,被拒时记录 warning 而非静默忽略,返回当前状态。
    pub fn finalize_status(&self, id: TaskId, enabled: bool, reason: &str) -> Option<TaskStatus> {
        let mut statuses = self.statuses.lock().unwrap();
        let mut current = self.current_of(&mut statuses, id).ok()?;

        // 失败是终态,收尾不覆盖
        if current == TaskStatus::Failed {
            return Some(current);
        }

        let target = if enabled {
            TaskStatus::Ready
        } else {
            TaskStatus::Disabled
        };

        // 第一步:运行族先回 READY
        if is_running_family(current) && current != TaskStatus::Ready {
            if !allowed_transitions(current).contains(&TaskStatus::Ready) {
                tracing::warn!(
                    "[状态机] 收尾归一化第一步被拒: {id} {current:?} -> Ready ({reason})"
                );
                return Some(current);
            }
            statuses.insert(id, TaskStatus::Ready);
            tracing::info!("[状态机] {id}: {current:?} -> Ready ({reason})");
            current = TaskStatus::Ready;
        }

        // 第二步:按 enabled 在 READY/DISABLED 间归一化
        if current == target {
            return Some(current);
        }
        if !allowed_transitions(current).contains(&target) {
            tracing::warn!(
                "[状态机] 收尾归一化第二步被拒: {id} {current:?} -> {target:?} ({reason})"
            );
            return Some(current);
        }
        statuses.insert(id, target);
        tracing::info!("[状态机] {id}: {current:?} -> {target:?} ({reason})");
        Some(target)
    }
}

impl Default for TaskStateManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! 状态转移、非法操作与收尾归一化的单元测试。

    use super::*;

    const T1: TaskId = TaskId(1);

    /// 构造指定状态和启用标志的任务。
    fn setup(mgr: &TaskStateManager, status: TaskStatus, enabled: bool) {
        mgr.reset(T1, enabled);
        // 直接写入初始状态,构造测试前置条件
        *mgr.statuses.lock().unwrap().get_mut(&T1).unwrap() = status;
    }

    // ---- transition 基本行为 ----

    #[test]
    fn test_transition_allowed() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Ready, true);
        assert_eq!(
            mgr.transition(T1, TaskStatus::Running, "go").ok(),
            Some(TaskStatus::Running)
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Running));
    }

    #[test]
    fn test_transition_forbidden_running_to_disabled() {
        // 运行族不可直接跳 DISABLED(必须经 READY 归一化)
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, true);
        assert!(mgr.transition(T1, TaskStatus::Disabled, "直跳").is_err());
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Running));
    }

    #[test]
    fn test_transition_forbidden_disabled_to_running() {
        // DISABLED -> RUNNING 非法(必须先 READY)
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, false);
        assert!(mgr.transition(T1, TaskStatus::Running, "直跳").is_err());
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Disabled));
    }

    #[test]
    fn test_transition_same_state_is_success() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, true);
        assert!(mgr.transition(T1, TaskStatus::Running, "").is_ok());
    }

    // ---- finalize_status:运行族两步归一化 ----

    #[test]
    fn test_finalize_running_enabled_lands_ready() {
        // RUNNING + enabled=True -> 先 READY,第二步 target==READY 不再转
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, true);
        assert_eq!(
            mgr.finalize_status(T1, true, "结束"),
            Some(TaskStatus::Ready)
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Ready));
    }

    #[test]
    fn test_finalize_running_disabled_lands_disabled() {
        // RUNNING + enabled=False -> 先 READY,再 READY -> DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, false);
        assert_eq!(
            mgr.finalize_status(T1, false, "结束"),
            Some(TaskStatus::Disabled)
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Disabled));
    }

    #[test]
    fn test_finalize_stopping_disabled_lands_disabled() {
        // STOPPING + enabled=False -> 先 STOPPING -> READY,再 READY -> DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Stopping, false);
        assert_eq!(
            mgr.finalize_status(T1, false, "停止完成"),
            Some(TaskStatus::Disabled)
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Disabled));
    }

    #[test]
    fn test_finalize_retrying_lands_ready_or_disabled() {
        // RETRYING + enabled=True -> READY;enabled=False -> DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Retrying, true);
        assert_eq!(mgr.finalize_status(T1, true, ""), Some(TaskStatus::Ready));
        setup(&mgr, TaskStatus::Retrying, false);
        assert_eq!(
            mgr.finalize_status(T1, false, ""),
            Some(TaskStatus::Disabled)
        );
    }

    // ---- finalize_status:FAILED 终态保留 ----

    #[test]
    fn test_finalize_failed_is_preserved() {
        // FAILED 是终态,收尾不应覆盖(无论 enabled 与否)
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Failed, true);
        assert_eq!(
            mgr.finalize_status(T1, true, "结束"),
            Some(TaskStatus::Failed)
        );
        setup(&mgr, TaskStatus::Failed, false);
        assert_eq!(
            mgr.finalize_status(T1, false, "结束"),
            Some(TaskStatus::Failed)
        );
    }

    // ---- finalize_status:非运行族按 enabled 归一化 ----

    #[test]
    fn test_finalize_ready_disabled_lands_disabled() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Ready, false);
        assert_eq!(
            mgr.finalize_status(T1, false, ""),
            Some(TaskStatus::Disabled)
        );
    }

    #[test]
    fn test_finalize_ready_enabled_stays_ready() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Ready, true);
        assert_eq!(mgr.finalize_status(T1, true, ""), Some(TaskStatus::Ready));
    }

    #[test]
    fn test_finalize_disabled_enabled_lands_ready() {
        // DISABLED + enabled=True -> DISABLED -> READY(表允许)
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, false);
        assert_eq!(mgr.finalize_status(T1, true, ""), Some(TaskStatus::Ready));
    }

    #[test]
    fn test_finalize_disabled_stays_disabled() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, false);
        assert_eq!(
            mgr.finalize_status(T1, false, ""),
            Some(TaskStatus::Disabled)
        );
    }

    // ---- finalize_status:转移被拒不静默 ----

    #[test]
    fn test_finalize_returns_current_when_step1_rejected() {
        // DISABLED 不在运行族,跳过第一步直接第二步;target=DISABLED==current,返回 DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, false);
        assert_eq!(
            mgr.finalize_status(T1, false, ""),
            Some(TaskStatus::Disabled)
        );
    }

    #[test]
    fn test_transition_rejected_does_not_mutate() {
        // 非法转移被拒时不应改变状态
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, true);
        assert!(mgr.transition(T1, TaskStatus::Disabled, "").is_err());
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Running));
    }

    // ---- 禁用任务与运行收尾场景 ----

    #[test]
    fn test_disabled_cannot_start_running() {
        // 对禁用任务点"立即运行":DISABLED -> RUNNING 被拒,不起进程
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, false);
        let started = mgr.transition(T1, TaskStatus::Running, "开始执行");
        assert!(started.is_err(), "DISABLED -> RUNNING 必须被拒");
        assert_eq!(
            mgr.get_status(T1),
            Some(TaskStatus::Disabled),
            "状态不应被改变"
        );
    }

    #[test]
    fn test_early_return_does_not_normalize() {
        // 守卫拒绝起进程后,不应再调用 finalize_status 改动状态
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Disabled, true);
        let started = mgr.transition(T1, TaskStatus::Running, "开始执行").is_ok();
        assert!(!started);
        // 模拟"仅 started 才收尾归一化"
        if started {
            mgr.finalize_status(T1, true, "执行结束");
        }
        assert_eq!(
            mgr.get_status(T1),
            Some(TaskStatus::Disabled),
            "未起进程不应改动状态"
        );
    }

    #[test]
    fn test_stopping_can_reach_disabled_via_finalize() {
        // 对"运行中被禁用"的任务点停止:RUNNING -> STOPPING -> 经 finalize -> DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, false);
        assert!(
            mgr.transition(T1, TaskStatus::Stopping, "用户请求停止")
                .is_ok()
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Stopping));
        // 单步从 STOPPING 跳到 DISABLED 会被拒,必须经过收尾归一化
        assert!(mgr.transition(T1, TaskStatus::Disabled, "直跳").is_err());
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Stopping));
        // 通过 finalize_status 两步归一化 -> 落 DISABLED
        assert_eq!(
            mgr.finalize_status(T1, false, "停止完成"),
            Some(TaskStatus::Disabled)
        );
        assert_eq!(mgr.get_status(T1), Some(TaskStatus::Disabled));
    }

    #[test]
    fn test_running_disabled_natural_end_keeps_disabled() {
        // 运行中被禁用后自然结束(不经 STOPPING):finalize 应落 DISABLED
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Running, false);
        assert_eq!(
            mgr.finalize_status(T1, false, "执行结束"),
            Some(TaskStatus::Disabled)
        );
    }

    // ---- 转移表完整性自检 ----

    #[test]
    fn test_running_family_cannot_directly_reach_disabled() {
        for s in RUNNING_FAMILY {
            assert!(
                !allowed_transitions(s).contains(&TaskStatus::Disabled),
                "{s:?} 不应允许直接跳 DISABLED"
            );
        }
    }

    #[test]
    fn test_running_family_can_reach_ready() {
        // 运行族应能回到 READY(finalize_status 第一步依赖此)
        for s in RUNNING_FAMILY {
            assert!(
                allowed_transitions(s).contains(&TaskStatus::Ready),
                "{s:?} 应允许跳 READY 作为收尾第一步"
            );
        }
    }

    // ---- QUEUED 并发排队语义 ----

    #[test]
    fn test_queued_is_a_first_class_status() {
        // 队列中的任务尚未占用执行槽,但状态必须可被 UI/CLI 观察。
        assert_eq!(TaskStatus::Queued.label(), "排队中");
        assert!(!is_running_family(TaskStatus::Queued));
    }

    #[test]
    fn test_ready_can_enter_queue_and_queue_can_start() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Ready, true);
        assert_eq!(
            mgr.transition(T1, TaskStatus::Queued, "并发槽已满").ok(),
            Some(TaskStatus::Queued)
        );
        assert_eq!(
            mgr.transition(T1, TaskStatus::Running, "队首获得并发槽")
                .ok(),
            Some(TaskStatus::Running)
        );
    }

    #[test]
    fn test_queued_finalize_normalizes_by_enabled() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Queued, true);
        assert_eq!(
            mgr.finalize_status(T1, true, "队列项取消"),
            Some(TaskStatus::Ready)
        );
        setup(&mgr, TaskStatus::Queued, false);
        assert_eq!(
            mgr.finalize_status(T1, false, "队列项取消"),
            Some(TaskStatus::Disabled)
        );
    }

    #[test]
    fn test_failed_task_can_queue_for_another_run() {
        let mgr = TaskStateManager::new();
        setup(&mgr, TaskStatus::Failed, true);
        assert_eq!(
            mgr.transition(T1, TaskStatus::Queued, "重新触发等待资源")
                .ok(),
            Some(TaskStatus::Queued)
        );
    }
}
