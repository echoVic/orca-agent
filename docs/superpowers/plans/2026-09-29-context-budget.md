# 上下文预算与压缩兜底 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 会话不再因为"回复预留 + 历史"超过模型上限而卡死：每次请求的 `max_tokens` 按剩余空间裁剪，压缩触发线扣除回复预留，测量以 API 用量为锚，pinned 按消息生效，压缩可以切分当前轮，超限时的紧急压缩要么真正缩小、要么明确失败，上下文仪表显示距离压缩的余量。

**Architecture:** `ContextConfig`（orca-provider `context.rs`）的 `reserved_for_response` 改为真实的回复预留（按思考强度，可配置），现有触发线公式 `min(0.8W, 0.9W − R)` 随之生效；新增 `request_reply_budget` 按测得的提示大小裁剪每次请求的回复预留，经 `ProviderConfig.max_output_tokens` 送到 `deepseek_http.rs`。`Conversation` 携带不持久化的 `UsageAnchor`（上次请求的真实 prompt_tokens 与同一请求的本地估算），`measure_prompt` 以它为锚只估变化量，压缩判断、请求预算、仪表都用这个数。压缩分区改为按"单元"（开启本轮的用户消息，或一条助手消息加它的工具结果）切分，pinned 只保留消息本身，当前轮可以被切分。运行时在发请求前统一走 `prepare_request`：该压缩就压缩，装不下最小回复就紧急压缩，紧急压缩必须真正缩小，否则给出明确说明并结束本轮；API 超限后的重试同样要求真正缩小。

**Tech Stack:** Rust 2024；`orca-core`（配置、`Conversation`）、`orca-provider`（`context.rs` 预算与压缩、`deepseek_http.rs` 请求）、`orca-runtime`（主 agent 与子 agent 的请求路径、压缩步骤、会话恢复、托管状态初始化）、`orca-tui`（状态文字）；`cargo test`、`cargo-nextest`。

**Spec:** `docs/superpowers/specs/2026-09-29-context-budget-design.md`

## Global Constraints

- 基线：分支 `context-budget`，从 `5838259b`（v0.5.4）开出；spec 已在 `9fe76561` 提交，随本计划更新。
- 窗口默认仍为 1,000,000（`orca_core::model::max_context_tokens`），可由 `[model_runtime] context_window` 覆盖。
- 回复预留默认值：Low 32,768 / High 65,536 / Max 131,072，且不超过窗口的 15%；`[model_runtime] max_output_tokens` 覆盖，取值限制在 16,384 到 384,000。
- 每次请求的回复预留 = `min(R, W − P − 8,192)`；小于 `min(16,384, R)` 视为装不下最小回复。
- 触发线公式不变：`soft_limit = min(0.8W 或覆盖值, effective_limit)`，`effective_limit = 0.9W − R`（或 `auto_compact_token_limit`）。
- `UsageAnchor` 不持久化；会话文件格式不变，唯一变化是新写入的投递消息不再带 pinned。
- 不解析 DeepSeek 错误文本里的数字；超限识别沿用 `is_prompt_too_long_error`。
- 压缩切分点只能落在单元之间：工具调用与它的结果永不拆开。
- 代码注释、测试名沿用周边风格：英文，解释"为什么"。
- 每个任务结束先跑该任务的定向测试再提交；不 push。
- 定向测试命令统一带 `CARGO_INCREMENTAL=0 ... --locked`；全量验证在 Task 8。

---

### Task 1: 回复预留与每次请求的预算

**Files:**
- Modify: `crates/orca-core/src/config/mod.rs`（`ModelRuntimeConfig`：新字段、上下限常量、`normalized`）
- Modify: `crates/orca-core/src/config/file.rs`（测试：解析新字段）
- Modify: `crates/orca-provider/src/context.rs`（常量、`default_reply_budget`、`for_model`、`for_model_with_runtime`、`request_reply_budget`；测试）
- Modify: 所有 `ContextConfig::for_model_with_runtime(` 调用处（`grep -rn "for_model_with_runtime(" crates tests`）：生产代码 `crates/orca-runtime/src/runtime_turn_setup.rs`、`crates/orca-runtime/src/session.rs`、`crates/orca-runtime/src/child_agent_loop_setup.rs` 传 `config.reasoning_effort`；测试传 `ReasoningEffort::default()`
- Modify: 所有 `ModelRuntimeConfig {` 字面量（8 处，`grep -rn "ModelRuntimeConfig {" crates src tests`）补 `max_output_tokens: None`

**Interfaces:**
- Produces: `ModelRuntimeConfig.max_output_tokens: Option<usize>`；`ModelRuntimeConfig::MIN_OUTPUT_TOKENS = 16_384`、`MAX_OUTPUT_TOKENS = 384_000`
- Produces: `pub fn default_reply_budget(effort: ReasoningEffort, context_window: usize) -> usize`（`orca_provider::context`）
- Produces: `ContextConfig::for_model_with_runtime(model: Option<&str>, runtime: &ModelRuntimeConfig, effort: ReasoningEffort) -> ContextConfig`
- Produces: `ContextConfig::request_reply_budget(&self, prompt_tokens: usize) -> Option<usize>`
- `ContextConfig.reserved_for_response` 的含义变为"每次请求的回复预留 R"（字段名不变，字面量不用改）

- [ ] **Step 1: 写失败测试（orca-provider）**

在 `crates/orca-provider/src/context.rs` 的 `mod tests` 里，把 `context_config_uses_model_runtime_overrides` 之后的预算测试替换/补充为下面这些（旧的 `for_model_derives_soft_line_from_window_fraction`、`soft_line_scales_down_with_a_smaller_window`、`deep_compaction_target_preserves_headroom_for_tiny_windows` 的断言按新语义改写；其余字面量补 `max_output_tokens: None`，调用补 `ReasoningEffort::default()`）：

```rust
    #[test]
    fn reply_budget_follows_reasoning_effort_and_override() {
        let runtime = ModelRuntimeConfig::default();
        let budget = |effort| {
            ContextConfig::for_model_with_runtime(Some(orca_core::model::FLASH_MODEL), &runtime, effort)
                .reserved_for_response
        };
        assert_eq!(budget(ReasoningEffort::Low), 32_768);
        assert_eq!(budget(ReasoningEffort::High), 65_536);
        assert_eq!(budget(ReasoningEffort::Max), 131_072);

        let runtime = ModelRuntimeConfig {
            max_output_tokens: Some(200_000),
            ..ModelRuntimeConfig::default()
        };
        let config = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
            ReasoningEffort::Low,
        );
        assert_eq!(config.reserved_for_response, 200_000);
    }

    #[test]
    fn compaction_line_leaves_room_for_the_reply() {
        let runtime = ModelRuntimeConfig::default();
        // 0.9 x 1_000_000 - 131_072: the reply comes off the input budget.
        let max = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
            ReasoningEffort::Max,
        );
        assert_eq!(max.effective_limit(), 768_928);
        assert_eq!(max.soft_limit(), 768_928);
        let high = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
            ReasoningEffort::High,
        );
        assert_eq!(high.soft_limit(), 800_000);
        // A 384K reservation pulls the line down to 516K instead of letting
        // prompts between 616K and 800K fail at the provider.
        let wide = ModelRuntimeConfig {
            max_output_tokens: Some(384_000),
            ..ModelRuntimeConfig::default()
        };
        let wide = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &wide,
            ReasoningEffort::Max,
        );
        assert_eq!(wide.soft_limit(), 516_000);
    }

    #[test]
    fn request_reply_budget_trims_to_the_room_left() {
        let config = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &ModelRuntimeConfig::default(),
            ReasoningEffort::Max,
        );
        // The 2026-09-29 request: 664,935 prompt tokens now go out with the
        // full reservation, 796,007 in all.
        assert_eq!(config.request_reply_budget(664_935), Some(131_072));
        // 1_000_000 - 900_000 - 8_192 = 91_808.
        assert_eq!(config.request_reply_budget(900_000), Some(91_808));
        // 11,808 left is below the 16,384 minimum reply.
        assert_eq!(config.request_reply_budget(980_000), None);
    }

    #[test]
    fn default_reply_budget_stays_small_in_a_small_window() {
        assert_eq!(default_reply_budget(ReasoningEffort::Max, 1_000_000), 131_072);
        assert_eq!(default_reply_budget(ReasoningEffort::Max, 200_000), 30_000);
        assert_eq!(default_reply_budget(ReasoningEffort::Low, 40_000), 6_000);
    }
```

把三条旧测试改写为：

```rust
    #[test]
    fn for_model_derives_soft_line_from_window_fraction() {
        let config = ContextConfig::for_model(Some(orca_core::model::PRO_MODEL));
        assert_eq!(config.max_tokens, 1_000_000);
        // The default reasoning effort (max) reserves 131,072 reply tokens.
        assert_eq!(config.reserved_for_response, 131_072);
        // Hard ceiling = 1_000_000 * 0.90 - 131_072 = 768_928.
        assert_eq!(config.effective_limit(), 768_928);
        // The soft line may not sit above the hard ceiling.
        assert_eq!(config.soft_limit(), 768_928);
    }

    #[test]
    fn soft_line_scales_down_with_a_smaller_window() {
        // A 200k window reserves at most 15% (30,000) for the reply and
        // compacts at 0.9 x 200_000 - 30_000.
        let runtime = ModelRuntimeConfig {
            context_window: Some(200_000),
            ..ModelRuntimeConfig::default()
        };
        let config = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::PRO_MODEL),
            &runtime,
            ReasoningEffort::default(),
        );
        assert_eq!(config.soft_limit(), 150_000);
    }

    #[test]
    fn deep_compaction_target_preserves_headroom_for_tiny_windows() {
        let runtime = ModelRuntimeConfig {
            context_window: Some(40_000),
            ..ModelRuntimeConfig::default()
        };
        let config = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::PRO_MODEL),
            &runtime,
            ReasoningEffort::default(),
        );

        // 0.9 x 40_000 - 6_000 (15% of the window) = 30_000, below the 80%
        // soft line (32,000), so it becomes the effective line.
        assert_eq!(config.soft_limit(), 30_000);
        assert_eq!(config.target_compaction_limit(), 18_000);
        assert!(config.target_compaction_limit() < config.soft_limit());
    }
```

