# Orca TUI 方案二（交互模型收敛）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 Orca TUI 的交互从"九种弹窗的集合"收敛成一个模型：需要用户决定的事都发生在输入框槽位，任务列表是一个 dock 而不是替换对话区，折叠内容可以逐条点开，Esc 有一张说得清的规则表，`Ctrl+Enter` 可以让一条追问插到队首。

**Architecture:** 审批复用问卷已经走通的 composer 槽位布局（`InputRegion` 增加一个变体，几何与命中测试都改为从 `viewport.input_area` 推导，而不是 `frame.area()`）。Tasks dock 扩展活动行里已经存在的 mini-dock，不新建区域。按条展开需要一个新的"屏幕行 → 消息下标"反查，建立在 `render_cache.message_row_range` 之上，命中区按帧重建，模式照搬 `image_hit_areas`。Esc 保持现有的两级流水线（preflight 然后 status），只补一条规则并用一个测试把整张表钉住。`Ctrl+Enter` 用 runtime 已有的 `PromptQueueAction::Reorder` 把新追问移到队首。

**Tech Stack:** Rust 2024，ratatui 0.29，crossterm（kitty keyboard protocol 已在 `input_runtime.rs` 无条件开启），`crates/orca-tui` 的 `chrome.rs` 共享组件，`crates/orca-runtime` 的 `prompt_queue`。

**Spec:** `docs/superpowers/specs/2026-09-20-tui-visual-interaction-redesign-design.md` 第 5 节（S1 到 S5）

## Global Constraints

- 基线 commit `025d814c`（方案一已合入 main）。所有任务从 main 起步。
- 视觉一律走 `crates/orca-tui/src/chrome.rs`：`panel_block`（自带 `Padding::horizontal(1)`，内容宽度 `popup.width - 4`）、`hint_line`、`option_line`（空 key 不占列）、`dialog_rect`、`rule_line`、`gutter`、`MARK_SELECTED = "›"`。不得新建 Block 样式。
- `ui.rs`、`chrome.rs`、`shortcuts.rs` 的非测试代码不得出现裸 `Color::` 字面量（`ui_sources_do_not_hardcode_ansi_colors` 守卫会失败）。
- 四个整帧黄金快照在 `crates/orca-tui/src/golden/*.txt`。任何改变这些画面的任务必须用 `ORCA_UPDATE_GOLDEN=1` 重新生成，**读一遍生成结果**确认符合设计，再用不带该变量的运行确认通过。黄金文件由 `.gitattributes` 锁定为 LF。
- 渲染器和它的命中测试必须从同一个 helper 取几何。方案一出过一个 Critical：`render_slash_menu` 传 `show_status = true` 而 `slash_menu_hit_index` 传 `false`，点击整体错一行。每个改动几何的任务都要在报告里写清楚渲染与命中两侧各自的算式。
- 测试命令：`cargo test -p orca-tui --lib -- --test-threads=1`（多个过滤词放在 `--` 之后，例如 `cargo test -p orca-tui --lib -- name_a name_b --test-threads=1`）。`--lib` 有既有的 ORCA_HOME 竞争，必须单线程。
- 提交前 `cargo fmt --all`，然后把自己没碰过的文件逐个 `git checkout --` 还原（仓库有 8 个文件存在既有 rustfmt 漂移，不要带进提交）。
- Clippy 门禁：`cargo clippy -p orca-tui --all-targets --no-deps` 不得在本任务新增或改动的行上报出诊断。仓库有约 82 条既有诊断，不在范围内。
- 每个任务一个 commit，英文 conventional commit。

## 与 spec 的偏差（勘察后确认，已在本计划中裁定）

| spec 的设想 | 代码实际 | 本计划的做法 |
|---|---|---|
| S2「在活动行位置展开一个 dock」 | 活动行里**已有** mini-dock，带 `agent_dock_selected_task_id` 选中态和 Shift+Up/Down + Enter 绑定 | 扩展现有 dock，不新建区域（Task 3） |
| S2「`/tasks` 不再切换 PanelMode」 | 面板可见性还会被后台任务生命周期自动切换（`apply_workflow_tasks_update` 在有任务需要审批时自动展开） | 只改用户手势的路径，保留自动展开（Task 3） |
| S3「长通知可折叠」 | `ChatMessage::System(String)` 没有 `expanded` 字段 | 改成结构体变体，单独一个任务（Task 6） |
| S1「Esc = deny」 | `APPROVAL_BINDINGS` 里**没有** Esc，但提示行一直写着 `Esc deny` | 补上绑定，让行为追上已宣传的文案（Task 2） |
| S5「当前工具步骤结束后立刻注入」 | 队列由 runtime 拥有，`try_drain_prompt_queue` 只在 `active.is_none()` 时触发；真正的 turn 中注入需要改 `ThreadActor` | **用户 2026-09-22 确认语义**：立即把消息送进队首，当前 turn 一结束就第一个处理，不打断正在执行的工具。这正是 Task 8 的做法，turn 中途注入不做 |

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `crates/orca-tui/src/ui.rs` | `InputRegion::Approval`、`approval_region_height`、`render_approval_panel`、dock 渲染、行反查与命中区、golden 测试 | 修改 |
| `crates/orca-tui/src/transcript_hit.rs` | 屏幕行 → 消息下标反查与可折叠行的命中区 | 新建 |
| `crates/orca-tui/src/shortcuts.rs` | `ApprovalShortcut::Deny` 绑 Esc、`IdleShortcut::ExpandAll`、`RunningShortcut::SubmitNow`、帮助文案 | 修改 |
| `crates/orca-tui/src/approval_dialog_actions.rs` | 审批按键：Esc = deny | 修改 |
| `crates/orca-tui/src/types.rs` | `ChatMessage::System` 形状、dock 展开态、`toggle_expandable_at` / `toggle_all_expandable` | 修改 |
| `crates/orca-tui/src/transcript_state.rs` | `ChatMessage::System { text, expanded }` | 修改 |
| `crates/orca-tui/src/input_event_actions.rs` | 折叠行点击路由（插在文本选择之前） | 修改 |
| `crates/orca-tui/src/workflow_panel.rs` | dock 展开态的 setter 与自动展开逻辑 | 修改 |
| `crates/orca-tui/src/slash_command_actions.rs` | `/tasks` 走 dock，`/agents` 保持面板 | 修改 |
| `crates/orca-tui/src/key_event_actions.rs`、`idle_key_actions.rs` | Esc 规则表的那一条新增行为 | 修改 |
| `crates/orca-tui/src/queued_input_actions.rs`、`protocol.rs` | `SubmitNow` 动作与队首插入 | 修改 |
| `crates/orca-tui/src/golden/*.txt` | 受影响画面的黄金快照 | 重新生成 |

