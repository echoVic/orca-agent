# Orca

为终端打造的 DeepSeek 原生编程智能体。

给 Orca 一个任务，它会读取代码、编辑文件、运行命令、验证结果，并持续工作，
直到任务完成或需要你的决定。交互式工作使用 TUI，脚本和 CI 使用 `orca exec`。
Orca 使用 Rust 构建，在本地运行，并采用 MIT 许可证。

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[官网](https://orcaagent.dev/) · [更新日志](https://orcaagent.dev/changelog/) · [版本发布](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## 安装

```bash
npm install -g @blade-ai/orca
```

也可以直接安装原生二进制文件：

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

Windows PowerShell 使用：

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

在项目目录中执行以下命令，为该工作区配置受限沙箱 capability：

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

npm 包支持 macOS、Linux 和 Windows 的 ARM64 与 x64 平台。也可以从
[GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest) 下载预编译文件。

Windows 上会优先使用 PowerShell 7；即使它不在 `PATH` 中，Orca 也会检查标准
安装目录。未安装 PowerShell 7 时，受限会话回退到 `cmd.exe`。Windows
PowerShell 5.1 仅适用于不需要 AppContainer 隔离的显式配置。协议中的命令数组
会作为原生 Windows argv 直接启动，不再经过 shell 二次解析；旧字符串命令仍按
已解析的 shell 方言执行。

## 使用

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # 打开终端界面
orca exec "修复失败的测试"                 # 无界面运行
printf '%s' "$INSTRUCTION" | orca exec   # 提示词不出现在命令行参数里
orca exec --verifier "cargo test" "修复它" # 完成前执行验证
orca exec resume SESSION_ID "继续"        # 恢复无界面会话
orca exec resume --last "继续"            # 恢复最近的会话
orca exec resume SID --resume-at MID "继续"  # 恢复到消息边界为止
orca --resume [SESSION_ID]                # 恢复保存的会话
orca --fork SESSION_ID                    # 分叉保存的会话
orca --mode=acp                           # 连接 ACP 客户端
orca doctor                               # 在本地检查密钥、目录信任和沙箱
```

Windows PowerShell 使用 `$env:DEEPSEEK_API_KEY = "sk-..."` 设置密钥；后续 `orca` 命令相同。
仍然支持把提示词作为位置参数传入；如果提示词里有任务之后会在进程表中搜索或结束的内容，请通过 stdin 传入，
这样它不会出现在 `orca` 的命令行里。

Orca 还提供可选的[共享 ACP 会话](docs/acp-daemon.md)（Unix 上的 `orca daemon`、`orca attach` 和 `orca acp-bridge`）、
[文件定义的子代理](docs/subagents.md)和[持久化终端输出分页](docs/architecture/adr/0006-unified-exec-terminal-service.md)。

### 终端界面

在项目目录里运行 `orca`。第一次在某个目录运行时，Orca 会请你选择信任该目录或以不信任方式继续：
信任后 Orca 才会加载项目自带的配置、指令、skills、agents 和 workflows，它不会开启、也不会绕过操作系统沙箱。
之后输入任务并按 `Enter`。

- **引用与命令。** `@` 引用文件、skills、插件和 MCP 资源；`$` 插入 skill；`/` 打开命令菜单；`?` 列出全部按键。`Ctrl+V` 粘贴剪贴板里的图片。
- **查看进展。** 回复以 `●` 标记，思考过程折叠成一行 `⋯ thinking`，每个工具调用的输出显示在 `│` 竖线下方。`e` 展开最近一条折叠的输出，`Shift+E` 全部展开。状态栏显示审批模式、模型和推理强度、上下文剩余比例以及用量。
- **引导运行中的轮次。** `Esc` 中断。`Enter` 把追加消息排进队列，等下一轮发送；`Ctrl+Enter` 把它直接送进正在运行的轮次（需要支持 kitty 键盘协议的终端）；`Ctrl+B` 把当前轮次放到后台。
- **审批工具调用。** 需要审批的调用会把输入框变成审批面板：允许一次、允许这个调用（同一工具、同一目标以后不再询问）、本会话内允许该工具，或拒绝（`Esc`）。`Shift+Tab` 依次切换 `suggest` → `auto-edit` → `full-auto` → `plan`。进入 `full-auto` 前需要明确确认 Full Access；正在运行的任务会在下一次工具调用时生效，已经在运行的工具和已经启动的子代理继续使用原来的策略。模式变更只作用于当前会话，不会保存。
- **后台工作。** `/tasks` 显示对话下方的任务 dock，后台轮次、子代理、命令、监控任务和 Workflow child 都在这里。`/agents` 打开 Agent Workspace，可以查看每个任务的实时对话或记录，并执行它能安全执行的操作：停止、恢复、重试或追加消息。你空闲时后台 agent 完成，Orca 会带着它们的结果继续对话。`/workflows` 保留 Workflow 运行树。
- **计划、目标与回顾。** `/plan` 只读调查，最后给出待你批准的计划；`/goal` 设置持久目标；`/recap` 总结当前会话，你离开一段时间再回来时 Orca 也会自动写一份；`/side` 打开侧边对话，适合临时问个问题。
- **会话。** `/new`、`/resume`（按项目分组，可分叉、重命名、归档、删除和复制 Session ID）、`/fork [名称]`、`/rename [名称]`、`/model`、`/config` 和 `/copy [N]`。模型和推理强度的选择会保存到用户 `config.toml`，新会话也会沿用。`/status` 显示实际的 execution profile、shell sandbox 和 permission profile。`Ctrl+L` 只清屏，不清除会话；退出时 Orca 会输出 `orca --resume <SESSION_ID>` 恢复命令。

[终端界面指南](https://orcaagent.dev/docs/#terminal-ui)介绍了屏幕上的各个部分和全部按键。

已记录的会话默认开启项目自动记忆；需要明确记住用户或项目事实时，使用 `/remember`。
采集、召回、存储、隐私和删除见 [Memory](docs/memory.md)。

### 在 Pilion Browser 中使用 Orca

[Pilion Browser](https://github.com/echoVic/pilion-browser) 是一个作为 ACP 客户端的桌面浏览器。在它的 Agent 面板中选择 **Orca**，Pilion 会以 `orca --mode=acp` 启动 Orca、转发 `DEEPSEEK_API_KEY`，并把自己的标签页作为 MCP 工具（`browser_snapshot`、`browser_screenshot`、导航、点击、输入）交给 Orca 操作，支持操作前确认和人工接管。macOS、Windows、Linux 安装包见 [Pilion Releases](https://github.com/echoVic/pilion-browser/releases)。

## 核心能力

- 直接适配 DeepSeek 的推理和工具调用语义，支持 SSE 流式输出、前缀缓存友好提示词、
  自动上下文管理和请求重试。
- 读取、搜索、编辑和写入代码，运行 Shell 命令，并使用指定命令验证结果。
  `bash` 是唯一的命令启动入口，它启动的命令归属于任务而不是工具调用：长时间
  构建、CI watch 和交互式 PTY 会话在调用返回后继续运行。`yield_time_ms` 只限
  制本次调用等待多久，`timeout_ms` 才是唯一的调用方执行期限。`task_read_output`、
  `task_send_input`、`task_wait`、`task_stop` 通过 `task_id` 读取输出、写入输入、
  等待完成和停止命令。后台监督器会在无需继续轮询的情况下结算已退出或停止的
  会话，并在下一轮模型执行前注入一次有界的完成通知。
- 通过 `suggest`、沙箱内 `auto-edit`、完全访问 `full-auto` 和只读 `plan`
  模式控制风险，同时提供目录信任机制。
- 在本地保存对话历史，支持恢复、分叉、搜索、重命名、归档和压缩。
- 默认没有隐式轮次上限；可通过 `[budget]` 配置（`--max-turns`、
  `--max-tool-calls`、`--max-cost-usd`、`--max-wall-time-secs`）显式约束
  单次运行，预算耗尽时先结算当前工具、创建检查点，再以退出码 4 结束，并在
  JSONL 流中携带类型化终端对象。
- 运行没有固定轮次上限的持久目标（Goal 累计 token 预算耗尽时会禁用自动续跑），
  并通过子智能体和 JavaScript 工作流处理长任务。Conversation 最多保留 4 行
  子代理实时摘要，每个 child 最多持久化 8 条活动历史；`/agents` 可进入实时对话和
  transcript，并按 child 的可恢复状态提供 stop、resume、retry 和 follow-up。
- 直接、嵌套、Workflow、托管、续跑和恢复的子代理共用每棵根任务树的持久执行
  作用域。默认 32 个执行 lease 是容量上限而非派单目标；超出的已接受任务排队且
  不提前创建 worker，等待子任务的父代理会释放名额并在恢复前重新进入公平队列。
- 在工作区受信任后加载项目指令、Skills、Plugins、自定义工具、MCP 工具和资源。
- 为编辑器、测试框架和 CI 提供稳定的 JSONL、app-server 与 Agent Client
  Protocol（ACP）协议。

配置优先级依次为环境变量、命令行参数、配置文件和默认值。运行 `orca --help`
或 `orca exec --help` 查看完整命令。用户配置位于 `~/.orca/config.toml`；
受信任的项目还可以提供 `.orca/config.toml`、`AGENTS.md`、规则、Skills 和工作流。

未配置模型或模型为 `auto` 时，Orca 使用 `deepseek-flash`（DeepSeek-V4.1-Flash）。
可以通过 `/model`、`--model`，或在 `config.toml` 中设置 `model = "deepseek-v4-pro"` 改用 Pro；
0.5.0 之前的版本会把 `auto` 路由到 Pro。两个模型均采用 100 万 token 上下文，并允许最多 384K 输出 token。
旧名称 `deepseek-v4-flash` 和 `deepseek-v4-flash-vision-exp` 仍可作为兼容输入，
Orca 会将其归一化为 `deepseek-flash`，并统一采用 Flash 计费和图片能力。
Orca 会显式开启 DeepSeek 思考模式：可以在 `config.toml` 中将 `reasoning_effort` 设为 `low`、`high` 或 `max`（默认），
也可以使用 `ORCA_REASONING_EFFORT`。

ACP 客户端和 TUI 都可以输入 JPEG、PNG、GIF 和 WebP 图片：Flash 直接读取图片，
Pro 会先用 Flash 做面向任务的图片分析，再交给 Pro 继续。在 TUI 中用 `Ctrl+V` 粘贴剪贴板里的图片，
也可以拖入或粘贴图片路径，或通过 `@` 选择图片文件。Orca 继续使用 Chat Completions，
并按 DeepSeek 要求在工具调用轮次完整回传服务端返回的 `reasoning_content`。

更多文档：

- [文档站](https://orcaagent.dev/docs/)与[终端界面指南](https://orcaagent.dev/docs/#terminal-ui)
- [持久 Goal 模式](docs/goal-mode.md)
- [Memory](docs/memory.md)
- [Harness 与 app-server 协议](docs/harness-contract.md)
- [动态工作流设计](docs/claude-code-workflow-parity.md)
- [生产路线图](docs/production-roadmap.md)

## 可靠性

- TUI、Headless、ACP 和 JSONL 会话共用同一个 Runtime Host，统一负责 turn
  生命周期、取消、持久化与终态。
- Goal 和会话存储在异步 Actor 循环之外执行；即使磁盘变慢或 SQLite 忙碌，
  取消、状态查询等无关控制也不会被一起卡住。
- 取消前台 turn 时，会同时停止它拥有的子智能体任务树，但不会误伤无关任务。
- 使用 Esc 取消时会提交唯一的 child 终态并忽略该 attempt 的迟到活动，父会话恢复
  输入后，已停止的子代理不会继续向终端刷屏。
- 后台分离运行的 worker 不会占住终端，任务不存在或租约丢失后会自行结束。
- 切换会话时先启动新 Runtime，再关闭当前 Runtime。重命名、分叉、归档与删除
  经过 revision 校验和持久化提交，旧会话附件排队中的事件不会污染新会话。
- Runtime Surface 与平台边界契约会在 CI 中验证，通过后才构建 macOS、Linux
  和 Windows 发布产物。

## 社区

- QQ 群：`472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## 参与贡献

贡献前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。对于较大或涉及兼容性的改动，
请先提交 Issue。

- [报告问题](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [提出功能建议](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [获取帮助](SUPPORT.md)
- [报告安全漏洞](SECURITY.md)

## 许可证

[MIT](LICENSE)
