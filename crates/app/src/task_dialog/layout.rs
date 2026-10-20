//! 表单控件布局与交互绑定。

use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::input::{Input, NumberInput, Textarea};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    ClickEvent, Context, Entity, IntoElement, ParentElement as _, Render, Styled, div, px,
};

use super::TaskDialogView;

fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_2()
        .child(div().w(px(110.)).flex_shrink_0().child(label))
        .child(control)
}

impl Render for TaskDialogView {
    fn render(
        &mut self,
        _window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity();
        let theme_fg = cx.theme().foreground;

        v_flex()
            .flex_shrink_0()
            .gap_3()
            .text_color(theme_fg)
            .child(field("任务名称", Input::new(&self.name).h(px(32.))))
            .child(field("脚本类型", Combobox::new(&self.kind_state)))
            .child(field(
                "脚本来源",
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("source-file")
                            .label("文件脚本")
                            .selected(!self.source_inline)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_inline = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("source-inline")
                            .label("内联脚本")
                            .selected(self.source_inline)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_inline = true;
                                cx.notify();
                            })),
                    ),
            ))
            .when(self.source_inline, |layout| {
                layout.child(field(
                    "脚本内容",
                    Textarea::new(&self.inline_content).h(px(160.)).flex_1(),
                ))
            })
            .when(!self.source_inline, |layout| {
                layout.child(field(
                    "脚本路径",
                    h_flex()
                        .flex_1()
                        .gap_2()
                        .child(Input::new(&self.path).h(px(32.)).flex_1())
                        .child(Button::new("btn-browse").label("浏览...").on_click({
                            let view = view.clone();
                            move |_: &ClickEvent, window, cx| {
                                view.update(cx, |this, cx| this.browse(window, cx))
                            }
                        })),
                ))
            })
            .when(self.kind_ix == 5, |layout| {
                layout.child(field(
                    "uv 项目目录",
                    Input::new(&self.uv_project).h(px(32.)).flex_1(),
                ))
            })
            .child(field("手工命令", Input::new(&self.manual_cmd).h(px(32.))))
            .child(field(
                "命令行参数",
                Textarea::new(&self.args).h(px(72.)).flex_1(),
            ))
            .child(field(
                "工作目录",
                h_flex()
                    .flex_1()
                    .gap_2()
                    .child(Input::new(&self.working_dir).h(px(32.)).flex_1())
                    .child(
                        Button::new("btn-browse-working-dir")
                            .label("浏览...")
                            .on_click({
                                let view = view.clone();
                                move |_: &ClickEvent, _, cx| {
                                    view.update(cx, |this, cx| this.browse_working_dir(cx))
                                }
                            }),
                    ),
            ))
            .child(field(
                "环境变量",
                Textarea::new(&self.env).h(px(84.)).flex_1(),
            ))
            .child(field(
                ".env 文件",
                h_flex()
                    .flex_1()
                    .gap_2()
                    .child(Input::new(&self.env_file).h(px(32.)).flex_1())
                    .child(
                        Button::new("btn-browse-env-file")
                            .label("浏览...")
                            .on_click({
                                let view = view.clone();
                                move |_: &ClickEvent, _, cx| {
                                    view.update(cx, |this, cx| this.browse_env_file(cx))
                                }
                            }),
                    ),
            ))
            .child(field("调度类型", Combobox::new(&self.schedule_state)))
            .child(field(
                "触发时间",
                Input::new(&self.times).h(px(32.)).flex_1(),
            ))
            .child(field("每月日期", Input::new(&self.day).h(px(32.))))
            .child(field("并发策略", Combobox::new(&self.concurrency_state)))
            .when(self.concurrency_ix == 2, |layout| {
                layout.child(field(
                    "互斥组名",
                    Input::new(&self.concurrency_group).h(px(32.)),
                ))
            })
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("chk-retry")
                            .checked(self.retry_on)
                            .label("自动重试")
                            .on_click({
                                let view = view.clone();
                                move |checked: &bool, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.retry_on = *checked;
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        Checkbox::new("chk-timeout")
                            .checked(self.timeout_on)
                            .label("启用超时")
                            .on_click({
                                let view = view.clone();
                                move |checked: &bool, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.timeout_on = *checked;
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        Checkbox::new("chk-enabled")
                            .checked(self.enabled)
                            .label("启用任务")
                            .on_click({
                                let view = view.clone();
                                move |checked: &bool, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.enabled = *checked;
                                        cx.notify();
                                    })
                                }
                            }),
                    ),
            )
            .child(field(
                "重试模式",
                Textarea::new(&self.pattern).h(px(96.)).flex_1(),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(110.)).flex_shrink_0().child("重试窗口/次数"))
                    .child(NumberInput::new(&self.window_n).w(px(110.)))
                    .child(NumberInput::new(&self.max_retries).w(px(110.))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(110.)).flex_shrink_0().child("超时(分钟)"))
                    .child(NumberInput::new(&self.timeout_min).w(px(110.))),
            )
            .when(self.retry_on, |layout| {
                layout.child(field(
                    "重试冷却(秒)",
                    NumberInput::new(&self.cooldown).w(px(110.)),
                ))
            })
    }
}

impl TaskDialogView {
    /// 操作按钮留在滚动区外，低分辨率下也能随时保存或取消。
    pub(super) fn actions(view: Entity<Self>) -> impl IntoElement {
        h_flex()
            .flex_shrink_0()
            .justify_end()
            .gap_2()
            .child(Button::new("btn-cancel").label("取消").on_click(
                move |_: &ClickEvent, window, cx| {
                    window.close_dialog(cx);
                },
            ))
            .child(Button::new("btn-save").label("保存").primary().on_click(
                move |_: &ClickEvent, window, cx| view.update(cx, |this, cx| this.save(window, cx)),
            ))
    }
}
