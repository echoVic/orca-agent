# Orca TUI 视觉与交互重设计

> 状态：已评审（2026-09-21，决策见第 9 节），待出实现计划。日期：2026-09-20。
> 范围：`crates/orca-tui`。不改 runtime 协议、ACP/JSONL 输出、渲染缓存与 scrollback 刷入架构。

## 0. 一句话结论

TUI 功能完备，但"生硬"来自三件事：**没有统一的视觉语言**（边框、标记、颜色、提示行各写各的）、
**transcript 是一面没有节奏的文字墙**（无角色标记、thinking 抢眼、工具行信息密度错位）、
**交互是九种弹窗的集合而不是一个模型**。建议分两步：先做一层设计 token 和共享组件并统一
所有表面（方案一，一个 PR），再按切片收敛交互模型（方案二）。不建议重写渲染层（方案三）。

---

## 1. 现状诊断

以下结论来自通读 `ui.rs` 全部渲染函数、`theme.rs`、`shortcuts.rs`、`composer_textarea.rs`、
`vim.rs`，以及在 tmux 里实际运行 `target/debug/orca` 抓取的画面（空态、`/` 菜单、`@` 候选、
Ctrl+K 快捷键、`/config`、`/resume`、`/tasks`、首次运行安全提示、真实历史会话 resume 后的
transcript、provider 错误诊断）。

### 1.1 视觉语言不统一

| 问题 | 证据 |
|---|---|
| 输入框是直角边框，其余弹窗全是圆角 | `vim.rs:163-169` 用 `Block::default().borders(ALL)`，没有 `border_type`；`ui.rs` 里所有弹窗都是 `BorderType::Rounded`（如 `ui.rs:274`、`423`、`818`、`1058`） |
| 输入框带 ` Input ` 文字标题，vim 模式时变成 ` Input [vi normal] ` | `vim.rs:151-161` |
| 选中标记六种写法 | 恢复提示 `>`（`ui.rs:232-283`）、配置 `▸ ‹ ›`（`884-906`）、会话选择 `>`（`1054+`）、setup `❯ [T]`（`5295-5303`）、问卷 `›`（`593+`）、审批 `▸ [1]`（`5083+`）、计划确认 `▸ 1.`（`4952+`）、任务列表 `› `+ 选中背景（`1824+`）、workflow 树 `>`/`v`（`1619+`） |
| 写死 ANSI 颜色，不走主题 | `render_setup`（`ui.rs:5304-5522`）用 `Color::White/Cyan/DarkGray/Green/Yellow/Red/Blue`；`shortcut_lines`（`shortcuts.rs:537-548`）用 `Color::Cyan/Yellow/White`。Light 主题和 16 色终端下失真 |
| 底部提示行格式各异 | `↑↓ select · Enter · 1/2/3/4 · legacy y/A/a/n`（`5182`）、`[↑/↓] Move   [Enter] Select   [Esc] Exit`（setup）、`↑↓ select · ←→ change · Enter apply · Esc cancel`（config）、问卷五种 footer（`573-590`） |
| 主题只有颜色 token，没有边框/间距/标记 token | `theme.rs:12-37` 的 `Theme` 结构体 22 个颜色字段，其余样式散落在各渲染函数里 |

### 1.2 transcript 没有节奏（最大的生硬来源）

消息渲染集中在 `append_message_lines`（`ui.rs:2922-3060`），这是好事：一处改，全局生效，
scrollback 刷入与实时区共用同一函数（`build_lines_for_message`）。但当前样式：

- **助手消息没有任何角色标记**，正文从第 0 列开始；用户消息只有 `> `。两者只靠颜色区分。
- **thinking 抢眼**：`[thinking] ` 前缀 + `truncate_lines(text, 3)` 截的是逻辑行，一个逻辑行
  就是一整段，实测一个 thinking 块占 8 行屏幕，比助手正文还长。
- **工具行信息密度错位**：格式是 `  ✓ name: target (status)`，resume 后显示的是
  `✓ tool:call_00_JxiwWkyhCvbqycLuMpBq8394 (completed)`，即历史重建时工具名丢失，只剩 call id
  （实测截图，根因待定位，疑在 history 到 `ChatMessage::ToolCall` 的投影）。
