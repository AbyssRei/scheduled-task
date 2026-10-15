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

use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command};

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
use windows::core::Result as WinResult;

/// CREATE_SUSPENDED 创建标志。
const CREATE_SUSPENDED_FLAG: u32 = 0x0000_0004;

/// Job Object 句柄守护。持有期间作业内进程存活;drop 只关句柄
/// (KILL_ON_JOB_CLOSE 意味着最后一把句柄关闭时作业内进程被内核终止,
/// 因此守护与子进程同生命周期)。
pub struct JobGuard {
    handle: HANDLE,
}

unsafe impl Send for JobGuard {}

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
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(Self { handle })
        }
    }

    /// 以 CREATE_SUSPENDED 启动命令 → 分配进作业 → 恢复主线程。
    pub fn spawn_into_job(command: &mut Command) -> Result<(Child, Self), JobError> {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(CREATE_SUSPENDED_FLAG);
        let child = command.spawn()?;
        let job = Self::create()?;
        unsafe {
            AssignProcessToJobObject(job.handle, HANDLE(child.as_raw_handle()))?;
        }
        resume_main_thread(child.id()).map_err(|_| JobError::ResumeFailed)?;
        Ok((child, job))
    }

    /// 终止作业内全部进程(树状终止,含孙进程)。
    pub fn terminate(&self) {
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            // 先确保作业内进程已死,再关句柄;即便忘记 terminate,
            // KILL_ON_JOB_CLOSE 也在句柄关闭时兜底。
            let _ = TerminateJobObject(self.handle, 1);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 恢复进程的主线程(快照里该进程的第一个线程)。
fn resume_main_thread(pid: u32) -> WinResult<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?;
        let close = |h: HANDLE| {
            let _ = CloseHandle(h);
        };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let result = (|| -> WinResult<()> {
            Thread32First(snapshot, &mut entry)?;
            loop {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)?;
                    ResumeThread(thread);
                    close(thread);
                    return Ok(());
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    return Err(windows::core::Error::from_thread());
                }
            }
        })();
        close(snapshot);
        result
    }
}

/// 系统 ANSI 代码页 → 字符集(GBK / UTF-8)。
pub fn system_charset() -> crate::runner::Charset {
    use crate::runner::Charset;
    match system_acp() {
        936 => Charset::Gbk, // 简体中文 GBK
        _ => Charset::Utf8,  // 其余代码页按 UTF-8 lossy 兜底
    }
}

fn system_acp() -> u32 {
    unsafe { windows::Win32::Globalization::GetACP() }
}
