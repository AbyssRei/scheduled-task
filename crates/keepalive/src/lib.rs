//! 当前用户级的管理器保活：配置、跨进程单实例、计划任务与守护循环。

use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod platform;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Config {
    enabled: bool,
    executable: PathBuf,
}

/// 持有期间当前用户只能运行一个管理器；必须保存到 GUI 生命周期结束。
pub struct InstanceGuard {
    _file: File,
}

fn directory() -> Result<PathBuf, String> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("scheduled-task"))
        .ok_or_else(|| "无法定位当前用户配置目录".into())
}

fn read_config(dir: &Path) -> Result<Config, String> {
    match fs::read_to_string(dir.join("keepalive.ron")) {
        Ok(text) => ron::from_str(&text).map_err(|e| format!("保活配置无效：{e}")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("读取保活配置失败：{e}")),
    }
}

static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("配置路径没有父目录")?;
    fs::create_dir_all(parent).map_err(|e| format!("创建保活目录失败：{e}"))?;
    let temporary = parent.join(format!(
        ".keepalive-{}-{}.tmp",
        std::process::id(),
        FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| format!("创建保活临时文件失败：{e}"))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("写入保活配置失败：{e}"))?;
        drop(file);
        #[cfg(windows)]
        platform::replace_file(&temporary, path)?;
        #[cfg(not(windows))]
        fs::rename(&temporary, path).map_err(|e| format!("替换保活配置失败：{e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn save_config(dir: &Path, config: &Config) -> Result<(), String> {
    let text = ron::ser::to_string_pretty(config, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("序列化保活配置失败：{e}"))?;
    atomic_write(&dir.join("keepalive.ron"), text.as_bytes())
}

fn lock_file(dir: &Path, name: &str, blocking: bool) -> Result<Option<InstanceGuard>, String> {
    fs::create_dir_all(dir).map_err(|e| format!("创建保活目录失败：{e}"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(name))
        .map_err(|e| format!("打开实例锁失败：{e}"))?;
    if blocking {
        file.lock()
            .map_err(|e| format!("获取配置操作锁失败：{e}"))?;
    } else {
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(e) => return Err(format!("获取实例锁失败：{e}")),
        }
    }
    Ok(Some(InstanceGuard { _file: file }))
}

pub fn enabled() -> Result<bool, String> {
    Ok(read_config(&directory()?)?.enabled)
}

pub fn acquire_app_instance() -> Result<Option<InstanceGuard>, String> {
    acquire_app_instance_with_restore(true)
}

/// 后台恢复使用 false，避免竞争启动时唤醒已有窗口。
pub fn acquire_app_instance_with_restore(restore: bool) -> Result<Option<InstanceGuard>, String> {
    let dir = directory()?;
    let guard = lock_file(&dir, "app.lock", false)?;
    if guard.is_some() {
        atomic_write(
            &dir.join("app.pid"),
            std::process::id().to_string().as_bytes(),
        )?;
    } else if restore {
        #[cfg(windows)]
        if let Ok(pid) = fs::read_to_string(dir.join("app.pid"))
            && let Ok(pid) = pid.parse::<u32>()
        {
            platform::restore_window(pid);
        }
    }
    Ok(guard)
}

fn spawn(executable: &Path, argument: &str) -> Result<(), String> {
    if !executable.is_absolute() || !executable.is_file() {
        return Err(format!(
            "保活程序路径不存在或不是绝对路径：{}",
            executable.display()
        ));
    }
    let mut command = Command::new(executable);
    command
        .arg(argument)
        .current_dir(executable.parent().ok_or("程序路径没有父目录")?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
    }
    command
        .spawn()
        .map_err(|e| format!("启动保活程序失败：{e}"))?;
    Ok(())
}

fn ensure_watchdog_inner(dir: &Path) -> Result<(), String> {
    let config = read_config(dir)?;
    if !config.enabled {
        return Ok(());
    }
    if let Some(guard) = lock_file(dir, "watchdog.lock", false)? {
        drop(guard);
        spawn(&config.executable, "--watchdog")?;
    }
    Ok(())
}

pub fn ensure_watchdog() -> Result<(), String> {
    let dir = directory()?;
    let _control = lock_file(&dir, "keepalive-control.lock", true)?;
    ensure_watchdog_inner(&dir)
}

/// 开启失败会回滚开关；关闭时先落盘，清理失败也不会继续拉起。
pub fn set_enabled(value: bool) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = value;
        Err("保活仅支持 Windows".into())
    }
    #[cfg(windows)]
    {
        let dir = directory()?;
        let executable = std::env::current_exe().map_err(|e| format!("获取程序路径失败：{e}"))?;
        update_config(&dir, value, executable, &SystemIntegration)
    }
}

