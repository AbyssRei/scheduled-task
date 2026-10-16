# AGENTS.md — 定时任务管理器（scheduled-task）

Windows 桌面定时任务管理器，使用 Rust + GPUI Kit。支持每日、每月及多时间点调度，输出监控重试，进程树终止，并发排队、系统托盘和管理器保活。

注释、UI 文案、commit message 均使用中文，commit 使用 conventional commits（如 `feat(runner): 添加执行选项`）。

行为规格见 `docs/BEHAVIOR.md`；状态机、重试判定及并发语义由核心单元测试与场景集成测试共同约束。**改变行为语义必须同步更新测试和行为规格。**

## 常用命令

- GUI：`cargo run -p scheduled-task`
- CLI：`cargo run -p scheduled-task-cli -- run <tasks.ron> [--task <id|名称>]`
- 全部测试：`cargo test --workspace`，包含状态、重试、配置、真实脚本、并发队列、表单及保活测试，必须全部通过。
- 静态检查：`cargo clippy --workspace --all-targets -- -D warnings`，必须零警告。
- 打包：`scripts\build.bat` 或 `pwsh -NoProfile -File scripts/build.ps1`。目标运行时为 PowerShell 7；bat 保持纯 ASCII。版本取 workspace `Cargo.toml`，产物为 `dist/scheduled-task-v<版本>-dist/` 与同名 `.7z`（无 7z 时生成 zip），支持 `-SkipBuild`、`-NoZip`。
- 依赖改动后执行 `cargo fetch`。

## 目录结构

- `Cargo.toml`：workspace 根，包含 core、cli、app、keepalive 四个 crate。
- `crates/core/src/`：不依赖 UI 的领域库。
  - `model.rs`：TaskConfig、RON schema v2、ConcurrencyPolicy。
  - `state.rs`：包含 Queued 的七态状态机和收尾归一化。
  - `retry.rs`：连续窗口交集、空白归一化及核心日志提取。
  - `runner/`：命令构建、执行生命周期、输出解码、进程注册与 Windows Job Object。
  - `scheduler.rs`：本地时间调度和 FireTracker 当日去重。
  - `store.rs`：`%APPDATA%\scheduled-task\tasks.ron` 读写。
  - `daemon.rs`：原子准入、执行上限、组互斥和有界队列。
  - `logger.rs`、`logger/daily.rs`：控制台和文件默认 INFO，文件等级即时调整并持久化；按 UTC 日追加、按日期保留 5 个日志并保护当前文件，退出时显式刷写，无单文件大小限制。
- `crates/core/tests/`：真实脚本执行及并发队列集成测试。
- `crates/cli/`：无头调度与立即执行入口；`testdata/` 含示例配置和脚本。
- `crates/keepalive/`：无 UI 的管理器保活、独立 `--watchdog` 入口、单实例和 Windows 计划任务集成；使用说明见 `docs/KEEPALIVE.md`。
- `crates/app/src/`：桌面应用。
  - `main.rs`、`desktop.rs`：启动、事件循环、托盘和原生窗口显隐。
  - `main_window/`：主窗口、表格、工具栏、任务操作和独立日志视图。
  - `task_dialog/`：对话框生命周期、纯表单转换校验和布局。
  - `settings_view.rs`：主窗口内设置对话框及主题应用。
  - `bridge.rs`：核心桥接和配置变更的统一处理。
- `scripts/build.bat`、`scripts/build.ps1`：构建与打包。
- `crates/app/assets/`：应用图标。

## 架构与开发纪律

- 依赖方向：app → core/keepalive；cli → core；core/keepalive 不依赖任何 UI crate。
- 常规状态变更必须经过 `TaskStateManager.transition()` 或 `finalize_status()`；`set()` 仅用于编辑任务时保留运行族状态。
- 进程注册表以 TaskId 为键，任务名称不是进程身份标识。
- 所有触发共用 `Daemon::admit_locked()` 原子准入决策，包括公开 `admit()`；每个 TaskId 单实例、最多一个等待项，重试冷却仍占执行槽位。
- UI 事件上行：std mpsc → 转发线程 → async-channel → `cx.spawn` 内 `.await`；命令下行直接调用 `bridge::AppCore`。
- 配置变更统一经 `AppCore::commit_task_changes()` 执行重载、保存及界面通知。`reload_preserving` 保留运行族状态和仍有效的当日触发标记。
- 管理器保活与任务重试分离：watchdog 不加载或执行任务；所有恢复入口遵守持久化开关。重启不恢复内存队列、不重跑中断任务、不补跑已过计划时刻。
- 保活测试使用临时状态和模拟系统适配层；未经明确要求，不得在开发机注册真实计划任务或启动真实配置中的任务。
- 交付前必须通过全部测试和零警告静态检查。模拟测试和程序启动不能代替真实桌面交互、登录、注销或关机验收。

## 平台约束

- gpui-tray 必须设置 `default-features=false, features=["gpui-kit"]`，避免引入两套 GPUI 依赖。
- 应用启动必须使用 `.with_assets(gpui_kit::assets::Assets)` 注册内置 SVG；图标颜色继承组件主题。
- `cx.new` 由 `AppContext` trait 提供；组件位于 `gpui_kit::component::<module>`；`Checkbox::on_click` 首参为 `&bool`。
- 设置、添加及编辑使用 `WindowExt::open_dialog` 主窗口内对话框，不创建独立原生子窗口。
- 关闭主窗口调用原生隐藏，托盘“显示窗口”使用 `SW_RESTORE` 恢复。
- RON 中建议使用正斜杠路径；runner 统一转换路径，并移除 cmd 不接受的 `fs::canonicalize` 结果中的 `\\?\` 前缀。
- Windows 子进程按 CREATE_SUSPENDED → 加入 Job Object → 恢复顺序启动，确保进程树受控。