---

### Task 1: 审批面板内联到输入框槽位

**Files:**
- Modify: `crates/orca-tui/src/ui.rs` — `InputRegion`（:337）、`input_region`（:341-355）、`render()` 的高度计算（:80-92）与分派（:149-171、:219-221）、`approval_dialog_geometry`（:5633）、`approval_option_hit_index`（:5674）、`render_approval_dialog` → `render_approval_panel`
- Modify: `crates/orca-tui/src/golden/approval.txt`（重新生成）
- Test: `crates/orca-tui/src/ui.rs` tests

**Interfaces:**
- Produces:
  - `enum InputRegion { Hidden, Composer, Interaction, Approval }`
  - `fn approval_region_height(area_width: u16, dialog: &ApprovalDialog) -> u16`（与 `user_input_region_height` 同构，返回值 `clamp(8, 18)`）
  - `fn render_approval_panel(frame: &mut Frame, area: Rect, dialog: &ApprovalDialog, theme: &Theme)`
  - `ApprovalDialogGeometry` 保留四个字段，但 `popup` 现在等于传入的 `area`（composer 槽位），`first_option_row` 相对它计算
- Consumes: `chrome::{panel_block, option_line, hint_line}`、`wrap_text`、`truncate_to_display_width`

- [ ] **Step 1: 写失败测试**

在 `ui.rs` tests 里加（`frame_string`、`test_state` 已有）：

```rust
fn approval_state() -> AppState {
    let mut state = test_state();
    state.push_message(ChatMessage::User("跑一下测试".into()));
    state.status = AppStatus::WaitingApproval;
    state.approval_dialog = Some(ApprovalDialog {
        id: "1".into(),
        interaction: None,
        tool: "bash".into(),
        target: Some("cargo test -p orca-tui".into()),
        permission_kind: None,
        background_task_id: None,
        selected: 0,
        options: ApprovalDialog::options_for("bash", Some("cargo test -p orca-tui")),
        diff: None,
    });
    state
}

#[test]
fn approval_renders_in_the_composer_slot_and_leaves_the_transcript_visible() {
    let mut state = approval_state();
    let frame = frame_string(&mut state, 100, 30);
    // The transcript above the panel still shows the user's message.
    assert!(frame.contains(" ›  跑一下测试"), "{frame}");
    // The panel occupies the input region, not a centered modal.
    let input = state.viewport.input_area.expect("approval occupies the input slot");
    assert!(input.height >= 8, "{input:?}");
    let panel_top = frame.lines().position(|line| line.contains("Approve · bash")).expect("title");
    assert_eq!(panel_top as u16, input.y, "panel must start at the input rect: {frame}");
    // The status bar is still the last row.
    assert!(frame.lines().last().unwrap().contains("? help"), "{frame}");
}

#[test]
fn approval_hit_test_agrees_with_the_rendered_option_rows() {
    let mut state = approval_state();
    let frame = frame_string(&mut state, 100, 30);
    let input = state.viewport.input_area.expect("input area");
    for (index, needle) in ["Allow once", "Allow this exact call", "Allow bash this session", "Deny"]
        .into_iter()
        .enumerate()
    {
        let row = frame
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("option {needle} not drawn: {frame}")) as u16;
        assert_eq!(
            approval_option_hit_index(&state, input.x + 4, row),
            Some(index),
            "click on the drawn row for {needle} must resolve to option {index}: {frame}"
        );
    }
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- approval_renders_in_the_composer_slot approval_hit_test_agrees --test-threads=1`
Expected: FAIL — 审批仍是居中弹窗，`viewport.input_area` 在 `WaitingApproval` 下是 `None`。

- [ ] **Step 3: 实现**

`InputRegion` 增加 `Approval`；`input_region` 改为：