测试模块顶部的 `use` 补上 `orca_core::config::ReasoningEffort`（若尚未导入）。

- [ ] **Step 2: 写失败测试（orca-core）**

在 `crates/orca-core/src/config/file.rs` 的 `parse_model_runtime_config` 中，TOML 加一行 `max_output_tokens = 200000`，并补断言：

```rust
        assert_eq!(config.model_runtime.max_output_tokens, Some(200_000));
```

在 `crates/orca-core/src/config/mod.rs` 的 `#[cfg(test)] mod tests`（若无则在文件末尾新建）加：

```rust
    #[test]
    fn model_runtime_clamps_the_reply_budget() {
        let clamp = |tokens| {
            ModelRuntimeConfig {
                max_output_tokens: Some(tokens),
                ..ModelRuntimeConfig::default()
            }
            .normalized()
            .max_output_tokens
        };
        assert_eq!(clamp(1_000), Some(16_384));
        assert_eq!(clamp(500_000), Some(384_000));
        assert_eq!(clamp(100_000), Some(100_000));
    }
```

- [ ] **Step 3: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked -- model_runtime parse_model_runtime_config
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked -- reply_budget compaction_line request_reply_budget soft_line deep_compaction_target for_model_derives
```

预期：编译失败（`max_output_tokens` 字段、`default_reply_budget`、三参数 `for_model_with_runtime`、`request_reply_budget` 都不存在）。

- [ ] **Step 4: 实现 orca-core 部分**

`crates/orca-core/src/config/mod.rs`：

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRuntimeConfig {
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub auto_compact_token_limit: Option<usize>,
    #[serde(default)]
    pub soft_compact_token_limit: Option<usize>,
    /// Reply tokens each request reserves, replacing the reasoning-effort
    /// default.
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl ModelRuntimeConfig {
    /// The smallest reply a request may reserve; below this a request has to
    /// shed context first.
    pub const MIN_OUTPUT_TOKENS: usize = 16_384;
    /// DeepSeek V4's documented output ceiling.
    pub const MAX_OUTPUT_TOKENS: usize = 384_000;

    pub fn normalized(self) -> Self {
        Self {
            context_window: self.context_window.map(|value| value.max(1)),
            auto_compact_token_limit: self.auto_compact_token_limit.map(|value| value.max(1)),
            soft_compact_token_limit: self.soft_compact_token_limit.map(|value| value.max(1)),
            max_output_tokens: self
                .max_output_tokens
                .map(|value| value.clamp(Self::MIN_OUTPUT_TOKENS, Self::MAX_OUTPUT_TOKENS)),
        }
    }
}
```

（保留该 impl 块里原有的其他方法。）把 8 处 `ModelRuntimeConfig {` 字面量补上 `max_output_tokens: None,`（`crates/orca-core/src/config/mod.rs:1029` 附近、`crates/orca-runtime/src/lifecycle.rs:2222` 附近、`tests/runtime_lifecycle_contract.rs:191` 附近，以及 context.rs 测试里的几处——这些在 Step 1 已改为 `..ModelRuntimeConfig::default()` 的可以不再补）。

- [ ] **Step 5: 实现 orca-provider 部分**

`crates/orca-provider/src/context.rs` 顶部：`use orca_core::config::{ModelRuntimeConfig, ProviderKind, ReasoningEffort};`。把 `COMPACTION_THRESHOLD` 与 `RESERVED_FOR_RESPONSE` 的注释及其后新增常量替换为：

```rust
// Hard compaction ceiling as a fraction of the context window. The reply
// reservation comes off it, so the soft line below never sits where a prompt
// plus its reply would overflow the window.
const COMPACTION_THRESHOLD: f64 = 0.90;
// Reply reservation for configs built without a model or reasoning effort
// (the `Default` impl, used by tests).
const RESERVED_FOR_RESPONSE: usize = 4096;
// Reply tokens reserved per request, by reasoning effort. DeepSeek V4 accepts
// up to 384K, but reserving that much leaves a 1M window about 620K of prompt
// before the provider rejects the request, which is how a long session died
// on 2026-09-29. Codewhale and Reasonix reserve 64K.
const LOW_EFFORT_REPLY_TOKENS: usize = 32_768;
const HIGH_EFFORT_REPLY_TOKENS: usize = 65_536;
const MAX_EFFORT_REPLY_TOKENS: usize = 131_072;
// The default reservation never takes more than this share of the window, so
// a small custom window keeps room for its prompt.
const MAX_REPLY_WINDOW_FRACTION: f64 = 0.15;
// Window room every request leaves beyond prompt and reply, for estimate error.
const REQUEST_MARGIN_TOKENS: usize = 8_192;

/// The reply tokens a request reserves when the configuration names none.
pub fn default_reply_budget(effort: ReasoningEffort, context_window: usize) -> usize {
    let by_effort = match effort {
        ReasoningEffort::Low => LOW_EFFORT_REPLY_TOKENS,
        ReasoningEffort::High => HIGH_EFFORT_REPLY_TOKENS,
        ReasoningEffort::Max => MAX_EFFORT_REPLY_TOKENS,
    };
    by_effort
        .min((context_window as f64 * MAX_REPLY_WINDOW_FRACTION) as usize)
        .max(1)
}
```

`impl ContextConfig` 中替换 `for_model`、`for_model_with_runtime`，并新增 `request_reply_budget`：

```rust
    pub fn for_model(model: Option<&str>) -> Self {
        let max_tokens = orca_core::model::max_context_tokens(model);
        Self {
            max_tokens,
            compaction_threshold: COMPACTION_THRESHOLD,
            reserved_for_response: default_reply_budget(ReasoningEffort::default(), max_tokens),
            auto_compact_token_limit: None,
            // None => soft line derived from the window fraction. An absolute
            // override is opt-in via `soft_compact_token_limit`.
            soft_compact_token_limit: None,
        }
    }

    pub fn for_model_with_runtime(
        model: Option<&str>,
        runtime: &ModelRuntimeConfig,
        effort: ReasoningEffort,
    ) -> Self {
        let mut config = Self::for_model(model);
        if let Some(context_window) = runtime.context_window {
            config.max_tokens = context_window.max(1);
        }
        config.reserved_for_response = runtime
            .max_output_tokens
            .map(|tokens| {
                tokens.clamp(
                    ModelRuntimeConfig::MIN_OUTPUT_TOKENS,
                    ModelRuntimeConfig::MAX_OUTPUT_TOKENS,
                )
            })
            .unwrap_or_else(|| default_reply_budget(effort, config.max_tokens));
        config.auto_compact_token_limit = runtime.auto_compact_token_limit;
        if let Some(limit) = runtime.soft_compact_token_limit {
            config.soft_compact_token_limit = Some(limit);
        }
        config
    }

    /// Reply tokens one request can reserve once its prompt measures
    /// `prompt_tokens`: the reply reservation, trimmed to the room the window
    /// has left. `None` when not even a minimal reply fits; the request has
    /// to shed context first.
    pub fn request_reply_budget(&self, prompt_tokens: usize) -> Option<usize> {
        let room = self
            .max_tokens
            .saturating_sub(prompt_tokens.saturating_add(REQUEST_MARGIN_TOKENS));
        let budget = self.reserved_for_response.min(room);
        let minimum = ModelRuntimeConfig::MIN_OUTPUT_TOKENS.min(self.reserved_for_response);
        (budget >= minimum).then_some(budget)
    }
```

- [ ] **Step 6: 更新调用处**

`grep -rn "for_model_with_runtime(" crates tests`，逐处补第三个参数：
- `crates/orca-runtime/src/runtime_turn_setup.rs`、`crates/orca-runtime/src/session.rs`（手动压缩那处）、`crates/orca-runtime/src/child_agent_loop_setup.rs`：`config.reasoning_effort`
- 其余（测试）：`orca_core::config::ReasoningEffort::default()`

- [ ] **Step 7: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked -- model_runtime parse_model_runtime_config
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked
CARGO_INCREMENTAL=0 cargo build -p orca-runtime --all-targets --locked
```

预期：全部通过、编译通过。orca-provider 中若有其他测试断言旧的 800,000 或 895,904（来自 `for_model`/`for_model_with_runtime` 默认值），按"回复预留扣在输入预算里"的新语义更新断言数值并在注释里写明来源；使用 `ContextConfig::default()` 或自定义字面量的测试不应受影响。

- [ ] **Step 8: 提交**

```bash
git add crates/orca-core crates/orca-provider crates/orca-runtime tests
git commit -m "fix(provider): reserve a real reply budget in the context config"
```

提交正文写明：`reserved_for_response` 从写死的 4,096 改为按思考强度的回复预留（不超过窗口 15%，可由 `[model_runtime] max_output_tokens` 覆盖），现有触发线公式因此扣除回复预留；新增 `request_reply_budget`。

---

### Task 2: 请求携带裁剪后的回复预留

**Files:**
- Modify: `crates/orca-provider/src/lib.rs`（`ProviderConfig` 新字段）
- Modify: `crates/orca-provider/src/deepseek_http.rs`（去掉 `DEFAULT_CHAT_MAX_TOKENS`，新增 `chat_max_tokens`；测试）
- Modify: 全部 `ProviderConfig { ... }` 字面量（98 处，脚本补字段）
- Modify: `crates/orca-provider/src/context.rs`（新增 `with_request_reply_budget`；测试）

**Interfaces:**
- Consumes: `default_reply_budget`、`ContextConfig::request_reply_budget`（Task 1）
- Produces: `ProviderConfig.max_output_tokens: Option<u32>`
- Produces: `pub fn with_request_reply_budget(provider_config: &ProviderConfig, context_config: &ContextConfig, prompt_tokens: usize) -> ProviderConfig`（`orca_provider::context`）

- [ ] **Step 1: 给 `ProviderConfig` 加字段并批量补字面量**

`crates/orca-provider/src/lib.rs` 的 `ProviderConfig` 末尾加：

```rust
    /// Reply tokens this request reserves (`max_tokens`). The runtime sets it
    /// per request from the context budget; `None` falls back to the
    /// reasoning-effort default.
    pub max_output_tokens: Option<u32>,
