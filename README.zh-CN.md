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

orca                                      # 打开 TUI
orca exec "修复失败的测试"                 # 无界面运行
orca exec --verifier "cargo test" "修复它" # 完成前执行验证
orca exec resume SESSION_ID "继续"        # 恢复无界面会话
orca exec resume --last "继续"            # 恢复最近的会话
orca exec resume SID --resume-at MID "继续"  # 恢复到消息边界为止
orca --mode=acp                           # 连接 ACP 客户端
orca --resume [SESSION_ID]                # 恢复保存的会话
orca --fork SESSION_ID                    # 分叉保存的会话
```

Windows PowerShell 使用 `$env:DEEPSEEK_API_KEY = "sk-..."` 设置密钥；
后续 `orca` 命令相同。

在 TUI 中，`@` 可以搜索文件、Skills、Plugins 和 MCP Resources。会话指令包括
`/new`、`/resume`、`/fork [名称]`、`/rename [名称]`、`/status` 和
`/copy [N]`。`/resume` 选择器还可以分叉、重命名、归档、删除会话和复制
Session ID。`/history` 已移除；`/clear` 仅作为 `/new` 的隐藏兼容别名保留。
`Ctrl+L` 只清除屏幕内容和终端回滚区，不会清除当前会话上下文。退出 TUI 时，
Orca 会输出准确的 `orca --resume <SESSION_ID>` 恢复命令。

使用 `/plan` 进行只读规划，使用 `/goal` 管理持久目标，使用 `/workflows`
查看后台任务，使用 `/trust` 管理当前目录的沙箱权限。

## 核心能力

- 直接适配 DeepSeek 的推理和工具调用语义，支持 SSE 流式输出、前缀缓存友好提示词、
  自动上下文管理和请求重试。
- 读取、搜索、编辑和写入代码，运行 Shell 命令，并使用指定命令验证结果。
- 通过 `suggest`、沙箱内 `auto-edit`、完全访问 `full-auto` 和只读 `plan`
  模式控制风险，同时提供目录信任机制。
- 在本地保存对话历史，支持恢复、分叉、搜索、重命名、归档和压缩。
- 默认没有隐式轮次上限；可通过 `[budget]` 配置（`--max-turns`、
  `--max-tool-calls`、`--max-cost-usd`、`--max-wall-time-secs`）显式约束
  单次运行，预算耗尽时先结算当前工具、创建检查点，再以退出码 4 结束，并在
  JSONL 流中携带类型化终端对象。
- 运行没有固定轮次上限的持久目标（Goal 累计 token 预算耗尽时会禁用自动续跑），
  并通过子智能体和 JavaScript 工作流处理长任务。
- 在工作区受信任后加载项目指令、Skills、Plugins、自定义工具、MCP 工具和资源。
- 为编辑器、测试框架和 CI 提供稳定的 JSONL、app-server 与 Agent Client
  Protocol（ACP）协议。

配置优先级依次为环境变量、命令行参数、配置文件和默认值。运行 `orca --help`
或 `orca exec --help` 查看完整命令。用户配置位于 `~/.orca/config.toml`；
受信任的项目还可以提供 `.orca/config.toml`、`AGENTS.md`、规则、Skills 和工作流。

Orca 会显式开启 DeepSeek V4 思考模式。可以在 `config.toml` 中将
`reasoning_effort` 设为 `low`、`high` 或 `max`（默认），也可以使用
`ORCA_REASONING_EFFORT`。`deepseek-v4-flash` 和 `deepseek-v4-pro` 均采用
100 万 token 上下文，并允许最多 384K 输出 token。Orca 继续使用 Chat
Completions，并按 DeepSeek 要求在工具调用轮次完整回传服务端返回的
`reasoning_content`。

更多文档：

- [持久 Goal 模式](docs/goal-mode.md)
- [Harness 与 app-server 协议](docs/harness-contract.md)
- [动态工作流设计](docs/claude-code-workflow-parity.md)
- [生产路线图](docs/production-roadmap.md)

## 可靠性

- TUI、Headless、ACP 和 JSONL 会话共用同一个 Runtime Host，统一负责 turn
  生命周期、取消、持久化与终态。
- Goal 和会话存储在异步 Actor 循环之外执行；即使磁盘变慢或 SQLite 忙碌，
  取消、状态查询等无关控制也不会被一起卡住。
- 取消前台 turn 时，会同时停止它拥有的子智能体任务树，但不会误伤无关任务。
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
