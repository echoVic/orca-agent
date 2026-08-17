# 工具系统对比分析

**初版日期**: 2026-06-22
**Goal 控制面复核**: 2026-07-18
**Orca 当前版本**: v0.3.8
**对比对象**:

- Claude Code (`@anthropic-ai/claude-code`)
- Codex CLI
- Grok Build
- Orca (`blade-deepseek`)

---

## 执行摘要

v0.3.8 基线下，Orca 的工具系统已经从早期的硬编码分发，升级为"规格驱动"的注册表模型。每个工具都有统一的 `ToolSpec`，包含名称、别名、JSON Schema、能力集合、展示方式、可见性和并发安全信息。运行时审批、TUI 展示、provider tool schema、MCP 工具与 resources/resource templates、external tools、skills 工具、持久 goal 工具和结构化用户输入都围绕同一套规格工作。实时 app-server 与持久化 thread projection 现在共享 MCP tool 解析、JSON 参数解析、MCP started/completed item builder、dynamic started/completed item builder、MCP result shaping、camelCase tool error helper、exit-code 错误归一化和 completed 状态检查，降低 `mcpToolCall` / `dynamicToolCall` item schema drift；实时 app-server 的 `agent_message`、`plan`、`reasoning`、`commandExecution`、`fileChange` 和 `workflow` lifecycle item 也改由共享 projection builder 构造，持久化 history 的 `commandExecution` 与 `fileChange` 投影也复用同一边界，TUI runtime event projection、runtime approval 和 user-input handlers，以及 tool approval gate 都已从 `bridge` 抽到专门模块，继续把 Codex/package 3 风格 item schema、runtime-to-surface 映射、approval request 构造、preview 生成和交互等待逻辑收束到明确边界；tag-driven Release workflow 会串行运行 Rust test harness，降低 server-heavy contract 在 Linux release runner 上的进程干扰；历史投影还保留失败 commandExecution 不写入聚合输出的回归守卫。stdio MCP 测试 fixture 现在通过 `/bin/sh` 启动临时脚本，避免 Linux release runner 偶发 `Text file busy` 阻断发布。后台 turn 测试在轮询活跃 shared writer 时也会忽略尾部半行 JSONL，避免 CI-only 竞态污染 release gate。实时 item error 仍会在工具完成事件提供 `exit_code` 时携带 `exitCode`。MCP clients 现在会缓存 initialize 结果里的 resources capability；all-server resources/templates 发现会跳过未声明资源能力的 tools-only server，同时显式 server 查询仍然直接调用目标 server 并返回真实错误。MCP resources/templates 的 all-server 发现结果也会同时携带 registry 级启动错误和按 server 聚合的 list 失败，避免失败 MCP server 在模型上下文里静默消失。工具参数执行前校验现在覆盖常见 object keyword、enum、array item 以及 `oneOf` / `anyOf` 组合分支，减少模型看到的 schema 与 runtime 实际拒绝行为之间的偏差；生命周期 hooks 的结构化 JSON stdout 也会校验声明的 `action` 和必需字符串字段，避免拼写错误被静默当作上下文注入。

这次变化的核心目标是接近 Codex CLI 的工具系统思路：工具不是散落在 prompt、审批和执行器里的字符串列表，而是一个可以解析、过滤、授权、渲染和扩展的统一能力表。v0.3.5 将结构化澄清统一为 TUI `ask_user_question` answer loop，让模型既能显式加载可复用流程，也能在交互式会话中提出结构化问题。

v0.2.46 补上了此前遗漏的另一半契约：工具 schema 可见不只代表
provider 收到了定义，也代表同一 turn 的 runtime 拥有可执行它的
session、extension store、取消和持久化能力。Goal 工具现在由 runtime
special dispatch 执行，不再通过普通工具 worker 或 thread-local callback
寻找 owner；控制面错误会结束当前 turn 并把 active Goal 原子标记为
`stalled`，普通参数错误仍允许模型在同一 turn 自纠。

---

## 当前 Orca 工具清单

