# Orca 交互体系重构方案：统一 PendingInteraction + 内联问答

> 目标：借鉴 grok 的 `PendingInteraction` 统一框架与 claude-code 的权限流水线复用思路，
> 收敛 Orca 当前发散的 `ask_user_question` 执行路径；同时把交互 UI 从「盖屏弹窗」
> 改为「替换输入框的内联交互文档」，并原生支持多轮问答。

## 实现状态（2026-09-11）

本文方案已在工作树完整实现：

- `ask_user_question` 与 `request_permissions` 由 `ToolSpec.capabilities` 驱动，统一进入
  `RuntimeSpecialToolDispatch::Interaction`，不再按工具名分流。
- 一次工具调用的 1-4 个问题形成一份 typed questionnaire，只进入 UI 一次、提交一次。
- runtime、surface、TUI、ACP 和 JSONL 均支持结构化 question/option/answer；旧单题
  `UserInput`、ACP v1 与 JSONL string answer 继续兼容。
- durable continuation capsule 升级到 v3，decoder 继续接受 v1/v2；旧会话不会因新载荷失效。
- TUI 问卷在 composer 槽位内联渲染，支持单选、多选、preview、自定义答案、问题翻页、
  未答确认、`Ctrl+T` Chat、取消、提交 ACK 后回显以及提交失败原状态恢复。
- transcript 在交互期间保持可见，鼠标滚轮、`PageUp/PageDown`、`Ctrl+U/Ctrl+D`
  可继续滚动。

实现保留各交互类型的 typed park/waiter，因为 permission、user input、MCP elicitation
的请求与答案契约不同；统一点位于 capability 分类、interaction dispatch、surface
lifecycle 和 durable continuation，而不是用无类型的通用 payload 抹平这些边界。

## 0. 一句话结论

Orca 的 **surface 层已经具备统一交互抽象**（`SurfaceInteractionKind` 五元枚举 + 每类的
durable 重启 capsule），收敛度甚至超过 grok/claude-code。真正的发散只剩**两处边缘**：

1. **runtime 入口**：`classify_dispatch` 把 `AskUserQuestion` 特判成 special-dispatch。
2. **TUI 渲染**：`ask_user_question` 走独立的盖屏 `UserInputDialog`，与 composer 互斥。

因此这不是「大重构」，而是**「向已有的统一抽象收敛 + 把 UI 从弹窗改内联」**。风险可控。

---

## 1. 现状盘点（已查证，附文件/行号）

### 1.1 已经统一的部分（surface 层，无需重建）

- `SurfaceInteractionKind`（`crates/orca-runtime/src/runtime_surface/interaction.rs:16-22`）：
  `ToolApproval / PermissionRequest / UserInput / McpElicitation / BackgroundApproval`——
  **五类交互已在同一枚举下**，这正是 grok `PendingKind` 的等价物。
- 每类都有 durable 重启语义（同文件 `:46-74`）：`RestartableToolApproval` /
  `RestartablePermissionRequest` / `RestartableUserInput` / `RestartableMcpElicitation`，
  底层是 `DurableInteractionContinuationCapsule`。**这是 Orca 独有、比 grok/claude-code 强的地方。**
- TUI 侧也已有对应的四元枚举 `TuiInteractionKind`（`crates/orca-tui/src/protocol.rs:68-74`）
  和统一的 `TuiInteractionKey` / `TuiInteractionResponse`（`:83-144`）。
- 统一的阻塞/回填骨架已存在：`operation_controller.rs:335-397` 的
  `await_queue_interaction` / `respond_queue_interaction`（sync_channel + key→sender map），
  以及 surface 侧的 `respond`（`:1008-1017`）。

**换言之：统一框架的「地基」已经浇好了。**

### 1.2 仍然发散/需要改的部分

