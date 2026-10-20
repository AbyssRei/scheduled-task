//! 脚本落盘、路径规范化与解析器启动命令构建。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{PyLauncher, ScriptKind, ScriptSource, ScriptSpec};

use super::RunnerError;

/// 同一进程中的每次执行独占临时脚本,防止不同任务相互覆盖。
static INLINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 按扩展名推断 ScriptKind(Auto)。
fn infer_kind(path: &Path) -> ScriptKind {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("bat") | Some("cmd") => ScriptKind::Cmd,
        Some("ps1") => ScriptKind::PowerShell,
        Some("py") => ScriptKind::Python {
            launcher: PyLauncher::System,
        },
        Some("sh") => ScriptKind::Bash,
        _ => ScriptKind::Auto, // exe / 其他:直接执行
    }
}

/// 脚本落盘:内联内容写临时文件,按类型给扩展名。
fn materialize(spec: &ScriptSpec) -> Result<PathBuf, RunnerError> {
    match &spec.source {
        // 相对路径按 CWD 解析后取绝对路径(工作目录会被切换到脚本所在目录,
        // 传给子进程的必须自洽),并统一反斜杠(cmd 下 `/` 会被当开关)
        ScriptSource::File(p) => {
            let pb = PathBuf::from(p);
            let abs = if pb.is_absolute() {
                pb
            } else {
                std::fs::canonicalize(&pb)
                    .map_err(|e| RunnerError::Resolve(format!("{}: {e}", p)))?
            };
            // canonicalize 产生 \\?\ VERBATIM 前缀,cmd 无法识别,剥掉
            let s = abs.to_string_lossy();
            let s = if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc}")
            } else {
                s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
            };
            Ok(normalize_separators(PathBuf::from(s)))
        }
        ScriptSource::Inline(content) => {
            let ext = match &spec.kind {
                ScriptKind::Cmd => "bat",
                ScriptKind::PowerShell | ScriptKind::Pwsh => "ps1",
                ScriptKind::Python { .. } => "py",
                ScriptKind::Bash | ScriptKind::Sh => "sh",
                _ => "txt",
            };
            let dir = std::env::temp_dir().join("scheduled-task-inline");
            std::fs::create_dir_all(&dir).map_err(|e| RunnerError::TempFile(e.to_string()))?;
            let sequence = INLINE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("inline-{}-{sequence}.{ext}", std::process::id()));
            std::fs::write(&path, content).map_err(|e| RunnerError::TempFile(e.to_string()))?;
            Ok(normalize_separators(path))
        }
    }
}

/// Windows 下统一路径分隔符为反斜杠。
fn normalize_separators(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().replace('/', "\\");
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    p
}

/// PATH 探测(避免引入 which 依赖)。
#[cfg(windows)]
fn find_shell(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    find_shell_in_paths(program, std::env::split_paths(&paths))
}

#[cfg(windows)]
fn find_shell_in_paths(program: &str, paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    let exe = format!("{program}.exe");
    paths.map(|directory| directory.join(&exe)).find(|path| {
        // 系统 bash.exe 是旧 WSL 启动器，不支持本机脚本的 Windows 路径。
        let launcher = program == "bash"
            && std::env::var_os("SystemRoot").is_some_and(|root| {
                ["System32", "Sysnative", "SysWOW64"]
                    .iter()
                    .any(|directory| {
                        path.to_string_lossy().eq_ignore_ascii_case(
                            &PathBuf::from(&root)
                                .join(directory)
                                .join(&exe)
                                .to_string_lossy(),
                        )
                    })
            });
        !launcher && path.is_file()
    })
}

/// 解析后的启动命令。
pub(super) struct ResolvedCommand {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    /// 启动后向 stdin 喂一个换行(自动应答 `pause`)
    pub(super) feed_newline_stdin: bool,
    pub(super) working_dir: Option<PathBuf>,
    pub(super) environment: BTreeMap<String, String>,
    /// 命令持有临时文件直到执行与输出读取全部完成。
    temp_files: Vec<PathBuf>,
}

