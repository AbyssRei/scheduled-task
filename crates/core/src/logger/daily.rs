//! 按 UTC 日期追加日志,清理时始终保护当前文件。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use jiff::tz::TimeZone;

const MAX_LOG_FILES: usize = 5;

pub(super) struct DailyLogWriter {
    directory: PathBuf,
    date: Date,
    file: File,
}

impl DailyLogWriter {
    pub(super) fn new(directory: &Path) -> io::Result<Self> {
        Self::new_at(directory, utc_date())
    }

    fn new_at(directory: &Path, date: Date) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let file = open_log(directory, date)?;
        let writer = Self {
            directory: directory.to_owned(),
            date,
            file,
        };
        writer.prune();
        Ok(writer)
    }

    fn write_at(&mut self, bytes: &[u8], date: Date) -> io::Result<usize> {
        if self.date != date {
            // 新文件打开成功后才替换当前文件,失败时不清理任何日志。
            let file = open_log(&self.directory, date)?;
            self.file.flush()?;
            self.file = file;
            self.date = date;
            self.prune();
        }
        self.file.write(bytes)
    }

    fn prune(&self) {
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
                logs.push((date, entry.path()));
            }
        }
        let mut excess = logs.len().saturating_sub(MAX_LOG_FILES);
        logs.sort_unstable_by_key(|(date, _)| *date);
        for (date, path) in logs {
            if excess == 0 {
                break;
            }
            // 即使时钟回拨或存在未来日期日志,也不能删除正在追加的文件。
            if date == self.date {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => excess -= 1,
                Err(error) => eprintln!("清理旧日志失败 {}: {error}", path.display()),
            }
        }
    }
}

impl Write for DailyLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_at(bytes, utc_date())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn utc_date() -> Date {
    jiff::Timestamp::now().to_zoned(TimeZone::UTC).date()
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

    fn day(day: i8) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn write_at(writer: &mut DailyLogWriter, text: &str, date: Date) {
        assert_eq!(writer.write_at(text.as_bytes(), date).unwrap(), text.len());
        writer.flush().unwrap();
    }

    #[test]
    fn 同日反复重启追加且五个文件不会减少() {
        let directory = TestDirectory::new();
        // 当天文件最先创建,保留决策仍应只依据日志日期。
        directory.seed(day(15), "启动前\n");
        for value in 11..15 {
            directory.seed(day(value), "历史\n");
        }
        for _ in 0..3 {
            let mut writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
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
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
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
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
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
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
        write_at(&mut writer, "当前\n", day(15));
        assert_eq!(directory.log_count(), 5);
        assert_eq!(directory.read(day(15)), "已有\n当前\n");
        assert!(!directory.0.join(log_name(day(16))).exists());
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
        let writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
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
        let mut writer = DailyLogWriter::new_at(&directory.0, day(15)).unwrap();
        assert!(writer.write_at(b"next", day(16)).is_err());
        assert_eq!(directory.log_count(), 5);
        assert_eq!(writer.date, day(15));
        write_at(&mut writer, "继续\n", day(15));
        assert_eq!(directory.read(day(15)), "历史\n继续\n");
        assert_eq!(directory.read(day(11)), "历史\n");
    }
}