| 问题 | 位置 | 现状 |
|---|---|---|
| A. runtime 入口特判 | `runtime_special.rs:119` | `ToolName::AskUserQuestion => RequestUserInput` special-dispatch，绕过普通工具执行 |
| B. 扁平化字符串耦合 | `runtime_user_input.rs:59-76` ↔ `user_input_dialog.rs:40-60` | 结构化 question/options 压成字符串，TUI 端按分隔符反解析，隐性强耦合 |
| C. 盖屏弹窗 | `ui.rs:195-198` + `user_input_dialog.rs` | `render_user_input_dialog` 居中盖屏；`composer_visible`（`ui.rs:321-336`）与 `user_input_dialog` 互斥 |
| D. 无多轮 | `runtime_user_input.rs:57` | 一次工具调用内多问题「串行阻塞」，但不支持「答一半、agent 追问、再答」的多轮对话式澄清 |

---

## 2. 重构方案 A：执行路径收敛（runtime 侧）

### 2.1 目标

去掉 `RequestUserInput` special-dispatch 分支，让 `ask_user_question` 成为一个
**声明自己需要交互的普通工具**——对齐 claude-code「`checkPermissions` 恒返回 `ask`」的思路，
但落到 Orca 的 surface 交互抽象上。

### 2.2 做法

1. **工具声明交互能力**（已有基础）：`registry.rs:1629` 已用
   `CapabilitySet::new(vec![ToolCapability::UserInputRequest])` 标记。让工具执行框架在
   看到 `UserInputRequest` 能力时，统一走 surface 的 `SurfaceInteractionKind::UserInput`
   park 流程——而不是在 `classify_dispatch` 里按**工具名**硬编码。

2. **`classify_dispatch` 去特判**：删除 `runtime_special.rs:119` 的
   `ToolName::AskUserQuestion => RequestUserInput`。改由「工具能力」驱动，
   与 `RequestPermissions`（`:118`）未来一并收敛到同一「interaction-capable tool」路径。

3. **统一 dispatch owner**：`RuntimeSpecialToolDispatch::Interaction` 承载
   `Permission` / `UserInput`，surface lifecycle 继续统一管理
   `UserInput` / `PermissionRequest` / `ToolApproval` / `McpElicitation`。具体 park/waiter
   保持 typed，避免把不同答案契约降级成 `Value` 或字符串。

### 2.3 收益

- runtime 不再按工具名分叉；新增交互类型只需实现「声明能力 + 提供 capsule」。
- 与 grok 的「一套 pending 抽象承载四类」对齐，但保留 Orca 的 durable capsule 优势。

---

## 3. 重构方案 B：传输结构化（去扁平化）

### 3.1 目标

消除 `runtime_user_input.rs` ↔ `user_input_dialog.rs` 之间的字符串分隔符耦合
（问题 B）——对齐 codex（JSON-RPC 结构化）/grok（ACP ext_method 结构化）。

### 3.2 做法

1. 定义结构化 surface 载荷（放 `runtime_surface/interaction.rs`）：

   ```rust
   pub struct SurfaceUserInputQuestion {
       pub id: NonEmptyText,
       pub header: NonEmptyText,          // ask_user_question 层校验 ≤12 chars
       pub question: NonEmptyText,
       pub options: Vec<SurfaceUserInputOption>,
       pub multi_select: bool,
   }
   pub struct SurfaceUserInputOption {
       pub label: NonEmptyText,
       pub description: DisplayText,
       pub preview: Option<DisplayText>,
   }
   pub struct SurfaceUserInputQuestionnaire {
       pub questions: NonEmptyVec<SurfaceUserInputQuestion>, // ask_user_question 层校验 1..=4
   }
   ```

2. 新请求使用 `SurfaceInteractionRequest::UserQuestionnaire`。旧
   `SurfaceInteractionRequest::UserInput { question, suggestions }` 保留为兼容分支，
   避免破坏旧 ledger/capsule/ACP 客户端。正式 runtime→TUI 路径不再拼接或反解析
   `MULTI_SELECT_HINT`、`" - "`、`"\nPreview:\n"`。

3. TUI 的 `UserInputDialog::new` 直接消费结构化数据，不再反解析。

4. 通用 surface question 的 `options` 使用 `Vec`，允许旧自由文本请求没有选项；
   `ask_user_question` 自身仍严格校验每题 2-4 个选项。

### 3.3 收益

- 删掉隐性耦合常量 `MULTI_SELECT_HINT`、`" - "`、`"\nPreview:\n"`。
- multi_select / preview 成为一等字段，杜绝「一边改常量另一边静默错乱」。

