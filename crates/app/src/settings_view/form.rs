//! 与界面无关的设置草稿、变更判断和统一校验。

use scheduled_task_core::logger::{LogLevel, LogRetention, LogTimeZone};
use scheduled_task_core::store::ThemeSetting;

use crate::bridge::AppSettings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsPage {
    General,
    TaskExecution,
    Logging,
}

#[derive(Clone, Debug)]
pub(super) struct SettingsDraft {
    pub theme: ThemeSetting,
    pub max_parallel: String,
    pub log_level: LogLevel,
    pub log_timezone_custom: bool,
    pub log_timezone: String,
    pub log_retention_days: String,
    pub log_max_size_mb: String,
    pub log_size_limit_enabled: bool,
}

#[derive(Debug)]
pub(super) struct ValidationError {
    pub page: SettingsPage,
    pub message: String,
}

impl SettingsDraft {
    pub fn from_settings(settings: &AppSettings) -> Self {
        Self {
            theme: settings.theme,
            max_parallel: settings.max_parallel.to_string(),
            log_level: settings.log_level,
            log_timezone_custom: matches!(settings.log_timezone, LogTimeZone::Custom(_)),
            log_timezone: match &settings.log_timezone {
                LogTimeZone::System => String::new(),
                LogTimeZone::Custom(value) => value.clone(),
            },
            log_retention_days: settings.log_retention.retention_days.to_string(),
            log_max_size_mb: settings.log_retention.max_size_mb.to_string(),
            log_size_limit_enabled: settings.log_retention.size_limit_enabled,
        }
    }

    pub fn validate(&self, baseline: &AppSettings) -> Result<AppSettings, ValidationError> {
        let max_parallel = self
            .max_parallel
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| ValidationError {
                page: SettingsPage::TaskExecution,
                message: "最大同时执行任务数应为大于 0 的整数".into(),
            })?;
        let log_timezone = if self.log_timezone_custom {
            LogTimeZone::Custom(self.log_timezone.trim().to_owned())
        } else {
            LogTimeZone::System
        };
        log_timezone.validate().map_err(|message| ValidationError {
            page: SettingsPage::Logging,
            message: format!("日志时区：{message}"),
        })?;
        let log_retention = parse_log_retention(
            &self.log_retention_days,
            &self.log_max_size_mb,
            self.log_size_limit_enabled,
            baseline.log_retention,
        )
        .map_err(|message| ValidationError {
            page: SettingsPage::Logging,
            message,
        })?;
        Ok(AppSettings {
            theme: self.theme,
            max_parallel,
            log_level: self.log_level,
            log_timezone,
            log_retention,
        })
    }

    pub fn is_dirty(&self, baseline: &AppSettings) -> bool {
        self.validate(baseline)
            .map_or(true, |settings| settings != *baseline)
    }
}

