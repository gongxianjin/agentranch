# AgentRanch

**把"定稿设计文档"变成"已验收交付物"的本地工作台**：原生 Rust + egui 的 Windows 管理器 + Linux agent-worker。设计文档导入 → 任务拆分 → 逐项审核 → 开发 → 测试 → 验收，每一步都有显式门槛和可追溯证据，绝不把"看起来完成了"当作"完成了"。

> **定位**：面向高级开发者的自用型编码 Agent 调度工作台，**不是企业级基础设施**，也不是可供他人依赖的生产系统。

- **支持**：SSH 远程 Worker、四角色流水线、调度 Claude Code / Codex CLI、git worktree 隔离。
- **不承诺**：高可用、权限加密、多租户、公网安全加固、SLA 与故障兜底。
- **安全提示**：模型 API key 与每台 Worker 的账号密码以**明文**保存在本机配置文件，仅限单机个人内网场景使用；这台机器交给别人之前，先删除 `connection-settings.json`。
- **维护状态**：单人维护，issue / PR 响应视作者空闲时间，请勿按成熟商业项目的标准要求。

```
┌────────────────────── Windows ──────────────────────┐   ┌──── Linux B ────┐
│  agentranch.exe (GUI Manager)                        │   │  aw-cli (worker) │
│  ├─ 项目工作流（导入→拆分→审核→开发→测试→验收）      │──▶│  签名派工/回传    │
│  └─ aw-cli.exe (CLI / 受管 worker，同目录同版)       │   │  退出证据        │
│  SQLite 账本 · 阶段门控 · 操作回执                    │   └─────────────────┘
└──────────────────────────────────────────────────────┘
```

## 项目状态（WIP / Under Construction）

这个项目很年轻（2026-10 建库、单作者、无 star），正处在**作者自用原型**阶段。下面按"已可用 / 待实现 / 已知限制"三类如实标注；README 里每个"已完成"都有本地测试背书，没跑通的都明说。

### 已可用

| 能力 | 验证范围 |
|---|---|
| 14 阶段工作流 + SQLite 账本 + 人工门控 | 本地 headless 测试 + 真实 GUI pointer 测试 |
| 双击默认进入 real 模式、首次打开连接设置 | GUI 测试 |
| 模型配置 + 四角色 Worker 明文保存 / 重启恢复 | `rust_connection_settings` 8/8 |
| CLI（`check` / `mcp` / `local-check` / `runner` / `controller`） | 本地 |

### 待实现（WIP）

| 能力 | 现状 |
|---|---|
| 多 Worker 四角色流水线派工（拆分→开发→测试→审查） | 角色配置已落地，自动流转未实现 |
| 外部模型连接与真实执行 | `NOT_RUN` |
| 远程退出 / 取消证据 | `NOT_RUN` |

### 已知限制

| 限制 | 说明 |
|---|---|
| 凭据明文存储 | API key 与 Worker 密码明文落在本机 `connection-settings.json`，不加密 |
| 单机 / 单用户 / 无并发设计 | 不支持多租户、权限隔离 |
| 无签名 / 无 installer | 双击会有 SmartScreen 提示，属预期 |
| Windows 原生窗口验收 | 未做（`NOT_RUN`） |
| 构建平台 | 仅声明 Windows（x64 MinGW）+ Linux worker 路径，未声明其他平台已验证 |

## 30 秒开始

