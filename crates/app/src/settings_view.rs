//! 主窗口内的设置对话框：左侧导航、独立滚动子页与统一保存栏。

mod form;

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, IntoElement, ParentElement as _, Render,
    Styled, Window, div, px,
};

use scheduled_task_core::logger::LogLevel;
use scheduled_task_core::store::ThemeSetting;

use crate::bridge::{AppCore, AppSettings};
use form::{SettingsDraft, SettingsPage};

/// 普通设置仅保存草稿；管理器保活继续独立执行异步系统操作。
struct SettingsView {
    core: Arc<AppCore>,
    baseline: AppSettings,
    theme: ThemeSetting,
    log_level: LogLevel,
    page: SettingsPage,
    max_parallel: Entity<InputState>,
    log_timezone_custom: bool,
    log_timezone: Entity<InputState>,
    log_retention_days: Entity<InputState>,
    log_max_size_mb: Entity<InputState>,
    log_size_limit_enabled: bool,
    message: String,
    message_is_error: bool,
    keepalive_enabled: bool,
    keepalive_busy: bool,
    keepalive_message: String,
}

impl SettingsView {
    fn draft(&self, cx: &App) -> SettingsDraft {
        SettingsDraft {
            theme: self.theme,
            max_parallel: self.max_parallel.read(cx).value().to_string(),
            log_level: self.log_level,
            log_timezone_custom: self.log_timezone_custom,
            log_timezone: self.log_timezone.read(cx).value().to_string(),
            log_retention_days: self.log_retention_days.read(cx).value().to_string(),
            log_max_size_mb: self.log_max_size_mb.read(cx).value().to_string(),
            log_size_limit_enabled: self.log_size_limit_enabled,
        }
    }

    fn draft_changed(&mut self, cx: &mut Context<Self>) {
        self.message.clear();
        self.message_is_error = false;
        cx.notify();
    }

    fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = SettingsDraft::from_settings(&self.baseline);
        self.theme = draft.theme;
        self.log_level = draft.log_level;
        self.log_timezone_custom = draft.log_timezone_custom;
        self.log_size_limit_enabled = draft.log_size_limit_enabled;
        for (input, value) in [
            (&self.max_parallel, draft.max_parallel),
            (&self.log_timezone, draft.log_timezone),
            (&self.log_retention_days, draft.log_retention_days),
            (&self.log_max_size_mb, draft.log_max_size_mb),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.draft_changed(cx);
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = match self.draft(cx).validate(&self.baseline) {
            Ok(settings) => settings,
            Err(error) => {
                self.page = error.page;
                self.message = error.message;
                self.message_is_error = true;
                cx.notify();
                return;
            }
        };
        match self.core.save_settings(settings.clone()) {
            Ok(()) => {
                self.baseline = settings;
                self.restore_draft(window, cx);
                apply_theme_setting(self.baseline.theme, cx);
                self.message = "全部设置已保存并生效".into();
            }
            Err(error) => {
                self.message = format!("保存失败：{error}");
                self.message_is_error = true;
            }
        }
        cx.notify();
    }

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
        self.keepalive_message = "正在更新保活设置，请稍候……".into();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = rx.recv().await;
            let _ = view.update(cx, |this, cx| {
                this.keepalive_busy = false;
                match result {
                    Ok(change) => {
                        this.keepalive_message = match change.result {
                            Ok(()) if enabled => {
                                "保活已开启，退出后将自动恢复，以后登录也会自动启动".into()
                            }
                            Ok(()) => "保活已关闭，当前任务继续运行".into(),
                            Err(error) => format!("更新失败：{error}"),
                        };
                        match change.enabled {
                            Ok(value) => this.keepalive_enabled = value,
                            Err(error) => {
                                this.keepalive_message = format!("无法确认保活状态：{error}")
                            }
                        }
                    }
                    Err(error) => this.keepalive_message = format!("保活设置未完成：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme_buttons = [
            ("theme-follow", "跟随系统", ThemeSetting::FollowSystem),
            ("theme-light", "浅色", ThemeSetting::Light),
            ("theme-dark", "深色", ThemeSetting::Dark),
        ]
        .into_iter()
        .map(|(id, label, setting)| {
            let button = Button::new(id).label(label).w(px(88.)).h(px(30.));
            let button = if self.theme == setting {
                button.primary()
            } else {
                button
            };
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.theme = setting;
                this.draft_changed(cx);
            }))
        });
        v_flex().w_full().min_w_0().flex_shrink_0().p_4().gap_6()
            .child(page_heading("常规设置", "界面外观与管理器运行方式。", cx))
            .child(v_flex().gap_3()
                .child(section_heading("界面外观", "选择浅色、深色或跟随系统主题，保存后应用。", cx))
                .child(h_flex().flex_wrap().gap_1().children(theme_buttons)))
            .child(v_flex().gap_3().pt_4().border_t_1().border_color(cx.theme().border)
                .child(section_heading("管理器保活 · 立即生效", "此开关独立生效，不需要保存，也不受“放弃修改”影响。", cx))
                .child(Switch::new("keepalive-enabled").label("开启管理器保活")
                    .checked(self.keepalive_enabled)
                    .disabled(self.keepalive_busy || crate::desktop::keepalive_changing())
                    .on_click(cx.listener(|this, enabled: &bool, _, cx| this.set_keepalive(*enabled, cx))))
                .child(hint("管理器意外退出或手动退出后自动恢复，并在登录后自动启动。默认关闭。关闭保活不会中断当前任务。", cx))
                .child(hint("需要彻底退出时，使用托盘菜单“关闭保活并退出”。", cx))
                .child(div().text_size(px(12.)).child(self.keepalive_message.clone())))
    }

    fn render_task_execution(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .p_4()
            .gap_6()
            .child(page_heading("任务运行", "控制可同时执行的任务数量。", cx))
            .child(
                v_flex()
                    .gap_3()
                    .child(section_heading(
                        "并发控制",
                        "保存后更新执行上限；降低上限不会中断正在执行的任务。",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(div().child("最大同时执行任务数"))
                            .child(NumberInput::new(&self.max_parallel).w(px(140.))),
                    )
                    .child(hint("填写大于 0 的整数。", cx)),
            )
    }

    fn render_logging(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let level_buttons = [
            ("log-error", "错误", LogLevel::Error),
            ("log-warn", "警告", LogLevel::Warn),
            ("log-info", "信息", LogLevel::Info),
            ("log-debug", "调试", LogLevel::Debug),
            ("log-trace", "跟踪", LogLevel::Trace),
        ]
        .into_iter()
        .map(|(id, label, level)| {
            let button = Button::new(id).label(label).w(px(64.)).h(px(30.));
            let button = if self.log_level == level {
                button.primary()
            } else {
                button
            };
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.log_level = level;
                this.draft_changed(cx);
            }))
        });
        let timezone_buttons = [
            ("log-timezone-system", "同步系统时区", false),
            ("log-timezone-custom", "自定义时区", true),
        ]
        .into_iter()
        .map(|(id, label, custom)| {
            let button = Button::new(id).label(label).h(px(30.));
            let button = if self.log_timezone_custom == custom {
                button.primary()
            } else {
                button
            };
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.log_timezone_custom = custom;
                this.draft_changed(cx);
            }))
        });
        v_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .p_4()
            .gap_6()
            .child(page_heading(
                "日志设置",
                "统一配置日志记录、时间显示与存储清理。",
                cx,
            ))
            .child(
                v_flex()
                    .gap_3()
                    .child(section_heading(
                        "记录内容",
                        "文件日志等级控制文件记录的详细程度，默认“信息”；任务输出面板不受影响。",
                        cx,
                    ))
                    .child(h_flex().flex_wrap().gap_1().children(level_buttons))
                    .child(hint(
                        "“调试”和“跟踪”记录更详细，排查问题后建议切回“信息”。",
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap_3()
                    .pt_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(section_heading(
                        "时间显示",
                        "日志区域时间前缀、文件时间戳和按天分文件共用此时区。",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(timezone_buttons)
                            .child(
                                // 始终保留输入框占位，切换模式不改变行高或换行位置。
                                div().w(px(220.)).h_8().min_w_0().flex_shrink_0().children(
                                    self.log_timezone_custom
                                        .then(|| Input::new(&self.log_timezone).w_full()),
                                ),
                            ),
                    )
                    .child(hint(
                        "自定义时区可填写 Asia/Shanghai、America/New_York、UTC 或 +08:00、-05:30。",
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap_3()
                    .pt_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(section_heading(
                        "存储清理",
                        "超过存储限制时自动移除最早的日志",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(div().child("存储保存天数不超过"))
                            .child(NumberInput::new(&self.log_retention_days).w(px(130.)))
                            .child(div().child("天")),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Checkbox::new("log-size-limit-enabled")
                                    .label("存储大小限制")
                                    .checked(self.log_size_limit_enabled)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.log_size_limit_enabled = *checked;
                                        this.draft_changed(cx);
                                    })),
                            )
                            .child(
                                NumberInput::new(&self.log_max_size_mb)
                                    .w(px(140.))
                                    .disabled(!self.log_size_limit_enabled),
                            )
                            .child(div().child("MB")),
                    )
                    .child(hint(
                        "保存天数：3–365，默认 7 天；大小限制：200–5120 MB，默认 1024 MB。",
                        cx,
                    ))
                    .child(hint(
                        "正在写入的当天日志会保留，单个当天日志超过大小限制时可能暂时超限。",
                        cx,
                    )),
            )
    }
}