```rust
fn input_region(state: &AppState) -> InputRegion {
    if state.plan_approval_dialog.is_some() {
        InputRegion::Hidden
    } else if matches!(state.status, AppStatus::WaitingApproval) && state.approval_dialog.is_some() {
        InputRegion::Approval
    } else if state.user_input_dialog.is_some() {
        InputRegion::Interaction
    } else {
        InputRegion::Composer
    }
}
```

`render()` 的高度分支增加 `InputRegion::Approval => state.approval_dialog.as_ref().map(|dialog| approval_region_height(frame.area().width, dialog)).unwrap_or(0)`；分派分支增加 `InputRegion::Approval => { state.viewport.input_area = Some(chunks[6]); render_approval_panel(frame, chunks[6], dialog, theme); }`；删除 `:219-221` 那个无条件的居中弹窗调用。

`approval_region_height` 照 `user_input_region_height`（`ui.rs:380-402`）的形状写：`2`（上下边框）+ `tool_rows` + `target_rows` + diff 行（上限 8）+ 截断行 + 1（空行）+ 选项数 + 1（空行）+ 1（提示行），最后 `.clamp(8, 18)`。

`approval_dialog_geometry(area, dialog)` 的 `area` 现在传 composer 槽位矩形而不是 `frame.area()`：`popup = area`（不再调 `dialog_rect`），`width` 用 `area.width`，`max_height` 用 `area.height`，其余算式不变。`first_option_row` 仍是 `popup.y + 1 + header_rows + shown_diff_lines + truncation_row + 1`。

`approval_option_hit_index` 把 `state.viewport.frame_area` 换成 `state.viewport.input_area`。

`render_approval_dialog` 改名 `render_approval_panel`，签名接收 `area: Rect`，去掉 `Clear` 和 `frame.area()`，其余内容构建（标题、tool 行、target 行、diff 左栏、`option_line` 选项、`hint_line`）原样保留。

- [ ] **Step 4: 运行并更新受影响的旧测试**

Run: `cargo test -p orca-tui --lib -- approval --test-threads=1`
以下测试断言的是居中弹窗或"审批时隐藏 composer"，按新布局更新（不得弱化：原本钉住"选项按语义顺序渲染"的仍要钉住顺序，原本钉住"长内容下动作仍可见"的仍要钉住可见）：`waiting_approval_does_not_render_composer_under_dialog`（改为断言 composer 被审批面板取代，且 transcript 仍可见）、`waiting_approval_frame_hides_the_hardware_cursor_without_moving_it`、`approval_dialog_keeps_actions_visible_with_long_content`、`waiting_approval_renders_numeric_shortcuts_in_semantic_order`、`approval_dialog_uses_shared_options_and_hints`、`approval_dialog_hint_lists_only_the_keys_the_options_actually_have`、`permission_approval_dialog_keeps_its_risk_title_and_shows_the_tool_line`。`input_event_actions.rs` 里的审批点击测试通过 `approval_option_hit_index` 动态定位，应当无需改坐标，但要跑一遍确认。

- [ ] **Step 5: 重新生成并审阅黄金快照**

Run: `ORCA_UPDATE_GOLDEN=1 cargo test -p orca-tui --lib -- golden_approval --test-threads=1`
然后**打开 `crates/orca-tui/src/golden/approval.txt` 读一遍**，确认：面板在底部贴着状态栏、上方 transcript 里的用户消息仍在、边框内左右各一格空白、选项行读作 `› 1  Allow once` / `2 Allow this exact call` / `3 Allow bash this session` / `4 Deny`、提示行在最后一行内容。确认后不带变量重跑：`cargo test -p orca-tui --lib -- golden --test-threads=1`。在报告里引用你检查的那几行。

- [ ] **Step 6: 全量 + 提交**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src/ui.rs crates/orca-tui/src/golden/approval.txt
git commit -m "feat(tui): render approvals inline in the composer slot"
```

---

### Task 2: 审批按键统一（Esc = deny）

**Files:**
- Modify: `crates/orca-tui/src/shortcuts.rs` — `APPROVAL_BINDINGS`（:437-486）
- Modify: `crates/orca-tui/src/approval_dialog_actions.rs`（:10-60）
- Test: `crates/orca-tui/src/approval_dialog_actions.rs` tests、`shortcuts.rs` tests

**Interfaces:**
- Consumes: `ApprovalShortcut::Deny`（已存在）、`ApprovalOption::Deny`
- Produces: 无新类型；`APPROVAL_BINDINGS` 多一条 `(ApprovalShortcut::Deny, KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE))`

- [ ] **Step 1: 写失败测试**

`shortcuts.rs` tests：

```rust
#[test]
fn esc_denies_an_approval() {
    assert_eq!(
        approval_shortcut(key(KeyCode::Esc, KeyModifiers::NONE)),
        Some(ApprovalShortcut::Deny)
    );
}
```

`approval_dialog_actions.rs` tests（照本文件现有测试的构造方式建 state 与 dialog）：

```rust
#[test]
fn pressing_esc_resolves_the_approval_as_denied() {
    let (mut state, action_rx) = approval_fixture();
    handle_approval_dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut state, &action_tx);
    assert!(state.approval_dialog.is_none(), "the dialog must close");
    let action = action_rx.try_recv().expect("a response is sent");
    assert!(format!("{action:?}").contains("Deny"), "{action:?}");
}
```

（`approval_fixture()` 指本文件测试里已有的构造方式；若名字不同，复用现成那套代码，不要新造抽象。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- esc_denies pressing_esc_resolves --test-threads=1`
Expected: FAIL — `APPROVAL_BINDINGS` 没有 Esc，按键被 `Some(_) | None => {}` 吞掉。