```

然后用脚本给所有没有展开语法（`..x`）的字面量补 `max_output_tokens: None,`（插在字面量的右花括号之前）：

```bash
python3 - <<'EOF'
import pathlib, re, subprocess
files = subprocess.run(
    ["grep", "-rl", "ProviderConfig {", "crates", "src", "tests", "--include=*.rs"],
    capture_output=True, text=True,
).stdout.split()
for name in files:
    path = pathlib.Path(name)
    lines = path.read_text().split("\n")
    out, index, changed = [], 0, False
    while index < len(lines):
        line = lines[index]
        out.append(line)
        index += 1
        if not re.search(r"\bProviderConfig \{\s*$", line) or re.search(r"struct|impl|->", line):
            continue
        depth = line.count("{") - line.count("}")
        body = []
        while index < len(lines) and depth > 0:
            depth += lines[index].count("{") - lines[index].count("}")
            body.append(lines[index])
            index += 1
        text = "\n".join(body)
        if "max_output_tokens" not in text and not re.search(r"^\s*\.\.", text, re.M):
            indent = re.match(r"\s*", body[-1]).group(0) + "    "
            body.insert(len(body) - 1, f"{indent}max_output_tokens: None,")
            changed = True
        out.extend(body)
    if changed:
        path.write_text("\n".join(out))
EOF
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets --locked 2>&1 | grep -E "^error" | head
```

预期：没有 `error`。若有遗漏的字面量（例如花括号不在行尾的写法），手动补上 `max_output_tokens: None`。

- [ ] **Step 2: 写失败测试**

`crates/orca-provider/src/deepseek_http.rs` 的 `mod tests` 中新增（沿用文件里已有的 `spawn_response_sequence_server`、`spawn_streaming_response_sequence_server` 与 `Value`）：

```rust
    #[test]
    fn chat_request_reserves_the_configured_reply_budget() {
        let ok = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"prompt_cache_hit_tokens":0}}"#;
        let (base_url, bodies) = spawn_response_sequence_server(vec![ok, ok]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let mut config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some(FLASH_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::High,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
            max_output_tokens: Some(50_000),
        };

        request_chat(&conversation, &config).expect("budgeted request");
        config.max_output_tokens = None;
        request_chat(&conversation, &config).expect("default request");

        let bodies = bodies.lock().expect("lock captured bodies");
        let budgeted: Value = serde_json::from_str(&bodies[0]).expect("budgeted body");
        let default: Value = serde_json::from_str(&bodies[1]).expect("default body");
        assert_eq!(budgeted["max_tokens"], 50_000);
        // High effort reserves 65,536 when the runtime names no budget.
        assert_eq!(default["max_tokens"], 65_536);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_request_reserves_the_configured_reply_budget() {
        let answer = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                      data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                      data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"prompt_cache_hit_tokens\":0}}\n\n\
                      data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![answer]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some(FLASH_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
            max_output_tokens: Some(40_000),
        };
        let cancel = CancelToken::new();

        request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("streaming request");

        let bodies = bodies.lock().expect("lock captured bodies");
        let body: Value = serde_json::from_str(&bodies[0]).expect("streaming body");
        assert_eq!(body["max_tokens"], 40_000);
    }
```

`crates/orca-provider/src/context.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn request_config_carries_the_trimmed_reply_budget() {
        let context_config = ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &ModelRuntimeConfig::default(),
            ReasoningEffort::Max,
        );
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        };

        let roomy = with_request_reply_budget(&provider_config, &context_config, 664_935);
        let tight = with_request_reply_budget(&provider_config, &context_config, 900_000);
        let full = with_request_reply_budget(&provider_config, &context_config, 995_000);

        assert_eq!(roomy.max_output_tokens, Some(131_072));
        assert_eq!(tight.max_output_tokens, Some(91_808));
        // No room left: the request still goes out with the minimum, and the
        // provider's rejection drives emergency compaction.
        assert_eq!(full.max_output_tokens, Some(16_384));
    }
```

- [ ] **Step 3: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked -- reserves_the_configured_reply_budget request_config_carries
```

预期：`with_request_reply_budget` 未定义导致编译失败；补上桩之前 `chat_request_...` 断言 `max_tokens` 为 384000。

- [ ] **Step 4: 实现**

`crates/orca-provider/src/deepseek_http.rs`：删除 `const DEFAULT_CHAT_MAX_TOKENS: u32 = 384_000;`，新增：

```rust
/// The `max_tokens` a chat request reserves for its reply: the runtime's
/// per-request budget, or the reasoning-effort default.
fn chat_max_tokens(config: &ProviderConfig) -> u32 {
    config.max_output_tokens.unwrap_or_else(|| {
        let window = orca_core::model::max_context_tokens(config.model.as_deref());
        u32::try_from(crate::context::default_reply_budget(config.reasoning_effort, window))
            .unwrap_or(u32::MAX)
    })
}
```

流式请求 `request_chat_streaming_with_budget` 中：

```rust
        max_tokens: Some(summary_budget.unwrap_or_else(|| chat_max_tokens(config))),
```

非流式 `request_chat` 中：

```rust
        max_tokens: Some(chat_max_tokens(config)),
```

测试里原来引用 `DEFAULT_CHAT_MAX_TOKENS` 的断言改为 `chat_max_tokens(&config)`（作用域里有 `config` 的地方）；直接构造 `ChatRequest` 并断言 `384_000` 的序列化测试改为 `max_tokens: Some(131_072)` 并断言 `131_072`。

`crates/orca-provider/src/context.rs` 新增：

```rust
/// The provider config for one request: its reply reservation trimmed to the
/// room `prompt_tokens` leaves in the window. A request that cannot fit a
/// minimal reply still goes out with the minimum; the provider's rejection
/// then drives emergency compaction.
pub fn with_request_reply_budget(
    provider_config: &ProviderConfig,
    context_config: &ContextConfig,
    prompt_tokens: usize,
) -> ProviderConfig {
    let budget = context_config
        .request_reply_budget(prompt_tokens)
        .unwrap_or_else(|| {
            ModelRuntimeConfig::MIN_OUTPUT_TOKENS.min(context_config.reserved_for_response)
        });
    let mut config = provider_config.clone();
    config.max_output_tokens = Some(u32::try_from(budget).unwrap_or(u32::MAX));
    config
}
```

- [ ] **Step 5: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets --locked
```

预期：全部通过。

- [ ] **Step 6: 提交**

```bash
git add -A crates src tests
git commit -m "fix(provider): send each request's trimmed reply budget as max_tokens"
```

正文写明：`ProviderConfig.max_output_tokens` 由运行时按请求设置；缺省时按思考强度取默认值，不再固定发 384,000；`with_request_reply_budget` 负责裁剪。

---

### Task 3: 以 API 用量为锚的测量

**Files:**
- Modify: `crates/orca-core/src/conversation.rs`（`UsageAnchor`、`Conversation.usage_anchor`、记录/校验/清除、指纹；测试）
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`（第 100 行附近的 `Conversation` 字面量补 `usage_anchor: None`）
- Modify: `crates/orca-provider/src/context.rs`（`PromptMeasurement`、`measure_prompt`；`context_pressure` 改用它；压缩结果清除锚点；测试）

**Interfaces:**
- Produces: `pub struct UsageAnchor { pub prompt_tokens: u64, pub estimated_tokens: usize, /* private */ }`
- Produces: `Conversation.usage_anchor: Option<UsageAnchor>`（pub，不持久化）
- Produces: `Conversation::record_usage_anchor(&mut self, prompt_tokens: u64, estimated_tokens: usize, message_count: usize)`、`Conversation::valid_usage_anchor(&self) -> Option<UsageAnchor>`、`Conversation::clear_usage_anchor(&mut self)`
- Produces: `pub struct PromptMeasurement { pub estimated: usize, pub tokens: usize }`、`pub fn measure_prompt(conversation: &Conversation, provider_config: &ProviderConfig) -> PromptMeasurement`（`orca_provider::context`）
- 行为变化：`context_pressure(...).wire_tokens` 变为 `measure_prompt(...).tokens`

- [ ] **Step 1: 写失败测试（orca-core）**

`crates/orca-core/src/conversation.rs` 的 `mod tests` 中：

```rust
    #[test]
    fn usage_anchor_holds_until_the_measured_messages_change() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("first".to_string());
        conv.record_usage_anchor(1_000, 900, 2);
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        assert_eq!(
            conv.valid_usage_anchor().map(|anchor| (anchor.prompt_tokens, anchor.estimated_tokens)),
            Some((1_000, 900))
        );

        // Editing a measured message invalidates the anchor.
        conv.messages[1] = Message::user("edited".to_string());
        assert!(conv.valid_usage_anchor().is_none());

        // So does rewinding past it.
        conv.record_usage_anchor(1_000, 900, 2);
        conv.messages.truncate(1);
        assert!(conv.valid_usage_anchor().is_none());

        // An anchor over no messages measures nothing.
        conv.record_usage_anchor(1_000, 900, 0);
        assert!(conv.valid_usage_anchor().is_none());
    }
```

- [ ] **Step 2: 写失败测试（orca-provider）**

`crates/orca-provider/src/context.rs` 的 `mod tests` 中（`provider_config()` 若测试模块里没有，就在测试里用一个 `tools_override: Some(vec![])`、其余为空的字面量，参照 Task 2 Step 2）：

