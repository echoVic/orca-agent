# Orca TUI Session Recap

> 状态：已实现，2026-09-24 合入 main。以下第 0–7 节是 2026-09-22 的方案原稿（基线 `025d814c`）；合入时按 main 的新设计体系做的调整和 review 修复见第 8 节。
> 目标：在现有 TUI 视觉统一方案上增加一个可取消、不会污染 transcript 的短会话 recap。

## 0. 基线和边界

实现基线是（原先审计的 dirty 视觉改动随后已由该 worktree 提交）：

```
/Users/qingyun/Documents/GitHub/blade-deepseek/.claude/worktrees/tui-visual-unification
branch: worktree-tui-visual-unification
HEAD:   025d814c fix(tui): fix slash-menu hit test, stale render cache, and add panel padding
parent: 5d9dc27c
tree:   source clean; only this spec and its implementation plan are untracked
```

`025d814c` 包含从父提交 `5d9dc27c` 观察到的 9 个视觉统一文件（相对父提交为 589 additions / 166 deletions）：`crates/orca-tui/src/chrome.rs`、`golden/approval.txt`、`golden/help.txt`、`input_event_actions.rs`、`theme.rs`、`transcript_state_tests.rs`、`transcript_view.rs`、`ui.rs`、`vim.rs`。recap 的实现要在这个已提交的视觉基线上增量接入，并保留当前两个未跟踪方案文档。该视觉改动已经验证了四类会直接影响 recap 的约束：

- `chrome::panel_block` 有左右一格内边距，面板内容宽度按外框宽度减 4 计算；
- slash popup 的绘制和鼠标命中共用含 hint 行的几何结果；
- `display_text::truncate_to_display_width` 可按 grapheme 和显示宽度截断，`ui::wrap_text` 不足以承担 recap 的长中文或超长 token；
- transcript cache 会根据前驱消息变化失效，recap 不能变成 `ChatMessage`，否则会进入 revision、scrollback、行间距和角色 gutter 逻辑。

现有视觉统一设计与计划仍是基础：

- [视觉设计](./2026-09-20-tui-visual-interaction-redesign-design.md) 继续约束 token、主题、scrollback 和输入优先级；
- [视觉统一计划](../plans/2026-09-21-tui-visual-unification.md) 的共享 chrome、composer、activity 和窄窗测试必须保持通过。

## 1. 用户可见行为

### 1.1 短 recap 条

recap 是一条临时的会话进展摘要，不是一条对话消息。生成完成后，在 transcript 与 composer 之间的 activity chrome 中显示 1–3 个视觉行，最多占 activity 区可让出的高度。示例：

```text
↳ Recap  已完成 3 轮：实现 TUI 视觉统一；鲸鱼欢迎页已接入；仍有 1 个 PR 待处理     /recap 查看
```

它使用 `Theme`、`chrome::hint_line` 和统一圆角 panel 的颜色语言；不写入 terminal scrollback，不移动 transcript viewport，不改变 composer 高度和 status 行。

空间不足时顺序固定为：先隐藏 recap，再减少 activity 背景任务行，最后才按现有 `main_layout` 规则处理 transcript；composer、队列、搜索和 status 永远保留。不能把 400 字摘要直接交给 `render_activity` 的 `take(visible_rows)`，因为那会静默截断且无法处理双宽字符。

### 1.2 `/recap` 命令

第一版命令为严格无参数的 `/recap`：

- `Idle` 且已有有效快照：复用缓存，或启动一次手动请求；
- `Running`、`Compacting`、审批、计划确认、问卷、搜索、子会话聚焦时：保持现有输入语义，不把 `/recap` 当成普通 prompt；命令在可安全处理时排队到下一个空闲点，否则显示一条短的 composer 错误提示；
- 没有会话、历史禁用且没有当前 surface、快照读取失败：显示可读错误，不弹 API key/setup；
- 成功后打开短详情面板，详情内容仍来自同一个 recap 结果，不重复调用模型；
- `Esc`、点击外部、现有全局取消优先级保持不变。recap 详情不抢审批、问卷、child focus 或搜索的 Esc。

