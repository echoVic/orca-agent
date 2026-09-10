# Subagent 功能增强方案

**日期**: 2026-06-16  
**目标**: 参考 Claude Code 的 Agent 工具，增强 Orca 的 subagent 功能

> **状态更新（2026-06-26）**: 本文最初是实现方案，后续章节中的代码结构和 checklist 保留为历史设计记录。当前代码已经支持默认嵌套深度 2、批量并行 `max_parallel = 6`、`model` 覆盖、`mode: "async"`、`subagent_status`、headless/`exec` worker-backed 持久 async handles、TUI session-local async handles、`isolation: "worktree"`、专用 `subagent_type`、可选 `schema` 校验，以及 completed async usage/timestamp/status 查询。最新对标状态以 `docs/agent-workflow-benchmark.md` 和 contract tests 为准。

---

## 一、现状分析

### 1.1 当前实现

**特性**:
- ✅ 同步阻塞执行
- ✅ 默认深度限制为 2，可通过 `[subagents] max_depth` 配置；显式 `max_depth = 1` 仍可阻止嵌套
- ✅ 独立的系统提示
- ✅ 完整的事件追踪（4个事件）
- ✅ 错误隔离和传播
- ✅ TUI专用渲染
- ✅ 支持批量并行执行（默认 `max_parallel = 6`）
- ✅ 支持模型覆盖（`auto` / `deepseek-flash` / `deepseek-v4-pro`）
- ✅ 支持 async 模式：headless/`exec` 使用 worker-backed 持久 task handle，TUI 使用 session-local handle
- ✅ 支持 `subagent_status` 查询 current/persisted async 状态、结果、错误、生命周期时间戳和 usage
- ✅ 支持 `isolation: "worktree"`，干净 worktree 自动清理，脏 worktree 保留供审查
- ✅ 支持可选 `schema` 校验，覆盖 sync、batch、async worker 完成路径
- ⚠️ 子代理中间推理/工具步骤仍主要通过事件流和任务状态间接观察，完整可交互 step-level 控制仍是未来增强方向

### 1.2 Claude Code Agent 工具特性

根据 `sdk-tools.d.ts` 分析：

```typescript
interface AgentInput {
  description: string;              // 任务描述
  prompt: string;                   // 实际提示词
  subagent_type?: string;           // 子代理类型（专用代理）
  model?: "sonnet" | "opus" | "haiku";  // 模型选择
  run_in_background?: boolean;      // 异步执行
  name?: string;                    // 代理名称（可寻址）
  team_name?: string;               // 团队上下文
  mode?: string;                    // 权限模式
  isolation?: "worktree";           // Git worktree隔离
}

type AgentOutput =
  | {
      status: "completed";
      content: [...];
      totalToolUseCount: number;
      totalDurationMs: number;
      totalTokens: number;
      usage: {...};                 // 详细的token使用统计
    }
  | {
      status: "async_launched";
      agentId: string;
      description: string;
      outputFile: string;           // 输出文件路径
      canReadOutputFile?: boolean;  // 是否可以读取
    };
```

**关键差异**:
1. **异步模式**: 返回 agentId 和 outputFile，不阻塞父代理
2. **模型选择**: 子代理可以使用不同的模型
3. **专用代理**: 支持 subagent_type（如 code-reviewer, Explore 等）
4. **Worktree隔离**: 子代理在独立的 git worktree 中工作
5. **统计信息**: 详细的 token 使用、工具调用次数、执行时间

---

## 二、增强目标

### Phase 1: 核心功能增强（P0）

#### 2.1 异步执行模式

**设计**:
```rust
pub enum SubagentMode {
    Sync,                           // 当前模式：阻塞等待
    Async {
        output_file: PathBuf,       // 输出写入文件
        notify: bool,               // 完成时通知父代理
    },
}

pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
    pub mode: SubagentMode,
    pub model: Option<String>,      // 可选的模型覆盖
    pub depth: u32,
}
```

**异步执行流程**:
```
1. 父代理调用 subagent 工具，设置 run_in_background=true
2. 创建子代理线程/进程
3. 立即返回 agentId 和 output_file
4. 父代理继续执行
5. 子代理输出写入 output_file
6. 父代理可以用 read_file 查看进度
7. 子代理完成时发出通知事件
```