- **输出预览是裸文本**：`append_tool_output_lines`（`3263-3295`）四空格缩进、折叠 2 行、
  `[+N lines]`，没有左栏或分组，连续多个工具调用之间没有视觉分隔。
- **系统通知与正文同权重**：`ChatMessage::System` 是 muted 纯文本；实测 6 行的 sandbox 警告
  和一行的 `Runtime settings updated: …` 都直接铺在对话里。
- **诊断是机器码风格**：`ERROR [provider.failed]: Model request failed / Cause: … / Next: …`
  （`3061-3105`），标题里带 code，Cause 里直接塞 JSON 原文。
- **间距没有规则**：用户消息后空一行；助手消息只在 `trailing_blank` 时空行；工具行不空；
  thinking 不空。结果是有的地方挤、有的地方空。
- **Jump to bottom 胶囊直接盖在正文上**（`1590-1612`，实测最后一行文字被截断覆盖）。

### 1.3 布局把状态塞在角落，把空白留在中间

- **欢迎页**：braille 鲸鱼（14 行）+ figlet 字标 + `model:` / `directory:` + 四条 Tips，共 28 行，
  36 行终端里几乎顶到输入框；Tips 是静态的，与用户当前状态无关（`build_welcome_lines`，`ui.rs:2807` 起）。
- **状态栏**（`status_line`，`4205-4299`）：`auto (max) · auto-edit · context 100% · cwd · git:main · F1 shortcuts`。
  审批模式是安全相关的最重要状态，却只是一个彩色单词，没有"Shift+Tab 可切换"的可供性；`F1 shortcuts`
  在 tmux 下 F1 根本收不到（实测变成字母 P）。
- **活动行**（`activity_lines`，`4339-4386`）：前台只有 `● running 12s`，看不到当前在跑哪个工具、
  处于流式输出还是思考；子代理反而有两行详情。
- **面板替换整个对话区**：`PanelMode::{Conversation, Workflows, Agents}`（`types.rs:334`）在
  `render()`（`ui.rs:116-121`）里三选一；`/tasks` 空态是一个 30 行的空边框 + 一句话（`1824-1857`）。
- **setup 安全提示弹窗固定 22 行高**（`5309-5310`），一半是空行；API key 步骤同样固定高度。
- **快捷键弹窗**（`4579-4600` + `shortcuts.rs:519-553`）：固定 58 宽，`{:<18}` 的按键列被
  `ctrl+b/f / alt+b/f`、`alt+enter / shift+enter` 撑爆，说明直接贴在按键后面；换行没有悬挂缩进；
  图片查看器的 `+/- · arrows · 0` 混在 Global 分组里。
- **会话选择器**（`1054-1262`）：所有项目的会话混排；测试写入真实 `ORCA_HOME` 留下的
  `mock` provider 会话（`schema_ok`、`mock_fail`…）和真实会话并列；没有分组和预览。
- **`/` 菜单**（`4737-4794`）：命令和描述之间两个空格，不对齐；**`@` 候选**（`4796-4879`）：
  描述被边框硬截断没有省略号，类型是 `[skill]` 这种方括号文字。

### 1.4 交互是"弹窗集合"

- 九种覆盖层各有各的按键文件和外观：审批（`approval_actions.rs`）、计划确认
  （`plan_approval_actions.rs`）、恢复提示、配置（`config_dialog_actions.rs`）、Full Access
  （`full_access_confirmation_actions.rs`）、快捷键、图片查看器、会话选择（`session_picker_actions.rs`）、
  setup（`setup_actions.rs`）。
- **审批仍是居中盖屏**：`input_region()`（`351-359`）在 `WaitingApproval` 时把 composer 藏掉，
  弹窗用 `Clear` 盖在 transcript 上。而 `ask_user_question` 已经按
  `docs/design/inline-interaction-refactor.md` 改成了内联到 composer 槽位（`412-591`），
  两种"需要用户决定"的交互长得完全不一样。
- **Esc 语义按上下文变化**：空闲态是"输入为空时回退"，运行态是"中断"，选择器里是"退出"，
  审批里没绑定，问卷里是"取消"。
- **展开只对最新一条有效**：`e` 只展开最新工具输出（`shortcuts.rs` hint）；已刷入 scrollback
  的内容不可再展开（`3270-3273` 注释说明了这是架构约束）。
