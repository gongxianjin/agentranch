# agentranch 用户手册（v0.2-win）

> 本手册与 v0.2-win 交付物对齐：Rust + egui 原生 Windows 管理端、随包 Python/Paramiko 网关、Linux 原生 Worker。旧版 Python/Web 入口已随本分支移除。
>
> **诚实边界**：本手册只描述已验证的能力。所有"已验证"均指本仓库 evidence 链背书的具体场景（详见附录 A），不泛化到未运行的环境。

## 1. 产品是什么

一个"Windows 管理 → Linux 真实执行"的跨机工作台：

```
Windows Manager（原生 GUI）          Linux Worker（172.28.8.221，示例部署）
┌─────────────────────────┐  SSH(Paramiko)  ┌──────────────────────────┐
│ agentranch.exe │ ──────────────> │ 原生 Rust ELF（owner-exec）│
│ + aw-cli.exe（同版配对）  │   signed start  │ → 真实模型调用 → artifact   │
│ + 随包 Python 运行时      │ <────────────── │ → signed terminal         │
└─────────────────────────┘  Store admission └──────────────────────────┘
```

- **项目工作流**（项目创建 → 设计文档导入确认 → 任务拆分审核 → 手动开发 → 测试/返修/验收）在 GUI 内闭环；
- **真实执行链**：GUI dispatch → Paramiko 网关 → Linux 原生 Worker → 进程内模型调用（TLS 证书锁定）→ artifact 回传 → Store admission → assignment 自动释放。

## 2. 包内容（v0.2-win 冻结包）

冻结包目录名形如 `agentranch-0.3.0-windows-x86_64-release-<source-sha8>`，包含：

| 文件 | 作用 |
|---|---|
| `agentranch.exe` | GUI 二进制（双击启动） |
| `aw-cli.exe` | CLI/受管 worker 二进制（**必须与 GUI 同目录同版**） |
| `runtime/python/` | 随包 Python 3.14.3 + Paramiko 4.0.0（网关用，不装服务、不改 PATH） |
| `libsqlite3-0.dll` | SQLite 3.52.0 运行库（包内携带） |
| `BUILD-IDENTITY.json` | source sha / 双 exe sha256 / toolchain 绑定 |
| `PACKAGE-MANIFEST.json` | payload 字节数与 SHA-256、ZIP entry 核对 |
| `README.md` / `PACKAGE-NOTES.txt` | 包内说明 |
| `source-identity/`、`SOURCE-INVENTORY.json`、`PYTHON-RUNTIME-INVENTORY.json`、`third-party-licenses/` | 审计身份与许可证 |

## 3. 安装与首次启动

1. 解压 ZIP 到任意本地目录（无需安装器；SmartScreen 警告见 §9）。
2. 保留完整解压目录，不要只复制 exe，也不要用其他版本的文件拼接。`agentranch.exe` 与 `aw-cli.exe` 同目录；真实 Worker 准备还需要随包的 `tools/paramiko_provision.py`、`tools/paramiko_gateway.py`、`runtime/python/` 和 `tools/` 下已审核的资产清单、known_hosts、TLS 证书。仅含 exe/DLL 的界面验收目录不是可连接 Worker 的完整程序包。
3. 双击 `agentranch.exe`（local 模式，受控 fixture），或在终端：

```powershell
.\agentranch.exe                        # local 模式
.\agentranch.exe --real                 # real 模式
.\agentranch.exe --real --state-dir "D:\ao-state\real-221"
```

- `--state-dir` 必须后跟非空路径，不能重复；未知参数显示启动错误而非静默忽略。
- 默认状态目录：local `%LOCALAPPDATA%\agentranch\local`，real `...\real`。
- **local/real 状态身份不同，不要复用账本**；状态目录含 SQLite/锁/身份文件，处理故障先备份。

## 4. 快速上手：六项输入到真实结果页面（real 模式）

普通项目页只显示三段主路径（①新建项目 → ②定稿设计 → ③提交项目）；"现在做："一行直接告诉你下一步点哪里；出现 ⚠ 需要处理 卡片时先按它的说明处理。任务拆分、手动开发、独立测试、返修和人工验收属于“高级 · 本地诊断”，不属于六字段 onboarding 的普通 real-mode 主路径。