详情面板不是默认路径，只是手动查看完整短摘要的入口。它遵循 `chrome::panel_block`、`chrome::dialog_rect` 和实际绘制矩形驱动 hit-test 的规则；外框有左右 padding 时，正文宽度始终是 `outer - 4`。

### 1.3 自动触发（第二阶段）

自动 recap 放在手动路径稳定后实现。它只在 terminal 从 unfocused 回到 focused、当前 session 没有运行或 attention 任务、且上次完成后满足下列条件时后台生成：

- 至少 3 个已完成的用户操作；
- 距离最后一个已完成用户操作至少 3 分钟；
- 同一 session/attachment 还没有为同一内容标记生成过 recap；
- 当前不是非交互模式，也没有待处理审批、计划或用户输入。

自动触发不新增定时器：沿用 renderer 现有 focus 事件和 16ms 有界轮询；focus 回来时检查条件即可。自动失败保持安静，手动 `/recap` 才显示失败原因。自动摘要连续触发需要跨过新的完成操作和新的焦点周期，不能由每个 `TurnStarted` 或每次 frame 触发。

## 2. 摘要来源和运行时合约

### 2.1 权威来源

recap 的输入由 runtime surface 快照构成，TUI 不从 `AppState.transcript` 拼接自然语言，也不把渲染后的截断行反送给模型。读取入口是 `crates/orca-tui/src/surface_actions.rs` 的 `TuiSurfaceActions::read_snapshot`，权威类型是 `orca-runtime/src/runtime_surface/projection.rs` 的 `SurfaceSnapshot`。

摘要证据按以下顺序构造，并在构造阶段完成字节和 token 上限：

1. 最近的 `SurfaceItem::UserMessage` 与 `AssistantMessage`，保留每个已完成用户操作的首句和结论句；
2. `AssistantPlan`、终态 tool 状态和未解决的 interaction/goal/plan；工具名/目标从同一快照的 `SurfaceSnapshot::tools` 按 tool-call id 安全 join，不能假定 `SurfaceItem::ToolResultMessage` 自带名称；
3. 必要时加入 `SurfaceThreadSnapshot` 的标题和 session 元数据；
4. 使用 `SurfaceUsageSnapshot` 只作为“上下文/额度是否异常”的事实，不把 usage 数字伪装成摘要内容。

排除 reasoning 原文、原始 tool 输出、图片/base64、系统指令、凭据、路径之外的环境秘密、仍在 streaming 的 chunk 和当前未结束 operation。工具输出只保留工具名、目标的安全显示片段和 terminal 状态；目标字符串仍需走现有安全显示/截断函数。

### 2.2 只读模型请求

现有 `orca-provider/src/context.rs` 的 `request_summary` 已具备摘要模型、`tools_override: Some(Vec::new())`、缓存、取消和 usage telemetry 的基础，但它目前是 compaction 内部函数。实现时抽出一个显示摘要专用的纯请求入口，复用其 provider 配置和安全提示语，不复用 `compact_with_summary`、`manual_compact` 或 `OperationKind::ManualCompaction`：

- 不改变 conversation、rolling summary、context revision 或 surface ledger；
- 不启动 tool call，不改变 approval、queue、goal 或 workflow；
- 使用当前 runtime 已解析的 provider、api key、base URL 和 reasoning 设置；默认摘要模型沿用 `orca_core::model::auxiliary_model()`；
- DeepSeek 的 summary cache 可以复用，但 cache key 必须包含 provider、base URL/模型指纹、摘要目的和证据 digest；
- 请求有独立 `CancelToken`、输入上限 4,096 tokens、输出上限 120 tokens、软超时 8 秒；必须实现真正的 typed deadline/timeout，不能把 `call_streaming_async_bounded` 返回的 partial assistant 文本当作成功；超时和取消都丢弃结果，不写入 runtime；
- 真实 provider usage 单独记录为 recap telemetry，不能增加主 operation 的 usage 或 context 百分比。