- **setup 页按键会漏进 composer**：实测按 `t` 信任工作区后，composer 里出现了字母 `t`。

### 1.5 架构约束（设计必须遵守）

- **inline viewport + scrollback 刷入**：已"沉淀"的消息前缀会被刷进终端原生 scrollback
  （`transcript_state.flushed_count`），刷入内容与实时区必须像素一致，且刷入后不可重绘。
  因此任何消息样式都必须是静态文本可表达的，折叠状态在刷入时按 `force_expand` 展开。
- **渲染缓存按消息 revision 失效**（`transcript_view.rs`），样式改动只要经过
  `build_lines_for_message` 就自动兼容。
- **终端能力降级**：`Theme::resolve`（`theme.rs:185-215`）把 truecolor 调色板映射到
  256/16/单色；新样式必须经过 `Theme`，不能出现裸 `Color::*`（已有测试
  `markdown_semantic_colors_do_not_use_fixed_ansi_accents` 做类似约束）。
- **鼠标已支持**：slash/mention 命中、审批选项命中、会话行命中、跳底胶囊、图片预览、文本选择
  都有 hit-test，新设计可以依赖点击。
- **测试面**：`ui.rs` 有 185 个测试、316 处 `contains("` 字符串断言；`app_integration_tests.rs`
  146 个测试、50 处。改样式的主要成本是更新这些断言。

### 1.6 顺带发现的缺陷（建议随方案一一起修）

1. resume 后工具行显示 call id 而不是工具名。
2. Jump to bottom 胶囊覆盖正文最后一行。
3. setup 页的按键泄漏到 composer。
4. F1 在 tmux/部分终端下收不到；Ctrl+K 又要求输入为空。
5. 欢迎页的 braille 鲸鱼在没有 braille 字形的终端字体上会显示成方块，需要一个纯 ASCII 回退
   （可挂在 `terminal_capabilities` 的能力探测上）。
6. `@` 候选在索引预热期把 skill 排在文件前面（`@ui` 无文件结果，等索引完成后 `@ui.rs` 才正常）。
7. `/resume` 列表混入测试产生的 mock 会话（测试写入了真实 `ORCA_HOME`）。

---

## 2. 设计原则

1. **一套视觉语言**：所有容器同一种边框、同一种标题、同一种选中标记、同一种提示行语法。
2. **transcript 有节奏**：角色 gutter + 固定间距规则 + 默认折叠。看一眼就能分清谁在说话、
   做了什么、结果如何。
3. **状态就近显示**：正在做什么放在 composer 上方，当前模式放在 composer 下方左侧，
   资源用量放右侧。
4. **需要用户决定的交互进 composer 槽位，不盖屏**；纯信息面板不替换对话区。
5. **可降级**：层级优先用字符（gutter、缩进、粗细）表达，颜色是加分项。
6. **刷入即定稿**：消息样式不依赖后续重绘；折叠信息在刷入时按现有 `force_expand` 规则展开。

---

## 3. 方案对比

### 方案一：视觉层统一，不动交互模型（推荐先做）

- 新增设计 token 与共享组件；重写消息样式、composer、状态栏、活动行；所有弹窗改用共享框架；
  快捷键面板、欢迎页、setup 重排。
- 改动文件：`theme.rs`、新建 `chrome.rs`、`ui.rs`、`shortcuts.rs`、`vim.rs`、
  `composer_textarea.rs`、`transcript_state.rs`（thinking 折叠标记）、相关测试。
- 风险：低。主要成本是快照测试断言更新。
- 收益：消掉大部分"生硬"感，并为方案二准备好组件。

### 方案二：在方案一之上收敛交互模型

- 审批搬进 composer 槽位与问卷共用布局和按键；`/tasks` 从替换对话区改为 composer 上方的
  可折叠 dock；按条展开工具输出；会话选择器按项目分组；统一 Esc 规则。
- 改动文件：`approval_actions.rs`、`key_event_actions.rs`、`status_key_actions.rs`、
  `state_reducer.rs`、`action_dispatcher.rs`、`session_picker_actions.rs`、鼠标命中计算。