主页面优先显示项目流程，然后是“模型实时输出（SSE）”；Worker 只保留紧凑状态和“连接设置”入口。点击“连接设置”，在弹窗内填写六项：

1. Linux Worker IP；
2. root 账号；
3. root 密码；
4. 模型服务地址（第三方模型 HTTPS origin，不带路径；中性标签不表示新增供应商协议支持）；
5. API key；
6. 默认模型。

- **保存配置**只保存本机设置，不启动 SSH、Worker 或模型：IP、root 账号、服务地址、模型保存在当前状态目录的 `worker-settings.json`；API key（最多 2560 UTF-8 字节）保存在同一 Windows 用户、本机的 Windows 凭据管理器中，不进入普通 JSON。root 密码永不保存。
- 关闭弹窗可回到流程；重开程序会恢复保存配置，仍需输入本次 root 密码并完成当前会话认证。保存或恢复配置不代表 Worker 已连接。
- 已保存的 API key 缺失、无效或 Windows 凭据管理器读取失败时，只要普通配置文件的版本、字段、状态目录及服务地址绑定校验通过，四项普通配置仍会恢复；API key 留空并提示重新输入和保存，root 密码仍需本次填写。恢复不会自动连接或提交任务。配置文件本身损坏、越界或绑定不符时不会部分恢复，也不会读取其凭据引用。
- 更改服务地址会清空已恢复的 API key，需为新地址重新填写；重开弹窗不会覆盖未保存的普通字段。同地址凭据后来可读时可恢复 key 并更新提示，仍不代表当前会话已认证。保存/读取失败会明确提示，不退回明文保存。小窗口内表单可滚动，保存、准备、关闭按钮在表单之外。

点“自动准备并连接 Worker”会先保存配置；保存失败时不发起准备。随后 Manager/随包 Helper 会自动完成 root 身份验证、普通 Linux 用户发现、目标用户 home/`.ssh`/`authorized_keys` 初始化、公钥追加安装、Worker/loader 上传、配置生成、资产与远端身份预检，并在本次 Manager 状态目录内激活生成的连接。缺失路径会自动创建；这三个对象的 owner/必要权限会自动修复，保留已有公钥，不递归改动用户文件。不需要手工 chmod/chown。非正规文件、符号链接或其他普通用户拥有的路径仍会阻断；SSH/TLS 校验不变。普通用户不需要选择或编辑 `gateway.json`。

远端预检还必须通过只读 `aw-cli --capabilities`：Linux Worker 需要声明 `project-work-v1`、模型 token 帧与 `AO-LIVE/1` 输出能力。仅版本号、文件哈希或旧版 `--help` 正确不能判为项目就绪。打包 provisioning 资产时须同时提供 `--linux-build-input` 与 `--linux-build-result`，将本次源码和 Linux Worker 输出绑定。能力声明和构建记录不等于真实模型/SSE 已通过；仍需实际任务验证。

下面是产品操作顺序；本仓库的 local/fixture 回归不证明真实跨主机 onboarding、真实模型调用或生产 readiness：

1. **连接 Worker**：填写上面的六项，点击自动准备；Windows 只负责控制和显示，任务、模型调用与资源门禁在 Linux Worker 上执行。
2. **创建项目**：GUI 内新建项目；onboarding 成功不会自动创建空项目。
3. **导入设计文档**：UTF-8 `.md`/`.txt`，上限 256 KiB。检查显示的版本 U、文件名、完整文本、小写 SHA-256，确认无误后点"确认此上传版本"。版本/哈希过期时确认会被拒绝。
4. **提交项目**：确认设计文档后，普通 real-mode 用户直接点击“提交项目”。只有当前 Manager 会话的自动准备、生成 Gateway、本地激活和远端 preflight 门禁都存在时才允许提交；“Worker 已连接，可提交项目”不等于模型或业务结果已完成。顶部状态与普通页按钮共用当前快照的就绪判断：准备或提交待回执、已有执行中/UNKNOWN 项目、当前凭据不可用、预检未通过或名额占用都会阻止新提交，并在页面说明原因；旧的已准入结果不会盖过另一任务的阻塞状态。名额占用时应查看现有任务和回执，不是反复准备 Worker、手动释放或自动重跑。
5. **查看结果**：只有 Store admission、完整终态证据和 artifact 校验全部通过后，才可把结果显示为已准入；未产生终态证据时保留执行中或 UNKNOWN，不自动重跑或手工释放。
6. **确认释放**：正常结束时 assignment 自动释放、used_slots=0；无需也不应手工释放。