impl Drop for ResolvedCommand {
    fn drop(&mut self) {
        for path in &self.temp_files {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 根据 ScriptKind 构建对应解释器的启动命令。
pub(super) fn resolve(spec: &ScriptSpec) -> Result<ResolvedCommand, RunnerError> {
    let path = materialize(spec)?;
    let working_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf);

    let kind = match &spec.kind {
        ScriptKind::Auto => infer_kind(&path),
        other => other.clone(),
    };

    let mut cmd = ResolvedCommand {
        program: path.to_string_lossy().into_owned(),
        args: Vec::new(),
        feed_newline_stdin: false,
        working_dir,
        environment: BTreeMap::new(),
        temp_files: if matches!(spec.source, ScriptSource::Inline(_)) {
            vec![path.clone()]
        } else {
            Vec::new()
        },
    };

    match kind {
        // Windows 直接交给标准库启动批处理，使用其专用参数转义，避免 CALL 二次展开。
        ScriptKind::Cmd => {
            #[cfg(not(windows))]
            {
                cmd.program = "cmd".into();
                cmd.args = vec![
                    "/c".into(),
                    "call".into(),
                    path.to_string_lossy().into_owned(),
                ];
            }
            cmd.feed_newline_stdin = true;
        }
        ScriptKind::PowerShell | ScriptKind::Pwsh => {
            cmd.program = if kind == ScriptKind::Pwsh {
                "pwsh".into()
            } else {
                "powershell".into()
            };
            cmd.args = [
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &path.to_string_lossy(),
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
        }
        ScriptKind::Python { launcher } => match launcher {
            PyLauncher::System => {
                cmd.program = "python".into();
                cmd.args = vec![path.to_string_lossy().into_owned()];
            }
            PyLauncher::Uv { project } => {
                cmd.program = "uv".into();
                cmd.args = vec!["run".into()];
                if let Some(p) = project {
                    cmd.args.push("--project".into());
                    cmd.args.push(p);
                }
                cmd.args.push(path.to_string_lossy().into_owned());
            }
        },
        ScriptKind::Bash | ScriptKind::Sh => {
            let shell = if kind == ScriptKind::Bash {
                "bash"
            } else {
                "sh"
            };
            #[cfg(windows)]
            {
                // Windows 优先探测 Git Bash,其次 WSL
                if let Some(program) = find_shell(shell) {
                    cmd.program = program.to_string_lossy().into_owned();
                    cmd.args = vec![path.to_string_lossy().replace('\\', "/")];
                } else {
                    cmd.program = "wsl.exe".into();
                    cmd.args = vec!["--exec".into(), shell.into(), wsl_path(&path)?];
                }
            }
            #[cfg(not(windows))]
            {
                cmd.program = shell.into();
                cmd.args = vec![path.to_string_lossy().into_owned()];
            }
        }
        ScriptKind::Manual { command } => {
            let line = command.replace("{script}", &path.to_string_lossy());
            #[cfg(windows)]
            {
                // 命令正文保留 shell 语义；参数通过标准库转义后由 %* 原样转交。
                let wrapper = materialize(&ScriptSpec {
                    kind: ScriptKind::Cmd,
                    source: ScriptSource::Inline(format!("@echo off\r\n{line} %*\r\n")),
                    args: Vec::new(),
                    working_dir: None,
                    env: BTreeMap::new(),
                    env_file: None,
                })?;
                cmd.program = wrapper.to_string_lossy().into_owned();
                cmd.temp_files.push(wrapper);
                cmd.feed_newline_stdin = true;
            }
            #[cfg(not(windows))]
            {
                cmd.program = "sh".into();
                cmd.args = vec![
                    "-c".into(),
                    format!("{line} \"$@\""),
                    "scheduled-task".into(),
                ];
            }
        }
        // exe / 未知扩展:直接执行
        ScriptKind::Auto => {}
    }

    // 脚本类型先构造解释器参数,任务参数再逐个追加,避免重新经过 shell 解析。
    cmd.args.extend(spec.args.iter().cloned());

    let script_dir = cmd.working_dir.clone();
    if let Some(value) = spec
        .working_dir
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = PathBuf::from(value.trim());
        let path = if path.is_absolute() {
            path
        } else {
            script_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(path)
        };
        cmd.working_dir = Some(normalize_separators(path));
    }

    if let Some(value) = spec
        .env_file
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = PathBuf::from(value.trim());
        let path = if path.is_absolute() {
            path
        } else {
            cmd.working_dir
                .clone()
                .or(script_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(path)
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            RunnerError::Resolve(format!("读取 .env 文件失败 {}: {error}", path.display()))
        })?;
        cmd.environment = parse_env_file(&text)?;
    }
    for (key, value) in &spec.env {
        let key = key.trim();
        validate_env_pair(key, value)?;
        insert_environment(&mut cmd.environment, key.to_owned(), value.clone());
    }
    Ok(cmd)
}

fn validate_env_key(key: &str) -> Result<(), RunnerError> {
    if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
        return Err(RunnerError::Resolve(format!("环境变量名无效: {key:?}")));
    }
    Ok(())
}

