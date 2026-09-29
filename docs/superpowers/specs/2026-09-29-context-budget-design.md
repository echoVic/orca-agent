# 上下文预算、压缩兜底与进度展示

> 状态：第一阶段已实现（分支 context-budget，2026-09-29）。基线 5838259b（v0.5.4）。
> 目标：会话不会再因为"回复预留 + 历史"超过模型上限而卡死；压缩在任何形状的历史上都能真正缩小；上下文仪表显示的是还能用多少，而不是整个窗口的剩余比例。

## 0. 背景

### 0.1 事故

2026-09-29，boss-skill 会话（full-auto，deepseek-flash，max 思考强度，902 条消息）连续两次请求被拒：

```
This model's maximum context length is 1048576 tokens. However, you requested
1048935 tokens (664935 in the messages, 384000 in the completion).
```

状态栏同时显示 `ctx 33%`。逐层原因：

1. 每个请求都带 `max_tokens = 384000`（`deepseek_http.rs` 的 `DEFAULT_CHAT_MAX_TOKENS`，v0.3.1 起）。DeepSeek 要求提示与回复预留之和不超过 1,048,576，所以提示超过 664,576 个 token 的请求必然失败。
2. 压缩阈值不扣除这份预留：软线是假设窗口 1,000,000 的 80%（800,000），硬线是 90% 减 4,096（895,904）。在 664,576 到 800,000 之间，请求必然失败，压缩也永远不会触发。
3. 被拒之后的恢复路径（`compact_and_persist` 的 `PromptTooLong` 分支）强制压缩一次再重试，但什么也没压掉：`before_messages 902, after_messages 902, collapsed 0`，重试原样失败。
4. 什么也没压掉的原因：后台任务完成通知以 pinned system 消息的形式插入（`controller.rs` 的 `Message::pinned_system(completion.model_notification())`，v0.3.23 起），而压缩规则是"一轮里只要有一条 pinned 消息，整轮不动"（`partition_for_compaction` 与 `cache_aware_micro_compaction`）。这个会话 7 个历史轮次各有至少一条通知，共 38 条，第一轮就有 468 条消息，全部受保护。
5. 仪表的"已用"是上一次请求的真实 prompt token，分母却是整个窗口（`runtime_host.rs` 的 `context_limit_tokens`），所以显示剩 33%，实际可用为 0。

会话从此无法继续：每个新请求至少一样大，`/compact` 走同一套规则，同样压不动。

### 0.2 调研