建议的跨层值对象：

```rust
pub struct RecapRequest {
    pub request_id: RecapRequestId,
    pub source: RecapSourceFence, // SurfaceCursor + RecapContentMarker; no TUI type dependency
    pub evidence: RecapEvidence,
    pub trigger: RecapTrigger, // Manual | FocusReturn
}

pub enum RecapResult {
    Ready { request_id: RecapRequestId, source: RecapSourceFence, text: String, usage: RecapUsage },
    Skipped { request_id: RecapRequestId, reason: RecapSkipReason },
    Failed { request_id: RecapRequestId, source: RecapSourceFence, error: SafeDiagnosticText },
}
```

值对象应放在 runtime/provider 能共享的窄模块，`SessionAttachmentId` 只存在 TUI 事件和 reducer 中。TUI 只接收 ready/failed/skipped 事件；不要把 provider response 或 `SurfaceSnapshot` 直接塞进 `TuiEvent`。

### 2.3 代际、缓存和取消

每个 TUI 请求都携带 `SessionAttachmentId`，worker 的跨层值对象携带 surface 的 thread/incarnation 和一个只由可见内容组成的 `RecapContentMarker`。marker 至少包含：快照 cursor 的 thread/incarnation、最新已完成用户操作的 surface item/operation 标识、完成状态和证据 digest。不能用完整 `SurfaceCursor` 把 usage 或无关 settings 刷新当成内容变化，也不能只比较 `last_completed_at`。

TUI 侧保存 `RecapState`：`Hidden`、`Pending`、`Ready`、`Failed`，并记录 request id、marker、显示文本和详情是否打开。以下事件立即取消/使结果失效：新 turn 开始、session/child attachment 切换、projection reset、backtrack、compact、当前内容 marker 变化。事件回到 renderer 时先经过现有 attachment routing；代际或 marker 不匹配的 ready/failed 静默丢弃，不能把旧摘要画到新会话。

同一 session、marker、provider 指纹和摘要触发条件只有一个 inflight 请求。manual 命中同一有效 cache 时直接展示；manual 失败允许下一次显式 `/recap` 重试，auto 失败不重试直到新的完成操作或新的 focus 周期。worker 生命周期由 runtime host 的专用 supervisor 管理；provider/runtime 仍拥有 actor 内的证据构造和摘要请求边界，hosted controller 只桥接 typed result，renderer 只接收结果。具体接入沿用 `runtime_host.rs` 的 `ThreadCommand`/`ThreadActor` 路径，增加 `RequestRecap`/`CancelRecap`，并在 `drain_closed_thread_commands` 中处理关闭时的 reply。

## 3. TUI 接入视觉统一

### 3.1 布局

当前 `ui.rs::render` 已将 `goal/plan/activity/queue/search/input` 纳入 `main_layout`，`render_activity` 也已有稳定测试。新增 recap 不扩展 `activity_lines` 的公开形状，而是在计算 `desired_activity_height` 时独立计算：

```text
recap_lines = recap_view::short_lines(state.recap, available_width, theme)
activity_height = existing_activity_rows + recap_lines.len()
```

推荐新增 `crates/orca-tui/src/recap_view.rs`：

- `short_lines` 使用 `display_text::truncate_to_display_width` 和明确的 visual-row 上限；
- `detail_lines` 做 Unicode grapheme-safe 换行，不复用当前只按空白、`/`、`-` 分词的 `ui::wrap_text`；
- `recap_strip_rect`、`recap_detail_rect` 是唯一布局来源，渲染和鼠标/键盘 hit-test 共用；
- `panel_block(...).inner(rect)` 之后再计算正文宽度，适配 dirty 分支的左右 padding；
- 为空或空间小于 1 行时返回空，不绘制空框。