### 执行活动与模型实际输出

**固定位置：项目工作流 → 选择项目 → 详情顶部流程卡下方“模型实时输出（SSE）”。** 输出区在提交前就显示；未建项目时欢迎页也说明该入口。任务提交被接受后，会自动选中对应项目并回到详情顶部一次，之后手动滚动不会被每帧拉回。

- 尚未提交：提示确认设计文档后点击“提交项目”，不会伪造连接或正文。
- 已提交但尚无正文：在正文区显示“等待首段正文”；连接状态和实际收到正文分开说明。
- 已有任务但本次会话没有实时正文：明确提示“不会回放历史输出”，请查看结果核对和操作回执，不要重复提交。
- 任务结束且未收到正文：明确说明本次没有正文，不继续显示等待首段。

收到正文后，该区显示当前请求的实际内容和下面三个独立字段；项目列表仍显示本次请求的 compact 执行徽标。切换项目不会清除当前输出，返回原项目时仍按请求和项目显示对应正文。页面只保留当前会话最近一次请求，关闭重开后不恢复正文，不提供历史回放。

- **输出通道**是 Manager 本地收到的显示状态：已提交等待连接、已连接等待正文、已连接并收到正文、中断、模型输出已结束、已结束。它只描述本地展示事件，不证明 Worker、供应商或模型仍然存活。
- **最终任务结果**只由最终任务回执及其证据路径决定：尚未返回、完成、失败、已取消或结果未知。收到 `Error` 不会直接把任务标成失败；“模型输出已结束”也不等于任务完成。SSE 面板内最终结果只列一次；终态回执到达后不再保留等待最终结果的阶段说明，已收到的正文、实际错误及截断提示仍保留。
- **下一步**是固定的本地操作提示，不是远端健康判断：等待正文/回执、继续查看实际输出、等待最终证据、查看操作回执、以取消回执为准，或在未知时保留名额。未知结果不得手动释放名额，也不会自动重跑。
- 正文按实际收到的内容逐段显示为纯文本，不执行模型生成的 HTML，不伪造进度或思考过程。
- “已接收正文”统计 Manager 回调收到的 UTF-8 bytes / KiB，不是 token 数。提交时刻标为 UTC；耗时和最近正文时间来自 Manager 本机，不能证明远端仍在运行。
- 默认跟随最新；向上滚动后暂停跟随，出现“有新内容”时可点“回到最新”。正文仍可选择；本轮不包含独立暂停的 OS 剪贴板修复。
- 页面最多保留前 1 MiB 正文；超出时明确提示“展示已截断”。这只限制显示，不替代或停止既有终态、退出证据、产物校验流程；未展示的正文不会回放，完整内容必须通过最终产物核对。
- 任务完成时仍需核对产物目录和操作回执；失败、取消和未知分别按各自回执处理，不根据局部正文推断任务结论。

### 结果核对摘要

普通项目页的“项目提交”卡会显示只读的“结果核对”摘要，分开说明四件事：

- **最终任务结果**来自最终任务回执，表示任务是完成、失败、已取消、结果未知，还是尚未返回。
- **准入状态**不是最终任务结果的同义词。只有已有 `owner_admission=admitted`、完整 `exit_proof` 和非空 `artifact_dir` 同时满足时，页面才显示“已准入”。因此 `done` 但证据不完整时会显示“完成 / 未准入”，不能据此宣称成功交付。
- **产物目录**只是回执/Store 已提供路径的只读显示；页面不会因为显示路径就验证本地目录存在，也不把路径当作远端存活证明。没有可用路径时显示“尚未物化”。
- **回执提示**告诉你下一步：完成时核对产物目录和操作回执；失败时查看回执且不会自动重跑；取消时以最终取消回执为准；未知时显示“名额保留”，不得手动释放或自动重跑；运行中则等待最终任务回执。