```rust
    fn bare_provider_config() -> ProviderConfig {
        ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::default(),
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        }
    }

    #[test]
    fn measured_prompt_anchors_on_the_reported_count() {
        let provider_config = bare_provider_config();
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("read the file".to_string());
        let first = wire_equivalent_tokens(&conv, &provider_config);
        conv.record_usage_anchor(first as u64 + 5, first, conv.messages.len());
        conv.add_assistant(Some("reading it now".to_string()), None, vec![]);

        let measurement = measure_prompt(&conv, &provider_config);

        let estimated = wire_equivalent_tokens(&conv, &provider_config);
        assert_eq!(measurement.estimated, estimated);
        // The provider counted 5 more than Orca did; only the change since is estimated.
        assert_eq!(measurement.tokens, estimated + 5);
    }

    #[test]
    fn measured_prompt_ignores_an_implausible_anchor() {
        let provider_config = bare_provider_config();
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("read the file".to_string());
        let first = wire_equivalent_tokens(&conv, &provider_config);
        // Three times Orca's own estimate: usage merged across retries.
        conv.record_usage_anchor(first as u64 * 3, first, conv.messages.len());

        let measurement = measure_prompt(&conv, &provider_config);

        assert_eq!(measurement.tokens, measurement.estimated);
    }

    #[test]
    fn compaction_drops_the_usage_anchor() {
        let provider_config = bare_provider_config();
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        for index in 0..6 {
            conv.add_user(format!("inspect file {index}"));
            conv.add_assistant(
                None,
                None,
                vec![RawToolCall {
                    id: format!("call-{index}"),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            );
            conv.add_tool_result(format!("call-{index}"), "evidence ".repeat(1_000));
        }
        conv.add_user("current request".to_string());
        let estimated = wire_equivalent_tokens(&conv, &provider_config);
        conv.record_usage_anchor(estimated as u64, estimated, conv.messages.len());
        let config = ContextConfig {
            max_tokens: 20_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(20_000),
            soft_compact_token_limit: Some(1_500),
        };

        let result = compact_with_summary(ProviderKind::DeepSeek, &conv, &config, &provider_config);

        assert!(result.conversation.usage_anchor.is_none());
    }
```

测试模块顶部确保导入了 `RawToolCall`、`ReasoningEffort`、`ProviderKind`（沿用已有导入）。

- [ ] **Step 3: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked -- usage_anchor
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked -- measured_prompt compaction_drops_the_usage_anchor
```

预期：编译失败（`record_usage_anchor`、`valid_usage_anchor`、`usage_anchor`、`measure_prompt` 不存在）。

- [ ] **Step 4: 实现 orca-core 部分**

`crates/orca-core/src/conversation.rs`，在 `Conversation` 定义之前：

```rust
/// The provider's prompt-token count for the last request, with Orca's own
/// estimate of that same request. The next request is measured as the
/// reported count plus the estimated change, so only the change carries
/// estimate error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageAnchor {
    pub prompt_tokens: u64,
    pub estimated_tokens: usize,
    message_count: usize,
    boundary: u64,
}
```

`Conversation` 加字段（放在 `summary` 之后）：

```rust
    /// Not persisted: a resumed conversation measures from a full estimate
    /// until its first response.
    pub usage_anchor: Option<UsageAnchor>,
```

`Conversation::new()` 里补 `usage_anchor: None`。`impl Conversation` 中新增：

```rust
    /// Anchors measurement on a request built from the first `message_count`
    /// messages.
    pub fn record_usage_anchor(
        &mut self,
        prompt_tokens: u64,
        estimated_tokens: usize,
        message_count: usize,
    ) {
        self.usage_anchor = message_count
            .checked_sub(1)
            .and_then(|index| self.messages.get(index))
            .map(|boundary| UsageAnchor {
                prompt_tokens,
                estimated_tokens,
                message_count,
                boundary: message_fingerprint(boundary),
            });
    }

    /// The anchor while the messages it measured are still in place.
    pub fn valid_usage_anchor(&self) -> Option<UsageAnchor> {
        let anchor = self.usage_anchor?;
        let boundary = self.messages.get(anchor.message_count.checked_sub(1)?)?;
        (message_fingerprint(boundary) == anchor.boundary).then_some(anchor)
    }

    pub fn clear_usage_anchor(&mut self) {
        self.usage_anchor = None;
    }
```

文件内（`impl Conversation` 之外）新增：

```rust
// Identifies the last message an anchor measured, so an edit or rewind of
// the history shows up as a different fingerprint.
fn message_fingerprint(message: &Message) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match message {
        Message::System { content, .. } => (0_u8, content).hash(&mut hasher),
        Message::User {
            content, images, ..
        } => (1_u8, content, images.len()).hash(&mut hasher),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            (2_u8, content).hash(&mut hasher);
            for call in tool_calls {
                call.id.hash(&mut hasher);
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => (3_u8, tool_call_id, content).hash(&mut hasher),
    }
    hasher.finish()
}
```

`crates/orca-runtime/src/thread_store/writer.rs` 第 100 行附近的 `orca_core::conversation::Conversation { ... }` 字面量补 `usage_anchor: None,`。若编译提示还有其他 `Conversation {` 字面量，同样补上。

- [ ] **Step 5: 实现 orca-provider 部分**

`crates/orca-provider/src/context.rs` 新增（放在 `wire_equivalent_tokens` 之后）：

```rust
/// What the next request's prompt measures. `estimated` is Orca's estimate of
/// the whole request; `tokens` anchors on the provider's count for the last
/// request when the conversation still holds what it measured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptMeasurement {
    pub estimated: usize,
    pub tokens: usize,
}

// A reported count this far from Orca's estimate of the same request does not
// describe that request (usage merged across retries); measure from the
// estimate instead.
const ANCHOR_MIN_RATIO: f64 = 0.5;
const ANCHOR_MAX_RATIO: f64 = 1.6;

pub fn measure_prompt(
    conversation: &Conversation,
    provider_config: &ProviderConfig,
) -> PromptMeasurement {
    let estimated = wire_equivalent_tokens(conversation, provider_config);
    let tokens = conversation
        .valid_usage_anchor()
        .filter(|anchor| {
            let ratio = anchor.prompt_tokens as f64 / anchor.estimated_tokens.max(1) as f64;
            (ANCHOR_MIN_RATIO..=ANCHOR_MAX_RATIO).contains(&ratio)
        })
        .map(|anchor| {
            usize::try_from(anchor.prompt_tokens)
                .unwrap_or(usize::MAX)
                .saturating_add(estimated)
                .saturating_sub(anchor.estimated_tokens)
        })
        .unwrap_or(estimated);
    PromptMeasurement { estimated, tokens }
}
```

`context_pressure` 改为：

```rust
pub fn context_pressure(
    conversation: &Conversation,
    config: &ContextConfig,
    provider_config: &ProviderConfig,
) -> ContextPressure {
    let wire_tokens = measure_prompt(conversation, provider_config).tokens;
    context_pressure_for_tokens(wire_tokens, config)
}
```

`compact_with_summary_inner` 中，除"无压力、原样返回 `normalized`"那条路径外，其余三个返回的会话都清除锚点（它们改写了历史）：微压缩结果 `micro_compacted`、`summarize_collapsed_messages` 的结果、`local_compaction` 的结果。写法示例：

```rust
    if wire_equivalent_tokens(&micro_compacted, provider_config) <= micro_target {
        let mut conversation = micro_compacted;
        conversation.clear_usage_anchor();
        return CompactionResult {
            conversation,
            kind: CompactionKind::LocalTruncation,
        };
    }
```

以及在 `match summarize_collapsed_messages(...)` 两个分支构造 `CompactionResult` 前对会话调用 `clear_usage_anchor()`。`compact_with_counter` 的微压缩与 `local_compaction` 返回值同样清除。

- [ ] **Step 6: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets --locked
```

预期：全部通过。

- [ ] **Step 7: 提交**

```bash
git add crates/orca-core crates/orca-provider crates/orca-runtime
git commit -m "feat(provider): measure the prompt from the provider's last count"
```

---

### Task 4: 运行时按请求裁剪回复预留并记录锚点

**Files:**
- Modify: `crates/orca-runtime/src/provider_turn.rs`（`RuntimeProviderTurnInput` 用 `context_config` 取代 `context_window`；请求前测量与裁剪；响应后记录锚点；`context.updated` 分母；测试）
- Modify: `crates/orca-runtime/src/runtime_turn_opening.rs`（首轮 `context.updated` 分母改为触发线）
- Modify: `crates/orca-runtime/src/child_agent_loop_runner.rs`（两处子 agent 请求：裁剪与锚点）

**Interfaces:**
- Consumes: `measure_prompt`、`PromptMeasurement`（Task 3）、`with_request_reply_budget`（Task 2）、`Conversation::record_usage_anchor`/`clear_usage_anchor`（Task 3）
- Produces: `pub(crate) fn record_request_usage_anchor(conversation: &mut Conversation, usage: Option<Usage>, estimated: usize, message_count: usize)`（`provider_turn.rs`）
- `RuntimeProviderTurnInput.context_window: usize` 被 `context_config: &'a context::ContextConfig` 取代

- [ ] **Step 1: 写失败测试**

`crates/orca-runtime/src/provider_turn.rs` 的 `mod tests` 中：

```rust
    #[test]
    fn a_successful_request_anchors_the_next_measurement() {
        let mut conversation = Conversation::new();
        conversation.add_system("sys".to_string());
        conversation.add_user("hello".to_string());

        record_request_usage_anchor(
            &mut conversation,
            Some(orca_core::provider_types::Usage {
                input_tokens: 1_200,
                output_tokens: 30,
                cache_tokens: 0,
            }),
            1_000,
            2,
        );

        let anchor = conversation.valid_usage_anchor().expect("anchor");
        assert_eq!((anchor.prompt_tokens, anchor.estimated_tokens), (1_200, 1_000));

        // A response without usage leaves nothing to anchor on.
        record_request_usage_anchor(&mut conversation, None, 1_000, 2);
        assert!(conversation.valid_usage_anchor().is_none());
    }
```

- [ ] **Step 2: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- a_successful_request_anchors_the_next_measurement
```

预期：`record_request_usage_anchor` 未定义，编译失败。

- [ ] **Step 3: 实现主 agent 路径**

`crates/orca-runtime/src/provider_turn.rs`：

1. `RuntimeProviderTurnInput` 中删除 `context_window: usize` 及其注释，换成：

```rust
    /// This request's budget: the reply reservation to trim per request, and
    /// the compaction line the local `context.updated` event reports against.
    pub(crate) context_config: &'a context::ContextConfig,
```

（若文件尚未导入 `orca_provider::context`，补 `use orca_provider::context;`。）构造处（`RuntimeProviderCycleStep::run` 中 `context_window: input.context_config.max_tokens,`）改为 `context_config: input.context_config,`。测试里构造 `RuntimeProviderTurnInput` 的地方（原 `context_window: 1_000_000`）改为 `context_config: &context_config`，并在该测试开头加：

```rust
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &ModelRuntimeConfig::default(),
            orca_core::config::ReasoningEffort::default(),
        );
```

2. 在 `for analysis in prepared_images.persisted_analyses { ... }` 之后、构造 `model_conversation` 之前：

```rust
        // The persisted messages this request is built from; the usage
        // anchor measures against them.
        let request_message_count = conversation.messages.len();
