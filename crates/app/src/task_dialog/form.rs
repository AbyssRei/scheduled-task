//! 表单值到任务配置的转换与校验,不依赖 UI 控件。

use std::collections::BTreeMap;

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
    pub concurrency_ix: usize,
    pub concurrency_group: String,
    pub name: String,
    pub path: String,
    pub kind_ix: usize,
    pub manual_cmd: String,
    pub args: String,
    pub working_dir: String,
    pub env: String,
    pub env_file: String,
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
    pub(super) fn into_config(self, id: TaskId) -> Result<TaskConfig, String> {
        let name = self.name;
        if name.trim().is_empty() {
            return Err("任务名称不能为空".into());
        }
        let path = self.path;
        if path.trim().is_empty() {
            return Err("脚本路径不能为空".into());
        }
        let manual_cmd = self.manual_cmd;
        let kind = kind_from_ix(self.kind_ix, &manual_cmd);

        let args = self
            .args
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let working_dir = optional_path(&self.working_dir);
        let env = parse_env_text(&self.env)?;
        let env_file = optional_path(&self.env_file);

        let times_text = self.times;
        let mut times = Vec::new();
        for part in times_text.split([',', ';', ',', ';', ' ', '\t']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let time: jiff::civil::Time = part
                .parse()
                .map_err(|_| format!("时间格式应为 HH:MM:SS,当前有误: {part}"))?;
            times.push(time);
        }
        if times.is_empty() {
            return Err("至少需要一个触发时间(HH:MM:SS)".into());
        }

        let schedule = match self.schedule_ix {
            1 => {
                let day_text = self.day;
                let day: u8 = day_text
                    .trim()
                    .parse()
                    .map_err(|_| "每月日期应为数字".to_string())?;
                let day = DayOfMonth::new(day).map_err(|e| e.to_string())?;
                Schedule::Monthly { day, times }
            }
            2 => Schedule::Custom { times },
            _ => Schedule::Daily { times },
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
                window: parse_range(&window_text, 1, 100, 5, "连续次数")?,
                max_retries: parse_range(&max_text, 1, 10, 3, "最大重试次数")?,
                cooldown: std::time::Duration::from_secs(2),
            })
        } else {
            None
        };

        let timeout = if self.timeout_on {
            let t = self.timeout_min;
            let minutes: u64 = t.trim().parse().map_err(|_| "超时应为分钟数".to_string())?;
            if !(1..=300).contains(&minutes) {
                return Err("超时应为 1–300 分钟".into());
            }
            Some(std::time::Duration::from_secs(minutes * 60))
        } else {
            None
        };

        let concurrency = match self.concurrency_ix {
            0 => ConcurrencyPolicy::Parallel,
            1 => ConcurrencyPolicy::Solo,
            2 => {
                let group = self.concurrency_group.trim();
                if group.is_empty() {
                    return Err("同组互斥需要填写组名".into());
                }
                ConcurrencyPolicy::Group(group.to_string())
            }
            _ => return Err("请选择有效的并发策略".into()),
        };

        let task = TaskConfig {
            id,
            name: name.trim().to_string(),
            script: ScriptSpec {
                kind,
                source: ScriptSource::File(path.trim().to_string()),
                args,
                working_dir,
                env,
                env_file,
            },
            schedule,
            retry,
            timeout,
            enabled: self.enabled,
            concurrency,
        };
        task.validate()?;
        Ok(task)
    }
}

fn optional_path(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 解析表单中的环境变量文本。每行一个 `KEY=VALUE`,空行和 `#` 注释忽略。
pub(super) fn parse_env_text(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
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
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(format!("环境变量第 {} 行的名称无效", line_no + 1));
        }
        let value = value.trim();
        if value.contains('\0') {
            return Err(format!("环境变量第 {} 行的值无效", line_no + 1));
        }
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_form() -> FormValues {
        FormValues {
            concurrency_ix: 0,
            concurrency_group: String::new(),
            name: "  定时备份  ".into(),
            path: "  E:/scripts/backup.cmd  ".into(),
            kind_ix: 0,
            manual_cmd: String::new(),
            args: String::new(),
            working_dir: String::new(),
            env: String::new(),
            env_file: String::new(),
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
        form.env = " MODE=fast\nCOUNT=2 ".into();
        form.env_file = " .env ".into();
        let task = form.into_config(TaskId(42)).unwrap();
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
        assert_eq!(task.script.env_file.as_deref(), Some(".env"));
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
            assert_eq!(form.into_config(TaskId(42)).unwrap().concurrency, policy);
        }
    }

    #[test]
    fn validates_and_trims_concurrency_group() {
        let mut form = valid_form();
        form.concurrency_ix = 2;
        form.concurrency_group = "   ".into();
        assert_eq!(
            form.into_config(TaskId(0)).unwrap_err(),
            "同组互斥需要填写组名"
        );
        let mut form = valid_form();
        form.concurrency_ix = 2;
        form.concurrency_group = "  备份组  ".into();
        assert_eq!(
            form.into_config(TaskId(0)).unwrap().concurrency,
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
        let task = form.into_config(TaskId(0)).unwrap();
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
        let task = form.into_config(TaskId(0)).unwrap();
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
                form.into_config(TaskId(0)).unwrap_err(),
                "连续次数 应在 1–100 之间"
            );
        }
        for value in ["0", "11"] {
            let mut form = valid_form();
            form.retry_on = true;
            form.max_retries = value.into();
            assert_eq!(
                form.into_config(TaskId(0)).unwrap_err(),
                "最大重试次数 应在 1–10 之间"
            );
        }
        for value in ["0", "301"] {
            let mut form = valid_form();
            form.timeout_on = true;
            form.timeout_min = value.into();
            assert_eq!(
                form.into_config(TaskId(0)).unwrap_err(),
                "超时应为 1–300 分钟"
            );
        }
    }

    #[test]
    fn parses_environment_lines_and_ignores_comments() {
        let env = parse_env_text("# 注释\nexport A=1\nB=hello=world\nA=2\n").unwrap();
        assert_eq!(env.get("A"), Some(&"2".to_string()));
        assert_eq!(env.get("B"), Some(&"hello=world".to_string()));
    }

    #[test]
    fn rejects_malformed_environment_lines() {
        assert!(parse_env_text("NO_VALUE").is_err());
        assert!(parse_env_text("=missing").is_err());
    }
}