读了四个 DeepSeek 原生 agent 的源码（报告在 `/tmp/ds-agent-research/reports/`，带文件行号）：官方 [dsh](https://github.com/deepseek-ai/deepseek-harness)、[Codewhale](https://github.com/Hmbown/Codewhale)（原 DeepSeek-TUI）、[Reasonix](https://github.com/esengine/DeepSeek-Reasonix)、[deepcode-cli](https://github.com/lessweb/deepcode-cli)。上下文管理上结论高度一致：

| | dsh | Codewhale | Reasonix |
|---|---|---|---|
| 回复预留 | 固定 256K | 默认 64K（384K 只当上限） | 按思考强度 16K/32K/64K，每次请求再按剩余空间裁剪 |
| 压缩触发线 | min(80%, 窗口 − 预留 − 64K) | min(80%, 窗口 − 预留 − 1K) | 85%（预留小且每次裁剪） |
| 用量测量 | 上次 API 用量 + 之后新增的估算 | 同 dsh，取两者较大 | 按真实用量校准的字符比例 |
| 受保护内容 | 无 pinned，必须保留的内容压缩后重新追加 | 只保护一条去重后的消息 | 系统提示与很短的首条用户消息 |
| 后台通知 | 普通消息 | 普通消息 | 普通消息 |
| 超限恢复 | 除最新一组工具调用外全部摘要，历史确实变了才重试 | 最多两次，变小才采用，否则给出原因并停止 | 强制压缩一次，请求没变就放弃并报错 |

Codewhale 的 CHANGELOG（#4293/#4368/#4378）记录了与本次相同的事故，修法也相同：触发线按扣除回复预留后的可用输入计算。Reasonix 的仪表显示"距离压缩还剩多少"，是唯一没有 33% 这类误导的实现。

## 1. 范围

本设计分三个阶段，第一阶段是 v0.5.5 的内容，后两个阶段各自单独出计划。

- **第一阶段（本次实施）**：统一上下文预算；以 API 用量为锚的测量；pinned 改为按消息生效；压缩可以切分当前轮；超限时的紧急压缩与明确失败；仪表改为显示距离压缩的余量。
- **第二阶段**：大工具输出在写入时落盘（第 8 节）。
- **第三阶段**：进度展示（第 9 节），先观察 v0.5.4 的实际数据再定。

不做：
- 替换 cl100k 估算器。本设计后它只估算增量和压缩内部的单条消息，开销与误差都变得次要；换成按用量校准的字符估算留作后续。
- 解析 DeepSeek 错误文本里的数字。三家都不这么做；每次请求裁剪回复预留之后，这类错误只剩兜底意义。
- 切换到 DeepSeek 的 Anthropic 兼容端点，或使用对话中途的 system 更新（dsh 的 Flash 路线用了，公开 V4 是否支持未经证实）。

## 2. 统一上下文预算

`ContextConfig`（`crates/orca-provider/src/context.rs`）成为唯一的预算来源，触发线、每次请求的回复预留、仪表上限都从它推出，不再各算各的。

| 量 | 取值 |
|---|---|
| 窗口 W | 仍为 1,000,000（`max_context_tokens`），可由 `model_runtime.context_window` 覆盖。比真实的 1,048,576 少约 48K，三家都这样留余量。 |
| 回复预留 R | 按思考强度：Low 32,768 / High 65,536 / Max 131,072，且默认值不超过窗口的 15%（自定义的小窗口仍留得出输入空间；1M 窗口不受影响）。新增配置项 `[model_runtime] max_output_tokens` 可覆盖，取值限制在 16,384 到 384,000。 |
| 触发线 T | `min(0.8 × W, 0.9 × W − R)`，即现有公式（`soft_limit` 与 `effective_limit`），只是 R 不再写死为 4,096。已有的 `soft_compact_token_limit` 与 `auto_compact_token_limit` 覆盖方式不变。 |
| 最小回复 | 16,384：一次请求最少要留给回复的空间。 |
| 请求余量 | 8,192：每次请求在窗口内额外留出的空间，吸收估算误差。 |

每次请求前，按第 3 节测得的提示大小 P 计算本次的 `max_tokens = min(R, W − P − 8,192)`：

- 结果不小于 16,384：照常发送。
- 结果小于 16,384：先走第 6 节的紧急压缩；压缩后仍不足，本次不发请求，直接给出失败说明。

按默认值（W = 1,000,000，Max 强度 R = 131,072），T = min(800,000, 768,928) = 768,928；High 与 Low 强度下 T = 800,000，与现在的软线相同。事故中的请求（P = 664,935）得到 `max_tokens = 131,072`，合计 796,007，远低于上限，不会再被拒。

`ProviderConfig` 新增 `max_output_tokens: Option<u32>`。运行时在发请求前把算好的值写进去；`deepseek_http.rs` 的流式与非流式请求都使用它，缺省时退回 R 的默认值。摘要请求保持自己的小预算不变。

`ContextConfig.reserved_for_response` 就是 R。现有的硬线 `0.9 × W − R` 保持原有用途（压缩事件分级、摘要合并）；发送前另外检查"装不下最小回复"，即 `P > W − 16,384 − 8,192`，这时走第 6 节的紧急压缩。

## 3. 以 API 用量为锚的测量

测量值 P 用于三处：压缩触发判断、每次请求的 `max_tokens`、仪表。

```
锚点有效：P = 锚点.prompt_tokens + 锚点.output_tokens
            + 估算(锚点之后追加的消息)
            + (估算(当前内部上下文) − 锚点.内部上下文估算)
锚点无效：P = 估算(整个请求)          （即现在的 wire_equivalent_tokens）
```

- **锚点**（`UsageAnchor`）存在 `Conversation` 上，不持久化，包含：上次请求的 `prompt_tokens`（`Usage.input_tokens`，含缓存命中）与 `output_tokens`、写入助手回复后的消息条数、最后一条已计入消息的指纹、当时内部上下文的估算值。
- **何时设置**：主 agent 与子 agent 每次成功拿到带用量的回复并写入助手消息之后（`provider_turn.rs` 与 `child_agent_provider_turn.rs`）。
- **何时失效**：任何压缩提交后；消息条数少于锚点记录的条数，或那条消息的指纹对不上（回退、编辑历史）；会话恢复后（新建的 `Conversation` 本来就没有锚点）。
- **为什么加上 `output_tokens`**：助手回复会成为下一次提示的一部分（带工具调用时还要回放 reasoning）。没有工具调用时 reasoning 不回放，这样估计略偏大，方向是安全的。
- **估算器**：仍用现有的 `DefaultTokenCounter`，但只估增量，不再每次估整个 60 万 token 的历史。

## 4. pinned 按消息生效

### 4.1 规则

- `partition_for_compaction` 不再因为一轮含 pinned 消息就保留整轮。被折叠区间里的 pinned 消息逐条移到保留区，按原顺序放在摘要之后，其余内容正常摘要。
- `cache_aware_micro_compaction` 只跳过 pinned 消息本身，不再跳过整轮。
- 旧版计数回放（`history.rs` 中按 `before_messages`/`after_messages` 重建）本来就按单条消息保留 pinned，不受影响。

### 4.2 哪些消息不再 pinned

以下都是"事件"：模型看过之后，摘要可以代替原文。它们改为普通 system 消息：

- 后台任务与子 agent 的完成通知（`controller.rs` 的 `model_notification`，两处）；
- 交给子 agent 的消息与 wait 结果（`runtime_turn_loop.rs` 三处 `add_system_pinned`）；
- 预算软着陆提醒（`runtime_turn_opening.rs` 两处）。

仍然 pinned 的：用户显式 pin 的上下文（`add_pinned_context`）、plan 模式切换说明、以 `PinnedUser`/`PinnedSystem` 形式放置的轮次提示、调查收敛的最终报告提示、仅存在于请求中的 hook 与运行时指令上下文。

### 4.3 旧会话

恢复会话时，`<task-notification>` 开头的 pinned system 消息按普通消息载入，与已有的 `strip_legacy_pinned_volatile` 同处处理。事故会话恢复后即可被压缩。

## 5. 压缩可以切分当前轮

现在当前轮永远整轮保留。full-auto 或 goal 模式下，一轮就可能有几百次工具调用（事故会话第一轮有 468 条消息），这样的轮次自己就压不动。

改为：当前轮保留"开启本轮的用户消息"加上"最新的若干个工具平衡单元"，更早的部分可以进入摘要。

- **工具平衡单元**：一条助手消息加上它所有工具调用的结果；没有工具调用的助手消息自成一个单元。切分点只能落在单元之间，工具调用与结果永不拆开。
- **保留多少**：从最新往前累加，直到达到现有的保留目标（`target_compaction_limit`，48K）。
- **最新单元总是保留**，即使它单独超过目标。
- **开启本轮的用户消息始终保留原文**：它是这一轮的任务说明。
- 摘要证据按原顺序包含当前轮被折叠的部分。

## 6. 紧急压缩与超限恢复

两种情况进入紧急压缩：

- 发送前测得本次请求装不下最小回复（第 2 节）；
- API 返回上下文超限（沿用 `is_prompt_too_long_error` 识别）。

紧急压缩与普通压缩使用同一套分区（第 4、5 节），区别如下：

- 不因为"低于触发线"而跳过；
- 强制把压缩线压到"测量值 P"与"整个请求的本地估算"两者中较小者的四分之三（不高于 T），保证至少做一次真正的缩减。取较小者，是因为压缩内部按本地估算裁剪，而以 API 用量为锚的 P 可能比估算大，按 P 算出的压缩线可能落在估算之上，结果什么也压不掉；
- pinned 消息仍逐条保留。改为按消息生效后，剩下的 pinned 只有用户显式 pin 的内容、plan 模式说明和轮次提示，都很短。如果"系统提示 + pinned 消息 + 最新单元"本身就放不下，紧急压缩按下面的方式失败。

压缩后重新测量（锚点已失效，走整份估算，再按压缩前 API 计数与估算之比放大，只放大不缩小）。估算严格小于压缩前，就采用结果，替换并持久化历史；估算没有变小，就不替换历史。之后：

- 发送前：无论是否采用，只要请求仍装不下最小回复，本轮就不发请求，直接停止并给出下面的失败说明；
- API 超限：只有历史确实变小才重试一次，重试的请求同样先经过发送前的检查；历史没有变小就不重试，在服务端的错误原文之后附上 `Compaction cannot shrink the conversation further; start a new conversation with /new.`（`unrecoverable_overflow_message`）。

失败说明（`compaction.rs` 的 `context_overflow_message`；`{prompt_tokens}` 是压缩后的测量值，没有采用时是压缩前的测量值；测试检查其中的 `/new`）：

```
The conversation no longer fits the model's context window (about {prompt_tokens} tokens) and compaction cannot shrink it further. Start a new conversation with /new.
```

API 超限的重试仍是每轮一次（`RuntimeCompactionRetryState` 的 `prompt_too_long_retried`）。

## 7. 仪表

- **上限**：`SurfaceContextSnapshot.limit_tokens` 由窗口改为触发线 T（`runtime_host.rs` 初始化处；模型或配置变化时同步更新）。
- **已用**：仍为上一次请求 API 报告的 `prompt_tokens`（`next_provider_context_snapshot`）。
- 状态栏的 `ctx N%` 因此表示"距离自动压缩还剩多少"，到 0% 就会压缩，而不是请求失败。
- ACP 的 `UsageUpdate(used, limit)` 一并改为以 T 为上限。
- 旧 TUI 路径的 `TuiAgentTurnCompactionOutcome` 本来就用软线作上限，改为 T 后两条路径一致。

## 8. 第二阶段：大工具输出落盘（单独出计划）

- 事故会话里，一次 `task_list` 返回了 189,075 个字符（约 5 万 token），整段进入历史。
- 做法：工具结果在写入历史前检查大小，超过约 12,500 token 的完整写入会话目录下的文件，模型只拿到开头、结尾、路径，以及"用 read_file 的 offset/limit 或 grep 查看"的提示。dsh 的阈值是 12.5K token，Reasonix 是 32 KiB，Codewhale 是 100 KiB。
- `task_list` 另外改为默认只返回摘要字段，详情按任务 id 查询。

## 9. 第三阶段：进度展示（先观察 v0.5.4，再单独出计划）

四家在这一点上有分歧：

- **督促措辞**：dsh、Reasonix、deepcode 都在工具描述里写"做完就标完成"。Codewhale 删掉了这类措辞并用测试禁止，理由是模型会把精力花在维护清单上。
- **过期提醒**：没有一家使用"每 N 次回复提醒"。Reasonix 在模型想结束但清单还有未完成项时，补一轮提示，最多几轮，并检测停滞。
- **更新成本**：Reasonix 用 `complete_step` 签收一步，由宿主程序把下一步推进为进行中，模型不用每次重写整份清单。

待定方案：

1. 保留 v0.5.4 的提示词措辞。
2. 用"结束时检查"替换或补充"10 次回复提醒"：模型给出最终回答（没有工具调用）而计划仍有未完成步骤时，追加一条提示，要求更新计划或说明原因；每轮最多一次。
3. 给 `update_plan` 增加轻量写法：只标记某一步完成，下一步自动变为进行中。
4. 先统计 v0.5.4 真实会话里每轮 `update_plan` 的次数与额外 token，再决定取舍。

## 10. 测试（第一阶段）

- **预算**：不同思考强度、W、R 与覆盖配置下 T 与 `max_tokens` 的取值；事故数值（P = 664,935）得到可发送的请求。
- **请求体**：流式与非流式请求的 `max_tokens` 取自 `ProviderConfig.max_output_tokens`；缺省时为按思考强度的默认值。
- **测量**：有锚点时 P = 锚点 + 增量；压缩、回退、指纹不符时锚点失效，走整份估算；内部上下文变化计入差值。
- **pinned**：复现事故形状，每个历史轮次都含一条 pinned 通知，压缩必须显著缩小，pinned 消息逐条保留在摘要之后。
- **切分当前轮**：一轮 400 条消息的历史可以被压缩；工具调用与结果不被拆开；开启本轮的用户消息与最新单元保留原文。
- **紧急压缩**：模拟服务端返回超限错误，压缩后重试成功；压缩无法缩小时不重试、不替换历史，并给出失败说明。
- **旧会话**：`<task-notification>` pinned 消息恢复后按普通消息处理。
- **仪表**：上限为 T；TUI 与 ACP 两条路径一致。

## 11. 兼容性与风险

- **仪表含义改变**：`ctx N%` 从"整个窗口的剩余比例"变为"距离压缩的余量"，同样的会话数字会更低。发版说明需要写明。
- **回复预留变小**：Max 强度 128K，High 64K。极长的推理可能被截断（`finish_reason = length`），可用 `model_runtime.max_output_tokens` 调大。每次请求的裁剪保证调大后也不会触发超限。
- **通知不再 pinned**：后台任务通知在压缩后只以摘要形式存在。压缩永远不会折叠当前单元，模型总会先看到通知原文。
- **当前轮被切分**：开启本轮的用户消息始终保留，但同一轮里较早的工具输出会进入摘要，可能损失细节；只在超过触发线时发生。
- **会话文件格式不变**：锚点不持久化；旧会话中的 pinned 通知在载入时迁移。