- 风险：中。审批对话框的几何与命中测试要迁移；集成测试大量覆盖审批流。
- 建议拆成 4 个独立切片，每个切片一个 PR。

### 方案三：渲染层重构成组件树（不建议现在做）

- 引入真正的 widget 抽象、布局引擎、样式系统，重建所有屏幕。
- `ui.rs` 已 1.2 万行加 1.3 万行测试，且刷入一致性、渲染缓存、inline viewport 是最脆弱的路径，
  重写风险集中在这里。等方案一二落地、组件边界自然显现后再评估。

---

## 4. 详细设计（方案一）

### 4.1 设计 token 与共享组件（新文件 `crates/orca-tui/src/chrome.rs`）

```rust
pub(crate) const BORDER: BorderType = BorderType::Rounded;
pub(crate) const MARK_SELECTED: &str = "›";
pub(crate) const MARK_IDLE: &str = " ";
pub(crate) const GUTTER: u16 = 3;          // 角色 gutter 宽度：图标 + 两空格

/// 所有弹窗/面板共用的外框：圆角，标题左对齐带空格，accent 只作用于边框和标题。
pub(crate) fn panel_block(theme: &Theme, title: &str, accent: Color) -> Block<'static>;

/// 统一提示行：`↑↓ move · Enter confirm · Esc cancel`，按键用 accent，动作用 muted，
/// 超宽时从右侧逐项裁掉。
pub(crate) fn hint_line(theme: &Theme, width: u16, items: &[(&str, &str)]) -> Line<'static>;

/// 选项行：`› 1  label   description`，选中行用 selection_bg + 粗体，不再只靠颜色。
pub(crate) fn option_line(theme: &Theme, selected: bool, key: &str, label: &str, detail: &str, width: u16) -> Line<'static>;

/// 按内容自适应高度的居中弹窗矩形，上限为 max_height。
pub(crate) fn centered_dialog(area: Rect, width: u16, content_rows: u16, max_height: u16) -> Rect;
```

- `theme.rs` 增加两个语义样式方法：`accent_style()`（边框/选中）、`dim_style()`（次要信息），
  并把 `render_setup`、`shortcut_lines` 里的裸 `Color::*` 全部替换。
- 增加一条守卫测试：`ui.rs`、`shortcuts.rs` 中不允许出现 `Color::` 字面量（白名单 `Color::Reset`）。

### 4.2 消息样式（`append_message_lines`）

所有消息共用 3 列 gutter。第一列是角色/状态图标，正文从第 4 列开始，换行宽度为 `width - GUTTER`。

```
 ›  你会用 ego-browser skill 吗

 ●  让我看看有哪些可用的 skill。
    ⋯ thinking · The user asks if I can use the "ego-browser" skill…

    ✓ list_skills
    │ baoyu-article-illustrator [user] - Analyzes article structure…
    │ baoyu-comic [user] - Knowledge comic creator supporting multipl…
    └ +26 lines · e expand

    ✓ read_skill  ego-browser
    └ 204 lines · e expand

    ✎ edit  src/main.rs  +1 −1
    │ -    let greeting = "hello";
    │ +    let greeting = "hello, whale";

 ●  有 ego-browser skill，我读一下它的使用说明。

    ⚠ Shell unavailable under the current restricted policy
      no OS-enforced sandbox backend on this host (seatbelt probe terminated by signal 6)
      next  run Orca on a host that permits sandbox enforcement, or select a trusted-host policy

    ✗ Model request failed · 401 Unauthorized
      next  retry once; if it persists check endpoint, network and provider status
```

逐类规则：