```

在 `model_conversation` 构造完成之后、写 prompt-cache checkpoint 之前：

```rust
        let measurement = context::measure_prompt(&model_conversation, input.provider_config);
        let request_config = context::with_request_reply_budget(
            input.provider_config,
            input.context_config,
            measurement.tokens,
        );
```

重试循环里 `start_streaming_bounded(...)` 与 `start_streaming(...)` 的 `input.provider_config` 参数改为 `&request_config`（prompt-cache checkpoint 与挂起流的 `model` 仍用 `input.provider_config`）。

3. 循环结束后的合并改为先记录本次尝试的用量：

```rust
        let attempt_usage = response.usage;
        merge_provider_usage(&mut response.usage, failed_attempt_usage);
        if response.error().is_none() {
            record_request_usage_anchor(
                conversation,
                attempt_usage,
                measurement.estimated,
                request_message_count,
            );
        }
```

4. `context_updated` 的分母改为触发线：

```rust
                sink.emit(events.context_updated(
                    usage.input_tokens as usize,
                    input.context_config.soft_limit(),
                ))?;
```

（同时把上方注释里 "against the full model window" 改为 "against the compaction line"。）

5. 文件中新增：

```rust
/// Anchors the next request's measurement on this one: the provider's prompt
/// count for the attempt that answered, against Orca's estimate of the same
/// request. Usage merged across failed attempts would overstate the prompt,
/// so callers pass the answering attempt's usage.
pub(crate) fn record_request_usage_anchor(
    conversation: &mut Conversation,
    usage: Option<orca_core::provider_types::Usage>,
    estimated: usize,
    message_count: usize,
) {
    match usage.filter(|usage| usage.input_tokens > 0) {
        Some(usage) => {
            conversation.record_usage_anchor(usage.input_tokens, estimated, message_count)
        }
        None => conversation.clear_usage_anchor(),
    }
}
```

- [ ] **Step 4: 首轮估算事件**

`crates/orca-runtime/src/runtime_turn_opening.rs` 中压缩之后发出 `context_updated` 的地方，分母由 `input.context_config.max_tokens` 改为 `input.context_config.soft_limit()`，并把注释 "on the full-window scale" 改为 "against the compaction line"。

- [ ] **Step 5: 实现子 agent 路径**

`crates/orca-runtime/src/child_agent_loop_runner.rs` 两处 `run_child_agent_provider_turn(...)`/`run_child_agent_provider_turn_observed(...)` 调用之前：

```rust
        let measurement = context::measure_prompt(&setup.conversation, &turn_provider_config);
        let request_config = context::with_request_reply_budget(
            &turn_provider_config,
            &setup.context_config,
            measurement.tokens,
        );
        let request_message_count = setup.conversation.messages.len();
```

调用参数里的 `&turn_provider_config` 改为 `&request_config`。在 `ChildAgentProviderTurn::Response(response) => response,` 所在的 `match` 结束之后：

```rust
        if response.error().is_none() {
            crate::provider_turn::record_request_usage_anchor(
                &mut setup.conversation,
                response.usage,
                measurement.estimated,
                request_message_count,
            );
        }
```

（按需补 `use orca_provider::context;`。）

- [ ] **Step 6: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- provider_turn child_agent
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets --locked
```

预期：全部通过。

- [ ] **Step 7: 提交**

```bash
git add crates/orca-runtime
git commit -m "fix(runtime): trim each request's reply budget and anchor on its usage"
```

---

### Task 5: 按单元划分的压缩：pinned 按消息生效，当前轮可切分

**Files:**
- Modify: `crates/orca-provider/src/context.rs`（新增 `CompactionUnit`/`compaction_units`；重写 `partition_for_compaction`、`cache_aware_micro_compaction`）
- Modify: `crates/orca-provider/src/context_tests.rs`（新测试；改写 `hard_pressure_keeps_pinned_and_current_tool_turn_atomically`）

**Interfaces:**
- 内部实现，不改公开签名：`compact_with_summary`、`compact_with_summary_cancellable`、`compact_with_counter` 行为变化——pinned 只保留消息本身（pinned 的助手/工具消息保留其所在单元），当前轮只保证保留开启本轮的用户消息与最新单元。

- [ ] **Step 1: 写失败测试**

`crates/orca-provider/src/context_tests.rs` 追加：

```rust
#[test]
fn pinned_notices_in_every_turn_do_not_block_compaction() {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    for index in 0..6 {
        tool_turn(&mut conversation, index, 1_000);
        conversation.add_system_pinned(format!(
            "<task-notification>task {index} finished</task-notification>"
        ));
    }
    conversation.add_user("current request".to_string());
    let before = conversation.messages.len();

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(500, 700),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(messages.len() < before, "nothing was compacted");
    for index in 0..6 {
        let notice = format!("<task-notification>task {index} finished</task-notification>");
        assert!(messages.iter().any(|message| message.is_pinned()
            && message.content_str() == Some(notice.as_str())));
    }
    assert!(!messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-0")));
    assert!(messages.iter().any(|message| message.content_str() == Some("current request")));
}

#[test]
fn compaction_cuts_into_a_long_current_turn() {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    conversation.add_user("refactor the whole module".to_string());
    for index in 0..30 {
        conversation.add_assistant(
            None,
            Some("next step".to_string()),
            vec![RawToolCall {
                id: format!("call-{index}"),
                function_name: "read_file".to_string(),
                arguments: format!(r#"{{"path":"file-{index}.rs"}}"#),
            }],
        );
        conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(200));
    }

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(2_000, 3_000),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(messages.len() < conversation.messages.len(), "nothing was compacted");
    assert!(messages.iter().any(|message| message.content_str() == Some("refactor the whole module")));
    assert!(messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-29")));
    assert!(!messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-0")));
    // Every kept tool result still follows the call that asked for it.
    for (index, message) in messages.iter().enumerate() {
        if let Message::Tool { tool_call_id, .. } = message {
            assert!(messages[..index].iter().any(|earlier| matches!(earlier,
                Message::Assistant { tool_calls, .. }
                    if tool_calls.iter().any(|call| &call.id == tool_call_id))));
        }
    }
}

#[test]
fn micro_compaction_shortens_output_beside_a_pinned_notice() {
    let mut conversation = tool_fixture();
    // Right after the first turn's tool result.
    conversation.messages.insert(
        5,
        Message::pinned_system("<task-notification>done</task-notification>".to_string()),
    );

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(1_500, 20_000),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(matches!(&messages[4],
        Message::Tool { content, .. } if content.starts_with("[tool output micro-compact]")));
    assert!(matches!(&messages[5],
        Message::System { content, pinned: true } if content.contains("done")));
}
```

把 `hard_pressure_keeps_pinned_and_current_tool_turn_atomically` 改名为 `hard_pressure_keeps_pinned_units_and_the_current_turn_tail`，删掉其中基于 `turn_ranges(...).last()` 的 `expected`/`active` 比较，换成：

```rust
    let messages = &result.conversation.messages;
    // The message that opened the current turn stays, and so does its newest unit.
    assert!(messages.iter().any(|message| message.content_str() == Some("inspect file 99")));
    assert!(messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, terminal: Some(_), .. } if tool_call_id == "pending-a")));
    // The pinned tool result keeps the call that produced it.
    assert!(messages.iter().any(|message| matches!(message,
        Message::Assistant { tool_calls, .. } if tool_calls.iter().any(|call| call.id == "call-0"))));
    assert!(!messages.iter().any(|message| message.content_str() == Some("inspect file 2")));
```

（保留测试原有的会话构造与 `compact_with_summary` 调用。）

- [ ] **Step 2: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked -- pinned_notices_in_every_turn compaction_cuts_into_a_long_current_turn micro_compaction_shortens_output_beside hard_pressure_keeps_pinned
```

预期：前三个失败（整轮保留导致什么也没压、当前轮无法切分、带 pinned 的轮次被微压缩跳过）；改写后的第四个在旧实现下也失败（当前轮中段被整轮保留时仍通过的话，至少前三个失败即可）。

- [ ] **Step 3: 实现单元划分**

`crates/orca-provider/src/context.rs`，在 `turn_ranges` 之后新增：

```rust
/// Compaction works on units: a user message that opens a turn, or an
/// assistant message together with the tool results that answer it. System
/// notices and image-analysis messages ride with the unit before them, so a
/// cut never separates a tool call from its result.
struct CompactionUnit {
    range: std::ops::Range<usize>,
    opens_turn: bool,
}

fn compaction_units(messages: &[Message]) -> Vec<CompactionUnit> {
    let mut units: Vec<CompactionUnit> = Vec::new();
    for (index, message) in messages
        .iter()
        .enumerate()
        .skip(leading_system_count(messages))
    {
        let opens_turn = matches!(message, Message::User { content, .. }
            if !content.starts_with(orca_core::conversation::IMAGE_ANALYSIS_MESSAGE_PREFIX));
        if units.is_empty() || opens_turn || matches!(message, Message::Assistant { .. }) {
            units.push(CompactionUnit {
                range: index..index + 1,
                opens_turn,
            });
        } else if let Some(unit) = units.last_mut() {
            unit.range.end = index + 1;
        }
    }
    units
}