1. 从 [Releases](https://github.com/gongxianjin/agentranch/releases) 下载 zip，解压到任意目录（**不要**装进 Program Files）。
2. 双击 `agentranch.exe`（首次会有 SmartScreen 提示，见下方 FAQ——包未签名是预期）。**双击即 real 模式**；首次打开会弹出连接设置窗口。
3. 在连接设置里填模型配置（地址 / API key / 模型名）和至少一台 Worker（详见[连接设置](#连接设置模型与-worker-机)）。
4. 左侧输入项目名 → 新建项目 → 导入一份**已定稿**的 UTF-8 `.md`/`.txt` 设计文档 → 核对 SHA-256 → 确认版本 → 按界面引导走完流程。

CLI 入口：`aw-cli.exe --help`（`check` / `mcp` / `local-check` / `runner` / `controller`）。

## 为什么是 AgentRanch

AI 编码助手最危险的不是写不出代码，而是把"看起来完成了"交付给你。AgentRanch 的整个设计围绕一件事：**让每个"完成"都有独立于 AI 自述的证据**。

- **证据优先**：每个动作落 SQLite 账本；"已接受请求"≠"已执行"，"测试通过"≠"验收通过"，阶段与证据一一对应。
- **显式门控**：全部 14 个阶段（①新建 → ✓已验收）都有人工确认点；驳回必须写原因，返修最多 2 轮，验收必须亲自点。
- **本地优先**：状态、账本、产物全在本地 `--state-dir`；不匿名上报，不云依赖。
- **一处入口**：GUI 看进度与证据，CLI 做脚本化/MCP 集成，二者共享同一账本。
- **诚实失败**：配置缺失、链路未通、执行结果未知（`UNKNOWN`），界面都用人话说清楚差什么，而不是静默降级或假装成功。

## 快速导航

| 想做什么 | 去哪里 |
|---|---|
| 图形界面走完整流程 | [USER-GUIDE.md](USER-GUIDE.md) §快速上手 |
| 配置模型与 Worker 机 | 本 README [连接设置](#连接设置模型与-worker-机) |
| CLI/脚本/MCP 集成 | [USER-GUIDE.md](USER-GUIDE.md) §CLI 参考 |
| 查看每个阶段的意思 | GUI 项目页"当前阶段"行（①–✓ 编号即顺序） |
| 理解"被阻塞"怎么办 | 界面 ⚠ 卡片的【现在的情况】/【怎么办】三段式说明 |
| 验证与证据口径 | 本 README [打包与验收边界](#打包与验收边界) |
| 从源码构建/回归 | 本 README [开发构建](#windows-开发构建使用已安装-sqlite) |

## GUI 启动方式

在解压后的产品目录中打开 PowerShell 或命令提示符：

```powershell
.\agentranch.exe                                  # 默认 real 模式（双击同此）
.\agentranch.exe --local                          # 显式 local 模式（本地诊断）
.\agentranch.exe --state-dir "D:\state\workbench" # 显式状态目录
```

- **双击或无参数 = real 模式**；`--local` 显式进入本地诊断；`--real` 仍被接受（兼容旧脚本）。
- 启动时**不会**自动准备、连接或提交任何东西——需要你显式操作。
- `--state-dir` 必须后跟非空路径，不能重复；未知参数会显示启动错误而不是静默忽略。
- 默认状态目录：local `%LOCALAPPDATA%\agentranch\local`，real `%LOCALAPPDATA%\agentranch\real`。两种模式账本身份不同，不要互换或删除账本绕过错误。
- 两个 exe（`agentranch.exe` + `aw-cli.exe`）必须同目录同版本；GUI 启动时会校验。

## 连接设置（模型与 Worker 机）

首次打开（或点"连接设置"）会出现设置窗口，分两个独立分区：

**模型配置** — API 地址（HTTPS）、API key、模型名。对外部模型服务的全部请求走这里。

**Worker 配置机** — 每台 Worker 一行：角色、IP、账号、密码。四个固定角色对应一条开发流水线：

| 角色 | 干什么 |
|---|---|
| 拆分（split） | 把定稿设计文档拆成可执行任务 |
| 开发（develop） | 逐任务实现代码 |
| 测试（test） | 独立运行测试、验证行为 |
| 审查（review） | 汇总产物、审核证据 |

- 每个角色恰好一台 Worker，用"＋ 添加 Worker"追加；角色、IP、id 均不能重复。
- 不足四台时可以先配一台跑通链路（后续协作派工需要四台各就位）。
- 全部配置**明文**保在本机 `%LOCALAPPDATA%\agentranch\real\connection-settings.json`（schema 2，原子写入，重启恢复）——这是个人使用定位的取舍：本机单用户、不加密、不并发。如果这台机器会交给别人用，先删掉这个文件。

> ⚠️ 协作派工（拆分→开发→测试→审查的自动流转）仍在建设中；当前版本的角色配置已就位，流水线执行未实现。

## CLI 子命令

```powershell
.\aw-cli.exe --help
.\aw-cli.exe check --config .\config.json
.\aw-cli.exe mcp --config .\config.json
.\aw-cli.exe local-check --state-dir "$env:LOCALAPPDATA\agentranch\local"
.\aw-cli.exe runner --config .\config.json --once
.\aw-cli.exe controller --config .\config.json --once
```

- `check` 只校验配置（`external_readiness: not_probed`），不连接外部服务。
- `local-check` 运行本地固定诊断，输出带 `simulated: true`，不是远程执行证明。
- `mcp` 走 stdin/stdout 协议；不要把日志混入协议流。
- `runner`/`controller` 需要受信任配置；`--once` 与 `--stop-on-stdin` 互斥。
- 非零退出码时保留 stdout/stderr 与状态目录；不要靠重跑或删账本把 `UNKNOWN` 当成功。

## local 与 real 的区别

- **local**（`--local` 显式进入）：随 Rust 核心内置的受控 fixture，用于熟悉界面/检查状态目录/SQLite/受管进程，明确标记模拟，不代表真实执行。
- **real**（默认）：显式真实模式。配置缺失不会静默回退 local；需要模型与 Worker 配置就位，由实际运行与退出证据证明链路可用。仅配置文件存在、`check` 通过、按钮可点都不等于真实执行完成。

## 常见问题（FAQ）

**双击后提示找不到 worker** — 确认 `aw-cli.exe` 与 `agentranch.exe` 同目录且来自同一冻结包；不要从 PATH 拼接。

**SmartScreen/签名警告** — 包默认未签名，属预期；本包无 installer、无签名声明。

**状态模式不匹配** — 复用了另一模式的状态目录。保留原目录作证据，用正确的 `--state-dir` 重启；不要删文件强启。

**real 模式未就绪** — 诚实失败路径。按界面 ⚠ 卡片的【怎么办】步骤补配置；以实际探测与退出证据为准。

**"拆分任务"点了没反应？** — 先看按钮下 ⚠ 卡片：未配置执行主机会明确告知三步操作；已配置但链路未打通会说明这是版本能力边界，不是操作问题。

**密码和 API key 是明文存的？** — 是，见[连接设置](#连接设置模型与-worker-机)。个人单机使用的定位；共享机器前请删除 `connection-settings.json`。

## Windows 开发构建：使用已安装 SQLite

不启用 `rusqlite/bundled`、不编译 `sqlite3.c`；使用本机已安装的 x64 MinGW SQLite（当前环境 `C:\msys64\mingw64`，SQLite 3.52.0；最低 3.45.3）：

```powershell
.\packaging\cargo-windows.cmd test --target x86_64-pc-windows-gnu --locked --offline --no-default-features
.\packaging\cargo-windows.cmd build --target x86_64-pc-windows-gnu --locked --offline --bins
```

入口只为子进程设置 MinGW-first PATH、固定 Rust 1.99.1、`SQLITE3_INCLUDE_DIR`/`SQLITE3_LIB_DIR`、`SQLITE3_STATIC=0`、`SQLITE3_NO_PKG_CONFIG=1`，不改全局环境、不下载安装。Windows 包必须经 PE import 递归收集器携带 SQLite DLL 及实际依赖；打包成功不等于运行验收通过。其他平台需本地可链接 SQLite；不声明 Linux 构建已验证。

一键回归（Rust + Python 全量，约 3–4 分钟）：

```powershell
py -3 -I .test-scripts\local\run_regression.py
```

## 打包与验收边界

`packaging/package_windows.py` 只从已生成且同版身份绑定的输入构造可审计便携目录和 ZIP；不构建 Rust、不启动 GUI、不外发请求、不签名、不安装。

冻结版要求同一 source inventory、offline Cargo metadata、build identity 与同目录两个 Windows PE；build identity 机械校验 source digest、target、toolchain、profile、版本及两个 exe 的字节 SHA-256。打包前检查：两 exe 存在且为 PE、source 无漂移、build identity 一致、PE imports 可解析、输出已存在则失败保留旧证据、`PACKAGE-MANIFEST.json` 与 ZIP entry/字节回环核对。失败退出码 `2` 并保留 `PACKAGE-FAILURE.json`；成功状态 `PACKAGE_CREATED_RUNTIME_NOT_YET_VERIFIED` 仅表示包内输入与哈希核验完成，发布前仍需同版原生运行验收。

## 验收边界口径

上面「待实现」与「已知限制」中的条目，在内部验收记录里统一标为 `NOT_RUN`，含义是**未实际运行验证**，不是"已失败"或"已损坏"：外部模型连接与真实执行、真实项目开发、多 Worker 协作派工、远程退出/取消、Windows native window acceptance、签名/installer、Stop10 及其他未实际运行的 gate。

已运行验证的范围仅限：本地 headless Rust/SQLite、真实 GUI pointer action、本地 service/ledger、synthetic ingress、独立 `ExitProof` 与 managed-artifact 状态投影、连接设置明文持久化。这些结果不证明外部执行，也不证明真实产品包已验收——引用本仓库时请以实际探测与退出证据为准。

## License

主仓库（Rust 核心 + egui 管理器 + CLI）：[MIT](LICENSE) © 2026 advengong。

`agentranch-dsh-plugin/` 子目录为独立 MIT 协议，见 [agentranch-dsh-plugin/LICENSE](agentranch-dsh-plugin/LICENSE)，其 `package.json` 亦声明 `"license": "MIT"`。

MIT 协议按"现状（AS IS）"提供，不附带任何担保。上文"定位 / 不承诺 / 安全提示"是使用前必须知道的边界，协议本身不会替你承担明文凭据、数据丢失或误操作的风险。

---

*AgentRanch · native Windows manager + Linux agent-worker workbench · Rust + egui + SQLite*