fn parse_log_retention(
    days: &str,
    size: &str,
    size_limit_enabled: bool,
    previous: LogRetention,
) -> Result<LogRetention, String> {
    let retention_days = days
        .trim()
        .parse::<u16>()
        .map_err(|_| "保存天数应为 3 至 365 的整数".to_owned())?;
    let parsed_size = size
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|size| (200..=5120).contains(size));
    // 关闭大小限制时，未完成的禁用输入不能阻止保存其他设置。
    let max_size_mb = if size_limit_enabled {
        parsed_size.ok_or_else(|| "存储大小应为 200 至 5120 MB 的整数".to_owned())?
    } else {
        parsed_size.unwrap_or(previous.max_size_mb)
    };
    let retention = LogRetention {
        retention_days,
        size_limit_enabled,
        max_size_mb,
    };
    retention.validate()?;
    Ok(retention)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> AppSettings {
        AppSettings {
            theme: ThemeSetting::FollowSystem,
            max_parallel: 3,
            log_level: LogLevel::Info,
            log_timezone: LogTimeZone::System,
            log_retention: LogRetention::default(),
        }
    }

    #[test]
    fn 跨页修改合并为一份设置且原值不被草稿改变() {
        let baseline = settings();
        let mut draft = SettingsDraft::from_settings(&baseline);
        draft.theme = ThemeSetting::Dark;
        draft.max_parallel = " 8 ".into();
        draft.log_level = LogLevel::Debug;
        draft.log_timezone_custom = true;
        draft.log_timezone = " Asia/Shanghai ".into();
        draft.log_retention_days = "30".into();
        draft.log_size_limit_enabled = true;
        draft.log_max_size_mb = "2048".into();
        let merged = draft.validate(&baseline).unwrap();
        assert_eq!(merged.theme, ThemeSetting::Dark);
        assert_eq!(merged.max_parallel, 8);
        assert_eq!(merged.log_level, LogLevel::Debug);
        assert_eq!(
            merged.log_timezone,
            LogTimeZone::Custom("Asia/Shanghai".into())
        );
        assert_eq!(
            merged.log_retention,
            LogRetention {
                retention_days: 30,
                size_limit_enabled: true,
                max_size_mb: 2048
            }
        );
        assert_eq!(baseline, settings());
        assert!(!SettingsDraft::from_settings(&merged).is_dirty(&merged));
    }

    #[test]
    fn 恢复原值或重建草稿后无待保存修改() {
        let baseline = settings();
        let mut draft = SettingsDraft::from_settings(&baseline);
        assert!(!draft.is_dirty(&baseline));
        draft.theme = ThemeSetting::Light;
        assert!(draft.is_dirty(&baseline));
        draft.theme = baseline.theme;
        draft.max_parallel = " 003 ".into();
        assert!(!draft.is_dirty(&baseline));
        draft.max_parallel = String::new();
        assert!(draft.is_dirty(&baseline));
        assert!(!SettingsDraft::from_settings(&baseline).is_dirty(&baseline));
    }

    #[test]
    fn 无效输入定位页面且禁用输入不阻止保存() {
        let baseline = settings();
        let mut draft = SettingsDraft::from_settings(&baseline);
        for value in ["", "0", "-1", "1.5", "4294967296", "abc"] {
            draft.max_parallel = value.into();
            assert_eq!(
                draft.validate(&baseline).unwrap_err().page,
                SettingsPage::TaskExecution
            );
        }
        draft.max_parallel = "8".into();
        draft.log_timezone = "Invalid/Zone".into();
        draft.log_max_size_mb = String::new();
        assert!(draft.validate(&baseline).is_ok());
        draft.log_timezone_custom = true;
        assert_eq!(
            draft.validate(&baseline).unwrap_err().page,
            SettingsPage::Logging
        );
        draft.log_timezone_custom = false;
        draft.log_size_limit_enabled = true;
        assert_eq!(
            draft.validate(&baseline).unwrap_err().page,
            SettingsPage::Logging
        );
    }

    #[test]
    fn 日志存储校验边界与禁用大小输入() {
        let previous = LogRetention::default();
        for (days, size) in [("3", "200"), ("365", "5120"), (" 7 ", " 1024 ")] {
            assert!(parse_log_retention(days, size, true, previous).is_ok());
        }
        for days in ["", "2", "366", "-1", "3.5", "abc", "65536"] {
            assert!(parse_log_retention(days, "1024", true, previous).is_err());
        }
        for size in ["", "199", "5121", "-1", "200.5", "abc", "4294967296"] {
            assert!(parse_log_retention("7", size, true, previous).is_err());
            assert_eq!(
                parse_log_retention("7", size, false, previous)
                    .unwrap()
                    .max_size_mb,
                previous.max_size_mb
            );
        }
    }

    #[test]
    fn 日志时区接受名称和偏移并拒绝无效值() {
        let baseline = settings();
        let mut draft = SettingsDraft::from_settings(&baseline);
        draft.log_timezone_custom = true;
        for value in [
            "Asia/Shanghai",
            "America/New_York",
            "UTC",
            "+08:00",
            "-05:30",
        ] {
            draft.log_timezone = format!(" {value} ");
            assert_eq!(
                draft.validate(&baseline).unwrap().log_timezone,
                LogTimeZone::Custom(value.into())
            );
        }
        for value in [
            "",
            " ",
            "Invalid/Zone",
            "Asia/Shangha",
            "+25:00",
            "+08:60",
            "08:00",
        ] {
            draft.log_timezone = value.into();
            assert!(draft.validate(&baseline).is_err());
        }
    }
}
