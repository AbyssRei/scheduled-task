//! Windows Job Object 进程树管理(windows-rs 的唯一职责)。
//!
//! `CreateJobObjectW` + `SetInformationJobObject(KILL_ON_JOB_CLOSE)` +
//! `AssignProcessToJobObject` + `TerminateJobObject`。子进程 CREATE_SUSPENDED
//! 挂起 → 入 Job → 恢复,防孙进程在入 Job 前逃逸。
//!
//! 作业统一管理 bat 链 `cmd.exe → 工作进程` 及其后代,实现整个进程树的终止。
//! 管理器自身崩溃时,KILL_ON_JOB_CLOSE 保证句柄随进程消亡后,
//! 作业内全部进程由操作系统内核终止。

#![cfg(windows)]

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command};

use thiserror::Error;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};
use windows::core::Result as WinResult;

/// Job Object 句柄守护。持有期间作业内进程存活;drop 只关句柄
/// (KILL_ON_JOB_CLOSE 意味着最后一把句柄关闭时作业内进程被内核终止,
/// 因此守护与子进程同生命周期)。
pub struct JobGuard {
    handle: OwnedHandle,
}

/// 启动尚未完成时，任何错误或异常都回收挂起进程。
struct SuspendedChild(Option<Child>);
impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug, Error)]
pub enum JobError {
    #[error("Windows API 调用失败: {0}")]
    Api(#[from] windows::core::Error),
    #[error("进程创建失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("无法恢复被挂起的子进程")]
    ResumeFailed,
}

impl JobGuard {
    /// 创建带 KILL_ON_JOB_CLOSE 的作业对象。
    pub fn create() -> Result<Self, JobError> {
        unsafe {
            let handle = CreateJobObjectW(None, None)?;
            let job = Self {
                handle: OwnedHandle::from_raw_handle(handle.0),
            };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(job)
        }
    }

    /// 无控制台窗口、挂起启动命令 → 分配进作业 → 恢复主线程。
    pub fn spawn_into_job(command: &mut Command) -> Result<(Child, Self), JobError> {
        Self::spawn_with(
            command,
            Self::create,
            |job, child| unsafe {
                AssignProcessToJobObject(job.raw_handle(), HANDLE(child.as_raw_handle()))
            },
            resume_main_thread,
        )
    }

    fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    fn spawn_with(
        command: &mut Command,
        create: impl FnOnce() -> Result<Self, JobError>,
        assign: impl FnOnce(&Self, &Child) -> WinResult<()>,
        resume: impl FnOnce(u32) -> WinResult<()>,
    ) -> Result<(Child, Self), JobError> {
        use std::os::windows::process::CommandExt as _;
        let job = create()?;
        // 创建时隐藏控制台,避免先弹窗再隐藏造成闪烁;输出仍通过管道捕获。
        command.creation_flags((CREATE_SUSPENDED | CREATE_NO_WINDOW).0);
        let mut pending = SuspendedChild(Some(command.spawn()?));
        let child = pending.0.as_ref().unwrap();
        assign(&job, child)?;
        resume(child.id()).map_err(|_| JobError::ResumeFailed)?;
        Ok((pending.0.take().unwrap(), job))
    }

    /// 终止作业内全部进程(树状终止,含孙进程)。
    pub fn terminate(&self) {
        unsafe {
            let _ = TerminateJobObject(self.raw_handle(), 1);
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            // 先确保作业内进程已死,再关句柄;即便忘记 terminate,
            // KILL_ON_JOB_CLOSE 也在句柄关闭时兜底。
            let _ = TerminateJobObject(self.raw_handle(), 1);
        }
    }
}

/// 恢复进程的主线程(快照里该进程的第一个线程)。
fn resume_main_thread(pid: u32) -> WinResult<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?;
        let snapshot = OwnedHandle::from_raw_handle(snapshot.0);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        Thread32First(HANDLE(snapshot.as_raw_handle()), &mut entry)?;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)?;
                let thread = OwnedHandle::from_raw_handle(thread.0);
                return resume_thread(HANDLE(thread.as_raw_handle()));
            }
            if Thread32Next(HANDLE(snapshot.as_raw_handle()), &mut entry).is_err() {
                return Err(windows::core::Error::from_thread());
            }
        }
    }
}

fn resume_thread(thread: HANDLE) -> WinResult<()> {
    if unsafe { ResumeThread(thread) } == u32::MAX {
        Err(windows::core::Error::from_thread())
    } else {
        Ok(())
    }
}

/// 系统 ANSI 代码页 → 字符集(GBK / UTF-8)。
pub fn system_charset() -> crate::runner::Charset {
    use crate::runner::Charset;
    match system_acp() {
        936 => Charset::Gbk,
        _ => Charset::Utf8,
    }
}

fn system_acp() -> u32 {
    unsafe { windows::Win32::Globalization::GetACP() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsHandle;
    use windows::Win32::System::Threading::GetExitCodeProcess;

    fn failure() -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005_u32 as i32))
    }

    #[test]
    fn 作业创建失败不创建进程且分配和恢复失败均回收挂起进程() {
        let mut command = Command::new("不应被创建的程序");
        assert!(matches!(
            JobGuard::spawn_with(
                &mut command,
                || Err(JobError::Api(failure())),
                |_, _| panic!("不应分配"),
                |_| panic!("不应恢复")
            ),
            Err(JobError::Api(_))
        ));
        assert!(resume_thread(HANDLE::default()).is_err());
        for fail_assignment in [true, false] {
            let mut command = Command::new("cmd");
            command.args(["/d", "/c", "exit 0"]);
            let mut process_handle = None;
            let result = JobGuard::spawn_with(
                &mut command,
                JobGuard::create,
                |job, child| {
                    process_handle = Some(child.as_handle().try_clone_to_owned().unwrap());
                    if fail_assignment {
                        Err(failure())
                    } else {
                        unsafe {
                            AssignProcessToJobObject(
                                job.raw_handle(),
                                HANDLE(child.as_raw_handle()),
                            )
                        }
                    }
                },
                |_| Err(failure()),
            );
            assert!(result.is_err());
            let mut code = 259;
            unsafe {
                GetExitCodeProcess(HANDLE(process_handle.unwrap().as_raw_handle()), &mut code)
                    .unwrap();
            }
            assert_ne!(code, 259, "失败路径不得遗留挂起进程");
        }
    }
}