| 消息 | 样式 |
|---|---|
| User | gutter `›`（`theme.user`，粗体）；正文同色；保留 `compact_long_text` 3 行折叠；消息后空一行 |
| Assistant / AssistantChunk | gutter `●`（`theme.border`）只出现在第一行；正文 markdown 缩进到第 4 列；完整消息后空一行，chunk 沿用 `trailing_blank` |
| Reasoning | 固定一行 `⋯ thinking · <首句，按宽度截断>`，muted 斜体；`e` 或点击展开到最多 12 行。需要 `ChatMessage::Reasoning { text, expanded }` |
| ToolCall | 一行头：状态图标 + 工具短名（`bash`/`read`/`edit`/`grep`/`subagent`…，不是 call id）+ 目标（muted，按宽度截断）；运行中用 spinner；输出用 `│` 左栏，折叠 2 行，尾行 `└ +N lines · e expand`；失败显示前 2 行 stderr；edit 类在头行附 `+a −b` 摘要，diff 仍走 `diff_highlight` |
| 连续 ToolCall | 之间不空行，形成一组；组后接助手正文时空一行 |
| System | 一行 `ℹ` + 首行文字，muted；多行通知折叠为首行 + `(+N)`，与工具输出同样可展开 |
| Diagnostic / Error | 第一行 `⚠`/`✗` + 标题（不带 code，code 移到行尾 dim 或只在 `/status` 展示）；`cause`/`next` 各自缩进 6 列，JSON 原文只保留 message 字段 |
| PlanUpdate / ProposedPlan | 沿用现有清单样式，加 gutter 对齐 |
| Image | 不变 |

间距规则收敛为一个函数 `separator_before(prev, next) -> bool`，替代各分支里手写的 `Line::from("")`。

### 4.3 底部 chrome

```
    ⠋ Running 12s · bash cargo test --workspace · Esc interrupt
──────────────────────────────────────────────────────────────────────────────
 › Message Orca…  / commands · @ files · $ skills
──────────────────────────────────────────────────────────────────────────────
  ⇧Tab auto-edit          ~/Documents/GitHub/blade-deepseek · main          deepseek-flash · max · ctx 96% · 8.7k · $0.03 · ? help
```

- **Composer**：去掉两侧竖线和 ` Input ` 标题，改成上下各一条横线（`theme.border`），像 Claude Code；
  行首固定 `›` 提示符，与 transcript 里用户消息的 gutter 对齐；多行输入向上生长，上横线跟着上移。
  高度仍是"行数 + 2"，与现在的边框盒一致，布局数学不变。vim 模式改为在状态栏左侧显示 `NORMAL`/`VISUAL` 标签，不再改标题；有弹窗/问卷获得
  焦点时分隔线变 muted。placeholder 缩短为 `Message Orca…  / commands · @ files · $ skills`，
  换行提示移到帮助面板。
- **队列预览**：运行中排队的追问显示在活动行下方，提示写全三个动作：
  `↳ queued 1 · <文本>  · Ctrl+Enter send now · Alt+↑ edit`（Enter 排队是默认行为，不用写）。`Ctrl+Enter` 是跳过队列
  立即发送的快捷键，帮助面板的 Composer 分组也要列出。
- **活动行**：前台运行时显示阶段与当前工具：`⠋ Running 12s · bash cargo test · Esc interrupt`。
  当前工具来自 transcript 里最后一条 `status == running` 的 ToolCall，无工具时显示
  `thinking` / `writing`（由最近一条 chunk 类型推断），不需要 runtime 改动。子代理行保留，
  改用同一 gutter。
- **状态栏**：三区。左区模式 chip `⇧Tab auto-edit`，颜色沿用 `approval_mode_color`，
  side conversation 时替换为 `Side · Ctrl+/ back`；中区 cwd · branch，宽度不足时最先裁掉；
  右区 `model · effort · ctx % · tokens · cost · ? help`。裁剪优先级沿用现有 `status_line`
  的逐项累加逻辑。

### 4.4 弹窗与面板统一

- 所有弹窗改用 `panel_block` + `hint_line` + `option_line`，按内容自适应高度，最大宽 72。
- **审批弹窗（方案一阶段仍是弹窗）**：头部 `bash · cargo test --workspace`，命令/diff 预览
  带 `│` 左栏，选项用 `option_line`，去掉 `legacy y/A/a/n` 文案（按键保留）。
- **setup 三步**：去掉固定 22 行，改为按内容高度；文案压缩到 6 行以内；全部走主题色。
- **帮助面板（原快捷键）**：宽度 `min(76, w-4)`，按键列宽按最长按键计算，动作列换行带悬挂
  缩进；分组标题用 accent；图片查看器快捷键移到独立分组，只在查看器打开时显示。
  新增 `?`（输入为空时）作为 F1/Ctrl+K 的别名，状态栏文案改为 `? help`。