---

## 4. 重构方案 C：UI 从「盖屏弹窗」改「内联交互文档」（核心 UX）

### 4.1 现状问题

`render_user_input_dialog`（`ui.rs:346-444`）是居中盖屏 modal；`composer_visible`
（`ui.rs:321-336`）在 `user_input_dialog.is_some()` 时返回 false，即**问答期间输入框消失、
对话被弹窗遮挡**。这与主流 agent（codex/grok/claude-code）都不一致——它们都是
**在输入框位置就地渲染交互组件**，transcript 完整可见、可上下滚动回看上下文。

### 4.2 目标形态（参考三家 + 你的偏好）

已定的两处样式决策：
- **边框**：复用 composer 的圆角边框——交互区与输入框是同一个视觉容器，内容在
  「选项列表 ↔ 文本输入」间切换，无边框跳变。
- **preview**：按需并排——仅当焦点选项带 `preview` 字段才展开右栏，否则单栏满宽。

总原则：交互区**占用 composer 的槽位（底部输入区）**，transcript 始终在上方完整可见、
可滚动回看，不盖屏、不居中。

#### 状态 1 · 单选（最常见）

```
  You    先把 special-dispatch 收敛了
  Orca   动手前我要确认几件事，避免改错方向。      ← transcript 完整可见、可滚动

╭──────────────────────────────────────────────────────────────────────╮  ← 与 composer 同款圆角边框
│ ◆ 架构方向                                              问题 1/3        │
│ 你希望优先收敛哪条路径？                                                │
│                                                                        │
│  › 1  收敛 special-dispatch (Recommended)   统一到 interaction 能力     │
│    2  先做结构化传输                          先去扁平化耦合，风险最低    │
│    3  一起做                                  两个 PR 合并推进          │
│    z  Type a custom answer                                             │
│                                                                        │
│ ↑↓ select · 1-3 pick · Enter next · Tab notes · Esc cancel             │
╰──────────────────────────────────────────────────────────────────────╯
```

#### 状态 2 · 多选（`multiSelect: true`）

```
╭──────────────────────────────────────────────────────────────────────╮
│ ◆ 发布门禁                                              问题 2/3        │
│ 这次 patch 前要跑哪些校验？（可多选）                                   │
│                                                                        │
│    [x] 1  contracts            契约测试                                 │
│    [x] 2  clippy               lint                                     │
│  › [ ] 3  站点构建             site build                               │
│    z  Type a custom answer                                             │
│                                                                        │
│ ↑↓ select · Space toggle · Enter submit · Tab notes · Esc cancel       │
╰──────────────────────────────────────────────────────────────────────╯
```

#### 状态 3 · 带 preview（按需并排，仅单选）

```
╭──────────────────────────────────────────────────────────────────────╮
│ ◆ 输入区抽象                                            问题 3/3        │
│ InputRegion 该怎么建模？                                                │
│                                     ╭─ preview ────────────────────╮   │
│  › 1  enum 双态 (Recommended)       │ enum InputRegion {           │   │  ← 焦点项有 preview → 右并排
│    2  trait 对象                    │   Composer,                  │   │
│    3  Option<InteractionView>       │   Interaction(View),         │   │
│    z  Type a custom answer          │ }                            │   │
│                                     ╰──────────────────────────────╯   │
│ ↑↓ select · Enter submit · Tab notes · Esc cancel                      │
╰──────────────────────────────────────────────────────────────────────╯

  ↑ 若焦点切到「2 trait 对象」(无 preview)，右栏自动收起，恢复单栏满宽
```

#### 状态 4 · 自定义答案 / 转对话（焦点在 `z`，或直接打字）

```
╭──────────────────────────────────────────────────────────────────────╮
│ ◆ 架构方向                                              问题 1/3        │
│ 你希望优先收敛哪条路径？                                                │
│                                                                        │
│    1  收敛 special-dispatch (Recommended)                              │
│    2  先做结构化传输                                                    │
│  › z  我想先聊聊，这几个方案的回滚成本分别是▍                           │  ← 就地变输入行，▍光标
│                                                                        │
│ Enter send as reply · ↑↓ back to options · Esc cancel                  │  ← 转对话：作为新一轮消息
╰──────────────────────────────────────────────────────────────────────╯
```