// A pinned assistant or tool message keeps its whole unit: kept alone, a tool
// result would lose the call it answers.
fn unit_pinned_whole(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.is_pinned() && matches!(message, Message::Assistant { .. } | Message::Tool { .. })
    })
}
```

- [ ] **Step 4: 重写分区**

`partition_for_compaction` 整体替换为：

```rust
fn partition_for_compaction(
    conversation: &Conversation,
    config: &ContextConfig,
    counter: &impl TokenCounter,
    tool_tokens: usize,
) -> Option<CompactionPartition> {
    let messages = &conversation.messages;
    let target_tokens = config.target_compaction_limit();
    let prefix = messages[..leading_system_count(messages)].to_vec();
    let units = compaction_units(messages);
    let newest = units.len().checked_sub(1)?;
    let current_opener = units.iter().rposition(|unit| unit.opens_turn);
    let tokens = |message: &Message| message_tokens_with_counter(message, counter);
    let cost = |unit: &CompactionUnit| messages[unit.range.clone()].iter().map(tokens).sum::<usize>();
    let pinned_cost = |unit: &CompactionUnit| {
        messages[unit.range.clone()]
            .iter()
            .filter(|message| message.is_pinned())
            .map(tokens)
            .sum::<usize>()
    };
    let image_cost = |unit: &CompactionUnit| {
        messages[unit.range.clone()]
            .iter()
            .map(|message| match message {
                Message::User { images, .. } => images.iter().map(image_tokens).sum::<usize>(),
                _ => 0,
            })
            .sum::<usize>()
    };

    // Always kept: the newest unit, the message that opened the current turn,
    // units whose pinned assistant or tool message cannot stand alone, and
    // every other pinned message on its own.
    let mut retain = units
        .iter()
        .enumerate()
        .map(|(index, unit)| {
            index == newest
                || Some(index) == current_opener
                || unit_pinned_whole(&messages[unit.range.clone()])
        })
        .collect::<Vec<_>>();
    let mut budget = prefix.iter().map(tokens).sum::<usize>()
        + internal_context_tokens_with_counter(conversation, counter)
        + tool_tokens
        + summary_budget(config)
        + 32
        + units
            .iter()
            .zip(&retain)
            .map(|(unit, keep)| if *keep { cost(unit) } else { pinned_cost(unit) })
            .sum::<usize>();

    let image_allowance = MAX_HISTORY_IMAGE_TOKENS.min(target_tokens / 4);
    let mut retained_history_images = 0;
    for index in (0..units.len()).rev() {
        if retain[index] {
            continue;
        }
        let unit = &units[index];
        let unit_cost = cost(unit).saturating_sub(pinned_cost(unit));
        if budget.saturating_add(unit_cost) > target_tokens
            || retained_history_images + image_cost(unit) > image_allowance
        {
            // Keep only a contiguous recent tail.
            break;
        }
        retain[index] = true;
        budget += unit_cost;
        retained_history_images += image_cost(unit);
    }

    let mut kept = vec![];
    let mut collapsed = vec![];
    for (unit, keep) in units.iter().zip(retain) {
        for message in &messages[unit.range.clone()] {
            if keep || message.is_pinned() {
                kept.push(message.clone());
            } else {
                collapsed.push(message.clone());
            }
        }
    }
    (!collapsed.is_empty()).then_some(CompactionPartition {
        prefix,
        collapsed,
        kept,
    })
}
```

- [ ] **Step 5: 重写微压缩**

`cache_aware_micro_compaction` 整体替换为：

```rust
fn cache_aware_micro_compaction(
    conversation: &Conversation,
    target: usize,
    retention_budget: usize,
    count: impl Fn(&Conversation) -> usize,
) -> Conversation {
    let mut result = conversation.clone();
    let units = compaction_units(&result.messages);
    let image_budget = MAX_HISTORY_IMAGE_TOKENS.min(retention_budget / 4);
    let mut remaining_images = image_budget;
    // Evidence recency takes priority over prefix reuse for images. The
    // newest unit and pinned messages are exempt; otherwise keep whole image
    // groups within the allowance.
    for unit in units.iter().rev().skip(1) {
        for message in result.messages[unit.range.clone()].iter_mut().rev() {
            if message.is_pinned() {
                continue;
            }
            if let Message::User {
                content, images, ..
            } = message
            {
                let cost = images.iter().map(image_tokens).sum::<usize>();
                if cost <= remaining_images {
                    remaining_images -= cost;
                } else {
                    content.push_str(&format!(
                        "\n[{} historical image input(s) omitted to fit context]",
                        images.len()
                    ));
                    images.clear();
                }
            }
        }
    }
    // Work backwards only until enough capacity is recovered. Everything
    // before the first changed unit remains an exact, useful cache prefix.
    // Track the estimate by the tokens each rewrite saves instead of
    // re-counting the whole conversation per unit.
    let mut estimate = count(&result);
    for unit in units.iter().rev().skip(1) {
        if estimate <= target {
            break;
        }
        for message in &mut result.messages[unit.range.clone()] {
            if let Message::Tool {
                content,
                pinned: false,
                ..
            } = message
                && content.len() > STALE_TOOL_OUTPUT_BYTES
                && !content.starts_with("[tool output micro-compact]")
            {
                let reduced = micro_compact_tool_output(content);
                let before = DefaultTokenCounter.count_text(content);
                let after = DefaultTokenCounter.count_text(&reduced);
                if after < before {
                    *content = reduced;
                    estimate = estimate.saturating_sub(before - after);
                }
            }
        }
    }
    result
}
```

改写后 `turn_ranges` 已没有调用者（`context_tests.rs` 里那两处随 Step 1 的测试改写一起删掉了），把它删除；若编译提示仍有测试在用，给它加 `#[cfg(test)]`。

- [ ] **Step 6: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked
```

预期：全部通过。若有旧测试断言"含 pinned 的整轮原样保留"，按新规则（pinned 只保留消息本身，pinned 的助手/工具消息保留其单元）更新断言，并在测试注释中写明。

- [ ] **Step 7: 提交**

```bash
git add crates/orca-provider
git commit -m "fix(provider): compact by unit so pins and long turns no longer block it"
```

正文写明：一轮里有 pinned 消息不再冻结整轮，pinned 只保留消息本身；当前轮只保证保留开启本轮的用户消息与最新单元；微压缩跳过的是 pinned 消息而不是整轮，并按节省量递减估算，不再每个单元重算整份会话。

---

### Task 6: 投递消息不再 pinned，旧会话迁移

**Files:**
- Modify: `crates/orca-core/src/conversation.rs`（`unpin_delivered_notices`；测试）
- Modify: `crates/orca-runtime/src/session.rs`（三处恢复路径调用迁移）
- Modify: `crates/orca-runtime/src/controller.rs`（两处 `Message::pinned_system(...)` 投递）
- Modify: `crates/orca-runtime/src/runtime_turn_loop.rs`（三处 `add_system_pinned(content)`）
- Modify: `crates/orca-runtime/src/runtime_turn_opening.rs`（两处预算软着陆提醒）
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`（`append_system_delivery_once` 存储为普通消息）
- Modify: 断言这些消息 pinned 的测试（见 Step 5）

**Interfaces:**
- Produces: `Conversation::unpin_delivered_notices(&mut self)`

- [ ] **Step 1: 写失败测试**

`crates/orca-core/src/conversation.rs` 的 `mod tests`：

```rust
    #[test]
    fn resumed_notices_lose_their_pin_but_user_pins_stay() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user_pinned("keep this constraint".to_string());
        conv.messages.push(Message::pinned_system(
            "<task-notification>Terminal session done</task-notification>".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Parent guidance id=7] look at the tests".to_string(),
        ));
        conv.messages
            .push(Message::pinned_system("[Plan mode on]\nPlan mode applies".to_string()));

        conv.unpin_delivered_notices();

        let pinned = conv
            .messages
            .iter()
            .filter(|message| message.is_pinned())
            .filter_map(Message::content_str)
            .collect::<Vec<_>>();
        assert_eq!(
            pinned,
            vec!["keep this constraint", "[Plan mode on]\nPlan mode applies"]
        );
    }
```

把 `crates/orca-runtime/src/controller.rs` 中断言终端通知的测试（约 2303 行，`Some(Message::System { content, pinned: true }) if content.contains("<task-notification>")`）改为断言 `pinned: false`。

- [ ] **Step 2: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked -- resumed_notices_lose_their_pin
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- controller
```

预期：`unpin_delivered_notices` 未定义；controller 测试因通知仍为 pinned 而失败。

- [ ] **Step 3: 实现迁移**

`crates/orca-core/src/conversation.rs`：

```rust
// Deliveries that earlier versions pinned: background task and subagent
// notices, and parent guidance to a subagent. Every turn holding one was
// kept whole by compaction, so a long session could no longer shrink.
const DELIVERED_NOTICE_PREFIXES: [&str; 2] = ["<task-notification>", "[Parent guidance id="];
```

`impl Conversation` 中：

```rust
    /// Unpins delivered notices in sessions written before they became
    /// ordinary messages.
    pub fn unpin_delivered_notices(&mut self) {
        for message in &mut self.messages {
            if let Message::System { content, pinned } = message
                && *pinned
                && DELIVERED_NOTICE_PREFIXES
                    .iter()
                    .any(|prefix| content.starts_with(prefix))
            {
                *pinned = false;
            }
        }
    }
```

`crates/orca-runtime/src/session.rs` 三处 `conv.strip_legacy_summary_messages();` 之后各加一行 `conv.unpin_delivered_notices();`。

- [ ] **Step 4: 新投递改为普通消息**

- `crates/orca-runtime/src/controller.rs`：`Message::pinned_system(completion.model_notification())` 与 `.push(Message::pinned_system(content))` 两处改为 `Message::system(...)`。
- `crates/orca-runtime/src/runtime_turn_loop.rs`：三处 `conversation.add_system_pinned(content);` 改为 `conversation.add_system(content);`。
- `crates/orca-runtime/src/runtime_turn_opening.rs`：两处软着陆提醒 `input.conversation.add_system_pinned(message);` 改为 `input.conversation.add_system(message);`，并把上方注释 "inject a pinned system reminder" 改为 "inject a system reminder"。
- `crates/orca-runtime/src/thread_store/writer.rs`：`append_system_delivery_once` 中 `StoredMessage::from(&Message::pinned_system(content.to_owned()))` 改为 `StoredMessage::from(&Message::system(content.to_owned()))`。

- [ ] **Step 5: 更新依赖旧行为的测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked 2>&1 | grep -E "^test .*FAILED|panicked" | head -20
```

对失败项逐一判断：断言后台任务通知、子 agent 结果、父级指导、wait 结果、预算软着陆提醒为 `pinned: true` 的，改为 `pinned: false`（例如 `crates/orca-runtime/src/lifecycle.rs` 中断言 "Cost budget is low" 的测试）；断言用户提示（`PinnedUser`）、hook 上下文、运行时指令为 pinned 的保持不变。

