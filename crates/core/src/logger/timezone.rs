//! GUI、控制台与文件日志共用的时区及时间戳格式。

use std::sync::{Arc, RwLock};

use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogTimeZone {
    #[default]
    System,
    Custom(String),
}

impl LogTimeZone {
    pub fn validate(&self) -> Result<(), String> {
        self.resolve().map(|_| ())
    }

    /// 自定义时区支持 IANA 名称及 ±HH:MM、UTC±HH:MM 固定偏移。
    pub fn resolve(&self) -> Result<TimeZone, String> {
        match self {
            Self::System => {
                TimeZone::try_system().map_err(|error| format!("读取系统时区失败: {error}"))
            }
            Self::Custom(value) => {
                let value = value.trim();
                let offset = value.strip_prefix("UTC").unwrap_or(value);
                if offset.starts_with(['+', '-']) {
                    let bytes = offset.as_bytes();
                    if bytes.len() != 6
                        || bytes[3] != b':'
                        || ![1, 2, 4, 5].iter().all(|&i| bytes[i].is_ascii_digit())
                    {
                        return Err("固定时区偏移格式必须为 +08:00 或 -05:30".to_owned());
                    }
                    let hours = i32::from(bytes[1] - b'0') * 10 + i32::from(bytes[2] - b'0');
                    let minutes = i32::from(bytes[4] - b'0') * 10 + i32::from(bytes[5] - b'0');
                    if hours > 23 || minutes > 59 {
                        return Err("固定时区偏移的小时须在 0–23、分钟须在 0–59 之间".to_owned());
                    }
                    let seconds =
                        (hours * 3600 + minutes * 60) * if bytes[0] == b'-' { -1 } else { 1 };
                    return jiff::tz::Offset::from_seconds(seconds)
                        .map(TimeZone::fixed)
                        .map_err(|error| format!("无效的固定时区偏移: {error}"));
                }
                TimeZone::get(value).map_err(|_| {
                    "无效的日志时区，请输入 Asia/Shanghai、UTC 或 +08:00 等时区".to_owned()
                })
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct SharedTimeZone(Arc<RwLock<ResolvedSetting>>);

#[derive(Clone)]
struct ResolvedSetting {
    setting: LogTimeZone,
    zone: TimeZone,
}

impl SharedTimeZone {
    pub(super) fn new(setting: LogTimeZone) -> Result<Self, String> {
        let zone = setting.resolve()?;
        Ok(Self(Arc::new(RwLock::new(ResolvedSetting {
            setting,
            zone,
        }))))
    }

    pub(super) fn set(&self, setting: LogTimeZone) -> Result<(), String> {
        // 先解析再替换，失败时保留原设置及已解析时区。
        let zone = setting.resolve()?;
        self.set_resolved(setting, zone);
        Ok(())
    }

    pub(super) fn set_resolved(&self, setting: LogTimeZone, zone: TimeZone) {
        *self.0.write().unwrap() = ResolvedSetting { setting, zone };
    }

    fn current(&self) -> TimeZone {
        let current = self.0.read().unwrap().clone();
        if current.setting == LogTimeZone::System {
            // jiff 的系统时区缓存有效期为 5 分钟；每次查询让运行中的程序跟随系统变更。
            // 读取临时失败时继续使用此前有效时区，不把日志意外切换为 UTC。
            TimeZone::try_system().unwrap_or(current.zone)
        } else {
            current.zone
        }
    }

    pub(super) fn date(&self, timestamp: Timestamp) -> Date {
        timestamp.to_zoned(self.current()).date()
    }

    pub(super) fn format(&self, timestamp: Timestamp) -> String {
        format_in_zone(timestamp, self.current())
    }
}

impl FormatTime for SharedTimeZone {
    fn format_time(&self, writer: &mut Writer<'_>) -> std::fmt::Result {
        writer.write_str(&self.format(Timestamp::now()))
    }
}

fn format_in_zone(timestamp: Timestamp, zone: TimeZone) -> String {
    let zoned = timestamp.to_zoned(zone);
    let seconds = zoned.offset().seconds();
    let sign = if seconds < 0 { '-' } else { '+' };
    let absolute = seconds.unsigned_abs();
    let mut offset = format!("{sign}{:02}:{:02}", absolute / 3600, absolute % 3600 / 60);
    // 少数 IANA 历史规则包含秒级偏移，保留它以便时间戳仍准确代表原始时刻。
    if !absolute.is_multiple_of(60) {
        offset.push_str(&format!(":{:02}", absolute % 60));
    }
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}{}",
        zoned.date(),
        zoned.hour(),
        zoned.minute(),
        zoned.second(),
        zoned.millisecond(),
        offset
    )
}

static LOG_TIME_ZONE: RwLock<Option<SharedTimeZone>> = RwLock::new(None);

/// 格式化日志时间戳，包含毫秒及 UTC 偏移，不改写脚本自身输出的时间。
pub fn format_log_timestamp(timestamp: Timestamp) -> String {
    let timezone = LOG_TIME_ZONE.read().unwrap().clone();
    match timezone {
        Some(timezone) => timezone.format(timestamp),
        None => format_in_zone(timestamp, TimeZone::system()),
    }
}

/// 即时切换所有日志输出时区；无效设置不会覆盖原有设置。
pub fn set_log_timezone(setting: LogTimeZone) -> Result<(), String> {
    let timezone = LOG_TIME_ZONE.read().unwrap().clone();
    timezone
        .ok_or_else(|| "日志系统尚未初始化".to_owned())?
        .set(setting)
}

pub(super) fn publish(timezone: SharedTimeZone) {
    *LOG_TIME_ZONE.write().unwrap() = Some(timezone);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(value: &str) -> LogTimeZone {
        LogTimeZone::Custom(value.to_owned())
    }

    #[test]
    fn 时区配置兼容序列化并严格校验() {
        assert_eq!(
            ron::from_str::<LogTimeZone>("system").unwrap(),
            LogTimeZone::default()
        );
        let setting = custom("Asia/Shanghai");
        assert_eq!(
            ron::from_str::<LogTimeZone>(&ron::to_string(&setting).unwrap()).unwrap(),
            setting
        );
        for value in [
            "UTC",
            "Asia/Shanghai",
            "America/New_York",
            "+08:00",
            "-05:30",
            "UTC+08:00",
        ] {
            assert!(custom(value).validate().is_ok(), "{value}");
        }
        for value in [
            "",
            "Mars/Olympus",
            "+24:00",
            "+08:60",
            "+8:00",
            "08:00",
            "+08:00:00",
            "UTC+",
        ] {
            assert!(custom(value).validate().is_err(), "{value}");
        }
    }

    #[test]
    fn 正负偏移与日期边界和毫秒格式() {
        let timestamp: Timestamp = "2026-10-19T20:30:45.123456Z".parse().unwrap();
        for (zone, expected) in [
            ("UTC", "2026-10-19T20:30:45.123+00:00"),
            ("+08:00", "2026-10-20T04:30:45.123+08:00"),
            ("-05:30", "2026-10-19T15:00:45.123-05:30"),
            ("Asia/Shanghai", "2026-10-20T04:30:45.123+08:00"),
        ] {
            let timezone = SharedTimeZone::new(custom(zone)).unwrap();
            assert_eq!(timezone.format(timestamp), expected);
            assert_eq!(timezone.date(timestamp).to_string(), &expected[..10]);
        }
    }

    #[test]
    fn 命名时区按时间戳处理夏令时转换() {
        let timezone = SharedTimeZone::new(custom("America/New_York")).unwrap();
        for (instant, expected) in [
            ("2026-03-08T06:59:59Z", "2026-03-08T01:59:59.000-05:00"),
            ("2026-03-08T07:00:00Z", "2026-03-08T03:00:00.000-04:00"),
            ("2026-11-01T05:30:00Z", "2026-11-01T01:30:00.000-04:00"),
            ("2026-11-01T06:30:00Z", "2026-11-01T01:30:00.000-05:00"),
        ] {
            assert_eq!(timezone.format(instant.parse().unwrap()), expected);
        }
    }

    #[test]
    fn 即时切换由所有共享引用观察且无效设置保留原值() {
        let timezone = SharedTimeZone::new(custom("+08:00")).unwrap();
        let copy = timezone.clone();
        let timestamp = "2026-10-19T02:00:00Z".parse().unwrap();
        timezone.set(custom("-05:30")).unwrap();
        assert_eq!(copy.format(timestamp), "2026-10-18T20:30:00.000-05:30");
        assert!(timezone.set(custom("不存在的时区")).is_err());
        assert_eq!(copy.format(timestamp), "2026-10-18T20:30:00.000-05:30");
    }
}
