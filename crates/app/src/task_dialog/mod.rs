//! 添加/编辑任务对话框。
//!
//! 全字段表单:名称、脚本类型(下拉)、程序路径(浏览=GPUI 内置文件对话框)、
//! 调度类型(每天/每月/自定义)+多时间点+monthly 日期、手工命令(Manual)、
//! 命令行参数、工作目录、环境变量及 `.env` 文件、
//! 自动重试(模式/窗口/最大次数)、超时(分钟)、启用开关。
//! 范围校验:window 1–100、max_retries 1–10、
//! timeout 1–300、day 1–31。

mod form;
mod layout;

use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::combobox::{ComboboxEvent, ComboboxState};
use gpui_kit::component::dialog::DialogDescription;
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::{App, AppContext as _, Context, Entity, ParentElement as _, PathPromptOptions, px};
use scheduled_task_core::model::{Schedule, ScriptKind, ScriptSource, TaskConfig, TaskId};

use crate::bridge::AppCore;
use crate::widgets::{PickDelegate, PickItem};
use form::{
    CONCURRENCY_OPTIONS, FormValues, KIND_OPTIONS, SCHEDULE_OPTIONS, concurrency_fields,
    ix_from_kind,
};

/// 表单视图(对话框内容)。
pub struct TaskDialogView {
    core: Arc<AppCore>,
    editing: Option<TaskConfig>,
    handle: gpui_kit::AnyWindowHandle,

    name: Entity<InputState>,
    path: Entity<InputState>,
    kind_state: Entity<ComboboxState<PickDelegate>>,
    kind_ix: usize,
    manual_cmd: Entity<InputState>,
    args: Entity<TextareaState>,
    working_dir: Entity<InputState>,
    env: Entity<TextareaState>,
    env_file: Entity<InputState>,
    schedule_state: Entity<ComboboxState<PickDelegate>>,
    schedule_ix: usize,
    times: Entity<InputState>,
    day: Entity<InputState>,
    retry_on: bool,
    /// 重试模式:每行一个候选模式,用多行 Textarea 承载(Enter=换行,不触发对话框确认)
    pattern: Entity<TextareaState>,
    window_n: Entity<InputState>,
    max_retries: Entity<InputState>,
    timeout_on: bool,
    timeout_min: Entity<InputState>,
    enabled: bool,
    concurrency_state: Entity<ComboboxState<PickDelegate>>,
    concurrency_ix: usize,
    concurrency_group: Entity<InputState>,
}

