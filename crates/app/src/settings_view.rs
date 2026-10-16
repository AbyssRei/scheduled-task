//! 主窗口内的设置对话框(主题、文件日志等级、并发上限与管理器保活)。
//!
//! 布局参考常见桌面应用设置页:左侧导航(基础设置 分组)+ 右侧内容区
//! 与任务编辑共用对话框层,不创建原生子窗口。

use std::sync::Arc;

use gpui_kit::component::button::Button;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::{InputState, NumberInput};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled, Window, div, px,
};

use scheduled_task_core::logger::LogLevel;
use scheduled_task_core::store::ThemeSetting;

use crate::bridge::AppCore;

/// 设置对话框内容,由当前主窗口的对话框层持有。
struct SettingsView {
    core: Arc<AppCore>,
    max_parallel: Entity<InputState>,
    message: String,
    log_level_message: String,
    keepalive_enabled: bool,
    keepalive_busy: bool,
    keepalive_message: String,
}

impl SettingsView {
    fn set_keepalive(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let rx = match crate::desktop::change_keepalive(enabled) {
            Ok(rx) => rx,
            Err(error) => {
                self.keepalive_message = error;
                cx.notify();
                return;
            }
        };
        self.keepalive_busy = true;
        self.keepalive_message = "正在更新保活设置,请稍候……".into();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = rx.recv().await;
            let _ = view.update(cx, |this, cx| {
                this.keepalive_busy = false;
                match result {
                    Ok(change) => {
                        this.keepalive_message = match change.result {
                            Ok(()) if enabled => {
                                "保活已开启,退出后将自动恢复,以后登录也会自动启动".into()
                            }
                            Ok(()) => "保活已关闭,当前任务继续运行".into(),
                            Err(error) => format!("更新失败: {error}"),
                        };
                        match change.enabled {
                            Ok(value) => this.keepalive_enabled = value,
                            Err(error) => {
                                this.keepalive_message = format!("无法确认保活状态: {error}");
                            }
                        }
                    }
                    Err(error) => this.keepalive_message = format!("保活设置未完成: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn parse_max_parallel(value: &str) -> Result<usize, &'static str> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|v| *v > 0)
        .map(|value| value as usize)
        .ok_or("最大并行数应为大于 0 的整数")
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut gpui_kit::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.core.theme_setting();

        // 主题三选一按钮组(当前项高亮 primary,其余默认态)
        let theme_buttons = [
            ("theme-follow", "跟随系统", ThemeSetting::FollowSystem),
            ("theme-light", "浅色", ThemeSetting::Light),
            ("theme-dark", "深色", ThemeSetting::Dark),
        ]
        .into_iter()
        .map(|(id, label, setting)| {
            let active = current == setting;
            let view = cx.entity();
            let core = self.core.clone();
            // 统一固定尺寸:切换主题时按钮几何与文字位置保持稳定
            let btn = Button::new(id).label(label).w(px(88.)).h(px(30.));
            let btn = if active { btn.primary() } else { btn };
            btn.on_click(move |_: &ClickEvent, _, cx| {
                core.set_theme_setting(setting);
                apply_theme_setting(setting, cx);
                view.update(cx, |_, cx| cx.notify());
            })
        });

        let current_log_level = self.core.log_level();
        let log_level_buttons = [
            ("log-error", "错误", LogLevel::Error),
            ("log-warn", "警告", LogLevel::Warn),
            ("log-info", "信息", LogLevel::Info),
            ("log-debug", "调试", LogLevel::Debug),
            ("log-trace", "跟踪", LogLevel::Trace),
        ]
        .into_iter()
        .map(|(id, label, level)| {
            let btn = Button::new(id).label(label).w(px(72.)).h(px(30.));
            let btn = if current_log_level == level {
                btn.primary()
            } else {
                btn
            };
            btn.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.log_level_message = match this.core.set_log_level(level) {
                    Ok(()) => format!("文件日志等级已设为{label},即时生效"),
                    Err(error) => format!("设置文件日志等级失败: {error}"),
                };
                cx.notify();
            }))
        });

        // 左侧导航:基础设置 分组 → 常规设置(当前页,高亮)
        let sidebar = div()
            .w(px(160.))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_3()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("基础设置"),
            )
            .child(
                div()
                    .id("nav-appearance")
                    .mt_2()
                    .rounded_md()
                    .px_3()
                    .py_1p5()
                    .bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
                    .child("常规设置"),
            );

        // 右侧内容区:标题 / 分区说明 / 主题卡片
        // h_flex 默认交叉轴居中:右栏需 h_full 顶满,内容才顶对齐
        let pane = v_flex()
            .id("settings-content")
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(div().text_size(px(22.)).child("常规设置"))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_size(px(15.)).child("界面设置"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("设置应用界面主题。"),
                    ),
            )
            .child(
                div()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .px_4()
                    .py_3()
                    .child(
                        v_flex()
                            .gap_3()
                            .child(
                                v_flex().child(div().child("界面主题")).child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("选择浅色、深色或跟随系统主题。"),
                                ),
                            )
                            .child(h_flex().gap_1().children(theme_buttons)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().child("文件日志等级"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("控制文件日志的详细程度,默认“信息”。任务输出面板不受影响。"),
                    )
                    .child(h_flex().gap_1().children(log_level_buttons))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("“调试”和“跟踪”会记录更多日志,排查问题后建议切回“信息”。"),
                    )
                    .child(div().text_size(px(12.)).child(self.log_level_message.clone())),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().child("最大同时执行任务数"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("超过上限的任务按顺序等待；降低上限不会中断正在执行的任务。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(NumberInput::new(&self.max_parallel).w(px(140.)))
                            .child(
                                Button::new("save-max-parallel")
                                    .label("应用")
                                    .primary()
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        match parse_max_parallel(
                                            this.max_parallel.read(cx).value().as_ref(),
                                        ) {
                                            Ok(value) => {
                                                this.core.set_max_parallel(value);
                                                this.message = "已保存并发上限".into();
                                            }
                                            Err(message) => this.message = message.into(),
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().text_size(px(12.)).child(self.message.clone())),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        Switch::new("keepalive-enabled")
                            .label("开启管理器保活")
                            .checked(self.keepalive_enabled)
                            .disabled(self.keepalive_busy || crate::desktop::keepalive_changing())
                            .on_click(cx.listener(|this, enabled: &bool, _, cx| {
                                this.set_keepalive(*enabled, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("管理器意外退出或手动退出后自动恢复,并在登录后自动启动。默认关闭。关闭保活不会中断当前任务。"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("需要彻底退出时,使用托盘菜单“关闭保活并退出”。"),
                    )
                    .child(div().text_size(px(12.)).child(self.keepalive_message.clone())),
            );

        h_flex()
            .w_full()
            .h(px(590.))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar)
            .child(pane)
    }
}

/// 应用界面主题设置。
/// - 跟随系统:sync_system_appearance
/// - 浅色/深色:Theme::change(自动刷新所有窗口)
pub fn apply_theme_setting(setting: ThemeSetting, cx: &mut App) {
    match setting {
        ThemeSetting::FollowSystem => Theme::sync_system_appearance(None, cx),
        ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// 在当前主窗口中打开设置,关闭时仅移除对话框,不销毁原生窗口。
pub fn open_settings(core: Arc<AppCore>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    // 视图在打开时创建一次,避免主题切换导致对话框重绘时反复创建。
    let view = cx.new(|cx| {
        let max_parallel = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_value(core.max_parallel().to_string(), window, cx);
            input
        });
        let (keepalive_enabled, keepalive_message) = match scheduled_task_keepalive::enabled() {
            Ok(enabled) => (enabled, String::new()),
            Err(error) => (false, format!("无法读取保活设置: {error}")),
        };
        SettingsView {
            core,
            max_parallel,
            message: String::new(),
            log_level_message: String::new(),
            keepalive_enabled,
            keepalive_busy: false,
            keepalive_message,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let view = view.clone();
        dialog
            .title("设置")
            .w(px(760.))
            .content(move |content, _, _| content.child(view.clone()))
    });
}

#[cfg(test)]
mod tests {
    use super::parse_max_parallel;

    #[test]
    fn validates_global_parallel_limit() {
        assert_eq!(parse_max_parallel(" 8 "), Ok(8));
        for value in ["", "0", "-1", "1.5", "4294967296", "abc"] {
            assert!(parse_max_parallel(value).is_err());
        }
    }
}