fn validate_env_pair(key: &str, value: &str) -> Result<(), RunnerError> {
    validate_env_key(key)?;
    if value.contains('\0') {
        return Err(RunnerError::Resolve(format!(
            "环境变量值包含 NUL 字符: {key}"
        )));
    }
    Ok(())
}

fn insert_environment(values: &mut BTreeMap<String, String>, key: String, value: String) {
    #[cfg(windows)]
    {
        // Windows 环境变量名不区分大小写,删除已有拼写后再写入,保证显式值覆盖 .env。
        if let Some(existing) = values
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(&key))
            .cloned()
        {
            values.remove(&existing);
        }
    }
    values.insert(key, value);
}

fn parse_env_file(text: &str) -> Result<BTreeMap<String, String>, RunnerError> {
    let mut values = BTreeMap::new();
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            return Err(RunnerError::Resolve(format!(
                ".env 第 {} 行缺少 '='",
                line_no + 1
            )));
        };
        let key = key.trim();
        validate_env_pair(key, value)?;
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        insert_environment(&mut values, key.to_owned(), value.to_owned());
    }
    Ok(values)
}

#[cfg(windows)]
fn wsl_path(path: &Path) -> Result<String, RunnerError> {
    // E:\x\y → /mnt/e/x/y
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s);
    if s.len() >= 3
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.as_bytes()[1] == b':'
        && s.as_bytes()[2] == b'/'
    {
        let drive = s[..1].to_ascii_lowercase();
        return Ok(format!("/mnt/{drive}{}", &s[2..]));
    }
    Err(RunnerError::Resolve(
        "WSL 脚本需使用默认 /mnt 挂载下的绝对盘符路径；UNC 路径请改用本机 Bash/Sh 或映射盘符"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn wsl转换盘符及中文空格路径并明确拒绝网络和相对路径() {
        for (input, expected) in [
            (r"E:\脚本目录\备份 空格.sh", "/mnt/e/脚本目录/备份 空格.sh"),
            ("C:/scripts/test.sh", "/mnt/c/scripts/test.sh"),
            (r"\\?\D:\文件.sh", "/mnt/d/文件.sh"),
        ] {
            assert_eq!(wsl_path(Path::new(input)).unwrap(), expected);
        }
        for input in [
            r"\\server\share\test.sh",
            "relative.sh",
            "E:relative.sh",
            "字:/test.sh",
            "E字:/test.sh",
        ] {
            assert!(wsl_path(Path::new(input)).is_err());
        }
    }

    #[test]
    #[cfg(windows)]
    fn 本机解释器按请求类型选择并支持空格和中文目录() {
        let directory =
            std::env::temp_dir().join(format!("scheduled-task-shell-{}", std::process::id()));
        let bash = directory.join("中文 bash");
        let sh = directory.join("中文 sh");
        std::fs::create_dir_all(&bash).unwrap();
        std::fs::create_dir_all(&sh).unwrap();
        std::fs::write(bash.join("bash.exe"), "").unwrap();
        std::fs::write(sh.join("sh.exe"), "").unwrap();
        assert_eq!(
            find_shell_in_paths("bash", [sh.clone(), bash.clone()].into_iter()),
            Some(bash.join("bash.exe"))
        );
        assert_eq!(
            find_shell_in_paths("sh", [bash.clone(), sh.clone()].into_iter()),
            Some(sh.join("sh.exe"))
        );
        assert!(find_shell_in_paths("sh", [bash.clone()].into_iter()).is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_inline_scripts_keep_distinct_content_and_are_cleaned_up() {
        let first = resolve(&ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline("echo first".into()),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            env_file: None,
        })
        .unwrap();
        let second = resolve(&ScriptSpec {
            kind: ScriptKind::Cmd,
            source: ScriptSource::Inline("echo second".into()),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            env_file: None,
        })
        .unwrap();
        let first_path = first.temp_files[0].clone();
        let second_path = second.temp_files[0].clone();
        assert_ne!(first_path, second_path);
        assert_eq!(std::fs::read_to_string(&first_path).unwrap(), "echo first");
        assert_eq!(
            std::fs::read_to_string(&second_path).unwrap(),
            "echo second"
        );
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
    }
}