trait Integration {
    fn register(&self, dir: &Path, executable: &Path) -> Result<(), String>;
    fn unregister(&self) -> Result<(), String>;
    fn ensure(&self, dir: &Path) -> Result<(), String>;
    fn save(&self, dir: &Path, config: &Config) -> Result<(), String> {
        save_config(dir, config)
    }
}

#[cfg(windows)]
struct SystemIntegration;

#[cfg(windows)]
impl Integration for SystemIntegration {
    fn register(&self, dir: &Path, executable: &Path) -> Result<(), String> {
        platform::register_task(dir, executable)
    }
    fn unregister(&self) -> Result<(), String> {
        platform::unregister_task()
    }
    fn ensure(&self, dir: &Path) -> Result<(), String> {
        ensure_watchdog_inner(dir)
    }
}

fn update_config(
    dir: &Path,
    value: bool,
    executable: PathBuf,
    integration: &impl Integration,
) -> Result<(), String> {
    let _control = lock_file(dir, "keepalive-control.lock", true)?;
    let previous = match read_config(dir) {
        Ok(config) => config,
        Err(_) if !value => Config::default(),
        Err(error) => return Err(error),
    };
    if !value {
        integration.save(
            dir,
            &Config {
                enabled: false,
                ..previous
            },
        )?;
        return integration
            .unregister()
            .map_err(|e| format!("保活已关闭，但计划任务清理失败：{e}"));
    }
    let config = Config {
        enabled: true,
        executable,
    };
    integration.register(dir, &config.executable)?;
    let result = integration
        .save(dir, &config)
        .and_then(|()| integration.ensure(dir));
    if let Err(error) = result {
        let rollback = integration.save(dir, &previous);
        let cleanup = if previous.enabled {
            integration.register(dir, &previous.executable)
        } else {
            integration.unregister()
        };
        return Err(format!(
            "开启保活失败：{error}{}{}",
            rollback
                .err()
                .map(|e| format!("；恢复开关失败：{e}"))
                .unwrap_or_default(),
            cleanup
                .err()
                .map(|e| format!("；恢复计划任务失败：{e}"))
                .unwrap_or_default()
        ));
    }
    Ok(())
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(match attempt {
        0 => 3,
        1 => 10,
        2 => 30,
        _ => 60,
    })
}

struct RestartPolicy {
    attempt: u32,
    running_since: Option<Duration>,
    next_launch: Duration,
}

impl RestartPolicy {
    fn new() -> Self {
        Self {
            attempt: 0,
            running_since: None,
            next_launch: retry_delay(0),
        }
    }

    fn observe(&mut self, running: bool, now: Duration) {
        if running {
            let start = self.running_since.get_or_insert(now);
            if now.saturating_sub(*start) >= Duration::from_secs(300) {
                self.attempt = 0;
            }
        } else if self.running_since.take().is_some() {
            self.next_launch = now + retry_delay(self.attempt);
        }
    }

    fn ready(&self, now: Duration) -> bool {
        self.running_since.is_none() && now >= self.next_launch
    }

    fn launched(&mut self, now: Duration) {
        self.attempt = self.attempt.saturating_add(1);
        // 即使启动失败或尚未获得实例锁也退避；给初始化至少五秒。
        self.next_launch = now + retry_delay(self.attempt).max(Duration::from_secs(5));
    }
}