fn hint(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn page_heading(
    title: &'static str,
    description: &'static str,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .gap_1()
        .child(div().text_size(px(22.)).child(title))
        .child(hint(description, cx))
}

fn section_heading(
    title: &'static str,
    description: &'static str,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .gap_1()
        .child(div().text_size(px(15.)).child(title))
        .child(hint(description, cx))
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.draft(cx).is_dirty(&self.baseline);
        let navigation = [
            ("nav-general", "常规设置", SettingsPage::General),
            (
                "nav-task-execution",
                "任务运行",
                SettingsPage::TaskExecution,
            ),
            ("nav-logging", "日志设置", SettingsPage::Logging),
        ]
        .into_iter()
        .map(|(id, label, page)| {
            let button = Button::new(id).label(label).w_full();
            let button = if self.page == page {
                button.primary()
            } else {
                button.ghost()
            };
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.page = page;
                cx.notify();
            }))
        });
        let sidebar = v_flex()
            .w(px(144.))
            .flex_shrink_0()
            .self_stretch()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_3()
            .gap_2()
            .children(navigation);
        // 各页独立滚动，左侧导航和底部保存栏固定在可视区。
        let (scroll_id, pane) = match self.page {
            SettingsPage::General => (
                "settings-general-scroll",
                self.render_general(cx).into_any_element(),
            ),
            SettingsPage::TaskExecution => (
                "settings-task-execution-scroll",
                self.render_task_execution(cx).into_any_element(),
            ),
            SettingsPage::Logging => (
                "settings-logging-scroll",
                self.render_logging(cx).into_any_element(),
            ),
        };
        let status = if self.message.is_empty() {
            if dirty {
                "有未保存的修改"
            } else {
                "所有设置已保存"
            }
            .to_owned()
        } else {
            self.message.clone()
        };
        let status_color = if self.message_is_error {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .overflow_hidden()
                    .child(sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .id(scroll_id)
                            .child(pane),
                    ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .gap_2()
                    .child(hint(
                        "保存所有页面的修改；关闭面板后，未保存的修改不生效。保活开关除外。",
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_3()
                            .justify_between()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(status_color)
                                    .child(status),
                            )
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .gap_2()
                                    .child(
                                        Button::new("discard-settings")
                                            .label("放弃修改")
                                            .disabled(!dirty)
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.restore_draft(window, cx)
                                                },
                                            )),
                                    )
                                    .child(
                                        Button::new("save-settings")
                                            .label("保存设置")
                                            .primary()
                                            .disabled(!dirty)
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.save_settings(window, cx)
                                                },
                                            )),
                                    ),
                            ),
                    ),
            )
    }
}

/// 应用界面主题设置。
pub fn apply_theme_setting(setting: ThemeSetting, cx: &mut App) {
    match setting {
        ThemeSetting::FollowSystem => Theme::sync_system_appearance(None, cx),
        ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// 在当前主窗口中打开设置，关闭时丢弃普通草稿，不销毁原生窗口。
pub fn open_settings(core: Arc<AppCore>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let view = cx.new(|cx: &mut Context<SettingsView>| {
        let baseline = core.settings();
        let draft = SettingsDraft::from_settings(&baseline);
        let max_parallel = cx.new(|cx| {
            let mut input = InputState::new(window, cx).min(1.);
            input.set_value(draft.max_parallel, window, cx);
            input
        });
        let log_timezone = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder("Asia/Shanghai", window, cx);
            input.set_value(draft.log_timezone, window, cx);
            input
        });
        let log_retention_days = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .min(3.)
                .max(365.)
                .validate(|value, _| value.bytes().all(|byte| byte.is_ascii_digit()));
            input.set_value(draft.log_retention_days, window, cx);
            input
        });
        let log_max_size_mb = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .min(200.)
                .max(5120.)
                .validate(|value, _| value.bytes().all(|byte| byte.is_ascii_digit()));
            input.set_value(draft.log_max_size_mb, window, cx);
            input
        });
        for input in [
            &max_parallel,
            &log_timezone,
            &log_retention_days,
            &log_max_size_mb,
        ] {
            cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.draft_changed(cx);
                }
            })
            .detach();
        }
        let (keepalive_enabled, keepalive_message) = match scheduled_task_keepalive::enabled() {
            Ok(enabled) => (enabled, String::new()),
            Err(error) => (false, format!("无法读取保活设置：{error}")),
        };
        SettingsView {
            core,
            baseline,
            theme: draft.theme,
            log_level: draft.log_level,
            page: SettingsPage::General,
            max_parallel,
            log_timezone_custom: draft.log_timezone_custom,
            log_timezone,
            log_retention_days,
            log_max_size_mb,
            log_size_limit_enabled: draft.log_size_limit_enabled,
            message: String::new(),
            message_is_error: false,
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
            .h(px(620.))
            .content(move |content, _, _| content.min_h_0().overflow_hidden().child(view.clone()))
    });
}
