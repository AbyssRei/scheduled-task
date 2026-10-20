# scheduled-task · 定时任务管理器

面向 Windows 的桌面定时任务管理器，使用 Rust 与 GPUI Kit 构建。通过图形界面管理脚本、执行时间、自动重试、并发限制和运行日志，也提供命令行入口。

作者：AbyssRei <AbyssRei@outlook.com>

## 功能

- **时间调度**：每天、每月指定日或自定义多个时间点；按本地时间执行，当月不存在指定日期时跳过。
- **脚本执行**：Cmd、Windows PowerShell、PowerShell 7、Python（系统解释器或 uv）、Bash、Sh，以及手工指定命令；支持文件和内联脚本。
- **执行选项**：任务可设置命令行参数、工作目录、环境变量和多个 `.env` 文件；文件按列表顺序加载，后面的同名变量覆盖前面的，任务显式变量最后覆盖。兼容旧单文件 `env_file` 配置，先于新列表加载。
- **后台运行**：执行脚本默认隐藏终端窗口，输出显示在日志面板；脚本主动打开的其他窗口需自行控制。
- **输出监控与重试**：按连续输出窗口匹配候选内容，配置重试次数及冷却时间。
- **超时与停止**：按分钟设置执行超时；Windows Job Object 管理并终止任务进程树。
- **并发与排队**：全局执行数量上限、同任务单实例、同组互斥和有界等待队列。
- **桌面交互**：任务表格、日志面板、深浅色主题、系统托盘；关闭主窗口隐藏到托盘。
- **运行信息**：最近执行结果、开始／结束时刻、耗时和退出码，下次本地计划时刻及排队原因；日志可按所选任务筛选。
- **管理器保活**：独立守护进程及 Windows 计划任务在管理器退出后恢复运行，可在设置中开启或关闭，默认关闭。

## 使用

将发布包解压到固定目录后运行 `scheduled-task.exe`。通过“添加”配置任务；在“设置 → 任务运行”调整最大同时执行任务数，在“常规设置”管理保活，在“日志设置”调整记录等级、时区和存储限制。普通设置修改后点击底部“保存设置”，管理器保活独立立即生效。

添加和编辑支持文件／内联脚本、uv 项目目录及重试冷却秒数。任务变更先校验并保存，成功后生效；保存失败会显示错误并保留原配置及表单。编辑名称等字段会保留已有参数、环境变量和未改动选项。

单任务可配置 `env_files: [".env", "config/.env.local"]`。表单每行填写一个路径，支持多选追加；相对路径统一按最终工作目录解析，任一文件读取或解析失败会在启动脚本前报错。任务显式设置的环境变量优先级最高。

任务配置保存为 `%APPDATA%\scheduled-task\tasks.ron`，使用支持注释的 RON 格式。文件名为复数 `tasks.ron`；当前配置版本为 `config_version: 2`。手工修改配置前请退出程序并备份文件，保活启用时应使用托盘中的“关闭保活并退出”。

- [任务行为与开发规格](docs/BEHAVIOR.md)
- [管理器保活说明](docs/KEEPALIVE.md)
- [并发配置示例](crates/cli/testdata/concurrency.ron)

## 并发与排队

全局并行上限默认为 1，必须为正数。任务支持 `parallel`、`solo` 和 `group("备份")` 三种策略；同一任务在所有策略下都最多运行一个实例。相同互斥组名的任务不能同时执行，不同组可使用空闲槽位。

重复触发最多保留一个等待项。队列默认容量为 1024，满时拒绝新触发并记录原因。调度器选择最早可执行的等待项，同组保持 FIFO；自动重试及冷却期间仍占用执行槽位。

“停止”取消等待项并停止当前执行；禁用、删除任务也会取消等待项。提高并行上限会立即启动符合条件的等待任务，降低上限不会中断已经执行的任务。编辑排队任务会在启动时使用最新配置，编辑运行任务只影响后续执行。队列不落盘，退出后不恢复。

## 构建与验证

需要支持 edition 2024 的 Rust 工具链、Windows MSVC 构建环境和 PowerShell 7。脚本任务所需解释器须自行安装；Python 任务使用 uv 时还需安装 uv。

```powershell
cargo run -p scheduled-task
cargo run -p scheduled-task-cli -- run <tasks.ron>
cargo run -p scheduled-task-cli -- run <tasks.ron> --task 1
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
scripts\build.bat
```

CLI 单任务成功返回 0，执行失败或配置错误返回 1，命令行用法错误返回 2，任务不存在或准入拒绝返回 3。未知选项、缺少值和多余参数会直接报错。Cmd 参数使用批处理专用转义，空参数和 `& | < > ^ % !` 等符号按字面值传递；手工命令正文保留 shell 语义。

Windows 的 Bash/Sh 优先选择 PATH 中对应的本机解释器，未找到时通过 WSL 执行，默认将盘符路径映射到 `/mnt/<盘符>/`。WSL 回退不接受 UNC 脚本路径，非默认挂载可使用手工命令，具体边界见行为规格。

