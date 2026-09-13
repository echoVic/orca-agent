# Subagent 交互机制

## 概述

Subagent 是 Orca 的第7个工具，用于运行同步的子代理循环来完成委托任务。子代理共享父代理的工作空间、provider配置和审批策略。

## 核心特性

### 1. 同步执行模型
- 父代理调用 `subagent` 工具时会阻塞，等待子代理完成
- 子代理运行完整的agent loop（prompt → model → tool_call → execute）
- 子代理完成后返回简洁的结果给父代理

### 2. 深度限制
```rust
const MAX_SUBAGENT_DEPTH: u32 = 1;
```
- 只允许一层子代理（父 → 子）
- 嵌套调用（父 → 子 → 孙）会被拒绝，返回错误：`"nested subagents are disabled in this MVP"`

### 3. 上下文隔离
子代理拥有独立的系统提示：
```rust
fn build_agent_system_prompt(cwd: &Path, subagent_depth: u32) -> String {
    let mut prompt = build_system_prompt(cwd);
    if subagent_depth > 0 {
        prompt.push_str(
            "\n\n## Subagent Role\nYou are running as a synchronous subagent. \
            Complete only the delegated task and return a concise report for the parent agent. \
            Do not assume the user can see your intermediate tool output.",
        );
    }
    prompt
}
```

### 4. 事件流设计

子代理的生命周期通过4个关键事件追踪：

#### a. `tool.call.requested`
```json
{
  "type": "tool.call.requested",
  "payload": {
    "id": "mock-tool-1",
    "name": "subagent",
    "action": "read",
    "target": "inspect repo"
  }
}
```

#### b. `subagent.started`
```json
{
  "type": "subagent.started",
  "payload": {
    "id": "mock-tool-1",
    "description": "inspect repo"
  }
}
```

#### c. `subagent.completed`
```json
{
  "type": "subagent.completed",
  "payload": {
    "id": "mock-tool-1",
    "description": "inspect repo",
    "status": "success",
    "output": "Mock runtime completed the headless harness contract.",
    "error": null
  }
}
```

#### d. `tool.call.completed`
```json
{
  "type": "tool.call.completed",
  "payload": {
    "id": "mock-tool-1",
    "name": "subagent",
    "status": "completed",
    "output": "Subagent status: success\n\nMock runtime completed...",
    "error": null,
    "exit_code": 0,
    "truncated": false
  }
}
```

## 实现细节

### 工具参数解析

Subagent 工具接受两个可选参数：
```rust
fn subagent_field(tool_request: &tools::ToolRequest, field: &str) -> Option<String> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value[field].as_str().map(String::from)
}
```

- `description`: 任务描述（显示用）
- `prompt`: 实际提示词（传递给子代理）
- 如果只提供 `target`，则同时用作 description 和 prompt

### 执行流程

```rust
fn execute_subagent_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
) -> io::Result<tools::ToolResult> {
    // 1. 提取参数
    let description = subagent_field(tool_request, "description")
        .or_else(|| tool_request.target.clone())
        .unwrap_or_else(|| "subagent".to_string());
    let prompt = subagent_field(tool_request, "prompt")
        .unwrap_or_else(|| description.clone());

    // 2. 发出启动事件
    sink.emit(&events.subagent_started(&tool_request.id, &description))?;

    // 3. 深度检查
    if subagent_depth >= MAX_SUBAGENT_DEPTH {
        let error = "nested subagents are disabled in this MVP";
        sink.emit(&events.subagent_completed(
            &tool_request.id,
            &description,
            RunStatus::Failed,
            None,
            Some(error),
        ))?;
        return Ok(tools::ToolResult::failed(tool_request, error, None));
    }

    // 4. 运行子代理循环
    let child = run_agent_loop(
        config,
        cwd,
        events,
        sink,
        &prompt,
        subagent_depth + 1,
        false, // emit_deltas = false: 子代理不输出中间步骤
    )?;

    // 5. 处理结果
    match child.status {
        RunStatus::Success => {
            let output = child.final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            sink.emit(&events.subagent_completed(
                &tool_request.id,
                &description,
                child.status,
                Some(&output),
                None,
            ))?;
            Ok(tools::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ))
        }
        status => {
            let error = child.error
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            sink.emit(&events.subagent_completed(
                &tool_request.id,
                &description,
                status,
                child.final_message.as_deref(),
                Some(&error),
            ))?;
            Ok(tools::ToolResult::failed(
                tool_request,
                format!("Subagent status: {status:?}\n\n{error}"),
                None,
            ))
        }
    }
}
```

## TUI 集成

### ChatMessage 枚举
```rust
pub enum ChatMessage {
    Subagent {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
    },
    // ... 其他消息类型
}
```

### 事件处理

