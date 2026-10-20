//! 添加/编辑任务对话框。
//!
//! 全字段表单:名称、脚本类型(下拉)、程序路径(浏览=GPUI 内置文件对话框)、
//! 调度类型(每天/每月/自定义)+多时间点+monthly 日期、手工命令(Manual)、
//! 命令行参数、工作目录、环境变量及 `.env` 文件、
//! 自动重试(模式/窗口/最大次数)、超时(分钟)、启用开关。
//! 范围校验:window 1–100、max_retries 1–10、
//! timeout 1–300、day 1–31。

mod entries;
mod feedback;
mod form;
mod layout;
mod records;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::combobox::{ComboboxEvent, ComboboxState};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{
    App, AppContext as _, Context, Entity, ParentElement as _, PathPromptOptions, ScrollAnchor,
    ScrollHandle, Styled, px,
};
use scheduled_task_core::model::{Schedule, ScriptKind, ScriptSource, TaskConfig, TaskId};

use crate::bridge::AppCore;
use crate::widgets::{PickDelegate, PickItem};
use entries::EnvInputs;
use feedback::{FormError, FormField, FormPage};
use form::{
    CONCURRENCY_OPTIONS, FormValues, KIND_OPTIONS, SCHEDULE_OPTIONS, concurrency_fields,
    ix_from_kind,
};
use records::Records;

/// 表单视图(对话框内容)。
pub struct TaskDialogView {
    core: Arc<AppCore>,
    editing: Option<TaskConfig>,
    handle: gpui_kit::AnyWindowHandle,
    page: FormPage,
    page_scrolls: [ScrollHandle; 4],
    error_scroll: Option<ScrollAnchor>,
    error: Option<FormError>,
    save_error: Option<String>,
    /// 当前拖动的条目；拖动预览显示时隐藏原条目，避免两份控件同时出现。
    dragging: Option<entries::ActiveRecordDrag>,
    record_lists: [Rc<Cell<entries::ListGeometry>>; 2],

