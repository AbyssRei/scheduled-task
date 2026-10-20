//! 表单分组与校验定位，不依赖界面实体。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FormPage {
    Basics,
    Environment,
    Schedule,
    Recovery,
}

impl FormPage {
    pub const ALL: [Self; 4] = [
        Self::Basics,
        Self::Environment,
        Self::Schedule,
        Self::Recovery,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Basics => "基本信息",
            Self::Environment => "执行环境",
            Self::Schedule => "调度策略",
            Self::Recovery => "重试与超时",
        }
    }

    pub fn scroll_id(self) -> &'static str {
        match self {
            Self::Basics => "task-basics-scroll",
            Self::Environment => "task-environment-scroll",
            Self::Schedule => "task-schedule-scroll",
            Self::Recovery => "task-recovery-scroll",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Basics => 0,
            Self::Environment => 1,
            Self::Schedule => 2,
            Self::Recovery => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FormField {
    Name,
    Path,
    Kind,
    ManualCommand,
    EnvironmentKey(usize),
    EnvironmentValue(usize),
    EnvironmentFile(usize),
    ScheduleType,
    Times,
    Day,
    Concurrency,
    Group,
    RetryWindow,
    MaxRetries,
    Cooldown,
    Timeout,
}

impl FormField {
    pub fn page(self) -> FormPage {
        match self {
            Self::Name | Self::Path | Self::Kind | Self::ManualCommand => FormPage::Basics,
            Self::EnvironmentKey(_) | Self::EnvironmentValue(_) | Self::EnvironmentFile(_) => {
                FormPage::Environment
            }
            Self::ScheduleType | Self::Times | Self::Day | Self::Concurrency | Self::Group => {
                FormPage::Schedule
            }
            Self::RetryWindow | Self::MaxRetries | Self::Cooldown | Self::Timeout => {
                FormPage::Recovery
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FormError {
    pub field: FormField,
    pub message: String,
}

impl FormError {
    pub fn new(field: FormField, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}