在 TUI 中，subagent 的事件处理与普通工具不同：

```rust
// 普通工具会在 ToolRequested/ToolCompleted 事件中处理
TuiEvent::ToolRequested { name, target } => {
    if name == "subagent" {
        return; // 忽略，使用专用的 SubagentStarted 事件
    }
    // ... 处理其他工具
}

// 专用的 Subagent 事件
TuiEvent::SubagentStarted { id, description } => {
    self.messages.push(ChatMessage::Subagent {
        id,
        description,
        status: "running".to_string(),
        output: None,
        error: None,
    });
}

TuiEvent::SubagentCompleted { id, description, status, output, error } => {
    // 更新现有的 Subagent 消息
    let updated = self.messages.iter_mut().rev().find_map(|msg| {
        if let ChatMessage::Subagent {
            id: existing_id,
            status: existing_status,
            output: existing_output,
            error: existing_error,
            ..
        } = msg {
            if existing_id == &id {
                *existing_status = status.clone();
                *existing_output = output.clone();
                *existing_error = error.clone();
                return Some(());
            }
        }
        None
    });
    
    // 如果没找到对应的 started 事件，创建新消息
    if updated.is_none() {
        self.messages.push(ChatMessage::Subagent {
            id, description, status, output, error,
        });
    }
}
```

## 测试覆盖

### 1. 成功场景
```rust
#[test]
fn subagent_tool_runs_child_agent_and_emits_events() {
    // 验证：
    // - tool.call.requested 包含正确的 name/action/target
    // - subagent.started 发出
    // - subagent.completed 状态为 success
    // - tool.call.completed 包含格式化的输出
    // - session.completed 状态为 success
}
```

### 2. 嵌套拒绝
```rust
#[test]
fn nested_subagent_calls_are_rejected() {
    // 验证：
    // - 子代理尝试调用 subagent 时被拒绝
    // - subagent.completed 状态为 failed
    // - error 包含 "nested subagents are disabled"
    // - 父代理 session.completed 状态为 failed
}
```

### 3. 失败传播
```rust
#[test]
fn subagent_child_failure_fails_parent_run() {
    // 验证：
    // - 子代理失败时，父代理也失败
    // - subagent.completed 包含错误信息
    // - tool.call.completed 状态为 failed
    // - session.completed 状态为 failed
}
```

## 使用场景

### 1. 任务分解
父代理可以将复杂任务委托给子代理：
```bash
orca exec "analyze the codebase and refactor the auth module"
# 父代理可能会：
# 1. 调用 subagent "analyze codebase structure"
# 2. 根据分析结果调用 subagent "refactor auth module"
```

### 2. 专注上下文
子代理拥有独立的对话历史，可以专注处理单一子任务而不受父代理上下文干扰。

### 3. 错误隔离
子代理的失败会被捕获并返回给父代理，父代理可以决定如何处理（重试、调整策略、报告错误）。

## 限制与权衡

### 优点
- ✅ 清晰的任务边界
- ✅ 简单的同步模型
- ✅ 独立的上下文管理
- ✅ 完整的事件追踪

### 限制
- ❌ 只支持一层嵌套（深度=1）
- ✅ 子代理支持批量并行执行（默认 `max_parallel = 6`），但仍不是异步非阻塞模式
- ❌ 子代理的中间步骤不向用户显示（emit_deltas=false）
- ❌ 共享相同的 provider 配置（无法为子任务选择不同模型）

## 对比其他工具

| 特性 | 普通工具 (bash/edit/grep) | Subagent |
|------|--------------------------|----------|
| 执行模式 | 直接执行操作 | 运行完整的 agent loop |
| 推理能力 | 无 | 有（使用 LLM） |
| 工具调用 | 不支持 | 支持（可调用其他工具） |
| 多轮对话 | 不支持 | 支持（最多128轮） |
| 上下文 | 无状态 | 有独立对话历史 |
| 事件输出 | 2个事件（requested/completed） | 4个事件（+started/completed） |
| TUI 渲染 | 作为 ToolCall 消息 | 作为专用 Subagent 消息 |

## 未来改进方向

1. **并行子代理**: 支持多个子代理并发执行
2. **更深嵌套**: 放宽深度限制（需要更好的循环检测）
3. **流式输出**: 允许子代理的中间步骤实时显示
4. **配置继承**: 支持子代理使用不同的模型或审批策略
5. **结果缓存**: 相同任务的子代理可以复用结果
6. **超时控制**: 为子代理设置执行时间限制
7. **资源追踪**: 统计子代理的 token 使用和工具调用次数

---

# 委派策略与角色目录（2026-09 更新）

本节描述当前实现，取代上文「只支持一层嵌套」「同步执行模型」等历史描述。