**事件流**:
```json
// 启动
{"type": "subagent.started", "id": "agent-1", "mode": "async"}

// 立即返回
{"type": "subagent.launched", "id": "agent-1", "output_file": "/tmp/agent-1.log"}

// 父代理继续工作...

// 完成通知（后台）
{"type": "subagent.completed", "id": "agent-1", "status": "success"}
```

#### 2.2 输出文件格式

**结构化输出**:
```json
{
  "agent_id": "agent-1",
  "status": "running" | "completed" | "failed",
  "started_at": "2026-06-16T01:23:45Z",
  "completed_at": null,
  "progress": {
    "current_turn": 3,
    "total_turns": 128,
    "tools_executed": 5
  },
  "output": "...",
  "error": null,
  "statistics": {
    "total_tool_use_count": 5,
    "total_duration_ms": 12345,
    "total_tokens": 2000
  }
}
```

**实时更新**:
- 子代理每完成一轮就更新文件
- 父代理可以实时读取进度
- 支持流式日志追加

#### 2.3 子代理状态查询

**新增工具**: `subagent_status`

```rust
pub struct SubagentStatusRequest {
    pub agent_id: String,
}

pub struct SubagentStatusResult {
    pub status: SubagentStatus,
    pub progress: Option<Progress>,
    pub output: Option<String>,
    pub error: Option<String>,
}

pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    NotFound,
}
```

**使用场景**:
```
父代理: 调用 subagent (async) "analyze codebase"
        -> 返回 agent-1

父代理: 继续其他工作...

父代理: 调用 subagent_status "agent-1"
        -> status: Running, progress: 40%

父代理: 等待或继续工作...

父代理: 调用 subagent_status "agent-1"
        -> status: Completed, output: "分析结果..."
```

---

### Phase 2: 高级特性（P1）

#### 2.4 模型选择

**设计**:
```rust
pub struct SubagentConfig {
    pub model: Option<String>,      // "deepseek-flash" | "deepseek-v4-pro"
    pub max_turns: Option<u32>,     // 覆盖默认的128轮
    pub temperature: Option<f32>,   // 温度参数
}
```

**使用场景**:
- 简单任务用 flash 模型（快速、便宜）
- 复杂推理用 pro 模型（深度思考）
- 并行多个子代理用不同模型

#### 2.5 专用子代理类型

**设计**:
```rust
pub enum SubagentType {
    General,                        // 通用代理
    CodeReviewer,                   // 代码审查
    TestWriter,                     // 测试编写
    Debugger,                       // 调试专家
    Documenter,                     // 文档编写
    Custom(String),                 // 自定义类型
}
```

**实现**:
- 每种类型有专门的系统提示
- 可用的工具集不同
- 审批策略不同

**系统提示示例**:
```rust
fn build_subagent_system_prompt(agent_type: SubagentType) -> String {
    match agent_type {
        SubagentType::CodeReviewer => "
            You are a code review specialist. Focus on:
            - Code quality and best practices
            - Potential bugs and edge cases
            - Performance implications
            - Security vulnerabilities
            Return a structured review report.
        ",
        SubagentType::TestWriter => "
            You are a test writing expert. Focus on:
            - Comprehensive test coverage
            - Edge cases and error handling
            - Clear test descriptions
            Write tests following the project's conventions.
        ",
        // ...
    }
}
```

#### 2.6 Worktree 隔离

**设计**:
```rust
pub struct WorktreeGuard {
    path: PathBuf,
    branch: String,
    auto_cleanup: bool,
}

impl WorktreeGuard {
    pub fn create(description: &str) -> Result<Self> {
        let temp_path = create_temp_worktree(description)?;
        Ok(Self {
            path: temp_path,
            branch: format!("subagent-{}", uuid()),
            auto_cleanup: true,
        })
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.auto_cleanup && !has_changes(&self.path) {
            remove_worktree(&self.path).ok();
        }
    }
}
```

**使用场景**:
```rust
// 并行子代理在独立环境中工作
let guard1 = WorktreeGuard::create("refactor-auth")?;
let agent1 = spawn_subagent_in_worktree(&guard1, "refactor auth module");

let guard2 = WorktreeGuard::create("add-tests")?;
let agent2 = spawn_subagent_in_worktree(&guard2, "add unit tests");

// 两个子代理并行工作，不会冲突
```

**工作流程**:
1. 创建临时 git worktree
2. 子代理在 worktree 中执行
3. 完成后检查是否有变更
4. 无变更自动删除
5. 有变更保留供审查

---

## 三、实现计划

### 3.1 代码结构