- [ ] **Step 3: 实现**

在 `APPROVAL_BINDINGS` 的 `ApprovalShortcut::Deny` 分组里加：

```rust
(
    ApprovalShortcut::Deny,
    KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
),
```

`handle_approval_dialog_key` 的 `ApprovalShortcut::Deny` 分支若尚未直接 resolve（现有代码对 `y/a/n/d` 已有处理），确保 Esc 走同一条 resolve 路径。不要改 `y/A/a/n` 的既有行为。

- [ ] **Step 4: 运行 + 提交**

Run: `cargo test -p orca-tui --lib -- approval --test-threads=1`，然后全量 `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src/shortcuts.rs crates/orca-tui/src/approval_dialog_actions.rs
git commit -m "feat(tui): make Esc deny an approval as the hint already promised"
```

---

### Task 3: Tasks dock 取代面板切换

**Files:**
- Modify: `crates/orca-tui/src/types.rs` — `AppState` 增加 `pub tasks_dock_expanded: bool`（`new` 里 `false`）
- Modify: `crates/orca-tui/src/workflow_panel.rs` — `show_agents`（:457-459）、`apply_workflow_tasks_update`（:461-527）
- Modify: `crates/orca-tui/src/slash_command_actions.rs`（:234-239）
- Modify: `crates/orca-tui/src/ui.rs` — `background_task_activity_lines`（:4973+）
- Modify: `crates/orca-tui/src/key_event_actions.rs`（:754-799，dock 导航）
- Test: `ui.rs` tests、`slash_command_actions.rs` tests

**Interfaces:**
- Produces: `AppState::tasks_dock_expanded`；`AppState::toggle_tasks_dock()`；`AppState::collapse_tasks_dock()`
- Consumes: 现有的 `agent_dock_selected_task_id`、`select_next_agent_dock_task`、`selected_agent_dock_task`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn slash_tasks_expands_the_dock_without_replacing_the_transcript() {
    let mut state = test_state();
    state.push_message(ChatMessage::User("并行做三件事".into()));
    state.tasks_dock_expanded = true;
    let frame = frame_string(&mut state, 100, 30);
    assert!(frame.contains(" ›  并行做三件事"), "transcript stays visible: {frame}");
    assert_eq!(state.panel_mode, PanelMode::Conversation, "the panel must not swap");
    assert!(frame.contains("◆ Tasks"), "{frame}");
    assert!(frame.contains("Esc"), "the dock advertises how to close: {frame}");
}

#[test]
fn esc_collapses_the_dock_before_anything_else_handles_it() {
    let mut state = test_state();
    state.tasks_dock_expanded = true;
    // drive the real Esc path, not the setter
    // (mirror the construction used by the neighbouring key tests in this file)
    press_esc(&mut state);
    assert!(!state.tasks_dock_expanded);
}
```

（`press_esc` 指本文件已有的按键驱动方式；照抄邻近测试的构造。）

`slash_command_actions.rs` tests：

```rust
#[test]
fn tasks_opens_the_dock_and_agents_still_opens_the_panel() {
    let mut state = test_state();
    apply_slash_command(&mut state, SlashCommand::TaskWorkspace);
    assert!(state.tasks_dock_expanded);
    assert_eq!(state.panel_mode, PanelMode::Conversation);

    let mut state = test_state();
    apply_slash_command(&mut state, SlashCommand::AgentDashboard);
    assert_eq!(state.panel_mode, PanelMode::Agents);
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- slash_tasks_expands esc_collapses_the_dock tasks_opens_the_dock --test-threads=1`
Expected: 编译失败，`tasks_dock_expanded` 不存在。

- [ ] **Step 3: 实现**

`types.rs` 加字段与两个方法。`slash_command_actions.rs` 把 `SlashCommand::TaskWorkspace` 从 `state.show_agents()` 改为 `state.toggle_tasks_dock()`；`AgentDashboard` 保持 `show_agents()`。

`background_task_activity_lines`（`ui.rs:4973+`）：展开态下把行数上限从现有的 `MAX_DEFAULT_SUBAGENTS`（4）提到 6，头行改为 `◆ Tasks · N active · M needs approval`，末行追加 `↑↓ select · Enter open · Esc close` 的 `hint_line`；未展开时保持现有的紧凑摘要行。

Esc：在 `key_event_actions.rs` 的 preflight 里，把"dock 展开"这条插在 `panel_mode ∈ {Workflows, Agents}` 那条（:828）**之前**，因为 dock 是更上层的临时展开。

`apply_workflow_tasks_update`（`workflow_panel.rs:461-527`）的自动展开保持不变，但把它对 `PanelMode` 的自动切换改为设置 `tasks_dock_expanded = true`——后台任务需要审批时展开 dock 而不是抢走整个对话区。自动收起同理。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`
更新依赖 `/tasks` 切换 `PanelMode` 的测试；`workflow_panel.rs` 里断言自动展开会切 `PanelMode::Workflows` 的测试改为断言 dock 展开。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): open tasks as a dock instead of replacing the transcript"
```

---

### Task 4: 屏幕行到消息下标的反查与折叠行命中区

**Files:**
- Create: `crates/orca-tui/src/transcript_hit.rs`
- Modify: `crates/orca-tui/src/lib.rs`（按字母序注册 `mod transcript_hit;`）
- Modify: `crates/orca-tui/src/types.rs` — `AppState` 增加 `pub collapsible_hit_areas: Vec<CollapsibleHitArea>`
- Modify: `crates/orca-tui/src/ui.rs` — `render_live_messages` 里按帧重建命中区（照 `image_hit_areas` 在 `:1664-1703` 的写法）
- Test: `crates/orca-tui/src/transcript_hit.rs` tests

**Interfaces:**
- Produces:

```rust
pub(crate) struct CollapsibleHitArea {
    pub(crate) rect: Rect,
    pub(crate) message_index: usize,
}

