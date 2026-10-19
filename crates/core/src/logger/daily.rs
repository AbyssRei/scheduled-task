//! 按记录时间戳的当地日期追加日志,清理时始终保护当前文件。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::LogRetention;
use super::timezone::SharedTimeZone;
use jiff::civil::Date;

const DIRECTORY_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(super) struct SharedLogWriter {
    writer: Arc<Mutex<DailyLogWriter>>,
    timezone: SharedTimeZone,
}

impl SharedLogWriter {
    pub(super) fn new(
        directory: &Path,
        retention: LogRetention,
        timezone: SharedTimeZone,
    ) -> io::Result<Self> {
        let date = timezone.date(jiff::Timestamp::now());
        Ok(Self {
            writer: Arc::new(Mutex::new(DailyLogWriter::new_unpruned_at(
                directory, date, retention,
            )?)),
            timezone,
        })
    }

    pub(super) fn prune(&self) {
        self.writer
            .lock()
            .unwrap()
            .prune(self.timezone.date(jiff::Timestamp::now()));
    }

    pub(super) fn set_retention(&self, retention: LogRetention) {
        self.writer
            .lock()
            .unwrap()
            .set_retention_at(retention, self.timezone.date(jiff::Timestamp::now()));
    }

    /// 同时切换时区和保留策略，防止后台写入使用新时区与旧限制组合清理。
    pub(super) fn apply_settings(
        &self,
        timezone: Option<(super::LogTimeZone, jiff::tz::TimeZone)>,
        retention: Option<LogRetention>,
    ) {
        let mut writer = self.writer.lock().unwrap();
        if let Some((setting, zone)) = timezone {
            self.timezone.set_resolved(setting, zone);
        }
        if let Some(retention) = retention {
            writer.set_retention_at(retention, self.timezone.date(jiff::Timestamp::now()));
        }
    }
}

impl Write for SharedLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut writer = self.writer.lock().unwrap();
        let today = self.timezone.date(jiff::Timestamp::now());
        writer.write_record_at(bytes, today)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.lock().unwrap().file.flush()
    }
}

struct DailyLogWriter {
    directory: PathBuf,
    date: Date,
    file: File,
    retention: LogRetention,
    total_size: u64,
    has_removable_logs: bool,
    last_pruned: Instant,
    oversize_reported: bool,
}

impl DailyLogWriter {
    #[cfg(test)]
    fn new_at(directory: &Path, date: Date, retention: LogRetention) -> io::Result<Self> {
        let mut writer = Self::new_unpruned_at(directory, date, retention)?;
        writer.prune(date);
        Ok(writer)
    }