**新增文件**:
```
src/
  runtime/
    subagent.rs           # 子代理核心逻辑
    subagent_async.rs     # 异步执行
    subagent_types.rs     # 专用代理类型
    worktree.rs           # Git worktree管理
  tools/
    subagent_status.rs    # 状态查询工具
```

**修改文件**:
```
src/
  runtime/
    controller.rs         # 增强 execute_subagent_tool
  tools/
    mod.rs                # 添加 SubagentStatus 工具
  event/
    schema.rs             # 添加新事件类型
```

### 3.2 关键数据结构

```rust
// src/runtime/subagent.rs
pub struct SubagentRuntime {
    pub id: String,
    pub description: String,
    pub prompt: String,
    pub mode: SubagentMode,
    pub config: SubagentConfig,
    pub worktree: Option<WorktreeGuard>,
    pub output_file: Option<PathBuf>,
    pub status: Arc<Mutex<SubagentStatus>>,
}

impl SubagentRuntime {
    pub fn new(request: SubagentRequest) -> Self { ... }
    
    pub fn execute_sync(&mut self) -> Result<SubagentResult> { ... }
    
    pub fn execute_async(self) -> Result<SubagentHandle> { ... }
    
    pub fn query_status(&self) -> SubagentStatus { ... }
}

pub struct SubagentHandle {
    pub id: String,
    pub output_file: PathBuf,
    pub join_handle: Option<JoinHandle<SubagentResult>>,
}

impl SubagentHandle {
    pub fn is_completed(&self) -> bool { ... }
    
    pub fn read_output(&self) -> Result<String> { ... }
    
    pub fn wait(self) -> Result<SubagentResult> { ... }
}
```

### 3.3 事件扩展

```rust
// src/event/schema.rs
pub enum EventType {
    // 现有事件
    SubagentStarted,
    SubagentCompleted,
    
    // 新增事件
    SubagentLaunched,     // 异步启动后立即发出
    SubagentProgress,     // 进度更新
    SubagentNotification, // 完成通知
}

impl EventFactory {
    pub fn subagent_launched(
        &mut self,
        id: &str,
        description: &str,
        output_file: &str,
    ) -> EventEnvelope { ... }
    
    pub fn subagent_progress(
        &mut self,
        id: &str,
        current_turn: u32,
        total_tools: u32,
    ) -> EventEnvelope { ... }
}
```

### 3.4 实现步骤

#### Step 1: 异步执行框架（1周）
- [ ] 实现 `SubagentRuntime` 结构
- [ ] 添加异步执行模式
- [ ] 实现输出文件管理
- [ ] 添加 `subagent.launched` 事件
- [ ] 测试异步执行

#### Step 2: 状态查询（3天）
- [ ] 实现 `subagent_status` 工具
- [ ] 添加进度追踪
- [ ] 实现结构化输出文件
- [ ] 添加状态查询测试

#### Step 3: 模型选择（2天）
- [ ] 扩展 `SubagentConfig`
- [ ] 支持模型参数传递
- [ ] 测试不同模型组合

#### Step 4: 专用代理类型（1周）
- [ ] 定义 `SubagentType` 枚举
- [ ] 为每种类型编写系统提示
- [ ] 实现工具集过滤
- [ ] 添加类型测试

#### Step 5: Worktree隔离（1周）
- [ ] 实现 `WorktreeGuard`
- [ ] Git worktree 创建/删除
- [ ] 变更检测
- [ ] 自动清理逻辑
- [ ] 并行子代理测试

#### Step 6: 集成与优化（3天）
- [ ] TUI 支持异步状态显示
- [ ] 完善错误处理
- [ ] 性能优化
- [ ] 文档更新

**总计**: 约3-4周

---

## 四、使用示例

### 4.1 同步模式（当前）

```bash
orca exec "use subagent to analyze the auth module"
```

**执行流程**:
```
父代理: 分析任务...
父代理: 调用 subagent "分析 auth 模块"
        [阻塞等待]
子代理: 读取文件...
子代理: 分析代码...
子代理: 返回结果
父代理: 收到结果，继续工作
```

### 4.2 异步模式（新增）

```bash
orca exec "analyze the entire codebase using parallel subagents"
```