/// The message whose rendered rows contain `row`, given the cache's per-message
/// row ranges and the viewport's first visible row.
pub(crate) fn message_index_at_row(
    row_ranges: &[std::ops::Range<usize>],
    viewport_base_row: usize,
    area_y: u16,
    row: u16,
) -> Option<usize>;

pub(crate) fn collapsible_hit_areas(
    messages: &[ChatMessage],
    row_range_for: impl Fn(usize) -> Option<std::ops::Range<usize>>,
    viewport_base_row: usize,
    visible_height: usize,
    area: Rect,
) -> Vec<CollapsibleHitArea>;
```

- Consumes: `render_cache.message_row_range(index)`、`viewport.viewport_base_row`、`viewport.visible_height`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_screen_row_back_to_the_message_that_drew_it() {
        // message 0 occupies rows 0..2, message 1 rows 2..7, message 2 rows 7..8
        let ranges = vec![0..2, 2..7, 7..8];
        // viewport starts at absolute row 2 and is drawn at screen y = 10
        assert_eq!(message_index_at_row(&ranges, 2, 10, 10), Some(1));
        assert_eq!(message_index_at_row(&ranges, 2, 10, 14), Some(1));
        assert_eq!(message_index_at_row(&ranges, 2, 10, 15), Some(2));
        assert_eq!(message_index_at_row(&ranges, 2, 10, 9), None, "above the area");
        assert_eq!(message_index_at_row(&ranges, 2, 10, 99), None, "past the last row");
    }

    #[test]
    fn only_collapsible_messages_get_a_hit_area_and_it_is_clipped_to_the_viewport() {
        let messages = vec![
            ChatMessage::User("hi".into()),
            ChatMessage::ToolCall { /* 照本 crate 现有 tool_call 测试助手构造 */ },
            ChatMessage::Reasoning { text: "a\nb".into(), expanded: false },
        ];
        let ranges = vec![0..1, 1..4, 4..5];
        let areas = collapsible_hit_areas(
            &messages,
            |index| ranges.get(index).cloned(),
            0,
            5,
            Rect::new(0, 3, 80, 5),
        );
        assert_eq!(areas.len(), 2, "user messages are not collapsible");
        assert_eq!(areas[0].message_index, 1);
        assert_eq!(areas[0].rect.y, 4);
        assert_eq!(areas[0].rect.height, 3);
        assert_eq!(areas[1].message_index, 2);
    }
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- transcript_hit --test-threads=1`
Expected: 编译失败，模块不存在。

- [ ] **Step 3: 实现**

新建 `transcript_hit.rs`，写入上面 Interfaces 里的两个函数与结构体。`message_index_at_row` 把屏幕行换算成绝对行（`viewport_base_row + (row - area_y)`），然后在 `row_ranges` 上找包含它的那个下标（用 `iter().position(|r| r.contains(&absolute))`，范围有序且不重叠，线性即可）。`collapsible_hit_areas` 遍历消息，只对 `ChatMessage::ToolCall { .. }` 与 `ChatMessage::Reasoning { .. }` 生成，范围与可见窗口取交集后换算成屏幕矩形，空交集跳过。

`ui.rs` 的 `render_live_messages` 在重建 `image_hit_areas` 的同一处，用同样的模式重建 `state.collapsible_hit_areas`。

- [ ] **Step 4: 运行 + 提交**