| 工具 | 类型 | 当前状态 | 说明 |
|------|------|----------|------|
| `read_file` | read | 已实现 | 读取 UTF-8 文件内容，输出按 8KB 截断 |
| `glob` | read | 已实现 | 首选文件发现工具，支持 glob pattern 和 `mode: "fuzzy"` 路径查询，返回 workspace-relative 路径 |
| `list_files` | read | 兼容 alias | 保留给旧 prompt 和历史会话，模型 prompt 不再优先推荐 |
| `grep` | read | 已实现 | 优先使用 ripgrep，缺失时回退到进程内正则搜索，空结果返回 `(no matches)` |
| `git_status` | read | 已实现 | `git status --short` |
| `web_search` | network | 已实现 | 网络搜索，按 network 能力走审批策略 |
| `bash` | shell | 已实现 | 通过 `sh -c` 执行命令，shell 能力需要对应审批 |
| `edit` | write | 已实现 | 精确文本替换 |
| `write_file` | write | 已实现 | 创建或覆盖文件 |
| `subagent` | agent | 已实现 | 同步子智能体，支持类型/模型参数和深度限制 |
| `Workflow` | agent | 已实现 | 启动 JavaScript 动态 workflow |
| `update_plan` | read/state | 已实现 | 更新可见计划状态 |
| `get_goal` | read/state | 已实现 | 仅 goal 上下文中可用，用于读取持久目标状态 |
| `create_goal` | read/state | 已实现 | 仅 goal 上下文中可用，在没有未完成 goal 时创建新目标 |
| `update_goal` | read/state | 已实现 | 仅 goal 上下文中可用，只允许模型标记 `complete` 或 `blocked` |
| `ask_user_question` | read/state | 已实现 | 1-4 个结构化问题，每题 2-4 个带说明选项，支持 preview、multiSelect、自定义答案和取消 |
| `list_skills` | read | 已实现 | 列出用户和项目 Markdown skills |
| `read_skill` | read | 已实现 | 读取指定 skill 的 Markdown 指令内容 |
| MCP tools | dynamic | 已实现基础路由 | 配置的 MCP server 工具以 namespaced tool 暴露 |
| `list_mcp_resources` / `list_mcp_resource_templates` / `read_mcp_resource` | read | 已实现基础路由 | 通过 MCP `resources/list`、`resources/templates/list` 和 `resources/read` 读取 server 暴露的只读资源；all-server 列表会聚合启动错误和部分 server 失败 |
| external tools | dynamic | 已实现 | `~/.orca/tools/*.toml` 或 `$ORCA_HOME/tools/*.toml` 描述符注册命令工具 |

---

## 与 Claude Code / Codex CLI 的设计对比

| 维度 | Claude Code | Codex CLI | Orca v0.3.5 |
|------|-------------|-----------|--------------|
| 工具定义 | 类型化 schema | 规格/能力驱动 | `ToolSpec` 规格驱动，执行前校验支持 `oneOf` / `anyOf` |
| 文件发现 | `Glob` | 文件搜索工具优先 | `glob` 优先，支持 glob/fuzzy 两种发现模式；`list_files` 兼容 |
| Shell | `Bash`，支持后台任务 | `exec_command`/shell session | `bash` 同步执行，后台任务待增强 |
| 文件写入 | `FileWrite`/`FileEdit` | patch/edit 类工具 | `write_file`/`edit` |
| 子代理 | 同步/异步能力 | 多代理/任务能力 | 同步 `subagent`，深度受配置限制 |
| 工作流 | workflow/task 能力 | 自动化/任务工具 | `Workflow` JS 动态 workflow |
| MCP | 支持 | 支持 | MCP 客户端工具路由与 resources list/templates/read 已接入 |
| 自定义工具 | 插件/扩展 | MCP/插件 | TOML external tools + MCP |
| Skills | skills / slash 工作流 | Codex skills / plugins | Markdown `SKILL.md` discovery + `$skill` 显式注入 |
| 用户输入 | `AskUserQuestion` 多问题问卷 | `request_user_input` 多问题结构 | TUI `ask_user_question` 逐题选择框，支持多选与自定义回答 |
| 审批 | 工具能力/策略 | 工具能力/策略 | 从 `ToolSpec.capabilities` 推导 |
| 上下文工具 | 按模式暴露 | 按模式暴露 | `get_goal` / `create_goal` / `update_goal` 等按 runtime context 过滤 |

---

## 执行上下文与 owner 对比