    fn new_unpruned_at(directory: &Path, date: Date, retention: LogRetention) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let file = open_log(directory, date)?;
        Ok(Self {
            directory: directory.to_owned(),
            date,
            file,
            retention,
            total_size: 0,
            has_removable_logs: false,
            last_pruned: Instant::now(),
            oversize_reported: false,
        })
    }

    fn set_retention_at(&mut self, retention: LogRetention, date: Date) {
        self.retention = retention;
        self.prune(date);
    }

    fn size_limit(&self) -> Option<u64> {
        self.retention
            .size_limit_enabled
            .then_some(u64::from(self.retention.max_size_mb) * 1024 * 1024)
    }

    fn write_record_at(&mut self, bytes: &[u8], today: Date) -> io::Result<usize> {
        // tracing 每次提交一条完整格式化记录。排队期间跨日或切换时区时，
        // 必须按记录自身的日期归档，不能用后台线程写入时的日期替代。
        let date = record_date(bytes).unwrap_or(today);
        self.write_with_dates(bytes, date, today)
    }

    #[cfg(test)]
    fn write_at(&mut self, bytes: &[u8], date: Date) -> io::Result<usize> {
        self.write_with_dates(bytes, date, date)
    }

    fn write_with_dates(&mut self, bytes: &[u8], date: Date, today: Date) -> io::Result<usize> {
        if self.date != date {
            // 新文件打开成功后才替换当前文件,失败时不清理任何日志。
            let file = open_log(&self.directory, date)?;
            self.file.flush()?;
            self.file = file;
            self.date = date;
            self.prune(today);
        }
        // 完整写入一条记录，防止短写重试将缺少时间前缀的后半段写入另一日文件。
        self.file.write_all(bytes)?;
        let written = bytes.len();
        self.total_size = self.total_size.saturating_add(written as u64);
        // 缓存目录总量,自身写入跨过上限时立即清理,无需逐行扫描目录。
        // 定期复核兼顾其他进程写入以及上次删除失败的日志。
        if self.last_pruned.elapsed() >= DIRECTORY_RECHECK_INTERVAL
            || (self.has_removable_logs
                && self
                    .size_limit()
                    .is_some_and(|limit| self.total_size > limit))
        {
            self.prune(today);
        }
        self.report_oversize();
        Ok(written)
    }

    fn report_oversize(&mut self) {
        let oversized = self
            .size_limit()
            .is_some_and(|limit| self.total_size > limit);
        if oversized && !self.oversize_reported {
            eprintln!(
                "日志存储超过大小限制,当前追加文件受到保护,无法删除的日志将在后续清理时重试: {}",
                self.directory.display()
            );
        }
        self.oversize_reported = oversized;
    }

    fn prune(&mut self, today: Date) {
        self.last_pruned = Instant::now();
        // 清理失败时等待下次定期复核,避免每条日志重复扫描和报错。
        self.has_removable_logs = false;
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("读取日志目录失败 {}: {error}", self.directory.display());
                return;
            }
        };
        let mut logs = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("读取日志目录项失败: {error}");
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    eprintln!("读取日志文件类型失败 {}: {error}", entry.path().display());
                    continue;
                }
            };
            // 不跟随符号链接,也不处理子目录或其他类型的目录项。
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            if let Some(date) = name.to_str().and_then(log_date) {
                // Windows 目录项可能尚未同步打开文件的长度,当前文件从句柄读取。
                let metadata = if date == self.date {
                    self.file.metadata()
                } else {
                    entry.metadata()
                };
                match metadata {
                    Ok(metadata) => logs.push((date, entry.path(), metadata.len())),
                    Err(error) => {
                        eprintln!("读取日志大小失败 {}: {error}", entry.path().display());
                    }
                }
            }
        }
        let oldest = today
            .checked_sub(jiff::Span::new().days(i64::from(self.retention.retention_days) - 1))
            .unwrap_or(Date::MIN);
        logs.sort_unstable_by_key(|(date, _, _)| *date);
        self.total_size = logs.iter().map(|(_, _, size)| *size).sum();
        let limit = self.size_limit();
        let mut deletion_failed = false;
        for (date, path, size) in logs {
            // 即使时钟回拨或存在未来日期日志,也不能删除正在追加的文件。
            if date == self.date {
                continue;
            }
            if date < oldest || limit.is_some_and(|limit| self.total_size > limit) {
                match fs::remove_file(&path) {
                    Ok(()) => self.total_size = self.total_size.saturating_sub(size),
                    Err(error) => {
                        deletion_failed = true;
                        eprintln!("清理旧日志失败 {}: {error}", path.display());
                    }
                }
            } else {
                self.has_removable_logs = true;
            }
        }
        if deletion_failed && limit.is_some_and(|limit| self.total_size > limit) {
            self.has_removable_logs = false;
        }
        self.report_oversize();
    }
}

fn record_date(bytes: &[u8]) -> Option<Date> {
    let timestamp =
        std::str::from_utf8(bytes.split(|byte| byte.is_ascii_whitespace()).next()?).ok()?;
    // 先验证整个时间戳，避免误将普通消息中恰好出现的日期作为归档依据。
    timestamp.parse::<jiff::Timestamp>().ok()?;
    timestamp.get(..10)?.parse().ok()
}

fn log_name(date: Date) -> String {
    format!("scheduled_task.{date}.log")
}

fn open_log(directory: &Path, date: Date) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(log_name(date)))
}