#### 状态 5 · 多问题翻页 · 顶部导航栏

多问题模式在交互区顶部增加一条**导航栏**：逐题展示 header chip + 已答状态，末尾 `⏎Submit`。
当前题高亮 `▸`、已答 `✓`、未答 `○`；翻页时**已答选择全程保留**。

```
╭──────────────────────────────────────────────────────────────────────╮
│ ‹ ▸架构方向   ✓发布门禁   ○输入区抽象   ⏎Submit ›        ← 顶部导航栏   │
│ ────────────────────────────────────────────────────────────────────  │
│ 问题 1/3 · 架构方向                                                     │
│ 你希望优先收敛哪条路径？                                                │
│                                                                        │
│  › 1  收敛 special-dispatch (Recommended)   统一到 interaction 能力     │
│    2  先做结构化传输                          先去扁平化耦合             │
│    3  一起做                                                            │
│    z  Type a custom answer                                             │
│                                                                        │
│ ↑↓ select · Tab/→ next question · Enter next · Esc cancel              │
╰──────────────────────────────────────────────────────────────────────╯

  图例：▸当前题(高亮)  ✓已答  ○未答  ‹ › 表示可左右翻
```

翻页态 B · 第 2 题（已答的第 1 题显示 ✓，选择被保留）：

```
╭──────────────────────────────────────────────────────────────────────╮
│   ✓架构方向   ▸发布门禁   ○输入区抽象   ⏎Submit              2/3        │
│ ────────────────────────────────────────────────────────────────────  │
│ 这次 patch 前要跑哪些校验？（可多选）                                   │
│    [x] 1  contracts                                                    │
│  › [ ] 2  clippy                                                       │
│    [ ] 3  站点构建                                                      │
│    z  Type a custom answer                                             │
│                                                                        │
│ ↑↓ select · Space toggle · ←/→ switch question · Esc cancel            │
╰──────────────────────────────────────────────────────────────────────╯

  ↑ ✓架构方向：第 1 题已选，翻回去仍是「收敛 special-dispatch」，不丢
```

翻页态 C · 末题（此题答完 Submit 可用，Enter=提交全部）：

```
╭──────────────────────────────────────────────────────────────────────╮
│   ✓架构方向   ✓发布门禁   ▸输入区抽象   ⏎Submit              3/3        │
│ ────────────────────────────────────────────────────────────────────  │
│ InputRegion 该怎么建模？                                                │
│  › 1  enum 双态 (Recommended)                                          │
│    2  trait 对象                                                        │
│    z  Type a custom answer                                             │
│                                                                        │
│ ↑↓ select · ← prev · Enter submit all · Esc cancel                     │
╰──────────────────────────────────────────────────────────────────────╯
```

翻页态 D · 有未答就提交 → 就地确认（借鉴 codex 的未答确认）：

```
╭──────────────────────────────────────────────────────────────────────╮
│   ✓架构方向   ○发布门禁   ✓输入区抽象   ▸Submit                        │
│ ────────────────────────────────────────────────────────────────────  │
│ 还有 1 题未回答（发布门禁）。                                           │
│                                                                        │
│  › 回去补答          跳到「发布门禁」                                   │
│    仍然提交          未答项留空提交                                     │
│                                                                        │
│ ↑↓ select · Enter confirm · Esc back                                   │
╰──────────────────────────────────────────────────────────────────────╯
```

#### 状态 6 · 答完 → 回显进 transcript，输入区变回 composer

```
  Orca   动手前我要确认几件事。
  ✓ 你的回答
     · 架构方向        → 收敛 special-dispatch
     · 发布门禁        → contracts, clippy
     · 输入区抽象      → enum 双态
  Orca   好，按「收敛 special-dispatch」开始，先出 PR-1…

╭──────────────────────────────────────────────────────────────────────╮  ← 同一个槽位，边框不变，内容变回 composer
│ ▍                                                                      │
│ ⏎ send · @ files · / commands                                         │
╰──────────────────────────────────────────────────────────────────────╯
```

