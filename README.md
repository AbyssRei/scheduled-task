# scheduled-task · 定时任务管理器

面向 Windows 的桌面定时任务管理器，使用 Rust 与 GPUI Kit 构建。通过图形界面管理脚本、执行时间、自动重试、并发限制和运行日志，也提供命令行入口。

作者：AbyssRei <AbyssRei@outlook.com>

## 功能

- **时间调度**：每天、每月指定日或自定义多个时间点；按本地时间执行，当月不存在指定日期时跳过。
- **脚本执行**：Cmd、Windows PowerShell、PowerShell 7、Python（系统解释器或 uv）、Bash、Sh，以及手工指定命令；支持文件和内联脚本。
- **后台运行**：执行脚本默认隐藏终端窗口，输出显示在日志面板；脚本主动打开的其他窗口需自行控制。
- **输出监控与重试**：按连续输出窗口匹配候选内容，配置重试次数及冷却时间。
- **超时与停止**：按分钟设置执行超时；Windows Job Object 管理并终止任务进程树。
- **并发与排队**：全局执行数量上限、同任务单实例、同组互斥和有界等待队列。
- **桌面交互**：任务表格、日志面板、深浅色主题、系统托盘；关闭主窗口隐藏到托盘。
- **管理器保活**：独立守护进程及 Windows 计划任务在管理器退出后恢复运行，可在设置中开启或关闭，默认关闭。

## 使用

将发布包解压到固定目录后运行 `scheduled-task.exe`。通过“添加”配置任务，在“设置 → 常规设置”调整最大同时执行任务数及保活开关。

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

打包版本取自 workspace `Cargo.toml`，产物为 `dist/scheduled-task-v<版本>-dist/` 和同名 `.7z`；未安装 7z 时生成 `.zip`。发布程序名称固定为 `scheduled-task.exe`，包内附带使用说明、行为规格、保活说明和并发配置示例。也可运行 `pwsh -NoProfile -File scripts/build.ps1`，支持 `-SkipBuild` 和 `-NoZip`。

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

管理器日志写入工作目录的 `logs/`，守护日志写入程序目录的 `logs/watchdog/`。文件日志默认 INFO，过滤 GPUI 等依赖的 DEBUG 调试输出。在“设置 → 常规设置 → 文件日志等级”选择 ERROR、WARN、INFO、DEBUG 或 TRACE，立即生效并保存；DEBUG 和 TRACE 适合临时排查问题，可能产生大量日志。此选项控制管理器文件日志，不影响界面中的任务输出；CLI 使用所加载配置的 `log_level`，守护文件日志保持 INFO。控制台默认 INFO，仍可用 `RUST_LOG` 单独调整。

日志按 UTC 日期轮转（北京时间每天 08:00 切换），同一天重启继续追加。按文件名日期保留最多 5 个日志，清理时始终保留当前文件，不处理备份文件。正常退出会刷新后台待写日志；强制结束、崩溃或断电仍可能丢失未落盘部分。当前没有单文件大小上限，高输出任务仍可能产生较大的当天日志。界面日志面板仅保留最近 200 行，不影响文件日志。