| 实现 | schema/listing owner | call/execution owner | 对 Orca 的启示 |
|------|----------------------|----------------------|-----------------|
| Codex | `build_tool_specs_and_registry` 从同一组 planned runtimes 同时生成 model-visible specs 和 `ToolRegistry` | `ToolCallRuntime` 显式持有 `Session`、`StepContext`、cancellation token 和 tracker | 可见 schema 与可调用 registry 不能由两条会漂移的路径产生 |
| Claude Code | 当前 `ToolUseContext.options.tools` 决定可用工具集合 | 每个 `Tool.call(args, context, ...)` 显式接收 `ToolUseContext`，其中包含 AbortController、app/session state 和交互能力 | 调用上下文必须作为参数传入，不能从执行线程隐式恢复 |
| Grok Build | `ListToolsContext` 决定 `should_list` 和动态 description | `ToolCallContext` 通过 typed extensions 携带 `SessionContext`、`Cancellation`、cwd 等，并要求 stream 产生一个 terminal | 展示上下文和执行上下文可以分型，但 capability 必须显式、类型化并有终态契约 |
| Orca v0.2.46 | `ThreadTurnToolMode::Goal` 同时驱动 provider schema 和 runtime `goal_mode` | `RuntimeToolRouter` 在 normal worker 前执行 Goal special dispatch，使用 persistent session id 和 live extension stores | Goal 控制面不再依赖 OS thread 身份，缺少 owner 时 fail closed |

`thread_local!` 适合线程绑定缓存、渲染状态或 telemetry：每个 OS
thread 有一份独立值。但 async task 可以换线程，worker 也会创建新线程，
所以 TLS 既不是 session-local，也不是 actor-local，更不会自动传播到
`orca-normal-tool`。持久 Goal 这类跨 turn 控制面 capability 必须由 runtime
owner 显式携带。

---

## v0.1.25 基线的关键改进

### 1. Canonical Tool Registry

工具注册集中在 `crates/orca-tools/src/registry.rs`。注册表负责：

- 注册 built-in tools。
- 解析 canonical name 与 alias。
- 暴露 model-visible tool schema。
- 路由 built-in、MCP、external tool 调用。
- 为审批和 TUI 提供统一的工具规格。

`list_files` 现在是 `glob` 的兼容 alias：模型优先看到 `glob`，旧会话仍可请求 `list_files`。

### 2. Capability-based Approval

审批不再依赖手写工具名列表，而是通过工具能力推导：

| Capability | ActionKind |
|------------|------------|
| `FsRead`, `FsList`, `FsSearch`, `GitInspect`, `PlanUpdate`, `GoalUpdate` | read |
| `FsWrite` | write |
| `ShellExecute` | shell |
| `NetworkSearch` | network |
| `AgentDelegate`, `WorkflowRun` | agent |

这让内置工具、MCP 工具和 external tools 能共用同一审批语义。

### 3. Context-scoped Tools

不是所有工具在所有上下文都应该暴露。例如：

- `get_goal`、`create_goal` 和 `update_goal` 只在有持久 session 的 Goal turn 中给模型使用。
- `update_goal` 只能标记 `complete` 或 `blocked`；pause/resume/edit/clear/budget/usage 由用户或系统路径控制。
- 超过 subagent 深度限制时，`subagent` 不应先进入审批，而应由执行路径返回明确失败。
- 工作流、MCP、external tools 需要根据当前 runtime 配置决定是否可用。
- MCP resources 通过 `list_mcp_resources` / `list_mcp_resource_templates` / `read_mcp_resource` 作为只读工具暴露，并沿用同一能力/并发安全元数据；all-server 发现会保留可用资源，同时把 registry 启动错误和部分 server list 错误放进 `errors`。

v0.2.46 进一步要求“可见即具有可执行 runtime capability”。Goal 工具会
在 runtime actor 路径上被 special dispatch 截获，永不进入 readonly batch
或 normal worker。无效 JSON/状态属于模型可恢复错误；缺少 persistent
session、live extension context 或 GoalStore I/O 失败属于控制面错误，记录
一次 tool result 后结束 turn。

### 4. Skills and Structured User Input

Orca 的 skills 和结构化问答共用 runtime-owned 交互边界：

- `list_skills` / `read_skill` 可发现和读取 `$ORCA_HOME/skills`、`~/.orca/skills` 和项目 `.orca/skills` 下的 Markdown skills。
- TUI `/skills` 会打开可搜索的 `$` picker；选择后的 `$skill_id` 作为原子 composer token 处理，退格或删除不会留下半个 skill 引用。
- 当 prompt 明确提到 `$skill_id` 时，Orca 会把对应 `SKILL.md` 注入当轮模型上下文。
- `ask_user_question` 是唯一注册且模型可见的澄清工具：单次接受 1-4 个问题，每题需要短 header、问题正文和 2-4 个 `label`/`description` 选项，可附 `preview` 并用 `multiSelect` 标记多选。runtime 逐题生成唯一 interaction id，经现有 broker 等待 composer 答案，最后返回 `answers` JSON；任一题取消会取消整次工具调用。
- headless 路径确定性失败而不阻塞。TUI 允许输入选项标签或自定义文本；多选答案以逗号分隔，preview 会随选项正文展示。