- [ ] **Step 6: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-core --lib --locked
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked
```

预期：全部通过。

- [ ] **Step 7: 提交**

```bash
git add crates/orca-core crates/orca-runtime
git commit -m "fix(runtime): deliver task notices as ordinary messages"
```

正文写明：后台任务与子 agent 通知、父级指导、wait 结果、预算提醒不再 pinned；旧会话恢复时这些通知解除 pinned，事故会话升级后即可被压缩。

---

### Task 7: 紧急压缩与超限恢复

**Files:**
- Modify: `crates/orca-runtime/src/compaction.rs`（`is_emergency`、`RuntimeCompactionAdoption`、`compact_and_persist` 校验、`prepare_request`、`compact_after_provider_error_retry` 返回是否采用、`unrecoverable_overflow_message`/`context_overflow_message`、TUI 旧路径；测试）
- Modify: `crates/orca-runtime/src/runtime_turn_opening.rs`（改用 `prepare_request`，失败时结束本轮）
- Modify: `crates/orca-runtime/src/provider_turn.rs`（`handle_provider_error`：未缩小则失败）
- Modify: `crates/orca-runtime/src/child_agent_provider_turn.rs`（新增 `prepare_child_agent_request`；超限重试未缩小则失败）
- Modify: `crates/orca-runtime/src/child_agent_loop_runner.rs`（两处改用 `prepare_child_agent_request`）

**Interfaces:**
- Consumes: `measure_prompt`（Task 3）、`request_reply_budget`（Task 1）、`context_pressure_for_tokens`（已有）
- Produces: `RuntimeCompactionStep::prepare_request(&mut self, conversation: &mut Conversation) -> io::Result<Result<(), String>>`
- Produces: `RuntimeCompactionStep::compact_after_provider_error_retry(...) -> io::Result<bool>`（true = 历史确实缩小并已替换）
- Produces: `pub(crate) fn unrecoverable_overflow_message(provider_message: &str) -> String`、`pub(crate) fn context_overflow_message(prompt_tokens: usize) -> String`
- Produces: `pub fn prepare_child_agent_request(config: &RunConfig, setup: &mut ChildAgentLoopSetup, cwd: &Path, hooks: &HookRunner) -> io::Result<Result<(), String>>`

- [ ] **Step 1: 写失败测试**

`crates/orca-runtime/src/compaction.rs` 的 `mod tests` 中（沿用该模块已有的测试构造方式：`EventFactory`、`EventSink::new(Vec::new(), OutputFormat::Jsonl)`、`HookRunner::default()`、`RuntimeTurnContext::new(...)`、`ProviderKind::Mock`；按需导入 `orca_core::subagent_types::SubagentType`、`orca_core::config::{OutputFormat, ReasoningEffort}`、`orca_core::event_schema::EventFactory`、`orca_core::event_sink::EventSink`、`orca_provider::ProviderConfig`）：

```rust
    fn bare_provider_config() -> ProviderConfig {
        ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::default(),
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        }
    }

    fn step_config(window: usize) -> context::ContextConfig {
        context::ContextConfig {
            max_tokens: window,
            compaction_threshold: 0.9,
            reserved_for_response: 4_096,
            auto_compact_token_limit: None,
            soft_compact_token_limit: None,
        }
    }

    fn history_with_turns(turns: usize, words: usize) -> Conversation {
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        for index in 0..turns {
            conversation.add_user(format!("inspect file {index}"));
            conversation.add_assistant(
                None,
                None,
                vec![orca_core::conversation::RawToolCall {
                    id: format!("call-{index}"),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            );
            conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(words));
        }
        conversation.add_user("current request".to_string());
        conversation
    }

    #[test]
    fn prepare_request_compacts_when_no_reply_fits() {
        // 40K window, 4,096 reply: a request needs the prompt at or under 27,712.
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("prepare-request".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = history_with_turns(8, 4_000);
        let before = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(config.request_reply_budget(before).is_none(), "fixture must overflow");

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        assert_eq!(prepared, Ok(()));
        // Micro-compaction may keep every message and shorten old tool
        // output instead, so compare the measured size, not the count.
        let after = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(after < before);
        assert!(config.request_reply_budget(after).is_some());
    }

    #[test]
    fn prepare_request_stops_when_compaction_cannot_make_room() {
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("prepare-request".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        // Only the current request, too large on its own: nothing to collapse.
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        conversation.add_user("evidence ".repeat(40_000));
        let before = conversation.messages.clone();

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        let message = prepared.expect_err("no room for a reply");
        assert!(message.contains("/new"), "{message}");
        assert_eq!(conversation.messages.len(), before.len());
    }

    #[test]
    fn overflow_retry_is_refused_when_compaction_cannot_shrink() {
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("overflow-retry".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        conversation.add_user("small request".to_string());

        let shrunk = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .compact_after_provider_error_retry(&mut conversation, RuntimeCompactionTrigger::PromptTooLong)
        .expect("compact");

        assert!(!shrunk);
    }
```


- [ ] **Step 2: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- prepare_request overflow_retry_is_refused
```

预期：`prepare_request` 未定义、`compact_after_provider_error_retry` 返回 `()`，编译失败。

- [ ] **Step 3: 实现压缩步骤**

`crates/orca-runtime/src/compaction.rs`：

1. `impl RuntimeCompactionTrigger` 中新增：

```rust
    /// Hard-limit and prompt-too-long compactions must make room: they force
    /// a real reduction and keep the result only when it shrank the prompt.
    fn is_emergency(self) -> bool {
        matches!(self, Self::HardLimit | Self::PromptTooLong)
    }
```

2. 新增类型与文案：

```rust
/// Whether a compaction replaced history, and what the next request measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionAdoption {
    pub(crate) adopted: bool,
    pub(crate) prompt_tokens: usize,
}

pub(crate) fn context_overflow_message(prompt_tokens: usize) -> String {
    format!(
        "The conversation no longer fits the model's context window (about {prompt_tokens} \
         tokens) and compaction cannot shrink it further. Start a new conversation with /new."
    )
}

pub(crate) fn unrecoverable_overflow_message(provider_message: &str) -> String {
    format!(
        "{provider_message} Compaction cannot shrink the conversation further; start a new \
         conversation with /new."
    )
}

// The compacted history carries no usage anchor. Scale its estimate by the
// ratio the provider's count showed for the history it replaces, never down.
fn calibrated_prompt_tokens(estimated: usize, reference: context::PromptMeasurement) -> usize {
    if reference.tokens <= reference.estimated {
        return estimated;
    }
    let scaled =
        estimated as u128 * reference.tokens as u128 / reference.estimated.max(1) as u128;
    usize::try_from(scaled).unwrap_or(usize::MAX)
}
```

3. `compact_and_persist` 改为返回 `io::Result<(RuntimeCompactionOutcome, RuntimeCompactionAdoption)>`：

```rust
    fn compact_and_persist(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<(RuntimeCompactionOutcome, RuntimeCompactionAdoption)> {
        let before_messages = conversation.messages.len();
        let task = RuntimeCompactionTask::start(trigger, before_messages);
        let before = context::measure_prompt(conversation, self.provider_config);
        // An emergency must make room even when the local estimate says the
        // prompt fits: force the compaction line under three quarters of the
        // measured prompt so at least one real reduction happens.
        let mut emergency_config;
        let context_config = if trigger.is_emergency() {
            emergency_config = self.context_config.clone();
            emergency_config.soft_compact_token_limit = Some(
                self.context_config
                    .soft_limit()
                    .min(before.tokens.saturating_mul(3) / 4)
                    .max(1),
            );
            &emergency_config
        } else {
            self.context_config
        };
        let compaction = if let Some(cancel) = self.cancel {
            context::compact_with_summary_cancellable(
                self.provider,
                conversation,
                context_config,
                self.provider_config,
                cancel,
            )
        } else {
            context::compact_with_summary(
                self.provider,
                conversation,
                context_config,
                self.provider_config,
            )
        };
        let after = context::measure_prompt(&compaction.conversation, self.provider_config);
        if trigger.is_emergency() && after.estimated >= before.estimated {
            // Nothing shrank: keep the history as it was.
            return Ok((
                task.finish(before_messages, &compaction.kind),
                RuntimeCompactionAdoption {
                    adopted: false,
                    prompt_tokens: before.tokens,
                },
            ));
        }
        let after_messages = compaction.conversation.messages.len();
        let outcome = task.finish(after_messages, &compaction.kind);
        let details = outcome.details();
        if outcome.should_persist_summary_state(self.turn_context.emit_deltas)
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            // Count-only replay cannot reconstruct suffix rewrites, images or
            // pinned turns. Reuse the existing atomic context snapshot record.
            let identity = crate::session::ManualCompactionPersistenceIdentity {
                operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                snapshot_id: uuid::Uuid::now_v7().to_string(),
            };
            writer.append_manual_compaction_snapshot(
                &identity,
                details.before_messages,
                details.strategy.as_str(),
                &compaction.conversation,
            )?;
        }
        *conversation = compaction.conversation;
        conversation.clear_usage_anchor();
        Ok((
            outcome,
            RuntimeCompactionAdoption {
                adopted: true,
                prompt_tokens: calibrated_prompt_tokens(after.estimated, before),
            },
        ))
    }
```

4. `compact_with_budget_hooks` 改为返回 `io::Result<RuntimeCompactionAdoption>`：把 `let outcome = self.compact_and_persist(conversation, trigger)?;` 改为 `let (outcome, adoption) = self.compact_and_persist(conversation, trigger)?;`，函数末尾 `Ok(adoption)`。

5. `compact_if_needed` 末尾改为 `Ok(self.compact_with_budget_hooks(conversation, trigger)?.adopted)`。

6. 新增 `prepare_request`：

```rust
    /// Before a request: compact when pressure calls for it, and as an
    /// emergency when the prompt leaves no room for a minimal reply. `Err`
    /// carries the user-facing message when not even that makes room.
    pub(crate) fn prepare_request(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<Result<(), String>> {
        let measured = context::measure_prompt(conversation, self.provider_config).tokens;
        let trigger = if self.context_config.request_reply_budget(measured).is_none() {
            Some(RuntimeCompactionTrigger::HardLimit)
        } else {
            RuntimeCompactionPolicy::decide_for_pressure(context::context_pressure_for_tokens(
                measured,
                self.context_config,
            ))
        };
        let Some(trigger) = trigger else {
            return Ok(Ok(()));
        };
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        let adoption = self.compact_with_budget_hooks(conversation, trigger)?;
        Ok(
            if self
                .context_config
                .request_reply_budget(adoption.prompt_tokens)
                .is_some()
            {
                Ok(())
            } else {
                Err(context_overflow_message(adoption.prompt_tokens))
            },
        )
    }
```

7. `compact_after_provider_error_retry` 改为：

```rust
    pub(crate) fn compact_after_provider_error_retry(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<bool> {
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        let (outcome, adoption) = self.compact_and_persist(conversation, trigger)?;
        self.emit_compaction_completed(&outcome)?;
        Ok(adoption.adopted)
    }
```

8. `handle_tui_agent_provider_error` 的 `CompactAndRetry` 分支改为：

```rust
            let shrunk = compaction
                .compact_after_provider_error_retry(runtime_parts.conversation, trigger)?;
            if shrunk {
                state.retry.record_prompt_too_long_retry();
                Ok(TuiAgentProviderErrorAction::RetryAfterCompaction)
            } else {
                state.retry.reset();
                Ok(TuiAgentProviderErrorAction::SurfaceError(
                    unrecoverable_overflow_message(&error.message),
                ))
            }
```

- [ ] **Step 4: 接入主 agent**

`crates/orca-runtime/src/runtime_turn_opening.rs`：把 `RuntimeCompactionStep::new(...).compact_if_needed(input.conversation)?;` 改为：

```rust
        let prepared = RuntimeCompactionStep::new(
            input.provider,
            input.context_config,
            input.provider_config,
            turn_context.clone(),
            input.hooks,
            input.events,
            input.sink,
            input.history_writer.as_deref_mut(),
        )
        .prepare_request(input.conversation)?;
        if let Err(message) = prepared {
            if turn_context.emit_deltas {
                input.sink.emit(input.events.error(&message))?;
            }
            return Ok(RuntimeTurnOpeningResult::Return(AgentLoopResult::failure(
                RunStatus::Failed,
                message,
            )));
        }
```

并补 `use orca_core::event_schema::RunStatus;`。

`crates/orca-runtime/src/provider_turn.rs` 的 `handle_provider_error` 中 `CompactAndRetry` 分支改为：

```rust
            RuntimeCompactionRetryDecision::CompactAndRetry { trigger, reason: _ } => {
                if compaction.compact_after_provider_error_retry(conversation, trigger)? {
                    Ok(RuntimeProviderErrorOutcome::ContinueAfterCompaction)
                } else {
                    let message = crate::compaction::unrecoverable_overflow_message(&error.message);
                    compaction.emit_error(&message)?;
                    Ok(RuntimeProviderErrorOutcome::Failed(message))
                }
            }
```

- [ ] **Step 5: 接入子 agent**

`crates/orca-runtime/src/child_agent_provider_turn.rs`：

1. 新增（放在 `compact_child_agent_conversation_if_needed` 之后，写法与它一致）：

```rust
/// Before a child request: compact when needed, and fail the child with a
/// clear message when not even emergency compaction leaves room to reply.
pub fn prepare_child_agent_request(
    config: &RunConfig,
    setup: &mut ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
) -> io::Result<Result<(), String>> {
    let mut events = EventFactory::new("child-agent-compaction".to_string());
    let mut sink = EventSink::new(child_event_output(), config.output_format);
    let subagent_type = SubagentType::General;
    let mut compaction = RuntimeCompactionStep::new(
        config.provider,
        &setup.context_config,
        &setup.provider_config,
        RuntimeTurnContext::new(cwd, "", 0, false, &subagent_type),
        hooks,
        &mut events,
        &mut sink,
        None,
    );
    compaction.prepare_request(&mut setup.conversation)
}
```

2. `handle_child_agent_provider_error` 的 `CompactAndRetry` 分支：

```rust
            if compaction.compact_after_provider_error_retry(&mut setup.conversation, trigger)? {
                setup.compaction_retry.record_prompt_too_long_retry();
                Ok(Some(ChildAgentProviderErrorDecision::RetryAfterCompaction))
            } else {
                Ok(Some(ChildAgentProviderErrorDecision::Fail(ChildAgentResult {
                    status: RunStatus::Failed,
                    final_message: None,
                    error: Some(crate::compaction::unrecoverable_overflow_message(
                        &error.message,
                    )),
                    budget_usage: None,
                })))
            }
```

`crates/orca-runtime/src/child_agent_loop_runner.rs` 两处 `compact_child_agent_conversation_if_needed(config, &mut setup, context.cwd, context.hooks)?;` 改为：

```rust
        if let Err(message) =
            prepare_child_agent_request(config, &mut setup, context.cwd, context.hooks)?
        {
            let result = ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(message),
                budget_usage: None,
            };
            return finish_lightweight_child_result(&setup, lease, checkpoint_observer, result);
        }
```

第二处（observed 循环）使用该循环里与 `ChildAgentProviderTurn::Fail` 分支相同的收尾函数与参数（照抄那个分支的 `return ...` 写法），并把导入里的 `compact_child_agent_conversation_if_needed` 换成（或补上）`prepare_child_agent_request`。`crates/orca-runtime/src/agent_child.rs` 的重新导出同样补上 `prepare_child_agent_request`。

- [ ] **Step 6: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- compaction prepare_request overflow child_agent provider_turn
CARGO_INCREMENTAL=0 cargo build --workspace --all-targets --locked
```

预期：全部通过。原有断言 "prompt-too-long 后总是重试" 的测试，若其会话其实可以缩小则仍通过；若构造的会话无法缩小，改为断言失败文案（包含 `/new`）。

- [ ] **Step 7: 提交**

```bash
git add crates/orca-runtime
git commit -m "fix(runtime): make emergency compaction shrink the prompt or stop clearly"
```

正文写明：发送前装不下最小回复时走紧急压缩；紧急压缩与超限重试只在确实缩小时采用；否则不替换历史、不重试，并提示 `/new`；主 agent、旧 TUI 路径与子 agent 一致。

---

### Task 8: 仪表上限、状态文字、全量验证

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs`（上下文快照上限改为触发线；测试）
- Modify: `crates/orca-tui/src/ui.rs`（`context_cell` 注释）
- Modify: `crates/orca-tui/src/slash_command_actions.rs`（`/status` 文案；相关测试）
- Modify: `docs/superpowers/specs/2026-09-29-context-budget-design.md`（状态行）

**Interfaces:**
- Produces: `fn surface_context_limit_tokens(config: &RunConfig) -> u64`（`runtime_host.rs`，私有）

- [ ] **Step 1: 写失败测试**

`crates/orca-runtime/src/runtime_host.rs` 的 `mod tests` 中（用该模块已有的 `surface_test_config(cwd, history_mode)` 构造 `RunConfig`）：

```rust
    #[test]
    fn context_meter_counts_down_to_the_compaction_line() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut run_config = surface_test_config(cwd.path().to_path_buf(), HistoryMode::Disabled);
        run_config.reasoning_effort = orca_core::config::ReasoningEffort::Max;
        // 0.9 x 1_000_000 - 131_072: where automatic compaction starts.
        assert_eq!(surface_context_limit_tokens(&run_config), 768_928);
        run_config.reasoning_effort = orca_core::config::ReasoningEffort::High;
        assert_eq!(surface_context_limit_tokens(&run_config), 800_000);
    }
```

- [ ] **Step 2: 运行，确认失败**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- context_meter_counts_down
```

预期：`surface_context_limit_tokens` 未定义。

- [ ] **Step 3: 实现**

`crates/orca-runtime/src/runtime_host.rs` 新增：

```rust
/// The context meter's ceiling: the line where automatic compaction starts,
/// so "ctx 0%" means compaction is due rather than that requests will fail.
fn surface_context_limit_tokens(config: &RunConfig) -> u64 {
    let model = config.model.as_option();
    orca_provider::context::ContextConfig::for_model_with_runtime(
        model.as_deref(),
        &config.model_runtime,
        config.reasoning_effort,
    )
    .soft_limit()
    .max(1) as u64
}
```

原 `let context_limit_tokens = config.model_runtime.context_window.unwrap_or_else(...).max(1) as u64;` 改为 `let context_limit_tokens = surface_context_limit_tokens(config);`。

`crates/orca-tui/src/ui.rs` 的 `context_cell` 文档注释改为：

```rust
/// Room left before automatic compaction, as a percentage of the compaction
/// line (100% = empty, 0% = compaction is due). Fed by the provider-reported
/// prompt tokens once a turn completes. Pure local observability — never sent
/// upstream, so it cannot affect DeepSeek's prefix cache. Hidden until a real
/// budget is known.
```

`crates/orca-tui/src/slash_command_actions.rs` 的 `format_status` 中：

```rust
        format!("{remaining} left before compaction (compacts at {context_limit_tokens})")
```

并更新断言旧文案 `remaining / ... total` 的测试（`grep -rn "remaining /" crates/orca-tui`）。

- [ ] **Step 4: 运行定向测试**

```bash
CARGO_INCREMENTAL=0 cargo test -p orca-runtime --lib --locked -- context_meter runtime_host
CARGO_INCREMENTAL=0 cargo test -p orca-tui --lib --locked -- status context_cell
```

预期：全部通过。断言初始上下文上限为 1,000,000 的托管/ACP 测试，按新语义改为触发线（默认 Max 强度 768,928）。

- [ ] **Step 5: 全量验证**

```bash
cargo fmt --all -- --check
CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked 2>&1 | grep -E "^(warning|error)" | sort | uniq -c
CARGO_INCREMENTAL=0 cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0
CARGO_INCREMENTAL=0 cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0
```

预期：fmt 通过；clippy 不新增警告（与基线 `5838259b` 对比本分支改动文件）；两轮 nextest 全部通过。与本分支无关的计时类用例若单次失败，单独复跑 10 次确认后在报告中说明。

- [ ] **Step 6: 更新 spec 状态并提交**

把 spec 第三行改为：`> 状态：第一阶段已实现（分支 context-budget，2026-09-29）。基线 5838259b（v0.5.4）。`

```bash
git add crates docs
git commit -m "fix(tui): show room left before compaction in the context meter"
```

正文写明：仪表上限改为触发线，`ctx N%` 与 `/status` 表示距离自动压缩的余量；ACP 的 usage 上限随快照一起改变；spec 标记第一阶段完成。