#### 交互流一览

| 键 | 行为 |
|---|---|
| `↑↓` / `j k` | 移动焦点 |
| `1-9` | 数字直选 |
| `Space` | 多选勾选（仅 multiSelect） |
| `Enter` | 单选=选中并翻到下一题 / 末题=提交全部；多选=确认本题 |
| `Tab` / `→` | 下一题（末题跳 Submit） |
| `Shift+Tab` / `←` | 上一题（首题无效） |
| 直接打字 / `z` | 转入当前题的自定义答案 |
| `Ctrl+T` | 转入 `Chat about this`，把自由文本交给 agent 继续澄清 |
| `PageUp/PageDown` | 滚动 transcript |
| `Ctrl+U/Ctrl+D` | 半页滚动 transcript |
| `Esc` | 文本/确认态返回选项；选项态取消并提交 typed `Cancelled` |

要点：
- **占用 composer 的布局槽位，不盖 transcript**。让 `composer_visible` 与交互组件共享
  「输入区」这一个区域：输入区渲染成一个 `InputRegion` 枚举——
  `Hidden | Composer | Interaction`。
- transcript 始终可见，可用鼠标滚轮、`PageUp/PageDown` 或 `Ctrl+U/Ctrl+D` 回看
  agent 为什么问。
- 输入区高度自适应（多选项/带 preview 时自动增高），沿用
  `composer_input_height`（`ui.rs:3153`）的自适应机制。
- 单选 `Enter` 自动翻页（少一次操作）；多选不自动翻（需多次 Space）。
- 顶部导航栏用 header chip（≤12 字符字段），`✓/○` 双态让「还剩几题」一目了然。

### 4.3 做法

1. **引入输入区抽象**：新增 `enum InputRegion { Composer, Interaction(InteractionView) }`
   作为「输入区当前渲染什么」的单一真相。`composer_visible` / `render_user_input_dialog`
   /`render_composer` 收敛为 `render_input_region`。

2. **删除盖屏分支**：移除 `ui.rs:195-198` 的居中 `render_user_input_dialog` 调用；
   改为在 composer 的 layout 槽位内 `render_interaction_view`。

3. **保留 `UserInputDialog` 的状态机**（`user_input_dialog.rs` 的导航/多选/自定义答案逻辑
   已经完善，`handle_user_input_dialog_key:139-217`），只改**渲染位置**与**可见性规则**，
   不重写交互逻辑。风险最小。

4. **状态耦合修正**：`main_composer_hardware_cursor_visible`（`ui.rs:325-331`）、
   `search_visible`（`:333-343`）等对 `user_input_dialog.is_none()` 的判断，改为统一查询
   `InputRegion`。

### 4.4 preview 布局

带 `preview` 的选项，采用 codex/grok 的**左右并排**：左侧选项列表、右侧 preview 框
（复用现有 `image_preview` 的 split 布局思路，`ui.rs:170`）。仅单选支持 preview 并排。

---

## 5. 重构方案 D：多轮问答（对话式澄清）

### 5.1 现状

`ask_user_question` 一次调用内多问题是「串行阻塞」（`runtime_user_input.rs:57`），
但**没有**「用户答一部分 → agent 根据答案追问 → 再答」的多轮能力。codex 的
`request_user_input_async`（非阻塞、回复作为新消息）和 grok plan-mode 的
`ChatAboutThis`（部分回答后让 agent 重组问题）都提供了这种能力。

### 5.2 目标

支持两种多轮模式：

- **模式 1：同工具内翻页多轮**（已有基础）——多问题 Tab 前后翻页，最后一题提交。
  当前已支持，内联化后体验更好（不盖屏）。

- **模式 2：跨轮对话式澄清**（新增）——参考 grok `ChatAboutThis`：内联交互区提供
  「Chat instead」动作，用户可放弃选项、直接在输入框打字回复；agent 收到后可再次
  `ask_user_question` 追问。实现上：
  - `Ctrl+T` 进入 `Chat about this`，避免与当前题的自定义答案混淆。
  - 提交后产生 typed `SurfaceUserInputDecision::Chat`；resident waiter 将其返回为
    `RuntimeUserInputResponse::Chat`，模型收到 `{"answers":{},"chat":"..."}` 后可继续说明
    或再次调用 `ask_user_question`。
  - cold recovery 将 Chat 文本按 durable continuation 注入后继 turn，不依赖原进程 waiter。

