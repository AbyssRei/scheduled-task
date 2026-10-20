//! 表单值到任务配置的转换与校验,不依赖 UI 控件。

use std::collections::BTreeMap;

use super::feedback::{FormError, FormField};
use scheduled_task_core::model::{
    ConcurrencyPolicy, DayOfMonth, PyLauncher, RetryPolicy, Schedule, ScriptKind, ScriptSource,
    ScriptSpec, TaskConfig, TaskId,
};

pub(super) const KIND_OPTIONS: &[&str] = &[
    "自动(按扩展名)",
    "批处理 Cmd",
    "PowerShell",
    "PowerShell 7 (pwsh)",
    "Python(系统)",
    "Python(uv run)",
    "Bash",
    "Sh",
    "手工命令",
];

pub(super) const SCHEDULE_OPTIONS: &[&str] = &["每天", "每月", "自定义"];
pub(super) const CONCURRENCY_OPTIONS: &[&str] = &["并行执行", "任务串行", "同组互斥"];

pub(super) fn concurrency_fields(policy: &ConcurrencyPolicy) -> (usize, String) {
    match policy {
        ConcurrencyPolicy::Parallel => (0, String::new()),
        ConcurrencyPolicy::Solo => (1, String::new()),
        ConcurrencyPolicy::Group(group) => (2, group.clone()),
    }
}

fn kind_from_ix(ix: usize, manual_command: &str) -> ScriptKind {
    match ix {
        1 => ScriptKind::Cmd,
        2 => ScriptKind::PowerShell,
        3 => ScriptKind::Pwsh,
        4 => ScriptKind::Python {
            launcher: PyLauncher::System,
        },
        5 => ScriptKind::Python {
            launcher: PyLauncher::Uv { project: None },
        },
        6 => ScriptKind::Bash,
        7 => ScriptKind::Sh,
        8 => ScriptKind::Manual {
            command: manual_command.to_string(),
        },
        _ => ScriptKind::Auto,
    }
}

pub(super) fn ix_from_kind(kind: &ScriptKind) -> usize {
    match kind {
        ScriptKind::Cmd => 1,
        ScriptKind::PowerShell => 2,
        ScriptKind::Pwsh => 3,
        ScriptKind::Python { launcher } => match launcher {
            PyLauncher::System => 4,
            PyLauncher::Uv { .. } => 5,
        },
        ScriptKind::Bash => 6,
        ScriptKind::Sh => 7,
        ScriptKind::Manual { .. } => 8,
        ScriptKind::Auto => 0,
    }
}

/// 保存时读取的表单快照,将控件访问与配置校验分离。
pub(super) struct FormValues {
    pub source_inline: bool,
    pub inline_content: String,
    pub uv_project: String,
    pub cooldown: String,
    pub concurrency_ix: usize,
    pub concurrency_group: String,
    pub name: String,
    pub path: String,
    pub kind_ix: usize,
    pub manual_cmd: String,
    pub args: String,
    pub working_dir: String,
    pub env: Vec<(String, String)>,
    pub env_files: Vec<String>,
    pub schedule_ix: usize,
    pub times: String,
    pub day: String,
    pub retry_on: bool,
    pub pattern: String,
    pub window_n: String,
    pub max_retries: String,
    pub timeout_on: bool,
    pub timeout_min: String,
    pub enabled: bool,
}

