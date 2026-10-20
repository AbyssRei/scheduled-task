//! 任务表格的行模型、展示转换与列渲染。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{App, Context, IntoElement, ParentElement as _, Styled, div, px};
use scheduled_task_core::model::{ScriptKind, ScriptSource, TaskConfig, TaskId};
use scheduled_task_core::state::TaskStatus;

use crate::bridge::AppCore;

/// 表格行数据。
pub(super) struct Row {
    pub id: TaskId,
    pub name: String,
    pub script: String,
    pub times: String,
    pub status: TaskStatus,
    pub next_run: String,
    pub last_run: String,
    pub queue_reason: Option<String>,
}

/// 表格 delegate(数据由视图刷新)。
pub(super) struct TaskRows {
    pub rows: Vec<Row>,
}

impl TableDelegate for TaskRows {
    fn columns_count(&self, _: &App) -> usize {
        7
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("id", "ID").width(px(56.)),
            1 => Column::new("name", "任务名称").width(px(180.)),
            2 => Column::new("script", "脚本").width(px(260.)),
            3 => Column::new("times", "执行时间").width(px(220.)),
            4 => Column::new("status", "状态 / 等待原因").width(px(230.)),
            5 => Column::new("next", "下次计划（本地）").width(px(190.)),
            _ => Column::new("last", "最近执行结果").width(px(240.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut gpui_kit::Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div();
        };
        let text: String = match col_ix {
            0 => row.id.0.to_string(),
            1 => row.name.clone(),
            2 => row.script.clone(),
            3 => row.times.clone(),
            4 => row
                .queue_reason
                .as_ref()
                .map(|reason| format!("{} · {reason}", row.status.label()))
                .unwrap_or_else(|| row.status.label().to_string()),
            5 => row.next_run.clone(),
            _ => row.last_run.clone(),
        };
        let color = if col_ix == 4 {
            match row.status {
                TaskStatus::Running | TaskStatus::Retrying => cx.theme().green,
                TaskStatus::Failed => cx.theme().danger,
                TaskStatus::Stopping => cx.theme().warning,
                TaskStatus::Disabled => cx.theme().muted_foreground,
                TaskStatus::Queued => cx.theme().warning,
                TaskStatus::Ready => cx.theme().foreground,
            }
        } else {
            cx.theme().foreground
        };
        div().text_color(color).child(text)
    }
}

pub(super) fn build_rows(core: &AppCore) -> Vec<Row> {
    core.tasks()
        .into_iter()
        .map(|t: TaskConfig| Row {
            next_run: core
                .next_fire(t.id)
                .map(|time| time.to_string().replace('T', " "))
                .unwrap_or_else(|| "—".into()),
            last_run: core
                .last_run(t.id)
                .map(|record| {
                    format!(
                        "{} · {:.2} 秒 · 退出码 {}",
                        if record.stopped {
                            "已停止"
                        } else if record.succeeded() {
                            "成功"
                        } else {
                            "失败"
                        },
                        record.duration.as_secs_f64(),
                        record.returncode
                    )
                })
                .unwrap_or_else(|| "尚未执行".into()),
            queue_reason: core.queue_reason(t.id),
            id: t.id,
            times: t
                .schedule
                .times()
                .iter()
                .map(|time| time.to_string())
                .collect::<Vec<_>>()
                .join(" / "),
            script: script_display(&t),
            status: core.status_of(t.id).unwrap_or(if t.enabled {
                TaskStatus::Ready
            } else {
                TaskStatus::Disabled
            }),
            name: t.name,
        })
        .collect()
}

fn script_display(t: &TaskConfig) -> String {
    let kind = match &t.script.kind {
        ScriptKind::Auto => "自动",
        ScriptKind::Cmd => "cmd",
        ScriptKind::PowerShell => "powershell",
        ScriptKind::Pwsh => "pwsh",
        ScriptKind::Python { .. } => "python",
        ScriptKind::Bash => "bash",
        ScriptKind::Sh => "sh",
        ScriptKind::Manual { .. } => "manual",
    };
    let source = match &t.script.source {
        ScriptSource::File(p) => p.clone(),
        ScriptSource::Inline(_) => "(内联脚本)".into(),
    };
    format!("[{kind}] {source}")
}