**执行流程**:
```
父代理: 任务规划...
父代理: 启动 subagent (async) "分析 auth 模块"
        -> agent-1, output: /tmp/agent-1.log
父代理: 启动 subagent (async) "分析 database 模块"
        -> agent-2, output: /tmp/agent-2.log
父代理: 启动 subagent (async) "分析 API 模块"
        -> agent-3, output: /tmp/agent-3.log
        
父代理: 查询 agent-1 状态 -> Running (50%)
父代理: 查询 agent-2 状态 -> Completed
父代理: 读取 agent-2 输出
父代理: 等待所有子代理完成...
父代理: 汇总所有结果
```

### 4.3 专用代理类型

```bash
orca exec "review the PR using a code reviewer subagent"
```

**执行流程**:
```
父代理: 调用 subagent (type=CodeReviewer) "review PR #123"
子代理: [使用代码审查专用系统提示]
子代理: 检查代码质量...
子代理: 识别潜在问题...
子代理: 生成结构化报告
父代理: 收到审查报告
```

### 4.4 Worktree 隔离

```bash
orca exec "refactor auth module in an isolated environment"
```

**执行流程**:
```
父代理: 调用 subagent (isolation=worktree) "重构 auth"
系统: 创建 git worktree -> /tmp/worktree-xyz
子代理: [在 worktree 中工作]
子代理: 修改文件...
子代理: 运行测试...
子代理: 提交变更
系统: 检查变更 -> 有变更，保留 worktree
父代理: 审查 worktree 中的变更
父代理: 合并或放弃
```

---

## 五、API 设计

### 5.1 工具参数

**Subagent 工具输入**:
```json
{
  "name": "subagent",
  "arguments": {
    "description": "Analyze authentication module",
    "prompt": "Read the auth/ directory and analyze security vulnerabilities",
    "mode": "async",
    "model": "deepseek-v4-pro",
    "type": "CodeReviewer",
    "isolation": "worktree",
    "max_turns": 64
  }
}
```

**Subagent 工具输出（同步）**:
```json
{
  "status": "completed",
  "output": "分析结果...",
  "statistics": {
    "total_tool_use_count": 12,
    "total_duration_ms": 45000,
    "total_tokens": 5000,
    "turns": 8
  }
}
```

**Subagent 工具输出（异步）**:
```json
{
  "status": "async_launched",
  "agent_id": "agent-abc123",
  "description": "Analyze authentication module",
  "output_file": "/tmp/orca-agent-abc123.json",
  "can_read_output_file": true
}
```

### 5.2 SubagentStatus 工具

**输入**:
```json
{
  "name": "subagent_status",
  "arguments": {
    "agent_id": "agent-abc123"
  }
}
```

**输出**:
```json
{
  "agent_id": "agent-abc123",
  "status": "running",
  "progress": {
    "current_turn": 5,
    "max_turns": 64,
    "tools_executed": 8,
    "elapsed_ms": 12000
  },
  "partial_output": "已完成文件读取，正在分析...",
  "error": null
}
```

---

## 六、性能与资源管理

### 6.1 并发控制

```rust
pub struct SubagentPool {
    max_concurrent: usize,          // 最大并发数
    active: HashMap<String, SubagentHandle>,
    pending: VecDeque<SubagentRequest>,
}

impl SubagentPool {
    pub fn spawn(&mut self, request: SubagentRequest) -> Result<String> {
        if self.active.len() >= self.max_concurrent {
            self.pending.push_back(request);
            return Err("max concurrent subagents reached");
        }
        
        let id = uuid();
        let handle = SubagentRuntime::new(request).execute_async()?;
        self.active.insert(id.clone(), handle);
        Ok(id)
    }
    
    pub fn cleanup_completed(&mut self) {
        self.active.retain(|_, handle| !handle.is_completed());
        
        // 启动等待中的子代理
        while self.active.len() < self.max_concurrent {
            if let Some(request) = self.pending.pop_front() {
                let _ = self.spawn(request);
            } else {
                break;
            }
        }
    }
}
```

### 6.2 资源限制

```rust
pub struct SubagentLimits {
    pub max_concurrent_subagents: usize,      // 默认: 3
    pub max_subagent_turns: u32,              // 默认: 64
    pub max_subagent_duration_ms: u64,        // 默认: 300000 (5分钟)
    pub max_output_file_size: usize,          // 默认: 10MB
    pub max_worktrees: usize,                 // 默认: 5
}
```

### 6.3 超时处理

```rust
impl SubagentRuntime {
    pub fn execute_with_timeout(&mut self, timeout: Duration) -> Result<SubagentResult> {
        let (tx, rx) = mpsc::channel();
        
        let handle = thread::spawn(move || {
            let result = self.execute_sync();
            tx.send(result).ok();
        });
        
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                // 超时，终止子代理
                Err("subagent timeout exceeded".into())
            }
        }
    }
}
```