- **`/` 菜单**：命令列按最长命令对齐，描述 muted；选中行用 selection_bg。
- **`@` 候选**：类型改为定宽标签列（`file` / `skill` / `mcp`），描述超宽用 `…`；
  索引预热期在面板底部显示 `indexing files…`（`mention_popup_status` 已有状态位）。
- **会话选择器**：按项目分组，当前项目置顶；每行 `title · 相对时间 · 消息数`；
  `mock` provider 的会话默认隐藏，Tab 里增加"显示测试会话"开关。
- **Tasks / Workflows 面板**：方案一阶段保留全面板模式，空态改为居中 3 行提示
  （`No tasks yet` + 一句怎么触发 + `Esc back`），去掉大空框。
- **欢迎页**：改成左右两栏。左栏是工作树最新版本的 braille 鲸鱼（14 行、36 列）；右栏从第 42 列起，
  依次是 figlet 字标 + 版本、空行、`model   deepseek-flash · max · auto-edit`、
  `cwd     ~/Documents/GitHub/blade-deepseek · main`、空行、两条随状态变化的提示
  （未信任目录时提示 `/trust`，否则提示 `/resume`；第二条固定为 `? keys · / commands · @ files · $ skills`）。
  整块高度等于鲸鱼的 14 行，比现在的 28 行少一半。宽度不足 80 列时上下堆叠（鲸鱼在上），
  不足 44 列时只显示右栏文字；braille 不可用的终端回退到 ASCII 版鲸鱼。

  ```
    ⢀⣤⣶⣶⣶⣶⣄⢀⣤⣶                                 ___
    ⡠⠾⠿⠿⢿⣿⠻⠿⢻⣿⠿⠋                                / _ \ _ __ ___ __ _
          ⣿⣿                                        | | | | '__/ __/ _` |
    …（鲸鱼共 14 行）…                                | |_| | | | (_| (_| |
                                                      \___/|_|  \___\__,_|  v0.4.32

                                                     model   deepseek-flash · max · auto-edit
                                                     cwd     ~/Documents/GitHub/blade-deepseek · main

                                                     › /resume to continue a saved conversation
                                                     › ? keys · / commands · @ files · $ skills
  ```

### 4.5 顺带修复

1. resume 时工具名投影：定位 history → `ChatMessage::ToolCall.name` 的路径，恢复真实工具名。
2. 跳底胶囊：在胶囊矩形先 `Clear` 再绘制，或把胶囊放到活动行右侧。
3. setup 键盘事件：setup 状态下不把字符事件转发给 textarea。
4. F1：状态栏与欢迎页改宣传 `?`；保留 F1/Ctrl+K。

---

## 5. 详细设计（方案二切片）

### S1 审批内联

- 复用问卷的 composer 槽位布局：头行 `◆ Approve · bash · cargo test`，中部预览（命令或 diff，
  PgUp/PgDn 滚动），选项 `1 allow once / 2 allow bash this session / 3 allow this exact call / 4 deny`，
  尾行 `hint_line`。transcript 保持可见可滚动。
- 按键统一：`↑↓`/`j k` 移动、数字直选、Enter 确认、Esc = deny。y/A/a/n 保留为隐藏别名。
- `input_region()` 增加 `Approval` 变体；`approval_dialog_geometry` 与 `approval_option_hit_index`
  改为基于 composer 槽位矩形。

### S2 Tasks dock

- `/tasks` 不再切换 `PanelMode`，而是在活动行位置展开一个最多 6 行的 dock：每行一个任务
  （状态图标、名称、阶段、耗时），`↑↓` 选择、Enter 打开该任务的 transcript（这一步仍用全面板）、
  Esc 收起。`PanelMode::Agents` 只保留给 transcript 详情视图。

### S3 按条展开

- 折叠行（工具输出、thinking、长通知）注册 hit area，点击切换该条的 `expanded`。
- 键盘：`e` 仍作用于最新一条；`Shift+E` 展开/收起实时区内所有折叠行。

### S5 Ctrl+Enter 跳过队列立即发送

- 现状：运行态 `Enter` 绑定为 `RunningShortcut::SubmitQueued`（`shortcuts.rs:384`），协议里只有
  `UserAction::{Submit, SubmitWithMentions, SubmitQueued}`，没有"立即发送"。
- 目标：`Ctrl+Enter` 在运行态把当前输入作为下一条用户消息立即提交，不进入队列；语义为
  "当前工具步骤结束后立刻注入"，不打断正在执行的工具（与 `Esc` 中断区分）。
- 改动：`RunningShortcut::SubmitNow` + `KeyBinding::new(KeyCode::Enter, KeyModifiers::CONTROL)`；
  `UserAction::SubmitNow(String)` 及 runtime 侧的插队投递；队列预览提示行与帮助面板补上该键。
- 终端兼容：部分终端不区分 `Enter` 与 `Ctrl+Enter`（发送同一字节序列），需要 kitty keyboard
  protocol 或 `CSI u` 支持；不支持的终端在帮助面板里标注"此终端不可用"。

### S4 Esc 规则表

| 上下文 | Esc |
|---|---|
| 任何弹出菜单/面板/问卷/内联审批 | 关闭或取消该层，不做别的 |
| 运行态、无弹层 | 中断当前 turn |
| 空闲态、输入非空 | 清空输入（改自"无动作"） |
| 空闲态、输入为空 | 回退（保持现状） |
| 会话选择器 | 返回对话（不退出程序） |

---

## 6. 迁移与测试策略

1. **先做无输出变化的重构**：引入 `chrome.rs` 与 `Theme` 新方法，把现有弹窗改为调用共享组件但
   输出保持逐字节一致，测试全绿后再改样式。
2. **一次改一个表面**：composer/状态栏 → transcript → 弹窗/面板 → 欢迎页/setup，每步更新对应的
   字符串断言。
3. **补充整帧黄金测试**：用 `TestBackend` 对 6 个关键画面（欢迎、带工具调用的 transcript、
   审批、问卷、帮助、会话选择）做整帧快照，后续样式变更以整帧 diff 评审，逐步替代碎片化的
   `contains("` 断言。
4. **降级验证**：现有 `theme.rs` 的 color-level 测试覆盖新增样式；新增"无裸 `Color::`"守卫。
5. **刷入一致性**：新增测试断言 `build_lines_for_messages(force_expand = true)` 与实时区展开后的
   输出一致，覆盖 Reasoning 与 System 的新折叠标记。

## 7. 里程碑

| 里程碑 | 内容 | 估时 |
|---|---|---|
| M1 | token、`chrome.rs`、composer、状态栏、活动行 | 1 天 |
| M2 | transcript 样式、间距规则、thinking 折叠、工具名修复、胶囊修复 | 1 到 2 天 |
| M3 | 弹窗/面板/帮助/会话选择/欢迎/setup 统一 | 1 到 2 天 |
| S1 | 审批内联 | 2 到 3 天 |
| S2 到 S4 | dock、按条展开、Esc 规则 | 各 1 天 |

## 8. 风险

- 助手正文缩进 3 列会改变换行宽度，transcript 搜索与选择基于渲染后的行工作，不受影响；
  复制时是否剥离 gutter 需要在 `selection.rs` 里决定（建议剥离）。
- `ChatMessage::Reasoning` 增加 `expanded` 会波及 `transcript_state.rs`、`protocol.rs` 映射和测试。
- 双宽字符（emoji）在欢迎页与提示行的宽度计算依赖 `unicode-width`，需在 kitty、iTerm2、
  Windows Terminal 各验证一次。
- 方案二 S1 改变审批的鼠标命中区域，需要同步更新 `app_integration_tests.rs` 里的点击测试。

## 9. 已确认的决策（2026-09-21）

1. 范围：方案一和方案二都做；方案一先作为一个 PR 落地，方案二按 S1 到 S4 四个切片各一个 PR。
2. transcript 风格：带图标 gutter（`›` / `●` / `✓` 等），如第 4.2 节。
3. Composer：去掉两侧竖线，上下各一条横线加 `›` 提示符，像 Claude Code（评审意见："输入框没必要两边有竖杠框定"、"要两条横线"）。
4. thinking 默认折叠为一行，`e` 或点击展开。
5. `Ctrl+Enter` 为"跳过队列立即发送"（用户 2026-09-21 定义），作为方案二切片 S5 实现；
   方案一的提示文案在 S5 落地前不宣传这个键。
6. 欢迎页改为左右两栏布局（用户 2026-09-21 要求），见 4.4。