打包版本取自 workspace `Cargo.toml`，产物为 `dist/scheduled-task-v<版本>-dist/` 和同名 `.7z`；未安装 7z 时生成 `.zip`。发布程序名称固定为 `scheduled-task.exe`，包内附带使用说明、行为规格、保活说明和并发配置示例。也可运行 `pwsh -NoProfile -File scripts/build.ps1`，支持 `-SkipBuild` 和 `-NoZip`。

## GitHub Actions 构建与发布

[构建工作流](.github/workflows/build.yml) 在 `main` 推送、PR、`v*` 标签推送及手动运行时启动，使用 Windows x64 环境并从 `rust-toolchain.toml` 读取与本地一致的 Rust 工具链，依次执行 workspace 格式检查、零警告 Clippy、全量测试及 release 构建。工作流安装 uv 和测试用 Python，复用 `scripts/build.ps1 -SkipBuild` 打包，并验证 7z 压缩包；产物 `scheduled-task-windows-x64` 保留 30 天。

发布时先更新 workspace 版本及 `Cargo.lock`。每次推送前确认仓库没有未完成的 Action；先单独推送主分支改动，等待该次流水线全部成功后，再单独推送与版本完全一致的标签，标签也必须等上一条 Action 结束后逐个推送。标签不匹配会终止检查；仅标签推送且 CI 全部通过后才创建 GitHub Release，上传 `scheduled-task-v<版本>-dist.7z` 并自动生成发布说明。包含 `-` 的版本标签标记为预发布。主分支、PR 和手动运行只生成构建产物。提交格式、版本增量和标签不可变规则见[版本约定](docs/VERSIONING.md)。

所有直接使用的 Marketplace Action 仅固定大版本（2026-10-19 按官方发布页及实际 Git 引用核对）：[checkout@v7](https://github.com/actions/checkout/releases/latest)、[setup-rust-toolchain@v2](https://github.com/actions-rust-lang/setup-rust-toolchain/releases/latest)、[rust-cache@v2](https://github.com/Swatinem/rust-cache/releases/latest)、[setup-uv@v7](https://github.com/astral-sh/setup-uv/tree/v7)、[upload-artifact@v7](https://github.com/actions/upload-artifact/releases/latest)、[download-artifact@v8](https://github.com/actions/download-artifact/releases/latest)、[action-gh-release@v3](https://github.com/softprops/action-gh-release/releases/latest)。setup-uv 虽已发布 v10.3.0，但尚无 v10 大版本引用，因此使用当前可解析的最高大版本标签 v7。构建默认仅有仓库读取权限，只有发布任务获得 `contents: write`；无需另配个人访问令牌。自动测试不替代真实桌面交互验收。

## 项目结构

| 模块 | 职责 |
| --- | --- |
| `crates/core` | 配置、状态机、重试判定、调度、进程执行、并发队列和日志 |
| `crates/app` | GPUI Kit 桌面界面、托盘、设置及核心桥接 |
| `crates/cli` | 无头调度与立即执行入口 |
| `crates/keepalive` | 保活状态、单实例保护、守护进程与 Windows 计划任务 |
| `scripts` | 构建与发布打包 |
| `docs` | 行为规格和使用说明 |

依赖方向为 `app → core/keepalive`、`cli → core`，两个库均不依赖 UI。发布为单个可执行文件，守护使用同一程序的独立进程。

## 日志

任务输出以 INFO 逐行写入日志文件，包含任务 ID、名称和执行尝试序号，同时显示在界面日志面板。需要保留任务输出时，请将文件日志等级设为 INFO、DEBUG 或 TRACE。

管理器日志写入工作目录的 `logs/`，守护日志写入程序目录的 `logs/watchdog/`。文件日志默认 INFO，过滤 GPUI 等依赖的 DEBUG 调试输出。在“设置 → 日志设置”选择 ERROR、WARN、INFO、DEBUG 或 TRACE，再点击底部“保存设置”统一生效；DEBUG 和 TRACE 适合临时排查问题，可能产生大量日志。此选项控制管理器文件日志，不影响界面中的任务输出；CLI 使用所加载配置的 `log_level`，守护文件日志保持 INFO。控制台默认 INFO，仍可用 `RUST_LOG` 单独调整。

日志时区默认跟随系统，可改为 IANA 名称（如 `Asia/Shanghai`）或固定偏移（如 `+08:00`），统一控制界面、控制台和文件时间戳及按天轮转；任务调度仍使用系统本地时间。按文件名日期默认保留最近 7 天，可设置 3–365 天；可选总大小限制为 200–5120 MB，默认关闭。清理优先删除最早的历史日志，始终保护当前文件，因此当前文件独自超限时不会截断它。同一天重启继续追加，正常退出刷新待写日志；强制结束、崩溃、磁盘故障或断电仍可能丢失未落盘部分。

界面日志面板保留最近 200 行。高输出时界面事件缓冲有界，省略较早的展示日志并提示数量；文件记录仍按日志等级完整写入，后台文件队列拥塞时施加背压。执行结果默认只累计最近 256 KiB，超长物理行按字符边界分段回调，避免长期运行持续占用内存。每个任务的最近执行记录只保留在内存，删除或重启后清除。