同一项目已有一次 `project-work-v1` 任务后，“提交项目”按钮不会作为隐式 retry 入口再次开放。`done`、`failed` 和 `cancelled` 会显示“本项目已有任务结果；如需再次执行，请新建项目”，并保持提交按钮不可用；`unknown` 继续保留名额并禁止手动释放或自动重跑。当前版本不新增 retry、cancel、release、duplicate 或新建项目按钮；如需重新执行，应新建项目，而不是重复点击当前项目的提交入口。

结果摘要跟随当前项目，不会把其他项目的状态、正文或产物路径带到当前页面；当前页面仍不提供历史回放、打开目录、复制路径或下载动作。`ModelFinished` 和 live `Error` 仍不会替代最终任务结果权威。

这些行为有本地合成与 headless egui 测试覆盖，不代表当前版本已通过实际 Windows Manager、Linux Worker、真实供应商、provider/model、浏览器或 clipboard 验收。

“高级 · 本地诊断”仍保留旧的任务拆分、审核、手动开发、独立测试、返修和人工验收控件；它们不作为普通 onboarding 提交项目的前置步骤。

真实跨主机闭环仍待具备匹配版本 Worker 包、固定 known_hosts、TLS 信任材料和新的授权凭据后执行；当前文档不把历史 evidence 或本地 fixture 结果当作本次真实验收。

## 5. CLI（aw-cli.exe）

```powershell
.\aw-cli.exe --help
.\aw-cli.exe check --config .\config.json            # 仅校验配置，不探测外部
.\aw-cli.exe local-check --state-dir "$env:LOCALAPPDATA\agentranch\local"
.\aw-cli.exe mcp --config .\config.json              # stdin/stdout 协议，勿混入日志
.\aw-cli.exe runner --config .\config.json --once
.\aw-cli.exe runner --config .\config.json --stop-on-stdin
.\aw-cli.exe controller --config .\config.json --once
```

- `--once` 与 `--stop-on-stdin` 互斥。
- CLI 非零退出码时保留 stdout/stderr 与状态目录；不要靠重跑或删账本把 UNKNOWN 当成功。
- real 模式不会静默回退 local；配置存在 ≠ 执行链就绪。

## 6. Linux Worker 侧配置（部署者）

real 模式需要受信任配置提供网关与 Worker 依赖。已验证部署形态（示例 .221）：

- **内部网关配置**（`native-ssh-gateway/1`）：由 Manager 在本次 provisioning 状态目录内生成并校验；普通用户不需要接触。其字段包括 host/port、发现的普通用户、known_hosts 固定路径、worker ELF sha256/bytes、deployment 目录、`launch_mode`（direct / delegated-user-service）、可选 `remote_library_path/sha256/bytes`（LD_LIBRARY_PATH 注入）。
- **Worker 配置**（`native-worker-config/1`）：cgroup_root_mode（explicit / current-unit）、expected_worker_timeout_secs 等；字段校验规则见 `packaging/ssh-loader-path.txt`。
- **准入链**：worker ELF 身份 → signed start → 原生 owner-exec（cgroup 隔离）→ signed terminal evidence → Store admission → assignment 释放。传输层退出码/算术超时不释放槽位，Store 只认完整签名原生终态证据。
- **模型端点**：进程内调用，TLS 证书锁定（cert pinning），不落盘、不进 argv/日志。
- **秘密边界**：root 密码不保存；用户保存的 API key 仅由 Windows 凭据管理器持久化。准备 Worker 时，root 密码与 API key 通过有界 `AW-WORKER-ONBOARDING/1` 帧协议经 stdin 在内存传递；普通用户 SSH 私钥由 Helper 生成并只经结果管道交给 Manager，不落普通文件。模型 token 不进入 gateway.json、数据库、evidence 或日志。

## 7. 可观测性与验收证据

真实会话的判定不靠界面绿灯，靠 evidence 链（`dist/` 会话目录，.gitignore 排除、本地保留）：