Run: `cargo test -p orca-tui --lib -- transcript_hit --test-threads=1`，然后全量。

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): map transcript rows back to the message that drew them"
```

---

### Task 5: 点击折叠行展开，Shift+E 全部展开

**Files:**
- Modify: `crates/orca-tui/src/types.rs` — `toggle_expandable_at(index) -> bool`、`toggle_all_expandable() -> bool`
- Modify: `crates/orca-tui/src/input_event_actions.rs` — 左键路由（`handle_mouse_event`，:433+），插在文本选择兜底（:621-657）**之前**
- Modify: `crates/orca-tui/src/shortcuts.rs` — `IdleShortcut::ExpandAll` + `KeyBinding::new(KeyCode::Char('E'), KeyModifiers::SHIFT)` + 提示文案
- Modify: `crates/orca-tui/src/idle_navigation_actions.rs` — 新 shortcut 的分支
- Test: `types.rs` tests、`input_event_actions.rs` tests

**Interfaces:**
- Consumes: `CollapsibleHitArea`、`state.collapsible_hit_areas`（Task 4）
- Produces: `AppState::toggle_expandable_at(&mut self, index: usize) -> bool`（超出 `flushed_count` 之前的返回 `false`）、`AppState::toggle_all_expandable(&mut self) -> bool`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn clicking_a_collapsed_tool_row_expands_only_that_message() {
    let mut state = test_state();
    state.push_message(tool_call("bash", Some("a"), "completed", Some("l1\nl2\nl3\nl4"), false));
    state.push_message(tool_call("bash", Some("b"), "completed", Some("m1\nm2\nm3\nm4"), false));
    render_once(&mut state, 100, 30); // populates collapsible_hit_areas
    let area = state.collapsible_hit_areas[0];
    let handled = click_at(&mut state, area.rect.x + 2, area.rect.y);
    assert!(handled);
    assert!(matches!(&state.transcript.messages[0], ChatMessage::ToolCall { expanded: true, .. }));
    assert!(matches!(&state.transcript.messages[1], ChatMessage::ToolCall { expanded: false, .. }));
}

#[test]
fn a_click_that_hits_no_collapsible_row_still_starts_a_text_selection() {
    let mut state = test_state();
    state.push_message(ChatMessage::Assistant("plain paragraph".into()));
    render_once(&mut state, 100, 30);
    let transcript = state.viewport.transcript_area.expect("transcript");
    click_at(&mut state, transcript.x + 1, transcript.y);
    assert!(state.viewport.selection.is_some() || state.viewport.transcript_mouse_selecting);
}

#[test]
fn shift_e_toggles_every_collapsible_message_in_the_live_pane() {
    let mut state = test_state();
    state.push_message(tool_call("bash", None, "completed", Some("a\nb\nc"), false));
    state.push_message(ChatMessage::Reasoning { text: "x\ny".into(), expanded: false });
    assert!(state.toggle_all_expandable());
    assert!(matches!(&state.transcript.messages[0], ChatMessage::ToolCall { expanded: true, .. }));
    assert!(matches!(&state.transcript.messages[1], ChatMessage::Reasoning { expanded: true, .. }));
    assert!(state.toggle_all_expandable());
    assert!(matches!(&state.transcript.messages[0], ChatMessage::ToolCall { expanded: false, .. }));
}
```

（`render_once` / `click_at` 指本 crate 测试里已有的驱动方式；照抄邻近测试，不要新造抽象。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- clicking_a_collapsed_tool_row shift_e_toggles --test-threads=1`

- [ ] **Step 3: 实现**

`toggle_expandable_at(index)`：只允许 `index >= transcript.flushed_count`（已刷入 scrollback 的不可再变），命中 `ToolCall`/`Reasoning` 时经 `mutate_message` 翻转 `expanded` 并返回 `true`。`toggle_all_expandable()`：对 `flushed_count..` 范围内所有可折叠消息取"当前是否全部展开"的反面，统一设置，返回是否有改动。

`handle_mouse_event` 的左键分支：在文本选择兜底之前插入一次 `state.collapsible_hit_areas.iter().find(|a| a.rect.contains(pos))`，命中就 `toggle_expandable_at` 并 `return MouseFlow::Handled`。注意不要吃掉双击/三击的选择语义——只在单击（`click_count == 1`）时处理。

`Shift+E`：`IdleShortcut::ExpandAll`，绑定 `KeyCode::Char('E')` + `KeyModifiers::SHIFT`，分支里和 `ExpandToolOutput` 一样要求 composer 为空，否则按字母输入处理。帮助面板的 Composer 分组把 `e` 那条改成 `e · Shift+E  expand latest tool output · expand all`。

- [ ] **Step 4: 运行 + 提交**

Run: 全量 `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): expand a collapsed row by clicking it, or all of them with Shift+E"
```

---

### Task 6: 系统通知可折叠

**Files:**
- Modify: `crates/orca-tui/src/transcript_state.rs` — `System(String)` → `System { text: String, expanded: bool }`
- Modify: 编译器指出的所有构造点（`grep -rn "ChatMessage::System(" crates/orca-tui/src`）
- Modify: `crates/orca-tui/src/ui.rs` — `append_message_lines` 的 System 分支（:3407-3419）
- Modify: `crates/orca-tui/src/types.rs` — `toggle_latest_expandable`、`toggle_expandable_at`、`toggle_all_expandable` 纳入 System
- Modify: `crates/orca-tui/src/transcript_hit.rs` — 可折叠判定纳入 System
- Test: `ui.rs` tests

**Interfaces:**
- Consumes: Task 4 的 `collapsible_hit_areas`、Task 5 的三个 toggle
- Produces: `ChatMessage::System { text, expanded }`

- [ ] **Step 1: 改形状，让编译器指路**

`transcript_state.rs` 改成结构体变体，然后 `cargo build -p orca-tui --tests` 逐个修构造点（一律 `expanded: false`）与匹配点。

- [ ] **Step 2: 写失败测试**

```rust
#[test]
fn a_long_system_notice_collapses_to_its_first_line() {
    let text = "Shell unavailable under the current restricted policy\nno OS-enforced sandbox backend\nrun Orca on a host that permits sandbox enforcement";
    let collapsed = message_text(None, &ChatMessage::System { text: text.into(), expanded: false });
    assert_eq!(collapsed.len(), 2, "first line plus the expand affordance: {collapsed:?}");
    assert!(collapsed[0].starts_with("    ℹ Shell unavailable"), "{collapsed:?}");
    assert!(collapsed[1].contains("+2 lines"), "{collapsed:?}");

    let expanded = message_text(None, &ChatMessage::System { text: text.into(), expanded: true });
    assert_eq!(expanded.len(), 3);

    let flushed = lines_text(&build_lines_for_message_after(
        None,
        &ChatMessage::System { text: text.into(), expanded: false },
        &Theme::named(ThemeName::Dark), 80, 0, true, None,
    ));
    assert_eq!(flushed.len(), 3, "flush must commit every line");
}

