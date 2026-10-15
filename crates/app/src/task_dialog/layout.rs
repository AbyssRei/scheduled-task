//! 表单控件布局与交互绑定。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::input::{Input, NumberInput, Textarea};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{ClickEvent, Context, IntoElement, ParentElement as _, Render, Styled, div, px};

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
            .gap_3()
            .text_color(theme_fg)
            .child(field("任务名称", Input::new(&self.name).h(px(32.))))
            .child(field("脚本类型", Combobox::new(&self.kind_state)))
            .child(field(
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
            .child(field("手工命令", Input::new(&self.manual_cmd).h(px(32.))))
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
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("btn-cancel").label("取消").on_click({
                        move |_: &ClickEvent, window, cx| {
                            window.close_dialog(cx);
                        }
                    }))
                    .child(Button::new("btn-save").label("保存").primary().on_click({
                        let view = view.clone();
                        move |_: &ClickEvent, window, cx| {
                            view.update(cx, |this, cx| this.save(window, cx))
                        }
                    })),
            )
    }
}