    name: Entity<InputState>,
    path: Entity<InputState>,
    source_inline: bool,
    inline_content: Entity<TextareaState>,
    uv_project: Entity<InputState>,
    cooldown: Entity<InputState>,
    kind_state: Entity<ComboboxState<PickDelegate>>,
    kind_ix: usize,
    manual_cmd: Entity<InputState>,
    args: Entity<TextareaState>,
    working_dir: Entity<InputState>,
    env: Records<EnvInputs>,
    env_files: Records<Entity<InputState>>,
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
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_placeholder(placeholder, window, cx);
            state
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.draft_changed(cx);
            }
        })
        .detach();
        input
    }

    fn new_textarea(
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
        placeholder: &str,
    ) -> Entity<TextareaState> {
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_placeholder(placeholder, window, cx);
            state
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.draft_changed(cx);
            }
        })
        .detach();
        input
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
        Self::open(core, None, window, cx);
    }

    /// 打开"编辑任务"对话框。
    pub fn open_edit(
        core: Arc<AppCore>,
        task: TaskConfig,
        window: &mut gpui_kit::Window,
        cx: &mut App,
    ) {
        Self::open(core, Some(task), window, cx);
    }

    fn open(
        core: Arc<AppCore>,
        task: Option<TaskConfig>,
        window: &mut gpui_kit::Window,
        cx: &mut App,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let title = if task.is_some() {
            "编辑任务"
        } else {
            "添加任务"
        };
        let handle = window.window_handle();
        let view = cx.new(|cx| Self::form(core, task, handle, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let content = view.clone();
            let confirm = view.clone();
            dialog
                .title(title)
                .w(px(760.))
                .h(px(650.))
                .overlay_closable(false)
                .on_ok(move |_, window, cx| confirm.update(cx, |this, cx| this.save(window, cx)))
                .content(move |area, _, _| area.min_h_0().overflow_hidden().child(content.clone()))
        });
    }

    fn form(
        core: Arc<AppCore>,
        editing: Option<TaskConfig>,
        handle: gpui_kit::AnyWindowHandle,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() && this.dragging.take().is_some() {
                cx.stop_active_drag(window);
                cx.notify();
            }
        })
        .detach();
        cx.on_release_in(window, |this, window, cx| {
            if this.dragging.is_some() {
                cx.stop_active_drag(window);
            }
        })
        .detach();
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
        let inline_content = Self::new_textarea(window, cx, "输入内联脚本内容");
        let uv_project = Self::new_input(window, cx, "uv 项目目录(可选)");
        let cooldown = Self::new_input(window, cx, "重试冷却秒数，默认 2");
        let source_inline =
            t.is_some_and(|task| matches!(task.script.source, ScriptSource::Inline(_)));
        let manual_cmd = Self::new_input(window, cx, "手工命令,{script} 代表脚本路径");
        let args = Self::new_textarea(window, cx, "每行一个参数,空行忽略");
        let working_dir = Self::new_input(window, cx, "留空则使用脚本所在目录");
        let mut env = Records::default();
        let mut env_files = Records::default();
        if let Some(task) = t {
            env.insert_after(
                None,
                task.script
                    .ordered_env()
                    .into_iter()
                    .map(|(key, value)| Self::env_inputs(key.clone(), value.clone(), window, cx))
                    .collect::<Vec<_>>(),
            );
            env_files.insert_after(
                None,
                task.script
                    .env_file_paths()
                    .map(|path| Self::env_file_input(path.to_owned(), window, cx))
                    .collect::<Vec<_>>(),
            );
        }
        let times = Self::new_input(window, cx, "多个时间用逗号分隔,如 08:00:00,12:30:00");
        let day = Self::new_input(window, cx, "1–31");
        let pattern = Self::new_textarea(window, cx, "每行一个候选模式;留空=核心内容一致");
        let window_n = Self::new_input(window, cx, "连续次数(1–100,默认 5)");
        let max_retries = Self::new_input(window, cx, "最大重试次数(1–10,默认 3)");
        let timeout_min = Self::new_input(window, cx, "分钟(1–300)");
        let concurrency_group = Self::new_input(window, cx, "同组任务共用一个组名");
        concurrency_group.update(cx, |st, cx| st.set_value(group, window, cx));

        if let Some(t) = t {
            let values = FormValues::from_config(t);
            inline_content.update(cx, |state, cx| {
                state.set_value(values.inline_content, window, cx)
            });
            uv_project.update(cx, |state, cx| {
                state.set_value(values.uv_project, window, cx)
            });
            cooldown.update(cx, |state, cx| state.set_value(values.cooldown, window, cx));
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
                    this.draft_changed(cx);
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
                    this.draft_changed(cx);
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
                    this.draft_changed(cx);
                }
            },
        )
        .detach();

        Self {
            core,
            editing,
            handle,
            page: FormPage::Basics,
            page_scrolls: std::array::from_fn(|_| ScrollHandle::new()),
            error_scroll: None,
            error: None,
            save_error: None,
            dragging: None,
            record_lists: std::array::from_fn(|_| Rc::default()),
            name,
            path,
            source_inline,
            inline_content,
            uv_project,
            cooldown,
            kind_state,
            kind_ix,
            manual_cmd,
            args,
            working_dir,
            env,
            env_files,
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

    fn draft_changed(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.error_scroll = None;
        self.save_error = None;
        cx.notify();
    }

    /// 按全部页面校验，失败定位字段；返回是否可以关闭对话框。
    fn save(&mut self, window: &mut gpui_kit::Window, cx: &mut Context<Self>) -> bool {
        self.error = None;
        self.save_error = None;
        let task = match self.collect_config(cx) {
            Ok(task) => task,
            Err(error) => {
                self.page = error.field.page();
                let anchor = ScrollAnchor::for_handle(self.page_scrolls[self.page.index()].clone());
                self.error_scroll = Some(anchor.clone());
                // 等待目标页挂载并记录几何位置后，再滚动到错误字段。
                window.on_next_frame(move |window, cx| anchor.scroll_to(window, cx));
                self.focus_error(error.field, window, cx);
                self.error = Some(error);
                cx.notify();
                return false;
            }
        };
        // 原样编辑不落盘或重载任务，避免无意义地影响调度。
        if self.editing.as_ref() == Some(&task) {
            return true;
        }
        let result = if self.editing.is_some() {
            self.core.update(task)
        } else {
            self.core.add(task).map(|_| ())
        };
        match result {
            Ok(()) => true,
            Err(message) => {
                self.save_error = Some(format!("保存失败：{message}"));
                cx.notify();
                false
            }
        }
    }

    fn focus_error(&self, field: FormField, window: &mut gpui_kit::Window, cx: &mut Context<Self>) {
        let input = match field {
            FormField::Name => Some(&self.name),
            FormField::Path => Some(&self.path),
            FormField::ManualCommand => Some(&self.manual_cmd),
            FormField::Times => Some(&self.times),
            FormField::Day => Some(&self.day),
            FormField::Group => Some(&self.concurrency_group),
            FormField::RetryWindow => Some(&self.window_n),
            FormField::MaxRetries => Some(&self.max_retries),
            FormField::Cooldown => Some(&self.cooldown),
            FormField::Timeout => Some(&self.timeout_min),
            FormField::EnvironmentKey(index) => self.env.rows.get(index).map(|row| &row.value.key),
            FormField::EnvironmentFile(index) => {
                self.env_files.rows.get(index).map(|row| &row.value)
            }
            _ => None,
        };
        if let Some(input) = input {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        let combobox = match field {
            FormField::Kind => Some(&self.kind_state),
            FormField::ScheduleType => Some(&self.schedule_state),
            FormField::Concurrency => Some(&self.concurrency_state),
            _ => None,
        };
        if let Some(combobox) = combobox {
            combobox.update(cx, |state, cx| state.focus(window, cx));
        }
        if let FormField::EnvironmentValue(index) = field
            && let Some(row) = self.env.rows.get(index)
        {
            row.value
                .value
                .update(cx, |state, cx| state.focus(window, cx));
        }
    }

    fn collect_config(&self, cx: &App) -> Result<TaskConfig, FormError> {
        let values = FormValues {
            source_inline: self.source_inline,
            inline_content: self.inline_content.read(cx).value().to_string(),
            uv_project: self.uv_project.read(cx).value().to_string(),
            cooldown: self.cooldown.read(cx).value().to_string(),
            concurrency_ix: self.concurrency_ix,
            concurrency_group: self.concurrency_group.read(cx).value().to_string(),
            name: self.name.read(cx).value().to_string(),
            path: self.path.read(cx).value().to_string(),
            kind_ix: self.kind_ix,
            manual_cmd: self.manual_cmd.read(cx).value().to_string(),
            args: self.args.read(cx).value().to_string(),
            working_dir: self.working_dir.read(cx).value().to_string(),
            env: self
                .env
                .rows
                .iter()
                .map(|row| {
                    (
                        row.value.key.read(cx).value().to_string(),
                        row.value.value.read(cx).value().to_string(),
                    )
                })
                .collect(),
            env_files: self
                .env_files
                .rows
                .iter()
                .map(|row| row.value.read(cx).value().to_string())
                .collect(),
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
        values.into_config(
            self.editing.as_ref().map_or(TaskId(0), |task| task.id),
            self.editing.as_ref(),
        )
    }

    fn browse(&mut self, _window: &mut gpui_kit::Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择脚本文件".into()),
        });
        let view = cx.entity().downgrade();
        let handle = self.handle;
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.first()
            {
                let text = p.to_string_lossy().into_owned();
                let suggested_name = p
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned());
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.path
                            .update(cx, |state, cx| state.set_value(text, window, cx));
                        if this.editing.is_none()
                            && this.name.read(cx).value().trim().is_empty()
                            && let Some(name) = suggested_name
                        {
                            this.name
                                .update(cx, |state, cx| state.set_value(name, window, cx));
                        }
                    });
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

    fn browse_env_files(&mut self, after: Option<u64>, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择要插入的 .env 文件（可多选）".into()),
        });
        let view = cx.entity().downgrade();
        let handle = self.handle;
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let inputs = paths
                            .into_iter()
                            .map(|path| {
                                Self::env_file_input(
                                    path.to_string_lossy().into_owned(),
                                    window,
                                    cx,
                                )
                            })
                            .collect::<Vec<_>>();
                        this.env_files.insert_after(after, inputs);
                        this.draft_changed(cx);
                    });
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
