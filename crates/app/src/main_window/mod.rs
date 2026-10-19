//! 主窗口组合与 daemon 事件分发。

mod actions;
mod log_panel;
mod table;
mod toolbar;

use std::sync::Arc;

use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled, div,
};
use scheduled_task_core::daemon::DaemonEvent;
use scheduled_task_core::model::TaskId;

use crate::bridge::AppCore;
use log_panel::LogPanel;
use table::{TaskRows, build_rows};

/// 主窗口视图。
pub struct TaskListView {
    core: Arc<AppCore>,
    table: Entity<TableState<TaskRows>>,
    selected: Option<TaskId>,
    log_panel: Entity<LogPanel>,
}

impl TaskListView {
    pub fn new(core: Arc<AppCore>, window: &mut gpui_kit::Window, cx: &mut Context<Self>) -> Self {
        let rows = build_rows(&core);
        let table = cx.new(|cx| TableState::new(TaskRows { rows }, window, cx));
        cx.subscribe_in(
            &table,
            window,
            |this, _state, event: &TableEvent, _window, _cx| {
                if let TableEvent::SelectRow(ix) = event {
                    this.selected = this.table.read(_cx).delegate().rows.get(*ix).map(|r| r.id);
                }
            },
        )
        .detach();
        Self {
            core,
            table,
            selected: None,
            log_panel: cx.new(|_| LogPanel::new()),
        }
    }

    fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let rows = build_rows(&self.core);
        self.table.update(cx, |state, cx| {
            *state.delegate_mut() = TaskRows { rows };
            state.refresh(cx);
        });
        cx.notify();
    }

    /// daemon 事件 → 视图更新(UI 线程,由 main 的事件循环驱动)。
    pub fn apply_event(&mut self, event: DaemonEvent, cx: &mut Context<Self>) {
        match event {
            DaemonEvent::ConfigChanged => {
                self.refresh_rows(cx);
                // 日志时区配置变更后,缓冲中的时间戳也使用新时区重新显示。
                self.log_panel.update(cx, |_, cx| cx.notify());
            }
            DaemonEvent::Status { id, status } => {
                self.table.update(cx, |state, _| {
                    if let Some(row) = state.delegate_mut().rows.iter_mut().find(|r| r.id == id) {
                        row.status = status;
                    }
                });
                cx.notify();
            }
            DaemonEvent::LogLine { id, line } => {
                self.append_log(format!("[{id}] {line}"), cx);
            }
            DaemonEvent::Finished {
                id,
                returncode,
                retry_triggered,
            } => {
                self.append_log(
                    format!("[{id}] 执行结束(返回码 {returncode},重试触发 {retry_triggered})"),
                    cx,
                );
            }
            DaemonEvent::AdmittedRejected { id, reason } => {
                self.append_log(format!("[{id}] 触发被拒: {reason}"), cx);
            }
            DaemonEvent::Queued { id } => {
                self.append_log(format!("[{id}] 已进入并发等待队列"), cx);
            }
        }
    }

    fn append_log(&mut self, line: String, cx: &mut Context<Self>) {
        self.log_panel
            .update(cx, |panel, cx| panel.push_log(line, cx));
    }
}

/// 主视图句柄(事件循环向视图派发事件用)。
struct MainViewHandle(Entity<TaskListView>);
impl gpui_kit::Global for MainViewHandle {}

/// 注册主视图(main.rs 打开窗口后调用)。
pub fn set_main_view(cx: &mut App, view: Entity<TaskListView>) {
    cx.set_global(MainViewHandle(view));
}

/// 保活恢复等应用级消息不伪装成任务日志。
pub fn append_system_log(cx: &mut App, message: String) {
    let view = cx.try_global::<MainViewHandle>().map(|h| h.0.clone());
    if let Some(view) = view {
        view.update(cx, |view, cx| {
            view.append_log(format!("[应用] {message}"), cx)
        });
    }
}

/// 向主视图派发 daemon 事件(视图未注册时静默丢弃)。
pub fn apply_event_to_view(cx: &mut App, event: DaemonEvent) {
    let view = cx.try_global::<MainViewHandle>().map(|h| h.0.clone());
    if let Some(view) = view {
        view.update(cx, |view, cx| view.apply_event(event, cx));
    }
}

impl Render for TaskListView {
    fn render(&mut self, _: &mut gpui_kit::Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .p_4()
            .gap_3()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table)))
            .child(self.log_panel.clone())
    }
}