impl TaskDialogView {
    fn new_input(
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
        placeholder: &str,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_placeholder(placeholder, window, cx);
            state
        })
    }

    fn new_textarea(
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
        placeholder: &str,
    ) -> Entity<TextareaState> {
        cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_placeholder(placeholder, window, cx);
            state
        })
    }

    fn pick_state(
        options: &'static [&'static str],
        selected: usize,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> Entity<ComboboxState<PickDelegate>> {
        cx.new(|cx| {
            ComboboxState::new(
                PickDelegate {
                    items: options
                        .iter()
                        .enumerate()
                        .map(|(value, label)| PickItem {
                            label: label.to_string(),
                            value,
                        })
                        .collect(),
                },
                vec![gpui_kit::component::IndexPath::new(selected)],
                window,
                cx,
            )
        })
    }

    /// 打开"添加任务"对话框。
    pub fn open_new(core: Arc<AppCore>, window: &mut gpui_kit::Window, cx: &mut App) {
        let handle = window.window_handle();
        let view = cx.new(|cx| Self::form(core, None, handle, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("添加任务")
                .w(px(640.))
                .footer(Self::actions(view.clone()))
                // 使用对话框内置的限高滚动区，避免长表单超出窗口。
                .child(view)
        });
    }

    /// 打开"编辑任务"对话框。
    pub fn open_edit(
        core: Arc<AppCore>,
        task: TaskConfig,
        window: &mut gpui_kit::Window,
        cx: &mut App,
    ) {
        let handle = window.window_handle();
        let view = cx.new(|cx| Self::form(core, Some(task), handle, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("编辑任务")
                .w(px(640.))
                .footer(Self::actions(view.clone()))
                .child(view)
        });
    }

    fn form(
        core: Arc<AppCore>,
        editing: Option<TaskConfig>,
        handle: gpui_kit::AnyWindowHandle,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = editing.as_ref();
        let kind_ix = t.map_or(0, |t| ix_from_kind(&t.script.kind));
        let (concurrency_ix, group) =
            t.map_or((0, String::new()), |t| concurrency_fields(&t.concurrency));
        let schedule_ix = match t.map(|t| &t.schedule) {
            Some(Schedule::Monthly { .. }) => 1,
            Some(Schedule::Custom { .. }) => 2,
            _ => 0,
        };
        let (retry_on, timeout_on, enabled) = t.map_or((false, false, true), |t| {
            (t.retry.is_some(), t.timeout.is_some(), t.enabled)
        });

        let name = Self::new_input(window, cx, "任务名称");
        let path = Self::new_input(window, cx, "脚本绝对路径(如 E:\\work\\run.bat)");
        let manual_cmd = Self::new_input(window, cx, "手工命令,{script} 代表脚本路径");
        let args = Self::new_textarea(window, cx, "每行一个参数,空行忽略");
        let working_dir = Self::new_input(window, cx, "留空则使用脚本所在目录");
        let env = Self::new_textarea(window, cx, "每行一个 KEY=VALUE,空行和 # 注释忽略");
        let env_file = Self::new_input(window, cx, ".env 文件路径(可选)");
        let times = Self::new_input(window, cx, "多个时间用逗号分隔,如 08:00:00,12:30:00");
        let day = Self::new_input(window, cx, "1–31");
        let pattern = Self::new_textarea(window, cx, "每行一个候选模式;留空=核心内容一致");
        let window_n = Self::new_input(window, cx, "连续次数(1–100,默认 5)");
        let max_retries = Self::new_input(window, cx, "最大重试次数(1–10,默认 3)");
        let timeout_min = Self::new_input(window, cx, "分钟(1–300)");
        let concurrency_group = Self::new_input(window, cx, "同组任务共用一个组名");
        concurrency_group.update(cx, |st, cx| st.set_value(group, window, cx));

        if let Some(t) = t {
            name.update(cx, |st, cx| st.set_value(t.name.clone(), window, cx));
            if let ScriptSource::File(p) = &t.script.source {
                path.update(cx, |st, cx| st.set_value(p.clone(), window, cx));
            }
            if let ScriptKind::Manual { command } = &t.script.kind {
                manual_cmd.update(cx, |st, cx| st.set_value(command.clone(), window, cx));
            }
            args.update(cx, |st, cx| {
                st.set_value(t.script.args.join("\n"), window, cx)
            });
            if let Some(value) = &t.script.working_dir {
                working_dir.update(cx, |st, cx| st.set_value(value.clone(), window, cx));
            }
            if !t.script.env.is_empty() {
                let text = t
                    .script
                    .env
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                env.update(cx, |st, cx| st.set_value(text, window, cx));
            }
            if let Some(value) = &t.script.env_file {
                env_file.update(cx, |st, cx| st.set_value(value.clone(), window, cx));
            }
            times.update(cx, |st, cx| {
                let joined = t
                    .schedule
                    .times()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                st.set_value(joined, window, cx);
            });
            if let Some(day_val) = t.schedule.monthly_day() {
                day.update(cx, |st, cx| {
                    st.set_value(day_val.get().to_string(), window, cx)
                });
            }
            if let Some(r) = &t.retry {
                pattern.update(cx, |st, cx| st.set_value(r.pattern.clone(), window, cx));
                window_n.update(cx, |st, cx| st.set_value(r.window.to_string(), window, cx));
                max_retries.update(cx, |st, cx| {
                    st.set_value(r.max_retries.to_string(), window, cx)
                });
            }
            if let Some(d) = t.timeout {
                let minutes = d.as_secs() / 60;
                timeout_min.update(cx, |st, cx| st.set_value(minutes.to_string(), window, cx));
            }
        }

        let kind_state = Self::pick_state(KIND_OPTIONS, kind_ix, window, cx);
        let schedule_state = Self::pick_state(SCHEDULE_OPTIONS, schedule_ix, window, cx);
        let concurrency_state = Self::pick_state(CONCURRENCY_OPTIONS, concurrency_ix, window, cx);
        cx.subscribe_in(
            &concurrency_state,
            window,
            |this, _, event: &ComboboxEvent<PickDelegate>, _window, cx| {
                if let Some(v) = event_values(event).first() {
                    this.concurrency_ix = *v;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &kind_state,
            window,
            |this, _, event: &ComboboxEvent<PickDelegate>, _window, cx| {
                if let Some(v) = event_values(event).first() {
                    this.kind_ix = *v;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &schedule_state,
            window,
            |this, _, event: &ComboboxEvent<PickDelegate>, _window, cx| {
                if let Some(v) = event_values(event).first() {
                    this.schedule_ix = *v;
                    cx.notify();
                }
            },
        )
        .detach();

        Self {
            core,
            editing,
            handle,
            name,
            path,
            kind_state,
            kind_ix,
            manual_cmd,
            args,
            working_dir,
            env,
            env_file,
            schedule_state,
            schedule_ix,
            times,
            day,
            retry_on,
            pattern,
            window_n,
            max_retries,
            timeout_on,
            timeout_min,
            enabled,
            concurrency_state,
            concurrency_ix,
            concurrency_group,
        }
    }

    /// 校验并构造 TaskConfig;错误信息为中文,弹 alert 提示。
    fn save(&mut self, window: &mut gpui_kit::Window, cx: &mut App) {
        match self.collect_config(cx) {
            Ok(task) => {
                if self.editing.is_some() {
                    self.core.update(task);
                } else {
                    self.core.add(task);
                }
                window.close_dialog(cx);
            }
            Err(msg) => {
                let msg = msg.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let msg = msg.clone();
                    alert
                        .title("校验失败")
                        .child(DialogDescription::new().child(msg))
                        .show_cancel(false)
                });
            }
        }
    }

    fn collect_config(&self, cx: &App) -> Result<TaskConfig, String> {
        let values = FormValues {
            concurrency_ix: self.concurrency_ix,
            concurrency_group: self.concurrency_group.read(cx).value().to_string(),
            name: self.name.read(cx).value().to_string(),
            path: self.path.read(cx).value().to_string(),
            kind_ix: self.kind_ix,
            manual_cmd: self.manual_cmd.read(cx).value().to_string(),
            args: self.args.read(cx).value().to_string(),
            working_dir: self.working_dir.read(cx).value().to_string(),
            env: self.env.read(cx).value().to_string(),
            env_file: self.env_file.read(cx).value().to_string(),
            schedule_ix: self.schedule_ix,
            times: self.times.read(cx).value().to_string(),
            day: self.day.read(cx).value().to_string(),
            retry_on: self.retry_on,
            pattern: self.pattern.read(cx).value().to_string(),
            window_n: self.window_n.read(cx).value().to_string(),
            max_retries: self.max_retries.read(cx).value().to_string(),
            timeout_on: self.timeout_on,
            timeout_min: self.timeout_min.read(cx).value().to_string(),
            enabled: self.enabled,
        };
        values.into_config(self.editing.as_ref().map_or(TaskId(0), |task| task.id))
    }

    fn browse(&mut self, _window: &mut gpui_kit::Window, cx: &mut App) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择脚本文件".into()),
        });
        let path_input = self.path.clone();
        let handle = self.handle;
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.first()
            {
                let text = p.to_string_lossy().into_owned();
                let _ = handle.update(cx, |_, window, cx| {
                    path_input.update(cx, |st, cx| st.set_value(text, window, cx));
                });
            }
        })
        .detach();
    }

    fn browse_working_dir(&mut self, cx: &mut App) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择工作目录".into()),
        });
        let path_input = self.working_dir.clone();
        let handle = self.handle;
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.first()
            {
                let text = p.to_string_lossy().into_owned();
                let _ = handle.update(cx, |_, window, cx| {
                    path_input.update(cx, |st, cx| st.set_value(text, window, cx));
                });
            }
        })
        .detach();
    }

    fn browse_env_file(&mut self, cx: &mut App) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择 .env 文件".into()),
        });
        let path_input = self.env_file.clone();
        let handle = self.handle;
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.first()
            {
                let text = p.to_string_lossy().into_owned();
                let _ = handle.update(cx, |_, window, cx| {
                    path_input.update(cx, |st, cx| st.set_value(text, window, cx));
                });
            }
        })
        .detach();
    }
}

fn event_values(event: &ComboboxEvent<PickDelegate>) -> Vec<usize> {
    match event {
        ComboboxEvent::Change(v) | ComboboxEvent::Confirm(v) => v.clone(),
    }
}
