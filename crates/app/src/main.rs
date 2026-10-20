// release 产物免黑窗;debug 保留控制台看日志
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! scheduled-task:GPUI Kit 桌面应用。
//!
//! 线程模型:daemon 独立线程(1s tick 调度),事件经 std mpsc → 转发线程 →
//! async-channel → UI 在 `cx.spawn` 中 `.await`。
//! 命令下行:UI 直接调 AppCore(增删改查/运行/停止)。

mod bridge;
mod desktop;
mod main_window;
mod settings_view;
mod task_dialog;
mod widgets;

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, WindowOptions, px};

use scheduled_task_core::daemon::{Daemon, DaemonEvent};

use crate::bridge::{AppCore, spawn_event_forward};
use crate::main_window::TaskListView;

fn main() {
    // 守护模式必须在初始化任务、调度线程和 GPUI 之前分流。
    // 同一个发布文件作为独立守护进程运行,不加入任务的 Job Object。
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.iter().any(|arg| arg == "--watchdog") {
        let log_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join("logs/watchdog")))
            .unwrap_or_else(|| "logs/watchdog".into());
        let _ = scheduled_task_core::logger::init_logging(&log_dir);
        if let Err(error) = scheduled_task_keepalive::run_watchdog() {
            tracing::error!("保活守护已停止：{error}");
            scheduled_task_core::logger::shutdown_logging();
            std::process::exit(1);
        }
        scheduled_task_core::logger::shutdown_logging();
        return;
    }
    let background = args.iter().any(|arg| arg == "--background");
    // 关闭保活与守护拉起同时发生时,延迟启动的进程不得重新打开应用。
    if background && !scheduled_task_keepalive::enabled().unwrap_or(false) {
        return;
    }
    let _instance = match scheduled_task_keepalive::acquire_app_instance_with_restore(!background) {
        Ok(Some(guard)) => guard,
        Ok(None) => return,
        Err(error) => {
            show_startup_error(&error);
            return;
        }
    };
    // 获取单实例锁前可能曾等待原进程退出,必须再检查一次关闭保活的结果。
    if background && !scheduled_task_keepalive::enabled().unwrap_or(false) {
        return;
    }

    let (event_tx, event_rx) = scheduled_task_core::daemon::spawn_channel();
    let daemon = Arc::new(Daemon::new(event_tx));
    // 核心先读取配置,再按持久化的存储限制初始化 CWD logs/。
    let core = match AppCore::init(daemon.clone()) {
        Ok(core) => core,
        Err(error) => {
            show_startup_error(&error);
            scheduled_task_core::logger::shutdown_logging();
            return;
        }
    };

    if let Err(error) = scheduled_task_keepalive::ensure_watchdog() {
        tracing::error!("无法启动保活守护：{error}");
    }

    // 事件上行:std mpsc → async-channel
    let (async_tx, async_rx) = async_channel::bounded::<DaemonEvent>(128);
    spawn_event_forward(event_rx, async_tx);

    // daemon 调度线程(1s tick)
    let daemon_for_thread = daemon.clone();
    std::thread::Builder::new()
        .name("scheduler".into())
        .spawn(move || {
            loop {
                daemon_for_thread.tick();
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("启动调度线程失败");

    // 组件的关闭、下拉等图标通过资源路径加载,必须注册内置 SVG 资源。
    let daemon_for_quit = daemon.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
            // 普通退出及系统结束会话都会触发;同步刷新,避免进程退出跳过静态析构。
            cx.on_app_quit(move |_| {
                daemon_for_quit.shutdown();
                tracing::info!("管理器正在退出");
                scheduled_task_core::logger::shutdown_logging();
                async {}
            })
            .detach();
            gpui_kit::init(cx);
            // 启动时应用持久化的界面主题(未设置 = 跟随系统)
            settings_view::apply_theme_setting(core.theme_setting(), cx);

            let tray_ready = desktop::init_tray(cx);
            let start_hidden = background && tray_ready;

            let (handle, view) = gpui_kit::open_window(
                WindowOptions {
                    show: !start_hidden,
                    focus: !start_hidden,
                    titlebar: Some(gpui_kit::TitlebarOptions {
                        title: Some("定时任务管理器".into()),
                        ..Default::default()
                    }),
                    window_bounds: Some(gpui_kit::WindowBounds::Windowed(
                        gpui_kit::Bounds::centered(
                            None,
                            gpui_kit::Size {
                                width: px(1080.),
                                height: px(720.),
                            },
                            cx,
                        ),
                    )),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    // 关窗 → 隐藏到托盘;真正退出走托盘菜单
                    window.on_window_should_close(cx, |window, cx| {
                        desktop::hide_window(window, cx);
                        false
                    });
                    cx.new(|cx| TaskListView::new(core.clone(), window, cx))
                },
            )
            .expect("打开主窗口失败");

            desktop::register_main_window(handle, cx);
            main_window::set_main_view(cx, view);
            if background {
                main_window::append_system_log(
                    cx,
                    "管理器已由保活启动；继续后续调度，中断的任务不会自动补跑。".into(),
                );
            }

            // UI 事件循环:async-channel → 视图更新
            cx.spawn(async move |cx| {
                while let Ok(event) = async_rx.recv().await {
                    let mut batch = vec![event];
                    while batch.len() < 128 {
                        match async_rx.try_recv() {
                            Ok(event) => batch.push(event),
                            Err(_) => break,
                        }
                    }
                    cx.update(|cx| {
                        for event in batch {
                            main_window::apply_event_to_view(cx, event);
                        }
                    });
                }
            })
            .detach();

            tracing::info!("主窗口已打开");
        });
    daemon.shutdown();
    scheduled_task_core::logger::shutdown_logging();
}

fn show_startup_error(message: &str) {
    eprintln!("启动失败：{message}");
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        use windows::core::{PCWSTR, w};
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("定时任务管理器启动失败"),
            MB_OK | MB_ICONERROR,
        );
    }
}