### 5.3 收益

- 澄清从「一问一答的表单」升级为「可来回的对话」，匹配你「AI native + 多轮」的偏好。
- 不新造机制，复用已有 durable continuation。

---

## 6. 统一收益（对齐三家 + Orca 独有优势）

| 维度 | 重构后 Orca | grok | claude-code | codex |
|---|---|---|---|---|
| 统一交互抽象 | `SurfaceInteractionKind` 五类 + 能力驱动 | `PendingKind` 四类 | 权限流水线复用 | ElicitationService |
| 传输 | 结构化 surface 载荷 | ACP ext_method | 进程内+多端竞速 | JSON-RPC |
| UI | 内联替换输入框 | 全屏 overlay | Ink 内联 | bottom-pane overlay |
| 多轮 | 翻页 + 转对话 | plan ChatAboutThis | Tab 多问 | async 变体 |
| **持久化恢复** | **durable capsule（最强）** | park+replay（内存） | 队列（无） | watch 暂停（无跨进程） |

---

## 7. 实施记录（逻辑上可独立发布）

> 遵循「每个独立 feature/重构单独一个 patch 包」的节奏，拆成 4 个逻辑 commit/PR：

1. **传输结构化（方案 B，已完成）**：新增结构化 surface 载荷，正式路径去掉扁平化
   拼接/反解析，并保留旧协议兼容。

2. **UI 内联化（方案 C，已完成）**：引入 `InputRegion`，把盖屏弹窗改为就地替换
   composer，并完成宽屏、窄屏及真实 TUI 验证。

3. **执行收敛（方案 A，已完成）**：`classify_dispatch` 去掉
   `AskUserQuestion` / `RequestPermissions` 工具名特判，改为 capability 驱动的统一
   interaction dispatch。

4. **多轮对话（方案 D，已完成）**：新增 typed Chat 决策，复用 durable continuation，
   支持 agent 根据自由文本再次追问。

每个 PR 前置门禁：contracts、clippy、站点构建、npm/release 校验（沿用发布门禁）。

---

## 8. 风险与注意

- **capability dispatch**：permission 与 user input 已一并纳入，避免工具名分流不对称。
- **durable capsule 兼容**：当前编码版本为 v3，decoder 接受 v1/v2/v3；旧单题 intent
  仍接收 legacy `Answer`，新 questionnaire 接收 `Submitted` / `Chat` / `Answer` / `Cancel`。
- **客户端兼容**：ACP/JSONL 同时输出旧 `question`/`choices` 与新 `questionnaire`/`questions`；
  新 JSONL 客户端通过 `answers: [{ questionId, answers }]` 提交结构化答案，也可通过
  `chat` 提交澄清消息；旧客户端的字符串 `answer` 继续有效，三种响应形态互斥。
  `answers` 中的 ID 必须属于原问卷且不可重复，用户确认跳过的未答题不出现在数组中。
- **提交一致性**：TUI 只在 `InteractionResponseAck::Committed` 后回显答案；提交失败时
  恢复完整问卷、当前页、勾选、自定义文本、原 composer 草稿、mentions 和 pastes。
- **无头降级不变**：没有 runtime user-input handler 时继续确定性 fail closed，不等待输入。

## 9. 验证

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo clippy -p orca-runtime -p orca-tui -p orca-tools --lib --locked`（通过；仓库既有
  lint 仍使全仓 `-D warnings` 不可用）
- runtime questionnaire、capability dispatch、surface capsule、cold recovery、ACP/JSONL
  聚焦测试
- TUI 问卷状态机、ACK/恢复、宽屏/窄屏 TestBackend 渲染测试
- `runtime_lifecycle_contract`
- 隔离 `ORCA_HOME` 的真实 mock TUI：问卷内联、Chat 切换、ACK 回显和 composer 恢复
