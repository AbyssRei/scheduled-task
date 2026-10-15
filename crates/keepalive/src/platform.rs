use std::{
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, HWND, LPARAM, LRESULT, LocalFree, WPARAM},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            RemoteDesktop::{
                ProcessIdToSessionId, WTS_CONNECTSTATE_CLASS, WTSActive, WTSConnectState,
                WTSConnected, WTSDisconnected, WTSFreeMemory, WTSQuerySessionInformationW,
            },
            TaskScheduler::{
                ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
                TaskScheduler,
            },
            Threading::{GetCurrentProcess, OpenProcessToken},
            Variant::VARIANT,
        },
        UI::WindowsAndMessaging::*,
    },
    core::{BOOL, BSTR, HRESULT, PCWSTR, PWSTR, w},
};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub fn replace_file(source: &Path, target: &Path) -> Result<(), String> {
    unsafe {
        MoveFileExW(
            PCWSTR(wide(source).as_ptr()),
            PCWSTR(wide(target).as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| format!("原子替换保活配置失败：{e}"))
    }
}

fn current_sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| format!("读取当前用户身份失败：{e}"))?;
        let result = (|| {
            let mut length = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut length);
            if length == 0 {
                return Err("无法读取当前用户 SID 长度".into());
            }
            // usize 缓冲保证 TOKEN_USER 的对齐要求。
            let mut buffer = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
            .map_err(|e| format!("读取当前用户 SID 失败：{e}"))?;
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid)
                .map_err(|e| format!("转换当前用户 SID 失败：{e}"))?;
            let text = sid
                .to_string()
                .map_err(|e| format!("读取 SID 字符串失败：{e}"));
            let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
            text
        })();
        let _ = CloseHandle(token);
        result
    }
}

fn task_name(sid: &str) -> String {
    format!("ScheduledTaskKeepalive-{sid}")
}

struct ComGuard(bool);
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn task_folder() -> Result<(ITaskFolder, ComGuard), String> {
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED);
        // 已有 STA 也可使用任务计划程序；不能反向释放调用者的 COM 初始化。
        if initialized.is_err() && initialized != HRESULT(0x80010106_u32 as i32) {
            return Err(format!("初始化任务计划程序失败：{initialized}"));
        }
        let guard = ComGuard(initialized.is_ok());
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("连接任务计划程序失败：{e}"))?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .map_err(|e| format!("连接当前用户的任务计划程序失败：{e}"))?;
        let folder = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| format!("读取计划任务目录失败：{e}"))?;
        Ok((folder, guard))
    }
}

pub fn register_task(_dir: &Path, executable: &Path) -> Result<(), String> {
    let sid = current_sid()?;
    let xml = super::task_xml(executable, &sid)?;
    let (folder, _guard) = task_folder()?;
    let result = unsafe {
        folder.RegisterTask(
            &BSTR::from(task_name(&sid)),
            &BSTR::from(xml),
            TASK_CREATE_OR_UPDATE.0,
            &VARIANT::from(sid.as_str()),
            &VARIANT::default(),
            TASK_LOGON_INTERACTIVE_TOKEN,
            &VARIANT::default(),
        )
    }
    .map(|_| ())
    .map_err(|e| format!("注册保活计划任务失败：{e}；请检查当前用户的任务计划程序权限"));
    drop(folder);
    result
}

pub fn unregister_task() -> Result<(), String> {
    let sid = current_sid()?;
    let (folder, _guard) = task_folder()?;
    let result = match unsafe { folder.DeleteTask(&BSTR::from(task_name(&sid)), 0) } {
        Ok(()) => Ok(()),
        // 仅明确的文件不存在可视作成功，拒绝访问/服务失败必须反馈。
        Err(error) if error.code() == HRESULT(0x80070002_u32 as i32) => Ok(()),
        Err(error) => Err(format!("删除保活计划任务失败：{error}")),
    };
    drop(folder);
    result
}

pub fn restore_window(pid: u32) {
    unsafe extern "system" fn callback(hwnd: HWND, parameter: LPARAM) -> BOOL {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == parameter.0 as u32 {
                let mut title = [0_u16; 256];
                let length = GetWindowTextW(hwnd, &mut title);
                let title = String::from_utf16_lossy(&title[..length as usize]);
                if title.contains("定时任务管理器") {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            BOOL(1)
        }
    }
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(pid as isize));
    }
}

static SESSION_PENDING: AtomicBool = AtomicBool::new(false);
static SESSION_ENDED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn session_window(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_QUERYENDSESSION => {
            SESSION_PENDING.store(true, Ordering::SeqCst);
            LRESULT(1)
        }
        WM_ENDSESSION => {
            SESSION_ENDED.store(wparam.0 != 0, Ordering::SeqCst);
            SESSION_PENDING.store(false, Ordering::SeqCst);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// 隐藏顶层窗口接收会话结束广播；消息窗口收不到这些广播。
pub struct SessionMonitor(HWND);

impl SessionMonitor {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(session_window),
                lpszClassName: w!("ScheduledTaskKeepaliveSession"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(format!(
                    "注册保活会话窗口失败：{}",
                    windows::core::Error::from_thread()
                ));
            }
            let window = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("保活会话监视"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| format!("创建保活会话窗口失败：{e}"))?;
            Ok(Self(window))
        }
    }

    fn pump(&self) {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    pub fn ending(&self) -> bool {
        self.pump();
        SESSION_ENDED.load(Ordering::SeqCst)
    }

    pub fn pending(&self) -> bool {
        self.pump();
        SESSION_PENDING.load(Ordering::SeqCst)
            || unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
            || !session_connected()
    }
}

fn session_connected() -> bool {
    unsafe {
        let mut session = 0;
        if ProcessIdToSessionId(std::process::id(), &mut session).is_err() {
            return false;
        }
        let mut buffer = PWSTR::null();
        let mut length = 0;
        if WTSQuerySessionInformationW(None, session, WTSConnectState, &mut buffer, &mut length)
            .is_err()
        {
            return false;
        }
        let connected =
            if length as usize >= size_of::<WTS_CONNECTSTATE_CLASS>() && !buffer.is_null() {
                let state = *buffer.0.cast::<WTS_CONNECTSTATE_CLASS>();
                state == WTSActive || state == WTSConnected || state == WTSDisconnected
            } else {
                false
            };
        WTSFreeMemory(buffer.0.cast());
        connected
    }
}

impl Drop for SessionMonitor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
            let _ = UnregisterClassW(w!("ScheduledTaskKeepaliveSession"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_session_window_handles_cancelled_and_confirmed_logoff() {
        SESSION_ENDED.store(false, Ordering::SeqCst);
        SESSION_PENDING.store(false, Ordering::SeqCst);
        let monitor = SessionMonitor::new().unwrap();
        unsafe {
            SendMessageW(
                monitor.0,
                WM_QUERYENDSESSION,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
        }
        assert!(SESSION_PENDING.load(Ordering::SeqCst));
        assert!(!monitor.ending());
        unsafe {
            SendMessageW(monitor.0, WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        assert!(!SESSION_PENDING.load(Ordering::SeqCst));
        assert!(!monitor.ending());
        unsafe {
            SendMessageW(monitor.0, WM_ENDSESSION, Some(WPARAM(1)), Some(LPARAM(0)));
        }
        assert!(monitor.ending());
        drop(monitor);
        SESSION_ENDED.store(false, Ordering::SeqCst);
        // Drop 注销窗口类，后续创建也应成功。
        drop(SessionMonitor::new().unwrap());
    }
}