impl FormValues {
    pub(super) fn from_config(task: &TaskConfig) -> Self {
        let (concurrency_ix, concurrency_group) = concurrency_fields(&task.concurrency);
        Self {
            source_inline: matches!(task.script.source, ScriptSource::Inline(_)),
            inline_content: match &task.script.source {
                ScriptSource::Inline(content) => content.clone(),
                _ => String::new(),
            },
            uv_project: match &task.script.kind {
                ScriptKind::Python {
                    launcher: PyLauncher::Uv { project },
                } => project.clone().unwrap_or_default(),
                _ => String::new(),
            },
            cooldown: task
                .retry
                .as_ref()
                .map(|retry| retry.cooldown.as_secs().to_string())
                .unwrap_or_default(),
            concurrency_ix,
            concurrency_group,
            name: task.name.clone(),
            path: match &task.script.source {
                ScriptSource::File(path) => path.clone(),
                _ => String::new(),
            },
            kind_ix: ix_from_kind(&task.script.kind),
            manual_cmd: match &task.script.kind {
                ScriptKind::Manual { command } => command.clone(),
                _ => String::new(),
            },
            args: task.script.args.join("\n"),
            working_dir: task.script.working_dir.clone().unwrap_or_default(),
            env: environment_entries(&task.script),
            env_files: task.script.env_file_paths().map(str::to_owned).collect(),
            schedule_ix: match task.schedule {
                Schedule::Daily { .. } => 0,
                Schedule::Monthly { .. } => 1,
                Schedule::Custom { .. } => 2,
            },
            times: task
                .schedule
                .times()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            day: task
                .schedule
                .monthly_day()
                .map(|day| day.get().to_string())
                .unwrap_or_default(),
            retry_on: task.retry.is_some(),
            pattern: task
                .retry
                .as_ref()
                .map(|retry| retry.pattern.clone())
                .unwrap_or_default(),
            window_n: task
                .retry
                .as_ref()
                .map(|retry| retry.window.to_string())
                .unwrap_or_default(),
            max_retries: task
                .retry
                .as_ref()
                .map(|retry| retry.max_retries.to_string())
                .unwrap_or_default(),
            timeout_on: task.timeout.is_some(),
            timeout_min: task
                .timeout
                .map(|timeout| (timeout.as_secs() / 60).to_string())
                .unwrap_or_default(),
            enabled: task.enabled,
        }
    }
    pub(super) fn into_config(
        self,
        id: TaskId,
        original: Option<&TaskConfig>,
    ) -> Result<TaskConfig, FormError> {
        let name = self.name;
        if name.trim().is_empty() {
            return Err(FormError::new(FormField::Name, "任务名称不能为空"));
        }
        let path = self.path;
        if !self.source_inline && path.trim().is_empty() {
            return Err(FormError::new(FormField::Path, "脚本路径不能为空"));
        }
        let manual_cmd = self.manual_cmd;
        if self.kind_ix >= KIND_OPTIONS.len() {
            return Err(FormError::new(FormField::Kind, "请选择有效的脚本类型"));
        }
        if self.source_inline && self.kind_ix == 0 {
            return Err(FormError::new(
                FormField::Kind,
                "内联脚本请选择具体脚本类型",
            ));
        }
        if self.kind_ix == 8 && manual_cmd.trim().is_empty() {
            return Err(FormError::new(FormField::ManualCommand, "手工命令不能为空"));
        }
        let mut kind = kind_from_ix(self.kind_ix, &manual_cmd);
        if let ScriptKind::Python {
            launcher: PyLauncher::Uv { project },
        } = &mut kind
        {
            *project = original
                .and_then(|task| match &task.script.kind {
                    ScriptKind::Python {
                        launcher: PyLauncher::Uv { project },
                    } if self.uv_project == project.as_deref().unwrap_or_default() => {
                        Some(project.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| optional_path(&self.uv_project));
        }

        let args =
            if let Some(task) = original.filter(|task| self.args == task.script.args.join("\n")) {
                task.script.args.clone()
            } else {
                self.args
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            };
        let working_dir = preserved_path(
            &self.working_dir,
            original.map(|task| &task.script.working_dir),
        );
        let (env, env_order) = if let Some(task) =
            original.filter(|task| self.env == environment_entries(&task.script))
        {
            (task.script.env.clone(), task.script.env_order.clone())
        } else {
            collect_environment(self.env)?
        };
        let (env_file, env_files) = if let Some(task) = original
            .filter(|task| self.env_files == task.script.env_file_paths().collect::<Vec<_>>())
        {
            (task.script.env_file.clone(), task.script.env_files.clone())
        } else {
            for (index, path) in self.env_files.iter().enumerate() {
                if path.contains('\0') {
                    return Err(FormError::new(
                        FormField::EnvironmentFile(index),
                        format!("第 {} 个 .env 文件路径包含无效字符", index + 1),
                    ));
                }
            }
            (
                None,
                self.env_files
                    .iter()
                    .map(|path| path.trim())
                    .filter(|path| !path.is_empty())
                    .map(ToOwned::to_owned)
                    .collect(),
            )
        };

        let times_text = self.times;
        let mut times = Vec::new();
        for part in times_text.split([',', ';', '，', '；', ' ', '\t', '\r', '\n']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let time: jiff::civil::Time = part.parse().map_err(|_| {
                FormError::new(
                    FormField::Times,
                    format!("时间格式应为 HH:MM:SS,当前有误: {part}"),
                )
            })?;
            times.push(time);
        }
        if times.is_empty() {
            return Err(FormError::new(
                FormField::Times,
                "至少需要一个触发时间(HH:MM:SS)",
            ));
        }

        let schedule = match self.schedule_ix {
            1 => {
                let day_text = self.day;
                let day: u8 = day_text
                    .trim()
                    .parse()
                    .map_err(|_| FormError::new(FormField::Day, "每月日期应为数字"))?;
                let day = DayOfMonth::new(day)
                    .map_err(|error| FormError::new(FormField::Day, error.to_string()))?;
                Schedule::Monthly { day, times }
            }
            2 => Schedule::Custom { times },
            0 => Schedule::Daily { times },
            _ => {
                return Err(FormError::new(
                    FormField::ScheduleType,
                    "请选择有效的调度类型",
                ));
            }
        };

        let concurrency = match self.concurrency_ix {
            0 => ConcurrencyPolicy::Parallel,
            1 => ConcurrencyPolicy::Solo,
            2 => {
                let group = self.concurrency_group.trim();
                if group.is_empty() {
                    return Err(FormError::new(FormField::Group, "同组互斥需要填写组名"));
                }
                original.filter(|task| matches!(&task.concurrency, ConcurrencyPolicy::Group(original) if original == &self.concurrency_group))
                    .map(|task| task.concurrency.clone()).unwrap_or_else(|| ConcurrencyPolicy::Group(group.to_string()))
            }
            _ => {
                return Err(FormError::new(
                    FormField::Concurrency,
                    "请选择有效的并发策略",
                ));
            }
        };

        let retry = if self.retry_on {
            let pattern = self.pattern;
            let window_text = self.window_n;
            let max_text = self.max_retries;
            let parse_range =
                |s: &str, lo: u32, hi: u32, dft: u32, what: &str| -> Result<u32, String> {
                    if s.trim().is_empty() {
                        return Ok(dft);
                    }
                    let v: u32 = s.trim().parse().map_err(|_| format!("{what} 应为数字"))?;
                    if (lo..=hi).contains(&v) {
                        Ok(v)
                    } else {
                        Err(format!("{what} 应在 {lo}–{hi} 之间"))
                    }
                };
            Some(RetryPolicy {
                pattern,
                window: parse_range(&window_text, 1, 100, 5, "连续次数")
                    .map_err(|message| FormError::new(FormField::RetryWindow, message))?,
                max_retries: parse_range(&max_text, 1, 10, 3, "最大重试次数")
                    .map_err(|message| FormError::new(FormField::MaxRetries, message))?,
                cooldown: std::time::Duration::from_secs(if self.cooldown.trim().is_empty() {
                    2
                } else {
                    self.cooldown.trim().parse().map_err(|_| {
                        FormError::new(FormField::Cooldown, "重试冷却应为非负整数秒")
                    })?
                }),
            })
        } else {
            None
        };

        let timeout = if self.timeout_on {
            let t = self.timeout_min;
            let minutes: u64 = t
                .trim()
                .parse()
                .map_err(|_| FormError::new(FormField::Timeout, "超时应为分钟数"))?;
            if !(1..=300).contains(&minutes) {
                return Err(FormError::new(FormField::Timeout, "超时应为 1–300 分钟"));
            }
            Some(std::time::Duration::from_secs(minutes * 60))
        } else {
            None
        };

        let task = TaskConfig {
            id,
            name: original
                .filter(|task| task.name == name)
                .map(|task| task.name.clone())
                .unwrap_or_else(|| name.trim().to_string()),
            script: ScriptSpec {
                kind,
                source: if self.source_inline {
                    ScriptSource::Inline(self.inline_content)
                } else {
                    original.filter(|task| matches!(&task.script.source, ScriptSource::File(original) if original == &path))
                        .map(|task| task.script.source.clone()).unwrap_or_else(|| ScriptSource::File(path.trim().to_string()))
                },
                args,
                working_dir,
                env,
                env_order,
                env_file,
                env_files,
            },
            schedule,
            retry,
            timeout,
            enabled: self.enabled,
            concurrency,
        };
        task.validate()
            .map_err(|message| FormError::new(FormField::Name, message))?;
        Ok(task)
    }
}

fn environment_entries(script: &ScriptSpec) -> Vec<(String, String)> {
    script
        .ordered_env()
        .into_iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// 名称完全相同的记录保留最后一条及其位置，值保持原样（包括空格、换行和空值）。
fn collect_environment(
    entries: Vec<(String, String)>,
) -> Result<(BTreeMap<String, String>, Vec<String>), FormError> {
    let mut values = BTreeMap::new();
    let mut order = Vec::new();
    for (index, (key, value)) in entries.into_iter().enumerate() {
        if key.trim().is_empty() && value.is_empty() {
            continue;
        }
        let key = key.trim();
        validate_environment_pair(key, &value, index + 1).map_err(|message| {
            FormError::new(
                if valid_environment_key(key) {
                    FormField::EnvironmentValue(index)
                } else {
                    FormField::EnvironmentKey(index)
                },
                message,
            )
        })?;
        order.retain(|existing| existing != key);
        order.push(key.to_string());
        values.insert(key.to_string(), value);
    }
    Ok((values, order))
}

fn validate_environment_pair(key: &str, value: &str, row: usize) -> Result<(), String> {
    if !valid_environment_key(key) {
        return Err(format!("环境变量第 {row} 条的名称无效"));
    }
    if value.contains('\0') {
        return Err(format!("环境变量第 {row} 条的值无效"));
    }
    Ok(())
}

fn valid_environment_key(key: &str) -> bool {
    !key.is_empty() && !key.contains(['=', '\0', '\r', '\n'])
}

fn optional_path(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn preserved_path(value: &str, original: Option<&Option<String>>) -> Option<String> {
    original
        .filter(|original| original.as_deref().unwrap_or_default() == value)
        .cloned()
        .unwrap_or_else(|| optional_path(value))
}

/// 解析表单中的环境变量文本。每行一个 `KEY=VALUE`,空行和 `#` 注释忽略。
pub(super) fn parse_env_text(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut values = Vec::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("环境变量第 {} 行缺少 '='", line_no + 1));
        };
        let key = key.trim();
        let value = value.trim();
        validate_environment_pair(key, value, line_no + 1)?;
        values.push((key.to_string(), value.to_string()));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_form() -> FormValues {
        FormValues {
            source_inline: false,
            inline_content: String::new(),
            uv_project: String::new(),
            cooldown: String::new(),
            concurrency_ix: 0,
            concurrency_group: String::new(),
            name: "  定时备份  ".into(),
            path: "  E:/scripts/backup.cmd  ".into(),
            kind_ix: 0,
            manual_cmd: String::new(),
            args: String::new(),
            working_dir: String::new(),
            env: Vec::new(),
            env_files: Vec::new(),
            schedule_ix: 0,
            times: "12:30:00,08:00:00;12:30:00".into(),
            day: String::new(),
            retry_on: false,
            pattern: String::new(),
            window_n: String::new(),
            max_retries: String::new(),
            timeout_on: false,
            timeout_min: String::new(),
            enabled: true,
        }
    }

    #[test]
    fn saves_trimmed_fields_without_reordering_times() {
        let mut form = valid_form();
        form.args = " --source\ninput file.txt \n".into();
        form.working_dir = " E:/work ".into();
        form.env = parse_env_text(" MODE=fast\nCOUNT=2 ").unwrap();
        form.env_files = vec![" .env ".into()];
        let task = form.into_config(TaskId(42), None).unwrap();
        assert_eq!(task.id, TaskId(42));
        assert_eq!(task.name, "定时备份");
        assert!(matches!(
            task.script.source,
            ScriptSource::File(ref path) if path == "E:/scripts/backup.cmd"
        ));
        let times = task.schedule.times();
        assert_eq!(times.len(), 3);
        assert_eq!(times[0], times[2]);
        assert!(times[0] > times[1]);
        assert_eq!(task.script.args, vec!["--source", "input file.txt"]);
        assert_eq!(task.script.working_dir.as_deref(), Some("E:/work"));
        assert_eq!(task.script.env.get("MODE"), Some(&"fast".to_string()));
        assert!(task.script.env_file.is_none());
        assert_eq!(task.script.env_files, vec![".env"]);
    }

    #[test]
    fn concurrency_policy_survives_editing() {
        for policy in [
            ConcurrencyPolicy::Parallel,
            ConcurrencyPolicy::Solo,
            ConcurrencyPolicy::Group("备份组".into()),
        ] {
            let (ix, group) = concurrency_fields(&policy);
            let mut form = valid_form();
            form.concurrency_ix = ix;
            form.concurrency_group = group;
            assert_eq!(
                form.into_config(TaskId(42), None).unwrap().concurrency,
                policy
            );
        }
    }

    #[test]
    fn 原样保存内联任务保留项目冷却和不可无损展示的参数环境() {
        let mut original = valid_form().into_config(TaskId(42), None).unwrap();
        original.script.kind = ScriptKind::Python {
            launcher: PyLauncher::Uv {
                project: Some(" 项目目录 ".into()),
            },
        };
        original.script.source = ScriptSource::Inline("print('内联脚本')\r\n".into());
        original.script.args = vec!["".into(), " 前后空格 ".into(), "多\n行".into()];
        original
            .script
            .env
            .insert("VALUE".into(), " 原始值\n ".into());
        original.script.working_dir = Some(" 工作目录 ".into());
        original.script.env_file = Some("".into());
        original.retry = Some(RetryPolicy {
            cooldown: std::time::Duration::from_secs(37),
            ..Default::default()
        });
        original.concurrency = ConcurrencyPolicy::Group(" 原组名 ".into());
        let values = FormValues::from_config(&original);
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert_eq!(saved, original);
        let mut values = FormValues::from_config(&original);
        values.name = "仅修改名称".into();
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert_eq!(saved.name, "仅修改名称");
        assert_eq!(
            format!("{:?}", saved.script),
            format!("{:?}", original.script)
        );
        assert_eq!(saved.retry.unwrap().cooldown.as_secs(), 37);
    }

    #[test]
    fn 多环境文件表单保留旧字段与加载顺序并支持修改清空() {
        let mut original = valid_form().into_config(TaskId(42), None).unwrap();
        original.script.env_file = Some("legacy.env".into());
        original.script.env_files = vec!["配置/base.env".into(), "额外 配置.env".into()];
        let values = FormValues::from_config(&original);
        assert_eq!(
            values.env_files,
            vec!["legacy.env", "配置/base.env", "额外 配置.env"]
        );
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert_eq!(
            format!("{:?}", saved.script),
            format!("{:?}", original.script)
        );
        let mut values = FormValues::from_config(&original);
        values.name = "仅改名".into();
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert_eq!(saved.script.env_file, original.script.env_file);
        assert_eq!(saved.script.env_files, original.script.env_files);
        let mut values = FormValues::from_config(&original);
        values.env_files = vec![
            " base.env ".into(),
            "".into(),
            " config/local.env".into(),
            "base.env".into(),
        ];
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert!(saved.script.env_file.is_none());
        assert_eq!(
            saved.script.env_files,
            vec!["base.env", "config/local.env", "base.env"]
        );
        let mut values = FormValues::from_config(&original);
        values.env_files.clear();
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert!(saved.script.env_file.is_none() && saved.script.env_files.is_empty());
    }

    #[test]
    fn 可以修改脚本来源项目和冷却且关闭重试忽略无效冷却() {
        let original = valid_form().into_config(TaskId(42), None).unwrap();
        let mut values = FormValues::from_config(&original);
        values.source_inline = true;
        values.inline_content = "print('修改')".into();
        values.kind_ix = 5;
        values.uv_project = " E:/project ".into();
        values.retry_on = true;
        values.cooldown = "19".into();
        let saved = values.into_config(original.id, Some(&original)).unwrap();
        assert!(
            matches!(saved.script.source, ScriptSource::Inline(ref text) if text == "print('修改')")
        );
        assert!(
            matches!(saved.script.kind, ScriptKind::Python { launcher: PyLauncher::Uv { project: Some(ref project) } } if project == "E:/project")
        );
        assert_eq!(saved.retry.unwrap().cooldown.as_secs(), 19);
        let mut values = FormValues::from_config(&original);
        values.cooldown = "无效".into();
        assert!(values.into_config(original.id, Some(&original)).is_ok());
    }

    #[test]
    fn validates_and_trims_concurrency_group() {
        let mut form = valid_form();
        form.concurrency_ix = 2;
        form.concurrency_group = "   ".into();
        assert_eq!(
            form.into_config(TaskId(0), None).unwrap_err().message,
            "同组互斥需要填写组名"
        );
        let mut form = valid_form();
        form.concurrency_ix = 2;
        form.concurrency_group = "  备份组  ".into();
        assert_eq!(
            form.into_config(TaskId(0), None).unwrap().concurrency,
            ConcurrencyPolicy::Group("备份组".into())
        );
    }

    #[test]
    fn saves_monthly_manual_task_with_retry_defaults() {
        let mut form = valid_form();
        form.kind_ix = 8;
        form.manual_cmd = "custom {script}".into();
        form.schedule_ix = 1;
        form.day = "31".into();
        form.retry_on = true;
        form.pattern = "模式一\n模式二".into();
        form.timeout_on = true;
        form.timeout_min = "300".into();
        let task = form.into_config(TaskId(0), None).unwrap();
        assert!(
            matches!(task.script.kind, ScriptKind::Manual { command } if command == "custom {script}")
        );
        assert_eq!(task.schedule.monthly_day().unwrap().get(), 31);
        let retry = task.retry.unwrap();
        assert_eq!(retry.pattern, "模式一\n模式二");
        assert_eq!(retry.window, 5);
        assert_eq!(retry.max_retries, 3);
        assert_eq!(retry.cooldown.as_secs(), 2);
        assert_eq!(task.timeout.unwrap().as_secs(), 18_000);
    }

    #[test]
    fn ignores_inactive_optional_fields() {
        let mut form = valid_form();
        form.day = "不是日期".into();
        form.window_n = "不是数字".into();
        form.max_retries = "不是数字".into();
        form.timeout_min = "不是数字".into();
        let task = form.into_config(TaskId(0), None).unwrap();
        assert!(matches!(task.schedule, Schedule::Daily { .. }));
        assert!(task.retry.is_none());
        assert!(task.timeout.is_none());
    }

    #[test]
    fn rejects_out_of_range_retry_and_timeout_values() {
        for value in ["0", "101"] {
            let mut form = valid_form();
            form.retry_on = true;
            form.window_n = value.into();
            assert_eq!(
                form.into_config(TaskId(0), None).unwrap_err().message,
                "连续次数 应在 1–100 之间"
            );
        }
        for value in ["0", "11"] {
            let mut form = valid_form();
            form.retry_on = true;
            form.max_retries = value.into();
            assert_eq!(
                form.into_config(TaskId(0), None).unwrap_err().message,
                "最大重试次数 应在 1–10 之间"
            );
        }
        for value in ["0", "301"] {
            let mut form = valid_form();
            form.timeout_on = true;
            form.timeout_min = value.into();
            assert_eq!(
                form.into_config(TaskId(0), None).unwrap_err().message,
                "超时应为 1–300 分钟"
            );
        }
    }

    #[test]
    fn parses_environment_lines_and_ignores_comments() {
        let env = parse_env_text("# 注释\nexport A=1\nB=hello=world\nA=2\n").unwrap();
        assert_eq!(
            env,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "hello=world".into()),
                ("A".into(), "2".into())
            ]
        );
    }

    #[test]
    fn rejects_malformed_environment_lines() {
        assert!(parse_env_text("NO_VALUE").is_err());
        assert!(parse_env_text("=missing").is_err());
    }

    #[test]
    fn 环境条目重排后保存保留顺序和原始空格换行值() {
        let mut form = valid_form();
        form.env = vec![("Z".into(), " 原始值\n ".into()), ("A".into(), "".into())];
        let original = form.into_config(TaskId(42), None).unwrap();
        assert_eq!(original.script.env_order, ["Z", "A"]);
        let mut form = FormValues::from_config(&original);
        form.env.reverse();
        form.env.push((String::new(), String::new()));
        let saved = form.into_config(original.id, Some(&original)).unwrap();
        assert_eq!(saved.script.env_order, ["A", "Z"]);
        assert_eq!(saved.script.env, original.script.env);
        assert_eq!(
            FormValues::from_config(&saved).env,
            vec![("A".into(), "".into()), ("Z".into(), " 原始值\n ".into())]
        );
    }

    #[test]
    fn 重复环境变量保存最后一条及其位置且清空移除顺序() {
        let mut form = valid_form();
        form.env = parse_env_text("Z=first\nA=middle\nZ=last").unwrap();
        let original = form.into_config(TaskId(42), None).unwrap();
        assert_eq!(original.script.env_order, ["A", "Z"]);
        assert_eq!(original.script.env["Z"], "last");
        let mut form = FormValues::from_config(&original);
        form.env.clear();
        let saved = form.into_config(original.id, Some(&original)).unwrap();
        assert!(saved.script.env.is_empty() && saved.script.env_order.is_empty());
    }

    #[test]
    fn 批量解析整体验证且拒绝逐条输入无效名称和值() {
        assert!(
            parse_env_text("GOOD=1\nBROKEN\nNEXT=2")
                .unwrap_err()
                .contains("第 2 行")
        );
        assert!(parse_env_text("GOOD=1\nBAD=nu\0l").is_err());
        for (key, value) in [
            ("", "有值"),
            ("BAD=KEY", "value"),
            ("BAD\nKEY", "value"),
            ("KEY", "\0"),
        ] {
            let mut form = valid_form();
            form.env = vec![(key.into(), value.into())];
            assert!(form.into_config(TaskId(42), None).is_err());
        }
    }

    #[test]
    fn 跨页校验定位到具体字段和环境条目() {
        use super::super::feedback::FormPage;
        type InvalidCase = (fn(&mut FormValues), FormField, FormPage);
        let cases: &[InvalidCase] = &[
            (|v| v.name.clear(), FormField::Name, FormPage::Basics),
            (|v| v.path.clear(), FormField::Path, FormPage::Basics),
            (|v| v.kind_ix = 99, FormField::Kind, FormPage::Basics),
            (
                |v| v.source_inline = true,
                FormField::Kind,
                FormPage::Basics,
            ),
            (
                |v| v.kind_ix = 8,
                FormField::ManualCommand,
                FormPage::Basics,
            ),
            (
                |v| v.env = vec![("OK".into(), "1".into()), ("BAD=KEY".into(), "2".into())],
                FormField::EnvironmentKey(1),
                FormPage::Environment,
            ),
            (
                |v| v.env = vec![("OK".into(), "1".into()), ("BAD".into(), "\0".into())],
                FormField::EnvironmentValue(1),
                FormPage::Environment,
            ),
            (
                |v| v.env_files = vec!["ok.env".into(), "bad\0.env".into()],
                FormField::EnvironmentFile(1),
                FormPage::Environment,
            ),
            (
                |v| v.times = "25:00:00".into(),
                FormField::Times,
                FormPage::Schedule,
            ),
            (
                |v| v.schedule_ix = 99,
                FormField::ScheduleType,
                FormPage::Schedule,
            ),
            (
                |v| {
                    v.schedule_ix = 1;
                    v.day = "32".into();
                },
                FormField::Day,
                FormPage::Schedule,
            ),
            (
                |v| v.concurrency_ix = 2,
                FormField::Group,
                FormPage::Schedule,
            ),
            (
                |v| {
                    v.retry_on = true;
                    v.window_n = "0".into();
                },
                FormField::RetryWindow,
                FormPage::Recovery,
            ),
            (
                |v| {
                    v.retry_on = true;
                    v.max_retries = "11".into();
                },
                FormField::MaxRetries,
                FormPage::Recovery,
            ),
            (
                |v| {
                    v.retry_on = true;
                    v.cooldown = "-1".into();
                },
                FormField::Cooldown,
                FormPage::Recovery,
            ),
            (
                |v| {
                    v.timeout_on = true;
                    v.timeout_min = "0".into();
                },
                FormField::Timeout,
                FormPage::Recovery,
            ),
        ];
        for (mutate, field, page) in cases {
            let mut form = valid_form();
            mutate(&mut form);
            let error = form.into_config(TaskId(42), None).unwrap_err();
            assert_eq!(error.field, *field, "{}", error.message);
            assert_eq!(error.field.page(), *page);
        }
    }

    #[test]
    fn 多页同时无效时先定位调度再定位恢复选项() {
        let mut form = valid_form();
        form.concurrency_ix = 2;
        form.retry_on = true;
        form.window_n = "0".into();
        assert_eq!(
            form.into_config(TaskId(42), None).unwrap_err().field,
            FormField::Group
        );
    }

    #[test]
    fn 时间输入支持中文分隔符并保持原始触发顺序() {
        let mut form = valid_form();
        form.times = "12:30:00，08:00:00；12:30:00".into();
        let task = form.into_config(TaskId(42), None).unwrap();
        let times = task.schedule.times();
        assert_eq!(times.len(), 3);
        assert_eq!(times[0], times[2]);
        assert!(times[0] > times[1]);
    }
}