短条不注册输入焦点。点击短条只打开详情，任意普通输入仍进 composer；已有 slash/mention popup、审批、计划、问卷和 child focus 优先于 recap 详情。

### 3.2 视觉层级

- strip 的标题使用 `Recap`/`↳`，正文使用 `theme.text`，时间或 `/recap 查看` 使用 `theme.muted_style()`；
- 只使用 `Theme` 和 `chrome` token，不在新文件或 `ui.rs` 增加裸 `Color::`；
- 详情面板沿用 rounded border、`›` 选中标记和 hint grammar；
- recap 不进入 `ChatMessage`、`TranscriptRenderCache`、scrollback 和搜索索引；
- composer 的上下横线、input height、queue/search 状态和 status 行保持现有 dirty 方案不变。

## 4. 命令和输入优先级

`/recap` 在 `commands/mod.rs` 作为严格无参数 builtin 注册，并在 `idle_submit_actions.rs` 解析。它不是 `UserAction::Submit`，不会进入 prompt queue，也不会调用 `state.push_message`。

命令处理顺序保持现状：先处理 focus、全局取消、child Esc、approval/plan/config/user-input/search，再处理 idle command。详情 overlay 只有在这些 modal gate 关闭后才能响应 Esc；短条不吞 Esc。`input_event_actions.rs` 的 popup 几何修复必须继续由渲染结果驱动，recap 不能另算一套行号。

## 5. 配置和失败策略

第一版不增加必须填写的配置项，不在 recap 失败时跳 setup，也不把 recap 的 provider key 写入新的文件。复用启动时已解析的 `RunConfig`；如果当前 provider 没有可用 key/base URL，manual 结果显示“未配置可用模型，无法生成 recap”，并给出 `/config` 入口，不能重新质询用户已经配置好的本机 key。

自动 recap 默认开启，但应沿用现有 `terminal_notifications`/interactive 判定，不与通知开关混为一谈。若产品需要持久开关，增加 `session_recap` 的 `auto`/`off` 两态并通过现有 FileConfig/RunConfig 解析；默认行为必须兼容缺失字段，保存配置时保留未知字段和无关项。

失败分类只在 telemetry/debug 日志记录 provider 细节：

- `manual`: 在详情区域显示短错误，可再次 `/recap`；
- `auto`: 清除 pending，保留旧的 ready recap（如果有），不插入 transcript；
- stale/cancelled: 静默丢弃；
- provider 返回 tool call 或超出边界：按失败处理，绝不执行工具。

## 6. 非目标

- 不替代 compaction，不修改上下文、不删除或重排历史；
- 不把摘要写入 `SessionTranscript`、JSONL/ACP 协议或 runtime ledger；
- 不在 renderer 线程同步调用网络，不因为 recap 阻塞主 turn、审批、队列或 child task；
- 不先做长历史浏览器或全屏 recap 页面；
- 不重写现有视觉统一 dirty 改动，不恢复被其修复的 popup、窄窗、前驱 cache 行为；
- 不把“模型调用成功”说成“摘要一定准确”，所有展示都带当前 session/内容代际校验。

## 7. 验收标准

### 行为

1. `/recap` 在 Idle 只读当前 session，成功后短条和详情使用同一个结果，第二次不重复请求。
2. Running、审批、计划、问卷、child focus 和搜索的键盘语义与现有测试完全一致。
3. 新 turn、切换 session、projection reset、compact 或 backtrack 后，旧结果不可见且旧请求结果被丢弃。
4. provider 错误、超时、取消、缺 key、tool call 和超长输出都不会写 transcript 或改变 context/usage 主状态。
5. focus 回归自动条件只计算已完成用户操作，至少 3 轮且离开 3 分钟；同一 marker 不重复生成。