#[test]
fn a_short_system_notice_has_no_expand_affordance() {
    let one = message_text(None, &ChatMessage::System { text: "Resumed saved conversation.".into(), expanded: false });
    assert_eq!(one.len(), 1, "{one:?}");
    let two = message_text(None, &ChatMessage::System { text: "a\nb".into(), expanded: false });
    assert_eq!(two.len(), 2, "two lines fit without collapsing: {two:?}");
}
```

- [ ] **Step 3: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- a_long_system_notice a_short_system_notice --test-threads=1`

- [ ] **Step 4: 实现**

System 分支：首行永远是 `"    ℹ {first}"`。行数 `<= 2` 或 `expanded` 或 `force_expand` 时其余行按 `"      {row}"` 全出；否则只出首行加一行 `"    └ +{n} lines · click or e to expand"`（与工具输出的尾行同样式，用 `theme.dim_style()` 的 `└ ` 前缀加 `theme.muted_style()` 的文字）。

三个 toggle 与 `collapsible_hit_areas` 的可折叠判定都加上 System（判定条件是行数 > 2，短通知不该出现命中区）。

- [ ] **Step 5: 运行、重生成黄金、提交**

Run: 全量；`transcript.txt` 黄金里如果含多行 System，会变化——重新生成后读一遍确认折叠符合预期，再不带变量重跑。

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): collapse long system notices like tool output"
```

---

### Task 7: Esc 规则表

**Files:**
- Modify: `crates/orca-tui/src/idle_key_actions.rs`（:122 附近，Backtrack 的 composer-empty 判定）
- Modify: `crates/orca-tui/src/key_event_actions.rs` — 在 preflight 顶部加一段说明注释，把整张表写下来
- Test: `crates/orca-tui/src/key_event_actions.rs` tests

**Interfaces:**
- Produces: 无新类型。唯一的行为变化是"空闲态 + 输入非空 → 清空输入"。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn esc_clears_a_non_empty_composer_before_it_can_backtrack() {
    let mut state = test_state();
    state.status = AppStatus::Idle;
    let mut textarea = make_textarea_with_text("half-written prompt", &VimState::new(false), &theme());
    press_esc_with(&mut state, &mut textarea);
    assert!(textarea.is_empty(), "Esc clears the draft");
    assert!(no_backtrack_was_sent(), "and does not backtrack");
}

#[test]
fn esc_still_backtracks_when_the_composer_is_empty() {
    let mut state = test_state();
    state.status = AppStatus::Idle;
    let mut textarea = make_textarea(&VimState::new(false), &theme());
    press_esc_with(&mut state, &mut textarea);
    assert!(backtrack_was_sent());
}
```

