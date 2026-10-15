//! 主窗口工具栏。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::h_flex;
use gpui_kit::{ClickEvent, Context, IntoElement, ParentElement as _, Styled};

use super::TaskListView;

impl TaskListView {
    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let view = cx.entity();
        h_flex()
            .flex_wrap()
            .gap_2()
            .child(Button::new("btn-add").label("添加").primary().on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.open_add(window, cx))
                }
            }))
            .child(Button::new("btn-edit").label("编辑").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.open_edit(window, cx))
                }
            }))
            .child(Button::new("btn-del").label("删除").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.delete_selected(window, cx))
                }
            }))
            .child(Button::new("btn-toggle").label("启用/禁用").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.toggle_selected(window, cx))
                }
            }))
            .child(Button::new("btn-run").label("立即运行").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.run_selected(window, cx))
                }
            }))
            .child(Button::new("btn-stop").label("停止").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.stop_selected(window, cx))
                }
            }))
            .child(Button::new("btn-settings").label("设置").on_click({
                let core = self.core.clone();
                move |_: &ClickEvent, window, cx| {
                    crate::settings_view::open_settings(core.clone(), window, cx)
                }
            }))
            .child(Button::new("btn-help").label("帮助").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.show_help(window, cx))
                }
            }))
            .child(Button::new("btn-about").label("关于").on_click({
                let view = view.clone();
                move |_: &ClickEvent, window, cx| {
                    view.update(cx, |this, cx| this.show_about(window, cx))
                }
            }))
    }
}