| 证据 | 含义 |
|---|---|
| `evidence/result.json` | harness 判定（PASS_MANAGER_PROJECT_CLOSURE） |
| `evidence/manager-state/manager-diag.log` | 3 事件（gateway_configured / dispatch_result / artifact_materialized），run_id 匹配 |
| `evidence/store-acceptance.json` | 独立复刻（inspect_project_store 重推）PASS |
| `evidence/manager-state/artifacts/<run_id>/` | 结果 artifact（含 sha256 链） |
| `evidence/browser-render-check.json` | 渲染核验 RENDER_OK（辅助 sanity） |
| `model_evidence`（DB） | 模型 status/elapsed/request·response sha256 |

验收工具：`tools/live_project_closure.py`（含 `inspect_manager_diag` diag gate、`inspect_project_store` 独立复刻）。回归基线：Rust 388/388、Python 119 passed 1 skipped。

## 8. 安全边界（务必遵守）

- 不要手工修改 SQLite、身份文件、文档哈希、审批字段来绕过阶段门槛。
- 不要把历史报告/旧 attempt/旧审批当当前执行证明。
- 关闭 GUI 只停本地工作台，不等于远端任务已取消。
- SSH：Paramiko RejectPolicy + 安装者提供的固定 known_hosts，不弱化、不 TOFU、不猜 key；禁止从旧日志恢复秘密。
- root 密码、普通用户私钥只在受控内存/二进制管道中流转；API key 可由 Windows 凭据管理器按当前用户/本机保存，其他传递仍走受控内存/管道；模型 token 绝不进普通配置、签名 envelope/gateway.json/数据库/evidence/日志。凭据管理器不隔离同一用户权限下的恶意程序，不能替代当前会话认证。
- 未配置、未实现、执行失败、证据未建立是不同状态；按阻塞原因处理，不伪装成功。
- 已验证场景之外（其他主机/用户/模型端点/平台）一律视为未验证。

## 9. 常见问题

| 现象 | 处置 |
|---|---|
| 找不到 worker | 确认 `aw-cli.exe` 与 GUI 同目录同版，勿从 PATH 拼接 |
| 状态模式不匹配 | 保留原目录作证据，用正确 `--state-dir`，勿删文件强启 |
| real 未就绪 | 诚实失败路径：检查受信任配置、节点准入、AO/Reason 依赖，以实际探测与退出证据为准 |
| SmartScreen 警告 | 包未签名，属预期；无签名声明 |
| dispatch 后无 artifact | 检查 manager-diag.log 事件与 result.json status；中间截图外观不代表流程失败（已实证） |
| 中间状态"新项目"字样 | 输入框默认值，非占位符；不代表未创建 |

## 10. 已验证边界与未覆盖项

**当前树已验证**：Windows 本地 Rust/Python focused 与 formal fixture regression；六字段 CLI 契约、秘密帧边界、当前 provisioning output/ledger 绑定、失败不 dispatch、Store 证据结构校验和 GUI 产品路径测试。它们只证明代码在已覆盖的本地条件下按门禁工作。

**明确未覆盖**：真实 SSH/root UID 0、普通用户发现与公钥安装、匹配版本 Worker 部署、远端 preflight、真实模型认证/生成、签名终态证据、真实 Store admission、artifact materialization，以及打开并交互验证成都 HTML；这些均为 NOT_RUN/UNVERIFIED，不能用历史 evidence 或 local fixture 代替。

真实跨主机闭环仍待具备匹配包、known_hosts、TLS 与新授权材料后执行。

## 附录 A：历史材料索引（不代表当前树或当前环境已验收）

历史 `dist/` evidence 仅作为审计材料保留。除非同一源树、同一包身份、同一信任材料和同一运行记录被重新核对，不得把历史 run_id、artifact、模型响应或 Store 状态当作当前成功依据。

## 附录 B：术语

| 术语 | 含义 |
|---|---|
| Store | worker 侧准入/槽位账本；只认完整签名原生终态证据 |
| assignment / used_slots | 一次派发的占用与释放；正常闭环自动归零 |
| signed start/terminal | Worker 启动/终态的签名证据帧 |
| owner-exec | Worker 以资源属主身份原生执行业务子进程的入口 |
| diag gate | harness 对 manager-diag.log 三事件的永久门槛 |
| 冻结包 | source-inventory + build-identity + 双 exe 同目录的不可变交付目录 |