## 委派策略 `[subagents] delegation`

委派策略与审批模式（`mode` / `approval`）是两个独立的轴：审批模式决定工具能不能执行，委派策略只决定主 agent 是否应当主动拆分任务。full-auto 不等于要求尽可能多地派单。

```toml
[subagents]
max_depth = 2
max_parallel = 6
delegation = "adaptive"   # off | explicit | adaptive
```

| 值 | 行为 |
|---|---|
| `off` | 不开放新建子任务（`subagent` 工具不再下发，调用被硬拒绝）。已有子任务仍可 `subagent_status` 查询、`task_stop` 停止、按 id `resume_from` 续跑。 |
| `explicit`（默认） | 只有用户明确要求委派时才主动启动；提示词里保留关键路径规则作为兜底。 |
| `adaptive` | 允许在用户已授权的任务范围内自主拆分独立分支，同时保留本地关键路径。 |

默认值为 `explicit`，所以在完成真实模型评测前，现有会话的行为不会改变。策略随 `DelegationSnapshot` 传给子 agent，并受 `max_depth` 约束：到达深度上限的子任务生效策略为 `off`。

## 角色目录

内置角色只有一处定义（`orca-core/src/subagent_types.rs` 的 `builtin_agents()`），同一份描述同时生成：模型可见目录、`subagent_type` 枚举、运行期工具上限、子 agent 提示词。

| 角色 | 用途 | 工具上限 | 交付要求 |
|---|---|---|---|
| `explorer`（新增，别名 `explore`/`scout`） | 多文件只读探索、符号定位、调用链与依赖追踪 | `read_file` `glob` `grep` `git_status` | 直接结论、`path:line` 证据、未决问题 |
| `general`（默认） | 边界明确的多步骤实现、需要执行的专项调查 | 读写 + `bash` + `web_search` | 结果、改动文件、实际跑过的验证 |
| `code_reviewer`（`reviewer`） | 独立审查改动的缺陷与回归风险 | 只读 | 按严重程度、带 `path:line` 的缺陷 |
| `test_writer`（`tester`） | 指定范围内的测试实现与执行 | 读写 + `bash` | 测试文件、测试命令与结果、未覆盖项 |
| `debugger`（`debug`） | 复现、定位根因、受控验证 | 读写 + `bash` | 复现步骤、根因、是否真正复现 |
| `documenter`（`docs`） | 任务明确要求的文档 | 读写（不含 `bash`） | 文档改动、每条结论的来源 |

自定义 agent 仍然用 `.orca/agents/*.md` 定义，并且不能被 `explorer`/`explore`/`scout` 等保留名覆盖。

## 只读角色是硬约束

`explorer` 和 `code_reviewer` 的工具上限不只是提示词：角色上限会写入 `ChildAgentRequest.allowed_tools`，因此

- 子 agent 的 provider schema 里根本看不到 `bash` / `edit` / `write_file`；
- 即使模型硬报一个越界调用，也会在 `reject_disallowed_child_tool` 被拒绝并使该轮失败。

越界不会因为 `resume_from`、自定义定义继承或 plan mode 而扩大；恢复时沿用 checkpoint 里冻结的角色。

## 并发上限是会话级的

`max_parallel` 现在是所有**分离式**子 agent 共享的运行中上限：direct async、UI 触发的 continue、以及后续任何独立进程子任务都计入同一个计数。计数来自持久 `TaskRegistry`，因此子 agent 跑在独立进程里也准确。

同步批次子任务不计入这个计数：它已经被同一轮的批次窗口（宽度也是 `max_parallel`）限制住，而且父 agent 正阻塞等待它，把它也计入会让嵌套批次和自己的祖先互相饿死。两类瓶颈合起来保证不会有不受限的子任务数量。

容量满时启动被拒绝并返回说明（不会留下无主进程），提示主 agent 不要立刻重试，而是继续本地工作或用 `subagent_status` / `task_stop` 收敛。

## 分离模式的结果交付

父 agent 没有阻塞等待的子任务（`mode: "async"`）完成后，结果会在下一个回合边界以 `<task-notification>` 注入父对话，而不是只停留在 TUI 里。投递标记随任务记录持久化，因此：

- 同一结果不会重复注入；
- 运行中的子任务不会被提前投递；
- 失败也会带上失败原因；
- 超长结果会截断并提示用 `subagent_status` 的 `output_next_offset` 取回余下内容；
- 通知文本明确说明这是子 agent 的报告而非用户指令。

同步子任务不需要通知——它的结果已经在 `subagent` 工具返回值里。

## 预算边界（未变）

cost budget 激活时 async 仍然被拒绝：跨进程的持久预算预留与结算闭环尚未实现，这条保护在闭环完成前保留。