fn log_date(name: &str) -> Option<Date> {
    let value = name.strip_prefix("scheduled_task.")?.strip_suffix(".log")?;
    // 限定规范日期格式,不误删备份、临时文件或宽松解析接受的其他名称。
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }
    let date: Date = value.parse().ok()?;
    (date.to_string() == value).then_some(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir();
            loop {
                let path = root.join(format!(
                    "scheduled-task-daily-log-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("创建测试目录失败: {error}"),
                }
            }
        }

        fn seed(&self, date: Date, content: &str) {
            fs::write(self.0.join(log_name(date)), content).unwrap();
        }

        fn read(&self, date: Date) -> String {
            fs::read_to_string(self.0.join(log_name(date))).unwrap()
        }

        fn seed_size(&self, date: Date, size: u64) {
            File::create(self.0.join(log_name(date)))
                .unwrap()
                .set_len(size)
                .unwrap();
        }

        fn log_count(&self) -> usize {
            fs::read_dir(&self.0)
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().unwrap().is_file())
                .filter(|entry| entry.file_name().to_str().and_then(log_date).is_some())
                .count()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("回收测试目录失败");
        }
    }

    fn retention(days: u16) -> LogRetention {
        LogRetention {
            retention_days: days,
            ..LogRetention::default()
        }
    }

    fn day(day: i8) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn write_at(writer: &mut DailyLogWriter, text: &str, date: Date) {
        assert_eq!(writer.write_at(text.as_bytes(), date).unwrap(), text.len());
        writer.file.flush().unwrap();
    }

    #[test]
    fn 同日反复重启追加且五天文件不会减少() {
        let directory = TestDirectory::new();
        // 当天文件最先创建,保留决策仍应只依据日志日期。
        directory.seed(day(15), "启动前\n");
        for value in 11..15 {
            directory.seed(day(value), "历史\n");
        }
        for _ in 0..3 {
            let mut writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
            write_at(&mut writer, "重启后\n", day(15));
            drop(writer);
            assert_eq!(directory.log_count(), 5);
            for value in 11..15 {
                assert_eq!(directory.read(day(value)), "历史\n");
            }
        }
        assert_eq!(directory.read(day(15)), "启动前\n重启后\n重启后\n重启后\n");
    }

    #[test]
    fn 跨日新建后仅删除最早日期文件() {
        let directory = TestDirectory::new();
        for value in (11..=15).rev() {
            directory.seed(day(value), "历史\n");
        }
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
        write_at(&mut writer, "次日\n", day(16));
        assert_eq!(directory.log_count(), 5);
        assert!(!directory.0.join(log_name(day(11))).exists());
        for value in 12..=15 {
            assert_eq!(directory.read(day(value)), "历史\n");
        }
        assert_eq!(directory.read(day(16)), "次日\n");
    }

    #[test]
    fn 跨日目标已存在时继续追加且不额外删除() {
        let directory = TestDirectory::new();
        for value in 12..=16 {
            directory.seed(day(value), "已有\n");
        }
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
        write_at(&mut writer, "当天\n", day(15));
        write_at(&mut writer, "次日\n", day(16));
        assert_eq!(directory.log_count(), 5);
        assert_eq!(directory.read(day(15)), "已有\n当天\n");
        assert_eq!(directory.read(day(16)), "已有\n次日\n");
        assert_eq!(directory.read(day(12)), "已有\n");
    }

    #[test]
    fn 存在未来日志仍保护当前文件() {
        let directory = TestDirectory::new();
        for value in 15..=20 {
            directory.seed(day(value), "已有\n");
        }
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
        write_at(&mut writer, "当前\n", day(15));
        assert_eq!(directory.log_count(), 6);
        assert_eq!(directory.read(day(15)), "已有\n当前\n");
        assert_eq!(directory.log_count(), 6);
        for value in 17..=20 {
            assert_eq!(directory.read(day(value)), "已有\n");
        }
    }

    #[test]
    fn 清理只处理规范日期普通文件() {
        let directory = TestDirectory::new();
        let unrelated = [
            "scheduled_task_backup.log",
            "scheduled_task.2026-10-01.backup.log",
            "scheduled_task.2026-1-01.log",
            "scheduled_task.2026-02-30.log",
            "scheduled_task.2026-10-01.log.bak",
            "scheduled_task.2026-10-01.extra.log",
            "other.2026-10-01.log",
            "notes.txt",
        ];
        for name in unrelated {
            fs::write(directory.0.join(name), "保留").unwrap();
            assert!(log_date(name).is_none());
        }
        let subdirectory = directory.0.join(log_name(day(1)));
        fs::create_dir(&subdirectory).unwrap();
        fs::write(subdirectory.join("保留.txt"), "保留").unwrap();
        for value in 10..=15 {
            directory.seed(day(value), "历史");
        }
        let writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
        assert_eq!(directory.log_count(), 5);
        assert!(!directory.0.join(log_name(day(10))).exists());
        for name in unrelated {
            assert_eq!(fs::read_to_string(directory.0.join(name)).unwrap(), "保留");
        }
        assert_eq!(
            fs::read_to_string(subdirectory.join("保留.txt")).unwrap(),
            "保留"
        );
        drop(writer);
    }

    #[test]
    fn 跨日打开失败保留原文件并且不清理() {
        let directory = TestDirectory::new();
        for value in 11..=15 {
            directory.seed(day(value), "历史\n");
        }
        fs::create_dir(directory.0.join(log_name(day(16)))).unwrap();
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), retention(5)).unwrap();
        assert!(writer.write_at(b"next", day(16)).is_err());
        assert_eq!(directory.log_count(), 5);
        assert_eq!(writer.date, day(15));
        write_at(&mut writer, "继续\n", day(15));
        assert_eq!(directory.read(day(15)), "历史\n继续\n");
        assert_eq!(directory.read(day(11)), "历史\n");
    }

    #[test]
    fn 默认按七天实际年龄清理而非文件数量() {
        let directory = TestDirectory::new();
        directory.seed(day(1), "很早\n");
        directory.seed(day(8), "过期\n");
        directory.seed(day(9), "边界\n");
        let writer =
            DailyLogWriter::new_at(&directory.0, day(15), LogRetention::default()).unwrap();
        assert_eq!(directory.log_count(), 2);
        assert_eq!(directory.read(day(9)), "边界\n");
        assert!(!directory.0.join(log_name(day(8))).exists());
        drop(writer);
    }

    #[test]
    fn 动态缩短保留天数立即清理且扩展后允许更多历史() {
        let directory = TestDirectory::new();
        for value in 9..=15 {
            directory.seed(day(value), "历史\n");
        }
        let mut writer =
            DailyLogWriter::new_at(&directory.0, day(15), LogRetention::default()).unwrap();
        writer.set_retention_at(retention(3), day(15));
        assert_eq!(directory.log_count(), 3);
        assert!(directory.0.join(log_name(day(13))).exists());
        directory.seed(day(9), "新增历史\n");
        writer.set_retention_at(retention(7), day(15));
        assert_eq!(directory.read(day(9)), "新增历史\n");
    }

    fn size_limited() -> LogRetention {
        LogRetention {
            size_limit_enabled: true,
            max_size_mb: 200,
            ..LogRetention::default()
        }
    }

    #[test]
    fn 大小默认不限制且启用后立即按日期移除最早日志() {
        let directory = TestDirectory::new();
        for value in [14, 12, 13] {
            directory.seed_size(day(value), 100 * 1024 * 1024);
        }
        let mut writer =
            DailyLogWriter::new_at(&directory.0, day(15), LogRetention::default()).unwrap();
        assert_eq!(directory.log_count(), 4);
        writer.set_retention_at(size_limited(), day(15));
        assert_eq!(directory.log_count(), 3);
        assert!(!directory.0.join(log_name(day(12))).exists());
        assert_eq!(writer.total_size, 200 * 1024 * 1024);
        writer.set_retention_at(LogRetention::default(), day(15));
        directory.seed_size(day(12), 100 * 1024 * 1024);
        writer.prune(day(15));
        assert_eq!(directory.log_count(), 4);
    }

    #[test]
    fn 持续写入跨过总大小上限即清理() {
        let directory = TestDirectory::new();
        directory.seed_size(day(14), 100 * 1024 * 1024);
        directory.seed_size(day(15), 100 * 1024 * 1024);
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), size_limited()).unwrap();
        assert_eq!(directory.log_count(), 2);
        write_at(&mut writer, "a", day(15));
        assert_eq!(directory.log_count(), 1);
        assert_eq!(writer.total_size, 100 * 1024 * 1024 + 1);
        assert!(!directory.0.join(log_name(day(14))).exists());
    }

    #[test]
    fn 当前文件独自超限仍保留且不会每条日志扫描目录() {
        let directory = TestDirectory::new();
        directory.seed_size(day(15), 201 * 1024 * 1024);
        directory.seed(day(16), "未来\n");
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), size_limited()).unwrap();
        assert_eq!(directory.log_count(), 1);
        assert!(writer.oversize_reported);
        let last_pruned = writer.last_pruned;
        write_at(&mut writer, "a", day(15));
        assert_eq!(writer.last_pruned, last_pruned);
        assert!(directory.0.join(log_name(day(15))).exists());
        // 次日旧文件不再受保护,可立即释放容量。
        write_at(&mut writer, "b", day(16));
        assert!(!directory.0.join(log_name(day(15))).exists());
        assert_eq!(directory.read(day(16)), "b");
    }

    #[test]
    fn 定期复核其他进程新增日志() {
        let directory = TestDirectory::new();
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15), size_limited()).unwrap();
        directory.seed_size(day(14), 201 * 1024 * 1024);
        writer.last_pruned = Instant::now() - DIRECTORY_RECHECK_INTERVAL;
        write_at(&mut writer, "a", day(15));
        assert!(!directory.0.join(log_name(day(14))).exists());
        assert_eq!(writer.total_size, 1);
    }

    #[test]
    fn 跨月保留天数边界包含今天() {
        let directory = TestDirectory::new();
        directory.seed(day(30), "过期\n");
        directory.seed(day(31), "边界\n");
        let writer =
            DailyLogWriter::new_at(&directory.0, Date::new(2026, 11, 2).unwrap(), retention(3))
                .unwrap();
        assert!(!directory.0.join(log_name(day(30))).exists());
        assert_eq!(directory.read(day(31)), "边界\n");
        drop(writer);
    }

    #[test]
    fn 排队记录跨午夜或切换时区仍归入自身日期() {
        let directory = TestDirectory::new();
        let mut writer = DailyLogWriter::new_at(&directory.0, day(20), retention(7)).unwrap();
        let previous = b"2026-10-19T23:59:59.123+08:00 INFO queued\n";
        let current = b"2026-10-20T00:00:00.123+08:00 INFO current\n";
        let changed = b"2026-10-18T22:00:00.123-12:00 INFO changed\n";
        for record in [previous.as_slice(), current.as_slice(), changed.as_slice()] {
            writer.write_record_at(record, day(20)).unwrap();
        }
        writer.file.flush().unwrap();
        assert_eq!(directory.read(day(19)).as_bytes(), previous);
        assert_eq!(directory.read(day(20)).as_bytes(), current);
        assert_eq!(directory.read(day(18)).as_bytes(), changed);
    }

    #[test]
    fn 延迟记录按当前日期清理但保护正在追加的历史文件() {
        let directory = TestDirectory::new();
        directory.seed(day(16), "过期\n");
        let mut writer = DailyLogWriter::new_at(&directory.0, day(17), retention(3)).unwrap();
        writer
            .write_record_at(b"2026-10-15T23:59:59.000+08:00 INFO delayed\n", day(20))
            .unwrap();
        assert!(!directory.0.join(log_name(day(16))).exists());
        assert!(directory.0.join(log_name(day(15))).exists());
        assert!(!directory.0.join(log_name(day(17))).exists());
    }

    #[test]
    fn 非时间戳内容不误识别为日期且完整多行记录不拆分() {
        assert_eq!(record_date(b"2026-10-19 ordinary message"), None);
        assert_eq!(
            record_date(b"2026-10-19T99:00:00.000+08:00 INFO invalid"),
            None
        );
        let directory = TestDirectory::new();
        let mut writer = DailyLogWriter::new_at(&directory.0, day(20), retention(7)).unwrap();
        let record =
            "2026-10-19T23:59:59.000+08:00 INFO 首行\n2026-10-18T00:00:00.000Z 是消息正文\n";
        writer.write_record_at(record.as_bytes(), day(20)).unwrap();
        assert_eq!(directory.read(day(19)), record);
        assert!(!directory.0.join(log_name(day(18))).exists());
    }
}