### 5. Empty-result Semantics

读类工具的“没有结果”不再等同于失败：

- `glob` 没有匹配时返回 `(no matches)`。
- `list_files` 缺失或空目录返回 `(empty)`。
- `grep` 没有匹配时返回 `(no matches)`。

这避免子代理或主循环把正常探索过程误标为失败。

---

## 仍然存在的差距

| 能力 | 当前差距 | 建议优先级 |
|------|----------|------------|
| Bash 超时/PTY/session | 还没有 Codex CLI 风格的 `exec_command` + `write_stdin` 会话模型 | P1 |
| 图片/PDF/Notebook 读取 | `read_file` 仍以 UTF-8 文本为主 | P2 |
| apply_patch freeform | 仍以 JSON `edit` / `write_file` 为主 | P2 |

## 可靠性保证

- 历史恢复会按顺序重放压缩记录；相同消息计数的多次压缩只消费各自对应的一份摘要，避免恢复后上下文重新膨胀。
- 异步 subagent 结果持久保存，`subagent_status` 通过 `offset` / `limit` 分页返回完整结果，并显式给出下一页游标。
- 后台主会话、shell、subagent 和 workflow 的状态都进入任务列表；后台任务完成、失败、取消或等待审批时，TUI 会主动提示，完成结果可从任务详情查看。
- stdio MCP 在超时、连接关闭或管道失败后会重建 transport、重新初始化并同步工具；后续请求不需要重启 Orca。
- JSONL 会话记录逐条使用跨进程文件锁写入；事件序号区间也在锁内比较当前持久游标，旧游标写入会显式失败，不会生成重叠序号流。

---

## 后续路线建议

### 短期

1. 引入 Codex CLI 风格的 shell session 工具：`exec_command`、`write_stdin`、可选 PTY、超时和后台 session id。
2. 保留 `bash` 作为兼容 alias，逐步让 prompt 推荐新 shell 工具。
3. 为 `ask_user_question` 增加可配置自动超时和专用多选控件；当前答案通过 composer 逐题提交。

### 中期

1. 支持图片/PDF/Notebook 读取。
2. 将 `apply_patch` 做成 grammar-backed freeform tool。

### 长期

1. 延展 deferred tool discovery，让 MCP、插件、workflow 只在需要时展开。
2. 建立统一的工具结果类型和错误码。
3. 让工具系统、TUI、JSONL harness 和文档都从同一份 tool registry 派生。

---

## 相关实现文件

- `crates/orca-core/src/tool_types.rs` — `ToolName`、`ToolSpec`、capabilities、renderer、exposure。
- `crates/orca-tools/src/registry.rs` — built-in/MCP/external tool registry。
- `crates/orca-tools/src/glob.rs` — 首选文件发现工具。
- `crates/orca-tools/src/list_files.rs` — 兼容目录列表工具。
- `crates/orca-runtime/src/tool_router.rs` — runtime-special/normal 工具路由与 turn disposition。
- `crates/orca-runtime/src/runtime_special.rs` — Goal、workflow、task 等 runtime 控制面执行。
- `crates/orca-runtime/src/runtime_host.rs` — hosted turn 准入、失败 Goal stall 和 context 清理。
- `crates/orca-tools/src/update_goal.rs` — Goal 参数解析与模型结果格式化，不持有 session owner。
- `crates/orca-tools/src/skills.rs` — Markdown skill discovery、读取和 prompt 注入格式化。
- `crates/orca-tui/src/types.rs` — TUI user input request 状态和事件。

---

## 结论

Orca 已完成从“硬编码工具枚举”到规格、执行上下文和 runtime owner
共同约束工具调用的迁移。v0.2.46 的 Goal 事故说明，只有 schema 统一仍然
不够：工具可见性、执行 capability、持久 owner、取消和 terminal policy
必须在同一个 turn 边界闭合。后续新增任何 context-scoped 工具，都应先
证明这条完整链路，而不是用 TLS 或全局回调跨 worker 补洞。