---

## 七、测试计划

### 7.1 单元测试

```rust
#[test]
fn subagent_async_returns_immediately() {
    let request = SubagentRequest {
        prompt: "test task".into(),
        mode: SubagentMode::Async { ... },
        ..
    };
    
    let start = Instant::now();
    let handle = SubagentRuntime::new(request).execute_async().unwrap();
    let elapsed = start.elapsed();
    
    assert!(elapsed.as_millis() < 100); // 应该立即返回
    assert!(handle.output_file.exists());
}

#[test]
fn subagent_status_query() {
    let handle = spawn_test_subagent();
    thread::sleep(Duration::from_secs(1));
    
    let status = query_subagent_status(&handle.id).unwrap();
    assert_eq!(status.status, SubagentStatus::Running);
    assert!(status.progress.current_turn > 0);
}

#[test]
fn worktree_auto_cleanup() {
    let guard = WorktreeGuard::create("test").unwrap();
    let path = guard.path.clone();
    
    assert!(path.exists());
    drop(guard);
    
    // 无变更，应该被清理
    assert!(!path.exists());
}
```

### 7.2 集成测试

```rust
#[test]
fn parallel_subagents_no_conflict() {
    let agent1 = spawn_async_subagent("task 1", Some("worktree"));
    let agent2 = spawn_async_subagent("task 2", Some("worktree"));
    
    // 两个子代理并行执行
    thread::sleep(Duration::from_secs(5));
    
    let status1 = query_status(&agent1.id).unwrap();
    let status2 = query_status(&agent2.id).unwrap();
    
    assert_eq!(status1.status, SubagentStatus::Completed);
    assert_eq!(status2.status, SubagentStatus::Completed);
}

#[test]
fn subagent_output_file_format() {
    let handle = spawn_async_subagent("test", None);
    thread::sleep(Duration::from_secs(2));
    
    let content = fs::read_to_string(&handle.output_file).unwrap();
    let output: SubagentOutput = serde_json::from_str(&content).unwrap();
    
    assert_eq!(output.agent_id, handle.id);
    assert!(output.progress.current_turn > 0);
}
```

---

## 八、文档更新

### 8.1 README 更新

```markdown
## Subagent Tool

The subagent tool allows you to delegate tasks to child agents that can run:
- **Synchronously**: Block until completion (default)
- **Asynchronously**: Run in background, check progress later

### Async Subagents

```bash
# Launch async subagent
orca exec "analyze codebase with async subagent"

# The agent will:
# 1. Launch subagent in background
# 2. Get agent ID and output file
# 3. Continue other work
# 4. Check progress with subagent_status
# 5. Read results when complete
```

### Specialized Subagents

```bash
# Use a code reviewer
orca exec "review this PR with CodeReviewer subagent"

# Use a test writer
orca exec "write tests with TestWriter subagent"
```

### Worktree Isolation

```bash
# Refactor in isolated environment
orca exec "refactor auth module with worktree isolation"
```
```

---

## 九、总结

### 9.1 价值提升

| 功能 | 当前 | 增强后 | 价值 |
|------|------|--------|------|
| 并行能力 | ❌ | ✅ 异步+并发 | 3-5x 吞吐量 |
| 任务分解 | 有限 | ✅ 专用代理 | 更精准 |
| 安全性 | 一般 | ✅ Worktree隔离 | 零风险实验 |
| 灵活性 | 低 | ✅ 模型选择 | 成本优化 |
| 可观测性 | 基础 | ✅ 进度查询 | 实时监控 |

### 9.2 实现优先级

**P0 - 必须实现**:
1. 异步执行模式
2. 状态查询工具
3. 输出文件管理

**P1 - 应该实现**:
4. 专用代理类型
5. Worktree 隔离
6. 模型选择

**P2 - 可以实现**:
7. 并发池管理
8. 高级统计
9. 子代理通信

### 9.3 风险与挑战

**技术风险**:
- 异步执行的进程管理复杂度
- Worktree 创建/清理的可靠性
- 并发控制的资源竞争

**缓解措施**:
- 完善的错误处理和恢复机制
- 严格的资源限制和超时控制
- 充分的测试覆盖

---

**下一步**: 开始 Phase 1 实现 - 异步执行框架
