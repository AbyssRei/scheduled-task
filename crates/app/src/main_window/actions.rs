//! 主窗口任务操作与确认对话框。

use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::{DialogDescription, DialogFooter};
use gpui_kit::component::{WindowExt as _, v_flex};
use gpui_kit::{App, ParentElement as _, Styled, px};
use scheduled_task_core::model::TaskId;
use scheduled_task_core::state::{TaskStatus, is_running_family};

use super::TaskListView;
use crate::task_dialog::TaskDialogView;

fn show_task_error(message: String, window: &mut gpui_kit::Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .title("保存失败")
            .child(DialogDescription::new().child(message.clone()))
            .show_cancel(false)
    });
}

fn dismiss_footer() -> DialogFooter {
    DialogFooter::new().child(
        Button::new("btn-dismiss")
            .label("知道了")
            .on_click(|_, window, cx| window.close_dialog(cx)),
    )
}

impl TaskListView {
    fn require_selected(&self, window: &mut gpui_kit::Window, cx: &mut App) -> Option<TaskId> {
        if self.selected.is_none() {
            window.open_alert_dialog(cx, |alert, _, _| {
                alert
                    .title("未选择任务")
                    .child(DialogDescription::new().child("请先在表格中选择一个任务"))
                    .show_cancel(false)
            });
        }
        self.selected
    }

    pub(super) fn open_add(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        TaskDialogView::open_new(self.core.clone(), window, cx);
    }

    pub(super) fn open_edit(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        let Some(id) = self.require_selected(window, cx) else {
            return;
        };
        if let Some(task) = self.core.tasks().into_iter().find(|t| t.id == id) {
            TaskDialogView::open_edit(self.core.clone(), task, window, cx);
        }
    }

    pub(super) fn delete_selected(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        let Some(id) = self.require_selected(window, cx) else {
            return;
        };
        let core = self.core.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let core = core.clone();
            alert
                .title("确认删除")
                .child(
                    DialogDescription::new()
                        .child("删除后配置立即保存,且不可恢复。确认删除该任务?"),
                )
                .show_cancel(true)
                .on_ok(move |_, window, cx| match core.delete(id) {
                    Ok(()) => true,
                    Err(error) => {
                        show_task_error(error, window, cx);
                        false
                    }
                })
        });
    }

    pub(super) fn toggle_selected(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        let Some(id) = self.require_selected(window, cx) else {
            return;
        };
        if let Err(error) = self.core.toggle_enabled(id) {
            show_task_error(error, window, cx);
        }
    }

    pub(super) fn run_selected(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        let Some(id) = self.require_selected(window, cx) else {
            return;
        };
        let Some(task) = self.core.tasks().into_iter().find(|t| t.id == id) else {
            return;
        };
        if !task.enabled {
            window.open_alert_dialog(cx, |alert, _, _| {
                alert
                    .title("任务已禁用")
                    .child(DialogDescription::new().child("请先启用任务,再提交执行请求"))
                    .show_cancel(false)
            });
            return;
        }
        // 确认执行
        let core = self.core.clone();
        let name = task.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let core = core.clone();
            let name = name.clone();
            alert
                .title("确认执行")
                .child(DialogDescription::new().child(format!("确定要执行任务 '{name}' 吗? 当前任务或并发资源忙时会进入等待队列;已有等待项时不重复添加。")))
                .show_cancel(true)
                .on_ok(move |_, _, _| {
                    core.run_now(id);
                    true
                })
        });
    }

    pub(super) fn stop_selected(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        let Some(id) = self.require_selected(window, cx) else {
            return;
        };
        let Some(task) = self.core.tasks().into_iter().find(|t| t.id == id) else {
            return;
        };
        // 排队中的任务也可停止:取消等待项,不启动进程。
        let status = self.core.status_of(id);
        if !status.is_some_and(|status| is_running_family(status) || status == TaskStatus::Queued) {
            window.open_alert_dialog(cx, |alert, _, _| {
                alert
                    .title("未在运行")
                    .child(DialogDescription::new().child("任务当前未运行也未排队,无需停止"))
                    .show_cancel(false)
            });
            return;
        }
        // 确认停止
        let core = self.core.clone();
        let name = task.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let core = core.clone();
            let name = name.clone();
            alert
                .title("确认停止")
                .child(DialogDescription::new().child(format!(
                    "确定要停止任务 '{name}' 吗? 正在运行的进程将终止,该任务的等待项也会取消。"
                )))
                .show_cancel(true)
                .on_ok(move |_, _, _| {
                    core.stop(id);
                    true
                })
        });
    }

    pub(super) fn show_help(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title("帮助")
                .w(px(520.))
                .child(
                    v_flex()
                        .flex_shrink_0()
                        .gap_2()
                        .child("定时任务管理器(Rust/GPUI 版)")
                        .child("· 每天:按 HH:MM:SS 多时间点触发")
                        .child("· 每月:指定日 + 时间点,当月无该日则跳过")
                        .child("· 自动重试:输出窗口内连续命中同一模式即重试")
                        .child("· 超时:分钟为单位,超时强制终止进程树")
                        .child("· 最大并行数:在设置中调整,超出上限的任务自动排队")
                        .child("· 并发策略:任务串行或同组互斥;同一任务不会重叠运行")
                        .child("· 等待队列:优先启动最早可执行的任务,每个任务最多保留一次等待")
                        .child("· 停止任务:终止当前执行并取消该任务的等待项")
                        .child("· 重试冷却期间继续占用并发名额;禁用或删除会取消等待项")
                        .child("· 关闭窗口:隐藏到托盘;普通退出请用托盘菜单")
                        .child("· 管理器保活:在设置中开启,崩溃或普通退出后自动恢复")
                        .child("· 彻底退出:使用托盘菜单“关闭保活并退出”"),
                )
                .footer(dismiss_footer())
        });
    }

    pub(super) fn show_about(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title("关于")
                .w(px(520.))
                .child(
                        v_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .child("定时任务管理器")
                            .child("一款基于 Rust 与 GPUI 的 Windows 桌面定时任务管理工具。")
                            .child("支持每天、每月及自定义多时间点调度，并提供自动重试、超时终止、并发限制和排队执行。")
                            .child("作者：AbyssRei <AbyssRei@outlook.com>"),
                )
                .footer(dismiss_footer())
        });
    }
}