/// 由 GUI 可执行文件的 --watchdog 入口调用；不初始化 GUI 或任务执行器。
pub fn run_watchdog() -> Result<(), String> {
    #[cfg(not(windows))]
    return Err("保活仅支持 Windows".into());
    #[cfg(windows)]
    {
        let dir = directory()?;
        let Some(_guard) = lock_file(&dir, "watchdog.lock", false)? else {
            return Ok(());
        };
        if !read_config(&dir)?.enabled {
            return Ok(());
        }
        let session = platform::SessionMonitor::new()?;
        tracing::info!("保活守护已启动");
        let origin = Instant::now();
        let mut policy = RestartPolicy::new();
        loop {
            if session.ending() {
                tracing::info!("用户会话结束，保活守护退出");
                return Ok(());
            }
            if !read_config(&dir)?.enabled {
                tracing::info!("保活已关闭，守护退出");
                return Ok(());
            }
            if session.pending() {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            let now = origin.elapsed();
            let app_lock = lock_file(&dir, "app.lock", false)?;
            if app_lock.is_some() && policy.running_since.is_some() {
                tracing::info!(
                    delay_seconds = retry_delay(policy.attempt).as_secs(),
                    "管理器已退出，等待重新启动"
                );
            }
            policy.observe(app_lock.is_none(), now);
            if app_lock.is_some() {
                drop(app_lock);
                if policy.ready(now)
                    && let Some(_control) = lock_file(&dir, "keepalive-control.lock", false)?
                {
                    // 与关闭保活共享操作锁；关闭成功返回后不会再有在途拉起。
                    let config = read_config(&dir)?;
                    if !config.enabled || session.ending() {
                        return Ok(());
                    }
                    if session.pending() {
                        continue;
                    }
                    if let Some(app_lock) = lock_file(&dir, "app.lock", false)? {
                        drop(app_lock);
                        if let Err(error) = spawn(&config.executable, "--background") {
                            tracing::error!("保活拉起失败：{error}");
                        } else {
                            tracing::info!(executable = %config.executable.display(), "保活已请求后台启动管理器");
                        }
                        policy.launched(now);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn task_xml(executable: &Path, sid: &str) -> Result<String, String> {
    if !executable.is_absolute() {
        return Err("计划任务必须使用绝对程序路径".into());
    }
    let executable_text = executable.to_str().ok_or("程序路径包含无效 Unicode")?;
    let working_dir = executable
        .parent()
        .and_then(Path::to_str)
        .ok_or("程序目录无效")?;
    let sid = xml_escape(sid);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>定时任务管理器保活；关闭保活后此任务不会拉起程序。</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger>
    <CalendarTrigger><Repetition><Interval>PT1M</Interval><Duration>P1D</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><StartBoundary>2020-01-01T00:00:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>
  </Triggers>
  <Principals><Principal id="CurrentUser"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="CurrentUser"><Exec><Command>{}</Command><Arguments>--watchdog</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>"#,
        xml_escape(executable_text),
        xml_escape(working_dir)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "scheduled-task-keepalive-test-{}-{}",
            std::process::id(),
            FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_configuration_is_disabled_and_invalid_configuration_is_not_ignored() {
        let dir = temp_dir();
        assert!(!read_config(&dir).unwrap().enabled);
        fs::write(dir.join("keepalive.ron"), "not ron").unwrap();
        assert!(read_config(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistence_replaces_existing_configuration_and_keeps_unicode_paths() {
        let dir = temp_dir();
        let config = Config {
            enabled: true,
            executable: dir.join("中文 & 工具.exe"),
        };
        save_config(&dir, &config).unwrap();
        assert_eq!(read_config(&dir).unwrap(), config);
        save_config(
            &dir,
            &Config {
                enabled: false,
                ..config
            },
        )
        .unwrap();
        assert!(!read_config(&dir).unwrap().enabled);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn separate_handles_are_excluded_and_lock_releases_on_drop() {
        let dir = temp_dir();
        let first = lock_file(&dir, "app.lock", false).unwrap().unwrap();
        assert!(lock_file(&dir, "app.lock", false).unwrap().is_none());
        drop(first);
        assert!(lock_file(&dir, "app.lock", false).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(
            [0, 1, 2, 3, u32::MAX].map(|n| retry_delay(n).as_secs()),
            [3, 10, 30, 60, 60]
        );
    }

    #[test]
    fn repeated_crashes_follow_all_delays_and_stable_run_resets_backoff() {
        let mut policy = RestartPolicy::new();
        let mut now = Duration::ZERO;
        for seconds in [3, 10, 30, 60, 60] {
            policy.observe(true, now);
            now += Duration::from_secs(1);
            policy.observe(false, now);
            assert!(!policy.ready(now + Duration::from_secs(seconds - 1)));
            now += Duration::from_secs(seconds);
            assert!(policy.ready(now));
            policy.launched(now);
        }
        policy.observe(true, now);
        now += Duration::from_secs(300);
        policy.observe(true, now);
        policy.observe(false, now);
        assert!(!policy.ready(now + Duration::from_secs(2)));
        assert!(policy.ready(now + Duration::from_secs(3)));
    }

    #[test]
    fn failed_start_is_backed_off_even_without_observed_running_process() {
        let mut policy = RestartPolicy::new();
        assert!(!policy.ready(Duration::from_secs(2)));
        for now in [3, 13, 43, 103] {
            assert!(policy.ready(Duration::from_secs(now)));
            policy.launched(Duration::from_secs(now));
            policy.observe(false, Duration::from_secs(now + 1));
            assert!(!policy.ready(Duration::from_secs(now + 1)));
        }
    }

    #[derive(Default)]
    struct FakeIntegration {
        fail_register: bool,
        fail_save: bool,
        fail_ensure: bool,
        fail_delete: bool,
        actions: std::cell::RefCell<Vec<String>>,
    }
    impl Integration for FakeIntegration {
        fn register(&self, _: &Path, executable: &Path) -> Result<(), String> {
            self.actions
                .borrow_mut()
                .push(format!("register:{}", executable.display()));
            if self.fail_register {
                Err("模拟注册失败".into())
            } else {
                Ok(())
            }
        }
        fn unregister(&self) -> Result<(), String> {
            self.actions.borrow_mut().push("delete".into());
            if self.fail_delete {
                Err("模拟删除失败".into())
            } else {
                Ok(())
            }
        }
        fn ensure(&self, dir: &Path) -> Result<(), String> {
            assert!(read_config(dir)?.enabled);
            self.actions.borrow_mut().push("spawn".into());
            if self.fail_ensure {
                Err("模拟启动失败".into())
            } else {
                Ok(())
            }
        }
        fn save(&self, dir: &Path, config: &Config) -> Result<(), String> {
            if self.fail_save && config.enabled {
                return Err("模拟保存失败".into());
            }
            save_config(dir, config)
        }
    }

    #[test]
    fn failed_registration_does_not_enable_or_spawn() {
        let dir = temp_dir();
        let fake = FakeIntegration {
            fail_register: true,
            ..Default::default()
        };
        assert!(update_config(&dir, true, dir.join("app.exe"), &fake).is_err());
        assert!(!read_config(&dir).unwrap().enabled);
        assert_eq!(fake.actions.borrow().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_save_or_spawn_rolls_back_disabled_configuration_and_registration() {
        for fail_save in [true, false] {
            let dir = temp_dir();
            let fake = FakeIntegration {
                fail_save,
                fail_ensure: !fail_save,
                ..Default::default()
            };
            assert!(update_config(&dir, true, dir.join("app.exe"), &fake).is_err());
            assert!(!read_config(&dir).unwrap().enabled);
            assert_eq!(fake.actions.borrow().last().unwrap(), "delete");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn failed_upgrade_restores_previous_executable_registration() {
        let dir = temp_dir();
        let previous = Config {
            enabled: true,
            executable: dir.join("old.exe"),
        };
        save_config(&dir, &previous).unwrap();
        let fake = FakeIntegration {
            fail_ensure: true,
            ..Default::default()
        };
        assert!(update_config(&dir, true, dir.join("new.exe"), &fake).is_err());
        assert_eq!(read_config(&dir).unwrap(), previous);
        assert_eq!(
            fake.actions.borrow().last().unwrap(),
            &format!("register:{}", previous.executable.display())
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_delete_keeps_disabled_and_damaged_configuration_can_be_disabled() {
        let dir = temp_dir();
        fs::write(dir.join("keepalive.ron"), "broken").unwrap();
        let fake = FakeIntegration {
            fail_delete: true,
            ..Default::default()
        };
        let error = update_config(&dir, false, dir.join("app.exe"), &fake).unwrap_err();
        assert!(error.contains("保活已关闭"));
        assert!(!read_config(&dir).unwrap().enabled);
        // 延迟到达的计划任务入口在 false 状态下不尝试执行不存在的 exe。
        assert!(ensure_watchdog_inner(&dir).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disabling_waits_for_inflight_control_operation_and_prevents_subsequent_launch() {
        let dir = temp_dir();
        save_config(
            &dir,
            &Config {
                enabled: true,
                executable: dir.join("app.exe"),
            },
        )
        .unwrap();
        let control = lock_file(&dir, "keepalive-control.lock", true).unwrap();
        let worker_dir = dir.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = update_config(
                &worker_dir,
                false,
                worker_dir.join("app.exe"),
                &FakeIntegration::default(),
            );
            finished_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(read_config(&dir).unwrap().enabled);
        drop(control);
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        let _control = lock_file(&dir, "keepalive-control.lock", true).unwrap();
        assert!(!read_config(&dir).unwrap().enabled);
        assert!(ensure_watchdog_inner(&dir).is_ok());
        drop(_control);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "仅由跨进程锁测试作为隔离子进程启动"]
    fn lock_child() {
        let Some(dir) = std::env::var_os("SCHEDULED_TASK_LOCK_TEST_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let _guard = lock_file(&dir, "app.lock", false).unwrap().unwrap();
        fs::write(dir.join("ready"), "ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn process_crash_releases_instance_lock() {
        let dir = temp_dir();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::lock_child", "--ignored"])
            .env("SCHEDULED_TASK_LOCK_TEST_DIR", &dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.join("ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = dir.join("ready").exists();
        let blocked = lock_file(&dir, "app.lock", false).unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready && blocked);
        assert!(lock_file(&dir, "app.lock", false).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn task_xml_escapes_path_and_uses_interactive_non_elevated_account() {
        let path = std::env::temp_dir()
            .join("中文 & <项目>")
            .join("scheduled-task.exe");
        let xml = task_xml(&path, "S-1-5-21-123").unwrap();
        assert!(xml.contains("中文 &amp; &lt;项目&gt;"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<Interval>PT1M</Interval>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<Arguments>--watchdog</Arguments>"));
        assert!(!xml.contains("powershell"));
        assert!(task_xml(Path::new("relative.exe"), "sid").is_err());
    }
}
