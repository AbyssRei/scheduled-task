//! 桌面集成:托盘菜单、主窗口句柄和原生窗口显隐。

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, Global, MenuItem, actions};
use gpui_tray::Tray;
use std::sync::atomic::{AtomicBool, Ordering};
actions!(
    scheduled_task,
    [TrayQuit, TrayShow, TrayDisableKeepaliveQuit]
);

// 设置与托盘共用一个操作门闩,避免注册/删除计划任务交错执行。
static KEEPALIVE_CHANGING: AtomicBool = AtomicBool::new(false);

pub struct KeepaliveChange {
    pub result: Result<(), String>,
    pub enabled: Result<bool, String>,
    _operation: KeepaliveOperation,
}

struct KeepaliveOperation;

impl Drop for KeepaliveOperation {
    fn drop(&mut self) {
        KEEPALIVE_CHANGING.store(false, Ordering::Release);
    }
}

pub fn keepalive_changing() -> bool {
    KEEPALIVE_CHANGING.load(Ordering::Acquire)
}

/// 注册计划任务可能耗时,必须在工作线程执行。
pub fn change_keepalive(enabled: bool) -> Result<async_channel::Receiver<KeepaliveChange>, String> {
    KEEPALIVE_CHANGING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "保活设置正在处理中,请稍候再试".to_string())?;
    let (tx, rx) = async_channel::bounded(1);
    if let Err(error) = std::thread::Builder::new()
        .name("keepalive-settings".into())
        .spawn(move || {
            let operation = KeepaliveOperation;
            let result = scheduled_task_keepalive::set_enabled(enabled);
            let enabled = scheduled_task_keepalive::enabled();
            let _ = tx.send_blocking(KeepaliveChange {
                result,
                enabled,
                _operation: operation,
            });
        })
    {
        KEEPALIVE_CHANGING.store(false, Ordering::Release);
        return Err(format!("无法启动保活设置线程: {error}"));
    }
    Ok(rx)
}

struct MainWindowHandle(gpui_kit::AnyWindowHandle);
impl Global for MainWindowHandle {}

// Tray 句柄必须常驻(保活托盘);字段仅作存续,不读
#[allow(dead_code)]
struct TrayHandle(Tray);
impl Global for TrayHandle {}

pub fn init_tray(cx: &mut App) -> bool {
    cx.on_action(|_: &TrayQuit, cx| {
        if keepalive_changing() {
            show_keepalive_error("保活设置正在处理中,请稍候再退出".into(), cx);
        } else {
            cx.quit();
        }
    });
    cx.on_action(|_: &TrayDisableKeepaliveQuit, cx| {
        let rx = match change_keepalive(false) {
            Ok(rx) => rx,
            Err(error) => {
                show_keepalive_error(error, cx);
                return;
            }
        };
        cx.spawn(async move |cx| {
            let result = rx.recv().await;
            cx.update(|cx| match result {
                Ok(KeepaliveChange {
                    result: Ok(()),
                    enabled: Ok(false),
                    ..
                }) => cx.quit(),
                Ok(change) => {
                    let message = match (change.result, change.enabled) {
                        (Err(error), _) | (_, Err(error)) => error,
                        _ => "保活仍处于开启状态,请关闭后重试".into(),
                    };
                    show_keepalive_error(message, cx);
                }
                Err(error) => show_keepalive_error(format!("保活设置未完成: {error}"), cx),
            });
        })
        .detach();
    });
    cx.on_action(|_: &TrayShow, cx| {
        if let Some(handle) = cx.try_global::<MainWindowHandle>() {
            let _ = handle.0.update(cx, |_, window, _| {
                show_window(window);
                window.activate_window();
            });
        }
    });

    match build_tray(cx) {
        Ok(tray) => {
            cx.set_global(TrayHandle(tray));
            tracing::info!("托盘创建成功");
            true
        }
        Err(e) => {
            tracing::warn!("托盘创建失败(不阻断): {e}");
            false
        }
    }
}

fn show_keepalive_error(message: String, cx: &mut App) {
    tracing::warn!("保活操作未完成: {message}");
    if let Some(handle) = cx.try_global::<MainWindowHandle>() {
        let _ = handle.0.update(cx, |_, window, cx| {
            show_window(window);
            window.activate_window();
            window.open_alert_dialog(cx, move |alert, _, _| {
                alert
                    .title("保活操作未完成")
                    .description(message.clone())
                    .show_cancel(false)
            });
        });
    }
}

/// 保存主窗口句柄,供托盘的“显示窗口”动作恢复窗口。
pub fn register_main_window(handle: gpui_kit::AnyWindowHandle, cx: &mut App) {
    cx.set_global(MainWindowHandle(handle));
}
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;

/// 经 raw-window-handle 取本窗口 HWND 并调用 ShowWindow。
/// - `SW_HIDE`:关窗时隐藏到托盘
/// - `SW_RESTORE`:托盘"显示窗口"恢复(顺带还原最小化状态)
#[cfg(windows)]
fn with_native_window(window: &gpui_kit::Window, show_cmd: SHOW_WINDOW_CMD) -> bool {
    use raw_window_handle::RawWindowHandle;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::ShowWindow;

    // gpui Window 的固有 window_handle() 返回 AnyWindowHandle,
    // 须经 trait 全路径调用取 rwh 的 WindowHandle,再 as_raw 拿 Win32 句柄
    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return false;
    };
    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
        // SAFETY:ShowWindow 仅切换可见性;HWND 来自 gpui 自身的窗口实现
        unsafe {
            let _ = ShowWindow(HWND(win32.hwnd.get() as *mut _), show_cmd);
        }
        true
    } else {
        false
    }
}

#[cfg(windows)]
pub fn hide_window(window: &gpui_kit::Window) {
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    with_native_window(window, SW_HIDE);
}

#[cfg(windows)]
fn show_window(window: &gpui_kit::Window) {
    use windows::Win32::UI::WindowsAndMessaging::SW_RESTORE;
    with_native_window(window, SW_RESTORE);
}

#[cfg(not(windows))]
pub fn hide_window(_: &gpui_kit::Window) {}

#[cfg(not(windows))]
fn show_window(_: &gpui_kit::Window) {}

fn build_tray(cx: &mut App) -> gpui_tray::Result<Tray> {
    Tray::builder()
        .icon(tray_icon()?)
        .title("定时任务管理器")
        .tooltip("定时任务管理器")
        .menu(|_| {
            vec![
                MenuItem::action("显示窗口", TrayShow),
                MenuItem::separator(),
                MenuItem::action("退出", TrayQuit),
                MenuItem::action("关闭保活并退出", TrayDisableKeepaliveQuit),
            ]
        })
        .build(cx)
}

fn tray_icon() -> gpui_tray::Result<gpui_tray::Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let offset = ((y * SIZE + x) * 4) as usize;
            let dx = x as i32 - 15;
            let dy = y as i32 - 15;
            if dx * dx + dy * dy <= 14 * 14 {
                rgba[offset..offset + 4].copy_from_slice(&[230, 126, 34, 255]);
            }
        }
    }
    let rgba: std::sync::Arc<[u8]> = rgba.into();
    gpui_tray::Icon::from_rgba(rgba, SIZE, SIZE)
}