（两个 helper 照本文件现有按键测试的构造方式写。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- esc_clears_a_non_empty_composer esc_still_backtracks --test-threads=1`
Expected: 第一个失败——今天输入非空时 Esc 什么都不做。

- [ ] **Step 3: 实现**

`idle_key_actions.rs` 的 Backtrack 分支：composer 非空时不再什么都不做，而是清空 textarea（复用 `EditorShortcut::ClearInput` 走的那条清空路径，保持撤销行为一致）并返回已处理；为空时维持 `UserAction::Backtrack`。

`key_event_actions.rs` 的 preflight 顶部加注释，按 spec 第 5 节 S4 的五行表写清楚：有弹层 → 关闭最上层；运行/压缩中 → 中断；空闲且输入非空 → 清空；空闲且输入为空 → 回退；会话选择器 → 返回对话。注释里点名两级流水线的顺序（preflight 先于 status），方便后来者不再各自新增 Esc 分支。

- [ ] **Step 4: 加一个把整张表钉住的测试**

```rust
#[test]
fn esc_precedence_table_holds() {
    // one case per row of the table in the module comment
    for (setup, expectation) in esc_table_cases() {
        // …drive Esc through the real router and assert only the expected effect happened
    }
}
```

把它写成显式的若干个 case：选择激活时只清选择、帮助面板打开时只关面板、dock 展开时只收 dock（Task 3）、审批时 deny（Task 2）、运行时中断、空闲非空时清空、空闲为空时回退。每个 case 断言"预期效果发生"且"更下层的效果没有发生"。

- [ ] **Step 5: 运行 + 提交**

Run: 全量 `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): make Esc clear a non-empty draft and document the precedence table"
```

---

### Task 8: Ctrl+Enter 把追问插到队首

**Files:**
- Modify: `crates/orca-tui/src/shortcuts.rs` — `RunningShortcut::SubmitNow` + `KeyBinding::new(KeyCode::Enter, KeyModifiers::CONTROL)` + 帮助文案
- Modify: `crates/orca-tui/src/queued_input_actions.rs` — `SubmitNow` 分支（:179-283）、`enqueue_composer_follow_up_to_runtime`（:74-121）
- Modify: `crates/orca-tui/src/ui.rs` — 队列预览提示行（`queued_preview_lines`）
- Test: `shortcuts.rs` tests、`queued_input_actions.rs` tests

**Interfaces:**
- Consumes: `UserAction::QueuePrompt`、`UserAction::PromptQueueControl(PromptQueueAction::Reorder { expected_revision, ordered_ids })`
- Produces: `RunningShortcut::SubmitNow`；`fn enqueue_composer_follow_up_at_front(...)`

**语义（用户 2026-09-22 确认）：** `Ctrl+Enter` 把当前输入加入队列并立刻发一个 `Reorder`，把它移到队首，于是当前 turn 一结束它第一个跑。它**不**打断正在执行的工具，也**不**在 turn 中途注入。

- [ ] **Step 1: 写失败测试**

`shortcuts.rs` tests：

```rust
#[test]
fn ctrl_enter_submits_now_while_running() {
    assert_eq!(
        running_shortcut(key(KeyCode::Enter, KeyModifiers::CONTROL)),
        Some(RunningShortcut::SubmitNow)
    );
    // plain Enter still queues
    assert_eq!(
        running_shortcut(key(KeyCode::Enter, KeyModifiers::NONE)),
        Some(RunningShortcut::SubmitQueued)
    );
}
```

`queued_input_actions.rs` tests：

```rust
#[test]
fn submit_now_queues_the_prompt_and_moves_it_to_the_front() {
    let (mut state, action_rx) = running_fixture_with_two_queued_items();
    submit_now(&mut state, "urgent follow-up");
    let queued = action_rx.try_recv().expect("QueuePrompt");
    assert!(format!("{queued:?}").contains("QueuePrompt"), "{queued:?}");
    let reorder = action_rx.try_recv().expect("Reorder");
    let debug = format!("{reorder:?}");
    assert!(debug.contains("Reorder"), "{debug}");
    assert!(debug.contains("expected_revision"), "{debug}");
}
```

（fixture 照本文件现有的运行态测试构造。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib -- ctrl_enter_submits_now submit_now_queues --test-threads=1`

- [ ] **Step 3: 实现**

`RunningShortcut` 增加 `SubmitNow`，`RUNNING_BINDINGS` 增加 `Ctrl+Enter`（放在 `SubmitQueued` 的 `Enter` 之前，因为 `match_binding` 要求修饰键完全相等，顺序不影响正确性，但读起来更清楚）。

`handle_running_key` 增加 `SubmitNow` 分支：和 `SubmitQueued` 一样先处理斜杠命令直发的情况，否则调用 `enqueue_composer_follow_up_at_front`——它先走现有的 `enqueue_composer_follow_up_to_runtime` 发 `QueuePrompt`，随后从 `state.queued_submission_view()` 拿当前快照的 `revision` 与 `ordered_ids`，把新条目的 id 移到最前，发 `PromptQueueControl(PromptQueueAction::Reorder { expected_revision, ordered_ids })`。若快照里还看不到新条目（`QueuePrompt` 是异步的），把这次 reorder 记为 pending，在下一次队列快照更新时补发；用一个 `pending_front_prompt: Option<String>` 字段承载，快照里出现同文本的新条目时触发 reorder 并清空。

队列预览提示行（`queued_preview_lines`）改为 `↳ queued N · <文本>  · Ctrl+Enter send now · Alt+↑ edit`。帮助面板 Composer 分组增加 `Ctrl+Enter  send now, skipping the queue`，并在该行后追加一行说明：`needs a terminal with the kitty keyboard protocol`。

- [ ] **Step 4: 终端兼容性说明**

`input_runtime.rs:431-439` 已无条件推送 kitty keyboard 增强标志，所以支持该协议的终端能区分 `Ctrl+Enter`；不支持的终端两者发同样的字节，`Ctrl+Enter` 会退化成普通 `Enter`（排队，不插队）。这是可接受的降级——不要为此新增探测逻辑，只在帮助面板写清楚。在报告里确认你没有引入新的能力探测。

- [ ] **Step 5: 运行 + 提交**

Run: 全量 `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): Ctrl+Enter sends a follow-up to the front of the queue"
```

---

## 自检记录

- **Spec 覆盖**：S1 → Task 1、2；S2 → Task 3；S3 → Task 4、5、6；S4 → Task 7；S5 → Task 8（只做"插到队首"，turn 中途注入不在本计划内，理由见开头偏差表）。
- **占位符**：Task 2、3、5、7、8 里对测试 fixture 标了"照抄邻近测试的构造"，实现者需在对应文件里找现成代码复用；其余步骤给出了完整代码或精确算式。
- **类型一致性**：`CollapsibleHitArea` 与 `message_index_at_row`（Task 4）被 Task 5、6 消费；`toggle_expandable_at` / `toggle_all_expandable`（Task 5）被 Task 6 扩展；`tasks_dock_expanded`（Task 3）被 Task 7 的 Esc 表引用；`InputRegion::Approval`（Task 1）被 Task 7 的 Esc 表引用。
- **依赖顺序**：4 必须在 5 之前，5 必须在 6 之前，1 必须在 2 之前，3 必须在 7 之前。1/3/8 之间互相独立。
