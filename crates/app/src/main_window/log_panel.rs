//! 日志缓冲与滚动跟随，由独立视图在 UI 线程内管理。

use std::collections::VecDeque;

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement as _, Styled, div, px,
};
use jiff::Timestamp;
use scheduled_task_core::logger::format_log_timestamp;

const MAX_LOG_LINES: usize = 200;

struct LogEntry {
    received_at: Timestamp,
    message: String,
}

impl LogEntry {
    fn display(&self) -> String {
        // 保留原始时刻,切换时区时重新显示;不解析或改写脚本正文中的时间。
        format!(
            "[{}] {}",
            format_log_timestamp(self.received_at),
            self.message
        )
    }
}

pub(super) struct LogPanel {
    lines: VecDeque<LogEntry>,
    scroll: ScrollHandle,
    /// 向上滚暂停，向下滚或点“回到最新”恢复。
    auto_follow: bool,
    /// 暂停跟随期间新到日志条数。
    new_since_pause: usize,
}

impl LogPanel {
    pub(super) fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            scroll: ScrollHandle::new(),
            auto_follow: true,
            new_since_pause: 0,
        }
    }

    pub(super) fn push_log(&mut self, line: String, cx: &mut Context<Self>) {
        self.lines.push_back(LogEntry {
            received_at: Timestamp::now(),
            message: line,
        });
        while self.lines.len() > MAX_LOG_LINES {
            self.lines.pop_front();
        }
        if self.auto_follow {
            // 官方 API 在 paint 阶段精确滚动到底部。
            self.scroll.scroll_to_bottom();
        } else {
            self.new_since_pause += 1;
        }
        cx.notify();
    }
}

impl Render for LogPanel {
    fn render(&mut self, _: &mut gpui_kit::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log_panel = div()
            .relative()
            .h(px(140.))
            .w_full()
            .border_t_1()
            .border_color(cx.theme().border)
            .pt_2()
            .text_size(px(12.))
            .child(
                div()
                    .id("log-scroll")
                    .h_full()
                    .w_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        // 内容不足一屏时不参与跟随切换。
                        if this.scroll.max_offset().y <= px(0.) {
                            return;
                        }
                        let dy = event.delta.pixel_delta(px(16.)).y;
                        if dy > px(0.) {
                            this.auto_follow = false;
                        } else if dy < px(0.) {
                            this.auto_follow = true;
                            this.new_since_pause = 0;
                        }
                        cx.notify();
                    }))
                    .px_2()
                    // 行内容自由换行，长行不截断。
                    .child(
                        v_flex()
                            .children(self.lines.iter().map(|line| div().child(line.display()))),
                    ),
            );

        // 暂停跟随期间，右下角显示“回到最新”按钮和未读条数。
        let actions = if !self.auto_follow {
            div().absolute().bottom_2().right_4().child(
                Button::new("btn-log-follow")
                    .label(if self.new_since_pause > 0 {
                        format!("↓ 回到最新({})", self.new_since_pause)
                    } else {
                        "↓ 回到最新".into()
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.auto_follow = true;
                        this.new_since_pause = 0;
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    })),
            )
        } else {
            div()
        };
        log_panel.child(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日志重绘保留接收时刻和脚本中的原始时间() {
        let received_at: Timestamp = "2026-10-19T00:00:00.123Z".parse().unwrap();
        let entry = LogEntry {
            received_at,
            message: "[任务1] 脚本时间 2026-10-18T19:00:00-05:00".into(),
        };
        let first = entry.display();
        assert_eq!(entry.display(), first);
        let end = first.find(']').unwrap();
        let rendered: Timestamp = first[1..end].parse().unwrap();
        assert_eq!(rendered, received_at);
        assert_eq!(&first[end + 2..], entry.message);
    }
}