### 视觉和布局

1. 宽、中、窄终端均不会越过 composer/status；空间不足时 recap 先消失。
2. 中英文、emoji、CJK、连续无空格 token 的短条和详情都按显示宽度截断/换行，不出现半个 grapheme。
3. 详情外框的正文宽度为 `outer - 4`；绘制、鼠标命中和 Esc 关闭使用同一矩形。
4. recap 不产生 scrollback 行、transcript revision、搜索命中、角色 gutter 或多余滚动。

### 成本和回归

1. 单个 marker 最多一个 inflight 请求；输入、输出、deadline 和缓存命中可在测试中观察。
2. 现有 `cargo test -p orca-tui --lib -- --test-threads=1`、`cargo fmt --all`、`cargo clippy -p orca-tui --all-targets -- -D warnings` 不因 recap 回归。

## 8. 实现记录（合入 main）

合入时 main 已有 agents dock 的点击目标（`activity_rows` 每行带 `AgentHitTarget`）和 hanging indent，recap 按这些约定重做了 TUI 部分，并修了 review 发现的问题：

- **布局只有一个几何来源**：recap 行接在 activity 行之后，由 `ui::render` 用同一组计数画出并登记 `recap_strip_area`；recap 只拿 activity 行用剩的行数（`recap_view::strip_lines` 的 `max_rows`），所以矮终端先丢 recap、不挤 agent 行，点击区域也不会盖住 agent 行。原实现里点击矩形按 recap 自身行数算，空间不足时会压到 dock 行上。
- **视觉**：短条首行 ` ↳ Recap  `（accent）加正文（`theme.text`），续行悬挂在正文列下；放不下时末行以 `…` 收尾并带 `· /recap view` 提示，失败带 `· /recap retry`。详情面板标题 `Recap`，正文按显示宽度逐 grapheme 换行（`display_text::wrap_to_display_width`，CJK 字符之间可断），底部 `Esc close`。transcript 的 ratatui 兼容换行器会让 CJK 串在行尾溢出一列，所以没有复用它。
- **键位**：去掉了原实现的 `Ctrl+R` 开关——它在 composer 和 vim normal 模式里都是 redo。`Esc` 关详情放在 slash/mention 弹层和 agents 面板之后；`/recap` 在已有 recap 时直接打开详情、不再请求。
- **状态**：新增 `Requested`（`/recap` 已发出、controller 还没回 Pending）和 `Notice`（运行中或没有可总结内容时的一行说明），都不进 transcript。手动 recap 成功后自动打开详情；自动的只显示短条。另一 attachment 的 recap（side conversation、聚焦的子 agent、换过的 session）不绘制也不可点击。
- **自动触发条件**补上了：后台 agent/任务仍在运行或待审批时不触发；已有 recap 显示时不触发；side conversation 按是否可见（不是是否存在）判断。
- **取消**：`CancelRecap` 以前没有任何发送方。现在 controller 在新 turn、换 session、compact、backtrack、切换 side/子 agent 焦点等动作前撤回在途 recap；runtime 取消时不再在 actor 线程上 `join` 等待网络中的 worker，并立刻释放它的内容 key。
- **runtime**：证据裁剪从逐个后缀重拼重算 token 的 O(n²) 改为从最新一节往前累计、每节只计一次；工具目标截到 120 字符；去重 key 改为内容 marker + provider 指纹 + 触发方式，不再含 cursor 序号。
- **provider**：提示词要求两句以内、用用户的语言、纯文本；命中输出上限的回复保留已有内容并以 `…` 结尾，不再整体判为失败；显示文本按字符数截断，不再插入 `[summary truncated]`；错误文案改成可直接显示的原因。
- 消融实验脚手架（`scripts/recap-ablation/`、各 crate 里的 `include!` 探针）没有合入，见 [消融报告](../../reports/2026-09-22-session-recap-ablation.md)。

