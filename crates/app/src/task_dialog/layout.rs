//! 分页表单、条件字段及固定操作栏。

use gpui_kit::base::{Selectable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::input::{Input, NumberInput, Textarea};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled, div, px,
};

use super::{
    TaskDialogView,
    feedback::{FormField, FormPage},
};

fn hint(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn section(title: &'static str, description: &'static str, cx: &App) -> gpui_kit::Div {
    v_flex()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .gap_3()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .gap_1()
                .child(div().text_size(px(15.)).font_semibold().child(title))
                .child(hint(description, cx)),
        )
}

impl TaskDialogView {
    fn field(
        &self,
        field: Option<FormField>,
        label: &'static str,
        control: impl IntoElement,
        cx: &App,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let error = self
            .error
            .as_ref()
            .filter(|error| Some(error.field) == field);
        v_flex()
            .id(label)
            .anchor_scroll(if error.is_some() {
                self.error_scroll.clone()
            } else {
                None
            })
            .w_full()
            .min_w_0()
            .gap_1()
            .child(div().text_size(px(13.)).child(label))
            .child(
                div()
                    .min_w_0()
                    .w_full()
                    .rounded_md()
                    .when(error.is_some(), |control| {
                        control.border_1().border_color(cx.theme().danger)
                    })
                    .child(control),
            )
            .when_some(
                error.map(|error| error.message.clone()),
                |field, message| {
                    field.child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().danger)
                            .child(message),
                    )
                },
            )
    }

    fn render_basics(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .flex_shrink_0()
            .child(
                section("任务信息", "必填项标有 *；保存后才会生效。", cx)
                    .child(self.field(
                        Some(FormField::Name),
                        "任务名称 *",
                        Input::new(&self.name).w_full(),
                        cx,
                    ))
                    .child(
                        Checkbox::new("task-enabled")
                            .label("启用此任务的定时调度")
                            .checked(self.enabled)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.enabled = *checked;
                                this.draft_changed(cx);
                            })),
                    ),
            )
            .child(
                section(
                    "执行脚本",
                    "选择文件脚本或直接编写内联脚本；切换时保留已填写的内容。",
                    cx,
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("source-file")
                                .label("文件脚本")
                                .selected(!self.source_inline)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.source_inline = false;
                                    this.draft_changed(cx);
                                })),
                        )
                        .child(
                            Button::new("source-inline")
                                .label("内联脚本")
                                .selected(self.source_inline)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.source_inline = true;
                                    this.draft_changed(cx);
                                })),
                        ),
                )
                .child(self.field(
                    Some(FormField::Kind),
                    if self.source_inline {
                        "脚本类型 *"
                    } else {
                        "脚本类型"
                    },
                    Combobox::new(&self.kind_state).w_full(),
                    cx,
                ))
                .when(!self.source_inline, |section| {
                    section.child(
                        self.field(
                            Some(FormField::Path),
                            "脚本路径 *",
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .child(Input::new(&self.path).flex_1().min_w_0())
                                .child(Button::new("browse-script").label("选择文件...").on_click(
                                    cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                )),
                            cx,
                        ),
                    )
                })
                .when(self.source_inline, |section| {
                    section
                        .child(self.field(
                            None,
                            "脚本内容",
                            Textarea::new(&self.inline_content).h(px(170.)).w_full(),
                            cx,
                        ))
                        .when(self.kind_ix == 0, |section| {
                            section.child(hint(
                                "内联脚本需要选择具体类型，例如 Cmd、PowerShell 或 Python。",
                                cx,
                            ))
                        })
                })
                .when(self.kind_ix == 8, |section| {
                    section
                        .child(self.field(
                            Some(FormField::ManualCommand),
                            "手工命令 *",
                            Input::new(&self.manual_cmd).w_full(),
                            cx,
                        ))
                        .child(hint(
                            "使用 {script} 引用脚本；参数在“执行环境”页面逐条填写。",
                            cx,
                        ))
                })
                .when(self.kind_ix == 5, |section| {
                    section.child(self.field(
                        None,
                        "uv 项目目录（可选）",
                        Input::new(&self.uv_project).w_full(),
                        cx,
                    ))
                }),
            )
    }

    fn render_environment(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .flex_shrink_0()
            .child(
                section(
                    "参数与工作目录",
                    "可选；未填写时沿用脚本的默认执行环境。",
                    cx,
                )
                .child(self.field(
                    None,
                    "命令行参数",
                    Textarea::new(&self.args).h(px(80.)).w_full(),
                    cx,
                ))
                .child(hint("每行一个参数，带空格的参数不需要额外加引号。", cx))
                .child(
                    self.field(
                        None,
                        "工作目录",
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(Input::new(&self.working_dir).flex_1().min_w_0())
                            .child(
                                Button::new("browse-working-dir")
                                    .label("选择目录...")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.browse_working_dir(cx)),
                                    ),
                            ),
                        cx,
                    ),
                )
                .child(hint(
                    "留空使用脚本所在目录；相对路径按脚本所在目录解析。",
                    cx,
                )),
            )
            .child(
                section(
                    "环境文件",
                    "相对路径按最终工作目录解析；后面的文件覆盖前面的同名变量。",
                    cx,
                )
                .child(self.env_files_editor(cx)),
            )
            .child(
                section(
                    "显式环境变量",
                    "优先级高于环境文件；同名变量按列表顺序覆盖。",
                    cx,
                )
                .child(self.environment_editor(cx)),
            )
    }

    fn render_schedule(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let policy_hint = match self.concurrency_ix {
            1 => "同一任务始终只运行一个实例，执行中的再次触发最多保留一个等待项。",
            2 => "组名相同的任务互斥执行；等待任务仍受全局执行上限约束。",
            _ => "允许与其他任务并行，仍受全局执行上限约束；同一任务始终单实例。",
        };
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .flex_shrink_0()
            .child(
                section(
                    "触发计划",
                    "使用系统本地时间；启动管理器时不补跑已过去的计划。",
                    cx,
                )
                .child(self.field(
                    Some(FormField::ScheduleType),
                    "调度类型",
                    Combobox::new(&self.schedule_state).w_full(),
                    cx,
                ))
                .when(self.schedule_ix == 1, |section| {
                    section
                        .child(self.field(
                            Some(FormField::Day),
                            "每月日期 *",
                            NumberInput::new(&self.day).w(px(160.)),
                            cx,
                        ))
                        .child(hint("范围 1–31；当月不存在该日期时跳过。", cx))
                })
                .child(self.field(
                    Some(FormField::Times),
                    "触发时间 *",
                    Input::new(&self.times).w_full(),
                    cx,
                ))
                .child(hint(
                    "格式 HH:MM:SS；多个时间用逗号分隔，例如 08:00:00,12:30:00。",
                    cx,
                )),
            )
            .child(
                section("并发与等待", "全局执行上限在“设置 → 任务运行”调整。", cx)
                    .child(self.field(
                        Some(FormField::Concurrency),
                        "并发策略",
                        Combobox::new(&self.concurrency_state).w_full(),
                        cx,
                    ))
                    .when(self.concurrency_ix == 2, |section| {
                        section.child(self.field(
                            Some(FormField::Group),
                            "互斥组名 *",
                            Input::new(&self.concurrency_group).w_full(),
                            cx,
                        ))
                    })
                    .child(hint(policy_hint, cx)),
            )
    }

    fn render_recovery(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .flex_shrink_0()
            .child(
                section("自动重试", "监控脚本输出，满足连续匹配条件时重新执行。", cx)
                    .child(
                        Checkbox::new("retry-enabled")
                            .label("启用输出监控与自动重试")
                            .checked(self.retry_on)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.retry_on = *checked;
                                this.draft_changed(cx);
                            })),
                    )
                    .when(self.retry_on, |section| {
                        section
                            .child(self.field(
                                None,
                                "匹配模式（可选）",
                                Textarea::new(&self.pattern).h(px(96.)).w_full(),
                                cx,
                            ))
                            .child(hint(
                                "每行一个候选模式；留空时检查连续输出的核心内容是否一致。",
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .items_start()
                                    .flex_wrap()
                                    .gap_3()
                                    .child(div().flex_1().min_w(px(160.)).child(self.field(
                                        Some(FormField::RetryWindow),
                                        "连续次数 · 1–100",
                                        NumberInput::new(&self.window_n).w_full(),
                                        cx,
                                    )))
                                    .child(div().flex_1().min_w(px(160.)).child(self.field(
                                        Some(FormField::MaxRetries),
                                        "最大重试次数 · 1–10",
                                        NumberInput::new(&self.max_retries).w_full(),
                                        cx,
                                    ))),
                            )
                            .child(self.field(
                                Some(FormField::Cooldown),
                                "重试冷却（秒）",
                                NumberInput::new(&self.cooldown).w(px(160.)),
                                cx,
                            ))
                            .child(hint(
                                "留空使用默认值：连续 5 次、最多重试 3 次、冷却 2 秒。",
                                cx,
                            ))
                    }),
            )
            .child(
                section("执行超时", "超时后终止脚本及其子进程。", cx)
                    .child(
                        Checkbox::new("timeout-enabled")
                            .label("限制每次执行时长")
                            .checked(self.timeout_on)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.timeout_on = *checked;
                                this.draft_changed(cx);
                            })),
                    )
                    .when(self.timeout_on, |section| {
                        section.child(self.field(
                            Some(FormField::Timeout),
                            "超时分钟数 * · 1–300",
                            NumberInput::new(&self.timeout_min).w(px(160.)),
                            cx,
                        ))
                    }),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let message = self
            .error
            .as_ref()
            .map(|error| format!("{}：{}", error.field.page().title(), error.message))
            .or_else(|| self.save_error.clone());
        v_flex()
            .flex_shrink_0()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .when_some(message, |footer, message| {
                footer.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(message),
                )
            })
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .child(hint("保存全部页面的配置", cx))
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .child(
                                Button::new("task-cancel")
                                    .label("取消")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("task-save")
                                    .label(if self.editing.is_some() {
                                        "保存修改"
                                    } else {
                                        "添加任务"
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.save(window, cx) {
                                            window.close_dialog(cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for TaskDialogView {
    fn render(&mut self, _: &mut gpui_kit::Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.dragging = None;
        }
        let navigation = FormPage::ALL
            .into_iter()
            .enumerate()
            .map(|(index, page)| {
                let label = if self
                    .error
                    .as_ref()
                    .is_some_and(|error| error.field.page() == page)
                {
                    format!("{} · 待检查", page.title())
                } else {
                    page.title().to_owned()
                };
                let button = Button::new(("task-page", index))
                    .label(label)
                    .selected(self.page == page);
                let button = if self.page == page {
                    button.primary()
                } else {
                    button.ghost()
                };
                button.on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
            })
            .collect::<Vec<_>>();
        let pane = match self.page {
            FormPage::Basics => self.render_basics(cx).into_any_element(),
            FormPage::Environment => self.render_environment(cx).into_any_element(),
            FormPage::Schedule => self.render_schedule(cx).into_any_element(),
            FormPage::Recovery => self.render_recovery(cx).into_any_element(),
        };
        v_flex()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.dragging.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .capture_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && this.dragging.take().is_some() {
                        cx.stop_active_drag(window);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .flex_wrap()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(navigation),
            )
            .child(
                div()
                    .id(self.page.scroll_id())
                    .track_scroll(&self.page_scrolls[self.page.index()])
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(pane),
            )
            .child(self.footer(cx))
    }
}
