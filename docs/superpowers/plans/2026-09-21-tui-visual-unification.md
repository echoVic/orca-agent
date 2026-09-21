# Orca TUI 方案一（视觉统一）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用一层设计 token 和共享组件统一 Orca TUI 的所有表面，重写 transcript 消息样式、底部 chrome、弹窗、菜单、会话选择器和欢迎页，并顺手修掉 resume 丢工具名、跳底胶囊遮字、setup 按键泄漏三个缺陷。

**Architecture:** 新增 `crates/orca-tui/src/chrome.rs` 承载边框、选中标记、gutter 常量和 `panel_block` / `hint_line` / `option_line` / `rule_line` 组件；`ui.rs` 的渲染函数改为组合这些组件。`append_message_lines` 仍是唯一的消息渲染入口（scrollback 刷入与实时区共用），消息间距通过"看前一条消息的类型"决定。composer 去掉 `Block`，改为上下两条横线加 `›` 提示符，高度仍为"行数 + 2"。`ChatMessage::Reasoning` 变为 `{ text, expanded }`。状态栏与活动行改为多 span 的 `Line`。

**Tech Stack:** Rust 2024，ratatui 0.29（`scrolling-regions`、`unstable-rendered-line-info`），tui-textarea，crossterm，unicode-width。测试用 ratatui `TestBackend` 整帧断言。

**Spec:** `docs/superpowers/specs/2026-09-20-tui-visual-interaction-redesign-design.md`（交互稿：同目录 `…-mock.html`）

## Global Constraints

- 不改 runtime 协议、ACP/JSONL 输出（spec §0）。唯一的 core 改动是 Task 0 的默认模型路由。
- 刷入 scrollback 的内容必须与实时区一致；折叠内容在 `force_expand = true` 时全部展开（spec §1.5、§2.6）。
- `ui.rs`、`chrome.rs`、`shortcuts.rs` 的非测试代码不得出现裸 `Color::` 字面量（`Color::Reset` 除外），颜色一律来自 `Theme`（spec §1.5、§4.1）。
- 容器一律 `BorderType::Rounded`；选中标记一律 `›`；提示行一律由 `chrome::hint_line` 生成（spec §2.1、§4.1）。
- transcript gutter 宽 4 列：`" ›  "`（用户）、`" ●  "`（Orca），续行缩进 4 个空格；工具行缩进 4 列后接图标（spec §4.2）。
- composer 高度 = 可见行数 + 2（上下横线各一行），布局数学与旧边框盒一致（spec §4.3）。
- 方案一不宣传 `Ctrl+Enter`（它在方案二 S5 才绑定，spec §9.5）。
- 测试命令：`cargo test -p orca-tui --lib -- --test-threads=1`（`--lib` 里有 ORCA_HOME 竞争，必须单线程）。提交前 `cargo fmt --all` 和 `cargo clippy -p orca-tui --all-targets -- -D warnings` 必须通过。
- 每个任务一个 commit，commit 信息用英文 conventional commit（`feat(tui): …` / `fix(tui): …` / `test(tui): …`）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `crates/orca-tui/src/chrome.rs` | 设计 token + 共享组件（边框、提示行、选项行、横线、gutter） | 新建 |
| `crates/orca-tui/src/lib.rs` | 注册 `mod chrome;` | 修改 |
| `crates/orca-tui/src/theme.rs` | `accent_style` / `muted_style` / `dim_style` | 修改 |
| `crates/orca-tui/src/vim.rs` | composer 不再挂 `Block`；`status_label()` | 修改 |
| `crates/orca-tui/src/composer_textarea.rs` | placeholder 文案；调用改名后的 `configure_textarea` | 修改 |
| `crates/orca-tui/src/ui.rs` | composer、状态栏、活动行、消息样式、弹窗、菜单、帮助、会话选择器、setup、欢迎页 | 修改 |
| `crates/orca-tui/src/transcript_state.rs` | `ChatMessage::Reasoning { text, expanded }` | 修改 |
| `crates/orca-tui/src/state_reducer.rs`、`types.rs`、`hosted_session.rs`、`surface_projection.rs` | Reasoning 形状；新字段 `vim_mode_label`、`session_picker_show_tests`；`HistoryReplay` | 修改 |
| `crates/orca-tui/src/shortcuts.rs` | `?` 绑定；提示文案；`shortcut_lines` 表格布局 | 修改 |
| `crates/orca-tui/src/composer_input_actions.rs` | 普通字符在有文本时不触发全局快捷键；同步 vim 标签 | 修改 |
| `crates/orca-tui/src/session_picker.rs`、`session_picker_actions.rs` | 过滤 mock 会话、分组行、`Ctrl+T` | 修改 |
| `crates/orca-tui/src/idle_navigation_actions.rs`、`types.rs` | `e` 也能展开 thinking | 修改 |
| `crates/orca-tui/src/golden/*.txt` | 关键画面整帧快照 | 新建 |
| `crates/orca-core/src/model.rs` | auto 默认路由到 deepseek-flash | 修改（Task 0） |

---

### Task 0: 默认模型改为 deepseek-flash

**Files:**
- Modify: `crates/orca-core/src/model.rs:26-33`（`ModelRouteReason`）、`:150-175`（`route`）
- Modify: `crates/orca-tui/src/ui.rs`（新增 `displayed_model_name`，Task 3 与 Task 11 使用）

**Interfaces:**
- Produces: `ModelRouteReason::DefaultFlash`；`pub(crate) fn displayed_model_name(model_name: &str) -> &str`（`ui.rs`）

- [ ] **Step 1: 写失败测试（core）**

在 `crates/orca-core/src/model.rs` 的 `mod tests` 末尾加：

```rust
#[test]
fn unset_and_auto_selections_route_to_flash_by_default() {
    for value in [None, Some(AUTO_MODEL.to_string())] {
        let selection = ModelSelection::parse(value).unwrap();
        let decision = selection.route(ModelRouteContext {
            subagent_type: &SubagentType::default(),
            subagent_model: None,
            has_images: false,
        });
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::DefaultFlash);
    }
}
```

（`SubagentType` 若没有 `Default`，改用本文件里现有 `route` 测试所用的同一个变体。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-core unset_and_auto_selections_route_to_flash_by_default`
Expected: 编译失败，`DefaultFlash` 不存在。

- [ ] **Step 3: 实现**

`ModelRouteReason` 增加变体，旧变体保留以便反序列化历史事件：

```rust
pub enum ModelRouteReason {
    Explicit,
    /// Emitted by releases before the default moved to deepseek-flash; kept so
    /// stored JSONL events still deserialize.
    DefaultPro,
    DefaultFlash,
    SubagentType,
    SubagentOverride,
}
```

`route` 里的默认分支：

```rust
Some(AUTO_MODEL) | None => (FLASH_MODEL.to_string(), ModelRouteReason::DefaultFlash),
```

- [ ] **Step 4: 跑 core 测试并修正旧断言**

Run: `cargo test -p orca-core model`
Expected: 新测试 PASS；凡断言 `PRO_MODEL`/`DefaultPro` 为 auto 结果的旧测试改成 `FLASH_MODEL`/`DefaultFlash`。用 `grep -n "DefaultPro" crates/orca-core/src/model.rs` 定位。

- [ ] **Step 5: TUI 显示助手**

`crates/orca-tui/src/ui.rs` 在 `render` 上方加：

```rust
/// What the user sees for the model: an unset/auto selection shows the model
/// it routes to by default, so the welcome screen and status bar never read "auto".
pub(crate) fn displayed_model_name(model_name: &str) -> &str {
    if model_name == orca_core::model::AUTO_MODEL {
        orca_core::model::FLASH_MODEL
    } else {
        model_name
    }
}

#[cfg(test)]
mod displayed_model_tests {
    #[test]
    fn auto_displays_as_flash_and_explicit_models_pass_through() {
        assert_eq!(super::displayed_model_name("auto"), "deepseek-flash");
        assert_eq!(super::displayed_model_name("deepseek-v4-pro"), "deepseek-v4-pro");
    }
}
```

- [ ] **Step 6: 全量测试与提交**

Run: `cargo test -p orca-core && cargo test -p orca-tui --lib -- --test-threads=1`
Expected: PASS

```bash
git add crates/orca-core/src/model.rs crates/orca-tui/src/ui.rs
git commit -m "feat(model): route auto to deepseek-flash by default"
```

---

### Task 1: chrome.rs 共享组件与 Theme 样式方法（无输出变化）

**Files:**
- Create: `crates/orca-tui/src/chrome.rs`
- Modify: `crates/orca-tui/src/lib.rs`（`mod chrome;`，放在 `mod channels;` 之后按字母序）
- Modify: `crates/orca-tui/src/theme.rs`（`impl Theme` 内）

**Interfaces:**
- Produces（后续所有任务使用）：

```rust
pub(crate) const BORDER: BorderType;                 // Rounded
pub(crate) const MARK_SELECTED: &str;                // "›"
pub(crate) const MARK_IDLE: &str;                    // " "
pub(crate) const GUTTER_WIDTH: usize;                // 4
pub(crate) const GUTTER_CONTINUATION: &str;          // "    "
pub(crate) fn panel_block(theme: &Theme, title: &str, accent: Color) -> Block<'static>;
pub(crate) fn hint_line(theme: &Theme, width: usize, items: &[(&str, &str)]) -> Line<'static>;
pub(crate) fn option_line(theme: &Theme, selected: bool, key: &str, label: &str, label_width: usize, detail: &str, width: usize) -> Line<'static>;
pub(crate) fn rule_line(theme: &Theme, width: u16, focused: bool) -> Line<'static>;
pub(crate) fn gutter(theme: &Theme, glyph: &str, color: Color) -> Span<'static>;
pub(crate) fn dialog_rect(area: Rect, width: u16, content_rows: u16, max_height: u16) -> Rect;
// theme.rs
pub(crate) fn accent_style(&self) -> Style;   // fg = border
pub(crate) fn muted_style(&self) -> Style;    // fg = muted
pub(crate) fn dim_style(&self) -> Style;      // fg = muted + DIM
```

- [ ] **Step 1: 写失败测试**

新建 `crates/orca-tui/src/chrome.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    use super::*;
    use crate::theme::Theme;

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn hint_line_keeps_key_action_pairs_and_drops_from_the_right_when_narrow() {
        let items = [("↑↓", "move"), ("Enter", "confirm"), ("Esc", "cancel")];
        let wide = hint_line(&theme(), 80, &items);
        assert_eq!(text(&wide), "↑↓ move · Enter confirm · Esc cancel");
        let narrow = hint_line(&theme(), 24, &items);
        assert_eq!(text(&narrow), "↑↓ move · Enter confirm");
        assert_eq!(wide.spans[0].style.fg, Some(theme().border));
        assert_eq!(wide.spans[1].style.fg, Some(theme().muted));
    }

    #[test]
    fn option_line_marks_selection_with_background_and_pads_labels() {
        let selected = option_line(&theme(), true, "1", "Allow once", 12, "run it now", 60);
        assert_eq!(text(&selected), "› 1  Allow once    run it now");
        assert_eq!(selected.spans[2].style.bg, Some(theme().selection_bg));
        assert!(selected.spans[2].style.add_modifier.contains(Modifier::BOLD));
        let idle = option_line(&theme(), false, "2", "Deny", 12, "stop", 60);
        assert_eq!(text(&idle), "  2  Deny          stop");
        assert_eq!(idle.spans[2].style.bg, None);
    }

    #[test]
    fn option_line_without_a_key_collapses_the_key_column() {
        let line = option_line(&theme(), false, "", "/new", 8, "Start", 40);
        assert_eq!(text(&line), "  /new      Start");
    }

    #[test]
    fn option_line_truncates_detail_to_width() {
        let line = option_line(&theme(), false, "1", "Allow", 5, "a very long explanation", 24);
        assert_eq!(text(&line), "  1  Allow  a very long…");
    }

    #[test]
    fn rule_line_spans_the_width_and_dims_when_unfocused() {
        let focused = rule_line(&theme(), 10, true);
        assert_eq!(text(&focused), "──────────");
        assert_eq!(focused.spans[0].style.fg, Some(theme().border));
        let unfocused = rule_line(&theme(), 4, false);
        assert!(unfocused.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn gutter_is_four_cells_wide() {
        let span = gutter(&theme(), "›", theme().user);
        assert_eq!(span.content.as_ref(), " ›  ");
    }

    #[test]
    fn dialog_rect_fits_content_and_stays_inside_area() {
        let area = Rect::new(0, 5, 100, 40);
        let rect = dialog_rect(area, 72, 6, 20);
        assert_eq!((rect.width, rect.height), (72, 8));
        assert_eq!(rect.x, 14);
        assert_eq!(rect.y, 5 + (40 - 8) / 2);
        let clamped = dialog_rect(Rect::new(0, 0, 30, 10), 72, 30, 20);
        assert_eq!((clamped.width, clamped.height), (26, 8));
    }
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib chrome -- --test-threads=1`
Expected: 编译失败（模块未注册、函数不存在）。

- [ ] **Step 3: 实现 chrome.rs**

在测试模块上方写入：

```rust
//! Shared visual vocabulary for every TUI surface: one border, one selection
//! marker, one hint-line grammar, one transcript gutter. `ui.rs` composes
//! these instead of hand-rolling blocks and footers per dialog.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
use unicode_width::UnicodeWidthStr;

use crate::display_text::truncate_to_display_width;
use crate::theme::Theme;

pub(crate) const BORDER: BorderType = BorderType::Rounded;
pub(crate) const MARK_SELECTED: &str = "›";
pub(crate) const MARK_IDLE: &str = " ";
/// Transcript gutter: one leading space, a one-cell glyph, two spaces.
pub(crate) const GUTTER_WIDTH: usize = 4;
pub(crate) const GUTTER_CONTINUATION: &str = "    ";
const RULE: &str = "─";
const HINT_SEPARATOR: &str = " · ";

/// Rounded panel with a bold, accent-colored title. Every dialog and side
/// panel goes through here so titles and borders never drift apart.
pub(crate) fn panel_block(theme: &Theme, title: &str, accent: Color) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BORDER)
        .border_style(Style::default().fg(accent));
    if !title.is_empty() {
        block = block.title(Span::styled(
            format!(" {title} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    }
    let _ = theme;
    block
}

/// `↑↓ move · Enter confirm · Esc cancel`: keys in accent, verbs muted. Items
/// are dropped from the right until the line fits `width`.
pub(crate) fn hint_line(theme: &Theme, width: usize, items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (index, (keys, action)) in items.iter().enumerate() {
        let separator = if index == 0 { "" } else { HINT_SEPARATOR };
        let item_width = UnicodeWidthStr::width(separator)
            + UnicodeWidthStr::width(*keys)
            + 1
            + UnicodeWidthStr::width(*action);
        if used + item_width > width {
            break;
        }
        used += item_width;
        if !separator.is_empty() {
            spans.push(Span::styled(separator.to_string(), theme.muted_style()));
        }
        spans.push(Span::styled((*keys).to_string(), theme.accent_style()));
        spans.push(Span::styled(format!(" {action}"), theme.muted_style()));
    }
    Line::from(spans)
}

/// `› 1  label   detail`. The selected row gets the selection background so
/// it still reads on 16-color and monochrome terminals.
pub(crate) fn option_line(
    theme: &Theme,
    selected: bool,
    key: &str,
    label: &str,
    label_width: usize,
    detail: &str,
    width: usize,
) -> Line<'static> {
    let marker = if selected { MARK_SELECTED } else { MARK_IDLE };
    let marker_style = if selected {
        theme.accent_style().add_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };
    let key_style = if selected {
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let label_style = if selected {
        theme
            .selection_style()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let padding = label_width.saturating_sub(UnicodeWidthStr::width(label));
    let padded_label = format!("{label}{}", " ".repeat(padding));
    // An empty key (menus without numbers) collapses its column entirely.
    let key_cell = if key.is_empty() { String::new() } else { format!("{key}  ") };
    let used = UnicodeWidthStr::width(marker)
        + 1
        + UnicodeWidthStr::width(key_cell.as_str())
        + UnicodeWidthStr::width(padded_label.as_str())
        + 2;
    let detail = truncate_to_display_width(detail, width.saturating_sub(used));
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(key_cell, key_style),
        Span::styled(padded_label, label_style),
        Span::styled(format!("  {detail}"), theme.muted_style()),
    ])
}

/// A horizontal rule the width of the area; accent when the surface owns the
/// keyboard, dim otherwise.
pub(crate) fn rule_line(theme: &Theme, width: u16, focused: bool) -> Line<'static> {
    let style = if focused {
        theme.accent_style()
    } else {
        theme.dim_style()
    };
    Line::from(Span::styled(RULE.repeat(usize::from(width)), style))
}

/// Role gutter for transcript rows: `" ›  "`, `" ●  "`, always four cells.
pub(crate) fn gutter(theme: &Theme, glyph: &str, color: Color) -> Span<'static> {
    let _ = theme;
    Span::styled(format!(" {glyph}  "), Style::default().fg(color))
}

/// A centered rectangle sized to `content_rows` plus the two border rows,
/// clamped to `max_height` and to `area` (with a two-cell margin).
pub(crate) fn dialog_rect(area: Rect, width: u16, content_rows: u16, max_height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4)).max(1);
    let height = content_rows
        .saturating_add(2)
        .min(max_height)
        .min(area.height.saturating_sub(2))
        .max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
```

`theme.rs` 的 `impl Theme` 里加：

```rust
pub(crate) fn accent_style(&self) -> Style {
    Style::default().fg(self.border)
}

pub(crate) fn muted_style(&self) -> Style {
    Style::default().fg(self.muted)
}

/// Secondary chrome (rails, rule when unfocused, trailing hints): muted plus
/// DIM so it recedes even on 16-color terminals.
pub(crate) fn dim_style(&self) -> Style {
    Style::default().fg(self.muted).add_modifier(Modifier::DIM)
}
```

`lib.rs`：在 `mod channels;` 后加 `mod chrome;`。

- [ ] **Step 4: 运行测试**

Run: `cargo test -p orca-tui --lib chrome -- --test-threads=1`
Expected: 7 个测试 PASS。`option_line_truncates_detail_to_width` 的期望串来自：marker 1 + 空格 1 + key 1 + 2 + label 5 + 2 = 12 已用，剩 12 列，`truncate_to_display_width` 把 detail 截到 11 列加 `…`。若 `truncate_to_display_width` 的截断规则不同（先看 `display_text.rs` 的测试），以该函数实际行为改期望串，不改函数。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src/chrome.rs crates/orca-tui/src/lib.rs crates/orca-tui/src/theme.rs
git commit -m "feat(tui): add chrome design tokens and shared panel components"
```

---

### Task 2: Composer 改为上下横线 + › 提示符

**Files:**
- Modify: `crates/orca-tui/src/vim.rs:151-180`（`title` 保留给测试，`configure_block` → `configure_textarea`，新增 `status_label`）
- Modify: `crates/orca-tui/src/composer_textarea.rs:56-62`
- Modify: `crates/orca-tui/src/ui.rs`：`render()` 中 `InputRegion::Composer` 分支、`composer_input_height`、`composer_visual_layout`、`textarea_inner_width`、`render_input`、`composer_click_target`
- Test: `crates/orca-tui/src/ui.rs` tests 模块

**Interfaces:**
- Produces:
  - `pub(crate) const COMPOSER_PROMPT: &str = " › ";`（3 列）
  - `pub(crate) fn composer_inner(area: Rect) -> Rect`：`x+3, y+1, width-3, height-2`
  - `fn composer_focused(state: &AppState) -> bool`
  - `fn render_composer(frame, area, textarea, layout, state, theme, show_hardware_cursor)`（替代 `render_input`）
  - `VimState::configure_textarea(&self, textarea, theme)`、`VimState::status_label(&self) -> Option<&'static str>`
- Consumes: `chrome::rule_line`

- [ ] **Step 1: 写失败测试**

在 `ui.rs` tests 里加（`test_state()`、`render`、`TextArea` 已在作用域）：

```rust
#[test]
fn composer_renders_two_rules_and_a_prompt_without_side_borders() {
    let mut state = test_state();
    let theme = Theme::named(ThemeName::Dark);
    let textarea = crate::composer_textarea::make_textarea(&VimState::new(false), &theme);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12))
        .expect("test backend");
    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let input = state.viewport.input_area.expect("composer area");
    assert_eq!(input.height, 3);
    let row = |y: u16| -> String {
        (0..input.width)
            .map(|x| buffer[(input.x + x, y)].symbol().to_string())
            .collect()
    };
    assert_eq!(row(input.y), "─".repeat(60));
    assert_eq!(row(input.y + 2), "─".repeat(60));
    assert!(row(input.y + 1).starts_with(" › "));
    assert!(!row(input.y + 1).contains('│'));
    assert!(!format!("{:?}", buffer).contains("Input"));
}

#[test]
fn composer_click_target_accounts_for_prompt_and_top_rule() {
    let textarea = TextArea::from(["hello world"]);
    let area = Rect::new(0, 20, 60, 3);
    assert_eq!(composer_click_target(&textarea, area, 3, 21), Some((0, 0)));
    assert_eq!(composer_click_target(&textarea, area, 9, 21), Some((0, 6)));
    assert_eq!(composer_click_target(&textarea, area, 9, 20), None);
    assert_eq!(composer_click_target(&textarea, area, 1, 21), None);
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib composer_renders_two_rules composer_click_target_accounts -- --test-threads=1`
Expected: 第一个断言 `row(input.y)` 是 `┌ Input …` 失败；第二个因 `textarea.block()` 为 `Some` 时 inner 从 x+1 起而失败。

- [ ] **Step 3: vim.rs**

```rust
#[cfg_attr(not(test), allow(dead_code))]
pub fn title(&self) -> &'static str { /* 原实现保留 */ }

/// Label for the status bar; `None` when vim mode is off.
pub fn status_label(&self) -> Option<&'static str> {
    if !self.enabled {
        return None;
    }
    Some(match self.mode {
        VimMode::Insert => "INSERT",
        VimMode::Normal => "NORMAL",
        VimMode::Visual => "VISUAL",
    })
}

/// The composer has no block any more: the rules and prompt are drawn by
/// `ui::render_composer`. Only the cursor style follows the vim mode.
pub fn configure_textarea(&self, textarea: &mut TextArea<'_>, theme: &Theme) {
    textarea.set_block(Block::default());
    let cursor_color = match self.mode {
        VimMode::Insert => theme.border,
        VimMode::Normal => theme.warning,
        VimMode::Visual => theme.approval,
    };
    textarea.set_cursor_style(
        Style::default()
            .fg(cursor_color)
            .add_modifier(Modifier::REVERSED),
    );
}
```

`textarea.set_block(Block::default())` 之后 `textarea.block()` 仍是 `Some`（无边框、无标题，inner == area），所以旧的 `block.inner` 逻辑不会再吃掉行列。把文件里所有 `configure_block` 调用（`vim.rs` 的 `reset_insert`、`handle_at`，`composer_textarea.rs` 的 `configure_textarea`）改名为 `configure_textarea`；用 `grep -rn "configure_block" crates/orca-tui/src` 确认为零。

- [ ] **Step 4: composer_textarea.rs**

```rust
fn configure_textarea(textarea: &mut TextArea, vim_state: &VimState, theme: &Theme) {
    textarea.set_placeholder_text("Message Orca…  / commands · @ files · $ skills");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    vim_state.configure_textarea(textarea, theme);
}
```

- [ ] **Step 5: ui.rs composer 几何与渲染**

```rust
pub(crate) const COMPOSER_PROMPT: &str = " › ";
const COMPOSER_PROMPT_WIDTH: u16 = 3;

/// The editable cells of the composer: below the top rule, right of the prompt,
/// above the bottom rule.
pub(crate) fn composer_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(COMPOSER_PROMPT_WIDTH),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(COMPOSER_PROMPT_WIDTH),
        height: area.height.saturating_sub(2),
    }
}

fn composer_focused(state: &AppState) -> bool {
    state.config_dialog.is_none()
        && state.full_access_confirmation.is_none()
        && state.plan_approval_dialog.is_none()
        && !state.show_shortcuts
        && state.image_viewer.is_none()
        && !state.transcript.search.open
}

fn composer_input_height(layout: &TextareaVisualLayout) -> u16 {
    (layout.lines.len().max(1) as u16).saturating_add(2)
}

fn composer_visual_layout(
    area_width: u16,
    textarea: &TextArea,
    theme: &Theme,
) -> TextareaVisualLayout {
    let inner_width = usize::from(area_width.saturating_sub(COMPOSER_PROMPT_WIDTH)).max(1);
    textarea_visual_layout_with_selection(textarea, inner_width, theme.selection_style())
}

fn render_composer(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    layout: &TextareaVisualLayout,
    state: &AppState,
    theme: &Theme,
    show_hardware_cursor: bool,
) {
    if area.height < 3 || area.width <= COMPOSER_PROMPT_WIDTH {
        return;
    }
    let focused = composer_focused(state);
    let rule = crate::chrome::rule_line(theme, area.width, focused);
    frame.render_widget(Paragraph::new(rule.clone()), Rect::new(area.x, area.y, area.width, 1));
    frame.render_widget(
        Paragraph::new(rule),
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
    );
    let prompt_style = if focused {
        theme.accent_style()
    } else {
        theme.dim_style()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(COMPOSER_PROMPT, prompt_style)),
        Rect::new(area.x, area.y + 1, COMPOSER_PROMPT_WIDTH, 1),
    );
    // Transient "copied N chars" feedback sits on the right end of the top rule.
    if let Some(notice) = state.copy_notice_at(std::time::Instant::now()) {
        let text = if notice.local_only {
            format!(" copied {} chars (local clipboard only) ", notice.chars)
        } else {
            format!(" copied {} chars to clipboard ", notice.chars)
        };
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        if text_width + 2 < area.width {
            frame.render_widget(
                Paragraph::new(Span::styled(text, Style::default().fg(theme.approval))),
                Rect::new(area.x + area.width - text_width - 2, area.y, text_width, 1),
            );
        }
    }
    render_textarea_surface(
        frame,
        composer_inner(area),
        textarea,
        Some(layout),
        None,
        theme,
        show_hardware_cursor,
    );
}
```

`render()` 里：`composer_input_height(frame.area().width, textarea, layout)` 改为 `composer_input_height(layout)`；`render_input(...)` 改为 `render_composer(...)`（参数不变）。删除旧 `render_input`。`textarea_inner_width` 若只剩 `composer_visual_layout` 一个调用者则删除。

`composer_click_target` 开头两行改为：

```rust
let inner = composer_inner(area);
```

（去掉 `textarea.block().map(|block| block.inner(area))`。）`render_textarea_block_and_notice` 保留给搜索栏和 setup 的 textarea，不动。

- [ ] **Step 6: 运行新测试和 composer 相关旧测试**

Run: `cargo test -p orca-tui --lib composer -- --test-threads=1`
Expected: 新测试 PASS。以下旧测试会因几何变化失败，按新几何更新：
- `completed_turn_auto_scrolls_markdown_table_tail_above_composer`（`ui.rs` 约 8391 行）和 `overflowing_transcript_keeps_input_and_status_pinned`（约 12445 行）手工构造了 `.title(" Input ")` 的 block；改为 `crate::composer_textarea::make_textarea(&VimState::new(false), &theme)`，并把断言里的 `┌ Input` 改为 `─`。
- `compact_tall_slash_menu_keeps_cursor_on_reversed_composer_cell`、`compact_tall_mention_menu_keeps_cursor_on_reversed_composer_cell`：光标期望列从 `input_area.x + 1` 改为 `input_area.x + 3`，行仍为 `input_area.y + 1`。
- `app_integration_tests.rs` 里 `grep -n "Input" crates/orca-tui/src/app_integration_tests.rs` 找到的断言同样改为横线断言。

- [ ] **Step 7: 全量 + 提交**

Run: `cargo test -p orca-tui --lib -- --test-threads=1 && cargo clippy -p orca-tui --all-targets -- -D warnings`
Expected: PASS

```bash
cargo fmt --all
git add crates/orca-tui/src/vim.rs crates/orca-tui/src/composer_textarea.rs crates/orca-tui/src/ui.rs crates/orca-tui/src/app_integration_tests.rs
git commit -m "feat(tui): draw the composer as two rules with a prompt glyph"
```

---

### Task 3: 状态栏三区、模式 chip、`?` 帮助别名、vim 标签

**Files:**
- Modify: `crates/orca-tui/src/types.rs`（`AppState` 新字段 `pub vim_mode_label: Option<&'static str>`，`AppState::new` 初始化为 `None`）
- Modify: `crates/orca-tui/src/composer_input_actions.rs:54-67`、`:186-206`
- Modify: `crates/orca-tui/src/shortcuts.rs`（`GLOBAL_BINDINGS`、`SHORTCUT_HINTS` 的 F1 条目）
- Modify: `crates/orca-tui/src/ui.rs:4205-4299`（`status_line`）、`workspace_status_spans`、`context_cell`
- Test: `ui.rs` tests、`composer_input_actions.rs` tests

**Interfaces:**
- Produces: `status_line(state, theme, width) -> Line<'static>`（签名不变，内容三区）；`pub(crate) fn sync_vim_mode_label(state: &mut AppState, vim_state: &VimState)`
- Consumes: `displayed_model_name`（Task 0）、`approval_mode_color`

- [ ] **Step 1: 写失败测试**

`ui.rs` tests：

```rust
fn status_text(state: &AppState, width: usize) -> String {
    status_line(state, &Theme::named(ThemeName::Dark), width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn status_line_has_mode_chip_left_and_model_usage_right() {
    let mut state = test_state();
    state.approval_mode = ApprovalMode::AutoEdit;
    state.reasoning_effort = orca_core::config::ReasoningEffort::Max;
    state.model_name = "auto".to_string();
    let text = status_text(&state, 120);
    assert!(text.starts_with(" ⇧Tab auto-edit"), "{text}");
    assert!(text.trim_end().ends_with("? help"), "{text}");
    assert!(text.contains("deepseek-flash · max"), "{text}");
    assert!(!text.contains("F1"), "{text}");
    let line = status_line(&state, &Theme::named(ThemeName::Dark), 120);
    assert_eq!(line.spans[1].style.fg, Some(Theme::named(ThemeName::Dark).approval));
}

#[test]
fn status_line_drops_workspace_before_mode_and_model_when_narrow() {
    let mut state = test_state();
    state.cwd = "/Users/someone/very/long/project/path/that/keeps/going".to_string();
    let wide = status_text(&state, 140);
    assert!(wide.contains("very/long"), "{wide}");
    let narrow = status_text(&state, 60);
    assert!(narrow.starts_with(" ⇧Tab"), "{narrow}");
    assert!(narrow.contains("? help"), "{narrow}");
    assert!(!narrow.contains("very/long"), "{narrow}");
}

#[test]
fn status_line_shows_vim_mode_label_next_to_the_chip() {
    let mut state = test_state();
    state.approval_mode = ApprovalMode::Suggest;
    state.vim_mode_label = Some("NORMAL");
    let text = status_text(&state, 120);
    assert!(text.contains("⇧Tab suggest  NORMAL"), "{text}");
}
```

`composer_input_actions.rs` tests：

```rust
#[test]
fn question_mark_is_typed_when_the_composer_has_text() {
    let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    assert!(composer_editor_shortcut_is_active(key, true, &VimState::new(false)));
    assert!(!composer_editor_shortcut_is_active(key, false, &VimState::new(false)));
}
```

`shortcuts.rs` tests：

```rust
#[test]
fn question_mark_toggles_help() {
    assert_eq!(
        global_shortcut(key(KeyCode::Char('?'), KeyModifiers::NONE)),
        Some(GlobalShortcut::ToggleShortcuts)
    );
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib status_line_has_mode_chip question_mark -- --test-threads=1`
Expected: FAIL（无 `vim_mode_label` 字段编译失败；`?` 未绑定）。

- [ ] **Step 3: 实现**

`types.rs`：`AppState` 加 `pub vim_mode_label: Option<&'static str>`，`new` 里 `vim_mode_label: None`。

`shortcuts.rs` `GLOBAL_BINDINGS` 增加：

```rust
(
    GlobalShortcut::ToggleShortcuts,
    KeyBinding::new(KeyCode::Char('?'), KeyModifiers::NONE),
),
```

`SHORTCUT_HINTS` 里 `keys: "F1 / ctrl+k"` 的条目改为 `keys: "? / F1 / ctrl+k", action: "show this help (input must be empty)"`。

`composer_input_actions.rs`：

```rust
pub(crate) fn composer_editor_shortcut_is_active(
    key: KeyEvent,
    composer_has_text: bool,
    vim_state: &VimState,
) -> bool {
    let insert_like = !vim_state.enabled || vim_state.mode == crate::vim::VimMode::Insert;
    match resolve_shortcut(ShortcutContext::Editor, key) {
        Some(ShortcutAction::Editor(EditorShortcut::VimEscape)) => vim_state.enabled,
        Some(ShortcutAction::Editor(_)) => insert_like && composer_has_text,
        // A printable character with text already present is always typed,
        // never treated as a global shortcut (this is what makes `?` safe).
        _ => {
            composer_has_text
                && insert_like
                && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                && matches!(key.code, KeyCode::Char(_))
        }
    }
}

pub(crate) fn sync_vim_mode_label(state: &mut AppState, vim_state: &VimState) {
    state.vim_mode_label = vim_state.status_label();
}
```

在 `apply_composer_key_input` 里计算完 `changed` 之后（`if changed {` 之前）加一行 `sync_vim_mode_label(state, vim_state);`。再执行 `grep -rn "reset_insert(" crates/orca-tui/src --include=*.rs`（排除 vim.rs 自身定义），每个调用点后面加同一行（调用点都同时持有 `state` 与 `vim_state`）。

`ui.rs` `status_line` 整体替换：

```rust
fn status_line(state: &AppState, theme: &Theme, width: usize) -> Line<'static> {
    let separator = " · ";
    // Left zone: mode chip (or side-conversation label) plus vim mode.
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    if state.side_conversation_active() && let Some(side) = state.side_conversation.as_ref() {
        left.push(Span::styled(
            format!("Side · {} · Ctrl+/ back", side.parent_status.label()),
            Style::default().fg(theme.plan_mode),
        ));
    } else {
        left.push(Span::styled(
            format!("⇧Tab {}", state.approval_mode.as_str()),
            Style::default()
                .fg(approval_mode_color(state.approval_mode, theme))
                .add_modifier(Modifier::BOLD),
        ));
        if state.side_conversation_available() {
            left.push(Span::styled(
                "  Side · Ctrl+/".to_string(),
                Style::default().fg(theme.plan_mode),
            ));
        }
    }
    if let Some(label) = state.vim_mode_label {
        left.push(Span::styled(format!("  {label}"), Style::default().fg(theme.warning)));
    }

    // Right zone, highest priority first: model · effort · ctx · tokens · cost · ? help.
    let mut right: Vec<Span<'static>> = vec![Span::styled(
        format!(
            "{}{separator}{}",
            displayed_model_name(&state.model_name),
            state.reasoning_effort.as_str()
        ),
        theme.muted_style(),
    )];
    if state.context_limit_tokens() > 0 {
        right.push(context_cell(state, theme));
    }
    let usage = state.usage();
    if usage.total_tokens() > 0 {
        right.push(Span::styled(
            format!(
                "{separator}{}{separator}{}",
                format_token_count(usage.total_tokens()),
                format_cost(usage.estimated_cost_usd)
            ),
            theme.muted_style(),
        ));
    }
    right.push(Span::styled(format!("{separator}? help "), theme.muted_style()));

    let span_width = |spans: &[Span<'static>]| -> usize {
        spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    };
    let left_width = span_width(&left);
    let mut right_width = span_width(&right);
    // Drop tokens/cost, then context, then effort before ever touching the chip.
    while left_width + right_width + 2 > width && right.len() > 1 {
        right.remove(right.len() - 2);
        right_width = span_width(&right);
    }

    // Middle zone: cwd · branch, only when there is room for at least 12 cells.
    let mut spans = left;
    let available = width.saturating_sub(left_width + right_width);
    let middle = workspace_status_spans(state, theme, available.saturating_sub(4));
    let middle_width = span_width(&middle);
    if middle_width > 0 && available >= middle_width + 4 {
        let gap = (available - middle_width) / 2;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(middle);
        spans.push(Span::raw(" ".repeat(available - middle_width - gap)));
    } else {
        spans.push(Span::raw(" ".repeat(available)));
    }
    spans.extend(right);
    Line::from(spans)
}
```

`context_cell` 的文案从 `"  ·  context {percent}%"` 改为 `" · ctx {percent}%"`。`workspace_status_spans` 的 `separator` 从 `"  ·  "` 改为 `" · "`，第一个 span 不再带前导分隔符（把 `format!("{separator}{cwd}")` 改为 `cwd`，git 段保留 `format!("{separator}{git}")`）。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib status_line context_cell -- --test-threads=1`
Expected: 新测试 PASS。更新这些旧测试的期望串：`status_line_renders_each_approval_mode_in_its_semantic_color`（chip 文案 `⇧Tab <mode>`，颜色断言取 `spans[1]`）、`status_line_prioritizes_context_workspace_then_usage_and_shortcuts`（`F1 shortcuts` → `? help`，裁剪顺序按上面的 while 循环）、`status_line_reserves_known_context_before_truncating_a_long_model`、`status_line_is_pure_and_deterministic_for_captured_workspace_state`、`responsive_status_line_keeps_mode_and_context_before_optional_metadata`、`status_line_hides_usage_until_tokens_accumulate`、`context_cell_starts_at_full_remaining_capacity`（`"  ·  context 100%"` → `" · ctx 100%"`）。

- [ ] **Step 5: 全量 + 提交**

Run: `cargo test -p orca-tui --lib -- --test-threads=1 && cargo clippy -p orca-tui --all-targets -- -D warnings`

```bash
cargo fmt --all
git add crates/orca-tui/src/types.rs crates/orca-tui/src/composer_input_actions.rs crates/orca-tui/src/shortcuts.rs crates/orca-tui/src/ui.rs
git commit -m "feat(tui): three-zone status bar with mode chip and ? help alias"
```

---

### Task 4: 活动行显示当前工具

**Files:**
- Modify: `crates/orca-tui/src/ui.rs:4339-4530`（`activity_lines`、`foreground_activity_line`、`background_task_activity_lines`、`render_activity`，以及 `render()` 里的 `activity_lines` 用法）
- Test: `ui.rs` tests

**Interfaces:**
- Produces: `fn activity_lines(state, theme) -> Vec<Line<'static>>`；`fn current_tool_label(state: &AppState) -> Option<String>`；`fn render_activity(frame, area, lines: &[Line<'static>])`
- Consumes: `spinner_frame`、`format_elapsed_compact`、`tool_display_name`（在 Task 6 才引入，此任务先内联一个最小版本，见 Step 3）

- [ ] **Step 1: 写失败测试**

把测试助手 `activity_line` 改为返回文本：

```rust
#[cfg(test)]
fn activity_line(state: &AppState, theme: &Theme) -> Option<String> {
    activity_lines(state, theme)
        .into_iter()
        .next()
        .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
}
```

新增测试：

```rust
#[test]
fn running_activity_line_names_the_current_tool_and_how_to_interrupt() {
    let mut state = test_state();
    state.status = AppStatus::Running;
    state.running_started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(12));
    state.tick = 0;
    state.push_message(ChatMessage::ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        target: Some("cargo test -p orca-tui".into()),
        status: "running".into(),
        output: None,
        diff: None,
        kind: None,
        expanded: false,
    });
    let text = activity_line(&state, &Theme::named(ThemeName::Dark)).expect("activity");
    assert_eq!(text, " ⠋ Running 12s · bash cargo test -p orca-tui · Esc interrupt");
}

#[test]
fn running_activity_line_without_a_tool_reports_the_stream_phase() {
    let mut state = test_state();
    state.status = AppStatus::Running;
    state.running_started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
    state.push_message(ChatMessage::Reasoning { text: "hmm".into(), expanded: false });
    let text = activity_line(&state, &Theme::named(ThemeName::Dark)).expect("activity");
    assert_eq!(text, " ⠋ Running 3s · thinking · Esc interrupt");
}
```

（`ChatMessage::Reasoning { .. }` 的结构体形式在 Task 5 才引入；本任务先用 `ChatMessage::Reasoning("hmm".into())` 写第二个测试，Task 5 再改。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib running_activity_line -- --test-threads=1`
Expected: FAIL，现在的文本是 `● running 12s`。

- [ ] **Step 3: 实现**

```rust
/// Name and target of the tool currently executing in the live pane, if any.
fn current_tool_label(state: &AppState) -> Option<String> {
    state.transcript.messages.iter().rev().find_map(|message| match message {
        ChatMessage::ToolCall {
            name,
            target,
            status,
            ..
        } if matches!(status.as_str(), "running" | "receiving") => {
            let name = name.strip_suffix("_file").unwrap_or(name);
            Some(match target {
                Some(target) => format!("{name} {}", truncate_to_display_width(target, 48)),
                None => name.to_string(),
            })
        }
        _ => None,
    })
}

fn stream_phase_label(state: &AppState) -> &'static str {
    match state.transcript.messages.last() {
        Some(ChatMessage::Reasoning(_)) => "thinking",
        Some(ChatMessage::AssistantChunk { .. }) | Some(ChatMessage::Assistant(_)) => "writing",
        _ => "working",
    }
}

fn foreground_activity_line(state: &AppState, theme: &Theme) -> Option<Line<'static>> {
    let spinner = spinner_frame(state.tick);
    match &state.status {
        AppStatus::Idle | AppStatus::Setup | AppStatus::SessionPicker => None,
        AppStatus::Running => {
            let live_elapsed = state
                .running_started_at
                .map(|started| started.elapsed().as_secs())
                .unwrap_or_default();
            let persisted_goal_elapsed = state
                .current_goal()
                .filter(|goal| goal.status.should_continue())
                .map(|goal| goal.time_used_seconds.max(0) as u64)
                .unwrap_or_default();
            let elapsed =
                format_elapsed_compact(persisted_goal_elapsed.saturating_add(live_elapsed));
            let detail = current_tool_label(state)
                .unwrap_or_else(|| stream_phase_label(state).to_string());
            Some(Line::from(vec![
                Span::styled(format!(" {spinner} Running {elapsed}"), Style::default().fg(theme.warning)),
                Span::styled(format!(" · {detail}"), theme.muted_style()),
                Span::styled(" · Esc interrupt".to_string(), theme.dim_style()),
            ]))
        }
        AppStatus::Compacting => Some(Line::from(Span::styled(
            format!(" {spinner} Compacting context…"),
            Style::default().fg(theme.warning),
        ))),
        AppStatus::WaitingApproval => Some(Line::from(Span::styled(
            " ● Waiting for your approval".to_string(),
            Style::default().fg(theme.approval),
        ))),
        AppStatus::WaitingUserInput => Some(Line::from(Span::styled(
            " ● Waiting for your answer".to_string(),
            Style::default().fg(theme.approval),
        ))),
    }
}
```

`activity_lines` 返回 `Vec<Line<'static>>`：图片粘贴分支改为 `Line::from(Span::styled(" ● reading image…", Style::default().fg(theme.warning)))`；`background_task_activity_lines` 的每个 `(String, Color)` 改为 `Line::from(Span::styled(format!(" {text}"), Style::default().fg(color)))`（保持原文案）。`render_activity` 签名改为 `lines: &[Line<'static>]`，去掉 `format!(" {text}")` 包装。`render()` 中的 `activity_lines.len()` 逻辑不变。

- [ ] **Step 4: 更新旧断言**

Run: `cargo test -p orca-tui --lib activity_line -- --test-threads=1`
以下测试把 `"● running 1m 05s"` 一类的期望改成 `" ⠋ Running 1m 05s · working · Esc interrupt"`（tick 为 0 时 spinner 是 `⠋`；Goal 相关测试同理）：`running_activity_line_shows_elapsed_time`、`active_goal_activity_line_adds_persisted_and_live_elapsed_time`、`active_goal_activity_line_never_decreases_across_continuations`、`inactive_goal_does_not_change_the_current_turn_timer`、`active_goal_activity_line_clamps_negative_persisted_time`、`compacting_activity_line_shows_context_status`、`idle_has_no_activity_line`。

- [ ] **Step 5: 全量 + 提交**

```bash
cargo test -p orca-tui --lib -- --test-threads=1 && cargo fmt --all
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): activity line shows the running tool and interrupt hint"
```

---

### Task 5: 消息样式 A：gutter、间距规则、thinking 折叠、系统通知

**Files:**
- Modify: `crates/orca-tui/src/transcript_state.rs:21`（`Reasoning { text: String, expanded: bool }`）
- Modify: `crates/orca-tui/src/state_reducer.rs:231-240`、`:937`
- Modify: `crates/orca-tui/src/types.rs:1416`、`:1329-1350`（`toggle_latest_tool_output`）
- Modify: `crates/orca-tui/src/hosted_session.rs:721`、`crates/orca-tui/src/surface_projection.rs:667`、`:2807`
- Modify: `crates/orca-tui/src/ui.rs`：`build_lines_for_messages`、`build_lines_for_message`、`append_message_lines`（User/Reasoning/Assistant/AssistantChunk/System 分支）、`render_live_messages` 的 cache 闭包
- Test: `ui.rs` tests

**Interfaces:**
- Produces:
  - `ChatMessage::Reasoning { text: String, expanded: bool }`
  - `pub(crate) fn build_lines_for_message_after(previous: Option<&ChatMessage>, message: &ChatMessage, theme, width, tick, force_expand, refined_diff) -> Vec<Line<'static>>`（`build_lines_for_message` 保留，等价于 `previous = None`）
  - `fn leading_blank(previous: Option<&ChatMessage>, message: &ChatMessage) -> bool`
  - `AppState::toggle_latest_expandable()`（替代 `toggle_latest_tool_output`，同时覆盖 ToolCall 与 Reasoning）

- [ ] **Step 1: 改 Reasoning 形状（先让编译器指路）**

`transcript_state.rs`：`Reasoning(String)` → `Reasoning { text: String, expanded: bool }`。然后 `cargo build -p orca-tui` 按编译错误逐个改：

- `state_reducer.rs:231-240`：`Some(ChatMessage::Reasoning { .. })`；`let ChatMessage::Reasoning { text: existing, .. } = message else { unreachable!() }; existing.push_str(&text);`；`self.push_message(ChatMessage::Reasoning { text, expanded: false });`
- `state_reducer.rs:937`：`if let Some(ChatMessage::Reasoning { text, .. }) = …`
- `types.rs:1416`：`ChatMessage::Reasoning { .. }`
- `hosted_session.rs:721`：`vec![ChatMessage::Reasoning { text: reasoning, expanded: false }]`
- `surface_projection.rs:667` 与 `:2807`：同样改为结构体形式，`expanded: false`
- `ui.rs:2956`：`ChatMessage::Reasoning { text, expanded } =>`
- 测试里所有 `ChatMessage::Reasoning("…".into())` 改为 `ChatMessage::Reasoning { text: "…".into(), expanded: false }`（`grep -rn "ChatMessage::Reasoning(" crates/orca-tui/src`）。

Run: `cargo build -p orca-tui --tests`
Expected: 通过。

- [ ] **Step 2: 写失败测试**

```rust
fn lines_text(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
}

fn message_text(previous: Option<&ChatMessage>, message: &ChatMessage) -> Vec<String> {
    lines_text(&build_lines_for_message_after(
        previous,
        message,
        &Theme::named(ThemeName::Dark),
        80,
        0,
        false,
        None,
    ))
}

#[test]
fn user_message_gets_a_gutter_and_a_leading_blank_after_any_message() {
    let user = ChatMessage::User("hello".into());
    assert_eq!(message_text(None, &user), vec![" ›  hello"]);
    let after = message_text(Some(&ChatMessage::Assistant("x".into())), &user);
    assert_eq!(after, vec!["", " ›  hello"]);
}

#[test]
fn assistant_message_gets_a_gutter_and_indented_continuation() {
    let message = ChatMessage::Assistant("first line\n\nsecond paragraph".into());
    let text = message_text(None, &message);
    assert_eq!(text[0], " ●  first line");
    assert!(text.iter().skip(1).all(|line| line.is_empty() || line.starts_with("    ")), "{text:?}");
    let after_tool = message_text(
        Some(&ChatMessage::ToolCall {
            id: "1".into(), name: "bash".into(), target: None, status: "completed".into(),
            output: None, diff: None, kind: None, expanded: false,
        }),
        &message,
    );
    assert_eq!(after_tool[0], "");
}

#[test]
fn reasoning_collapses_to_one_line_and_expands_on_request() {
    let long = "The user asks something.\nSecond thought here.\nThird.".to_string();
    let collapsed = message_text(None, &ChatMessage::Reasoning { text: long.clone(), expanded: false });
    assert_eq!(collapsed.len(), 1);
    assert!(collapsed[0].starts_with("    ⋯ thinking · The user asks something."), "{collapsed:?}");
    let expanded = message_text(None, &ChatMessage::Reasoning { text: long.clone(), expanded: true });
    assert_eq!(expanded.len(), 3);
    assert_eq!(expanded[1], "      Second thought here.");
    let flushed = lines_text(&build_lines_for_message_after(
        None, &ChatMessage::Reasoning { text: long, expanded: false },
        &Theme::named(ThemeName::Dark), 80, 0, true, None,
    ));
    assert_eq!(flushed.len(), 3, "flush must commit the full text");
}

#[test]
fn system_notice_is_a_single_info_row_with_indented_detail() {
    let text = message_text(None, &ChatMessage::System("Resumed saved conversation.\nmodel deepseek-flash".into()));
    assert_eq!(text, vec!["    ℹ Resumed saved conversation.", "      model deepseek-flash"]);
}

#[test]
fn e_toggles_the_latest_reasoning_when_no_tool_follows_it() {
    let mut state = test_state();
    state.push_message(ChatMessage::Reasoning { text: "a\nb".into(), expanded: false });
    assert!(state.toggle_latest_expandable());
    assert!(matches!(state.transcript.messages[0], ChatMessage::Reasoning { expanded: true, .. }));
}
```

- [ ] **Step 3: 运行，确认失败**

Run: `cargo test -p orca-tui --lib gutter reasoning_collapses system_notice_is e_toggles -- --test-threads=1`
Expected: 编译失败（`build_lines_for_message_after`、`toggle_latest_expandable` 不存在）。

- [ ] **Step 4: 实现间距规则与新入口**

```rust
/// Whether `message` starts with a blank separator row. Decided from the
/// previous message's kind only: kinds never change once a later message
/// exists, so the per-message render cache stays valid.
fn leading_blank(previous: Option<&ChatMessage>, message: &ChatMessage) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let previous_ends_blank = matches!(
        previous,
        ChatMessage::AssistantChunk { trailing_blank: true, .. }
    );
    if previous_ends_blank {
        return false;
    }
    match message {
        ChatMessage::User(_)
        | ChatMessage::Diagnostic(_)
        | ChatMessage::Error(_)
        | ChatMessage::System(_)
        | ChatMessage::ProposedPlan(_)
        | ChatMessage::PlanUpdate { .. } => true,
        ChatMessage::Assistant(_) | ChatMessage::AssistantChunk { .. } => matches!(
            previous,
            ChatMessage::ToolCall { .. }
                | ChatMessage::Diagnostic(_)
                | ChatMessage::Error(_)
                | ChatMessage::System(_)
                | ChatMessage::User(_)
                | ChatMessage::Image(_)
        ),
        ChatMessage::ToolCall { .. } => matches!(
            previous,
            ChatMessage::Assistant(_)
                | ChatMessage::AssistantChunk { .. }
                | ChatMessage::Reasoning { .. }
                | ChatMessage::User(_)
        ),
        ChatMessage::Reasoning { .. } | ChatMessage::Image(_) => false,
    }
}

pub(crate) fn build_lines_for_message_after(
    previous: Option<&ChatMessage>,
    message: &ChatMessage,
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
    refined_diff: Option<&crate::diff_highlight::RefinedDiffStyles>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if leading_blank(previous, message) {
        lines.push(Line::from(""));
    }
    append_message_lines(&mut lines, previous, message, theme, width, tick, force_expand, refined_diff);
    lines
}
```

`build_lines_for_message` 改为调用 `build_lines_for_message_after(None, …)`。`build_lines_for_messages` 改为：

```rust
let mut previous = None;
for msg in messages {
    lines.extend(build_lines_for_message_after(previous, msg, theme, width, tick, force_expand, None));
    previous = Some(msg);
}
```

`render_live_messages` 的 cache 闭包：

```rust
|index, message, theme, width, tick, force_expand| {
    let refined = AppState::refined_diff_styles_for_message(revisions, highlights, index, message);
    let previous = index.checked_sub(1).and_then(|i| messages.get(i));
    build_lines_for_message_after(previous, message, theme, width, tick, force_expand, refined)
}
```

- [ ] **Step 5: 实现各分支样式**

`append_message_lines` 中：

```rust
ChatMessage::User(text) => {
    let text = compact_long_text(text, width.saturating_sub(GUTTER_WIDTH).max(1), 3);
    let mut rows = text.lines();
    let first = rows.next().unwrap_or_default().to_string();
    lines.push(Line::from(vec![
        crate::chrome::gutter(theme, "›", theme.user).patch_style(Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(first, Style::default().fg(theme.user)),
    ]));
    for row in rows {
        lines.push(Line::from(vec![
            Span::raw(GUTTER_CONTINUATION),
            Span::styled(row.to_string(), Style::default().fg(theme.user)),
        ]));
    }
}
ChatMessage::Reasoning { text, expanded } => {
    append_reasoning_lines(lines, text, *expanded, force_expand, width, theme);
}
ChatMessage::Assistant(text) => {
    append_assistant_markdown(lines, text, width, theme, false);
}
ChatMessage::AssistantChunk { text, trailing_blank } => {
    append_assistant_markdown(lines, text, width, theme, *trailing_blank);
}
ChatMessage::System(text) => {
    let mut rows = text.lines();
    let first = rows.next().unwrap_or_default();
    lines.push(Line::from(vec![
        Span::raw(GUTTER_CONTINUATION),
        Span::styled(format!("ℹ {first}"), theme.muted_style()),
    ]));
    for row in rows {
        lines.push(Line::from(Span::styled(format!("      {row}"), theme.muted_style())));
    }
}
```

（`use crate::chrome::{GUTTER_CONTINUATION, GUTTER_WIDTH};`）

助手正文的 gutter：`append_assistant_markdown` 改为先 `render_markdown(text, width.saturating_sub(GUTTER_WIDTH), theme)`，再给第一行前置 `gutter(theme, "●", theme.border)`，其余行前置 `Span::raw(GUTTER_CONTINUATION)`；空行保持为空（不加缩进）。**注意**：流式 `AssistantChunk` 只有一个 turn 的第一个 chunk 才该带 `●`。判定方式：`append_assistant_markdown` 增加参数 `first_of_turn: bool`，`Assistant` 传 `true`；`AssistantChunk` 传 `!matches!(previous, Some(ChatMessage::AssistantChunk { .. }))`——因此 `append_message_lines` 的签名改为 `append_message_lines(lines, previous: Option<&ChatMessage>, msg, theme, width, tick, force_expand, refined_diff)`，由 `build_lines_for_message_after` 传入（上面的调用已按此写）。

```rust
fn append_reasoning_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    expanded: bool,
    force_expand: bool,
    width: usize,
    theme: &Theme,
) {
    let style = theme.muted_style().add_modifier(Modifier::ITALIC);
    let mut rows = text.lines().filter(|row| !row.trim().is_empty());
    let first = rows.next().unwrap_or_default();
    let prefix = "    ⋯ thinking · ";
    if !(expanded || force_expand) {
        let budget = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(truncate_to_display_width(first, budget), style),
        ]));
        return;
    }
    lines.push(Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(first.to_string(), style),
    ]));
    let limit = if force_expand { usize::MAX } else { 11 };
    for row in rows.take(limit) {
        lines.push(Line::from(Span::styled(format!("      {row}"), style)));
    }
}
```

`types.rs`：把 `toggle_latest_tool_output` 改名为 `toggle_latest_expandable`，`rposition` 的谓词改为 `matches!(message, ChatMessage::ToolCall { .. } | ChatMessage::Reasoning { .. })`，`mutate_message` 的 match 增加 `ChatMessage::Reasoning { expanded, .. } => *expanded = !*expanded,`。`idle_navigation_actions.rs:63` 的调用改名。`grep -rn "toggle_latest_tool_output" crates/orca-tui/src` 清零。

- [ ] **Step 6: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`
Expected: 新测试 PASS。旧断言按新格式更新：凡期望以 `"> "` 开头的用户行改为 `" ›  "`；`"[thinking] "` 改为 `"    ⋯ thinking · "`（`completed_table_tail_state` 等）；助手正文首行现在带 `" ●  "` 前缀、其余行带 4 空格，凡对 `rendered[0]` 做 `starts_with("# ")` 之类断言的加上前缀；`welcome_screen_text_is_selectable_and_copyable` 不受影响。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): role gutters, spacing rules and collapsible thinking in the transcript"
```

---

### Task 6: 消息样式 B：工具行、输出左栏、diff 摘要、诊断卡片

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`：`append_message_lines` 的 `ToolCall` 分支、`append_tool_output_lines`、`append_diff_lines`、`append_diagnostic_lines`、`append_labeled_diagnostic_text`、`append_workflow_draft_preview_lines`
- Test: `ui.rs` tests

**Interfaces:**
- Produces: `pub(crate) fn tool_display_name(name: &str) -> &str`；`fn diff_summary(diff: &str) -> (usize, usize)`
- Consumes: `theme.dim_style()`、`GUTTER_CONTINUATION`

- [ ] **Step 1: 写失败测试**

```rust
fn tool_call(name: &str, target: Option<&str>, status: &str, output: Option<&str>, expanded: bool) -> ChatMessage {
    ChatMessage::ToolCall {
        id: "call-1".into(), name: name.into(), target: target.map(str::to_string), status: status.into(),
        output: output.map(str::to_string), diff: None, kind: None, expanded,
    }
}

#[test]
fn tool_rows_show_short_name_target_and_a_rail_for_output() {
    let text = message_text(None, &tool_call("read_file", Some("src/main.rs"), "completed", Some("l1\nl2\nl3\nl4"), false));
    assert_eq!(text[0], "    ✓ read  src/main.rs");
    assert_eq!(text[1], "    │ l1");
    assert_eq!(text[2], "    │ l2");
    assert_eq!(text[3], "    └ +2 lines · e to expand");
    let expanded = message_text(None, &tool_call("read_file", Some("src/main.rs"), "completed", Some("l1\nl2\nl3\nl4"), true));
    assert_eq!(expanded.last().unwrap(), "    └ 4 lines · e to collapse");
}

#[test]
fn tool_rows_only_spell_out_non_completed_statuses() {
    let running = message_text(None, &tool_call("bash", Some("cargo test"), "running", None, false));
    assert_eq!(running[0], "    ⠋ bash  cargo test");
    let cancelled = message_text(None, &tool_call("bash", Some("cargo test"), "cancelled", None, false));
    assert_eq!(cancelled[0], "    × bash  cargo test · interrupted");
    let failed = message_text(None, &tool_call("bash", None, "failed", Some("boom"), false));
    assert_eq!(failed[0], "    ✗ bash · failed");
    assert_eq!(failed[1], "    │ boom");
}

#[test]
fn tool_id_placeholders_render_as_plain_tool() {
    let text = message_text(None, &tool_call("tool:call_00_abc", None, "completed", None, false));
    assert_eq!(text[0], "    ✓ tool");
}

#[test]
fn flushed_tool_output_commits_every_line_without_a_stub() {
    let lines = lines_text(&build_lines_for_message_after(
        None, &tool_call("bash", None, "completed", Some("a\nb\nc\nd\ne"), false),
        &Theme::named(ThemeName::Dark), 80, 0, true, None,
    ));
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().skip(1).all(|line| line.starts_with("    │ ")), "{lines:?}");
}

#[test]
fn edit_tool_rows_carry_a_diff_summary() {
    let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n context\n";
    let message = ChatMessage::ToolCall {
        id: "1".into(), name: "edit".into(), target: Some("src/main.rs".into()), status: "completed".into(),
        output: None, diff: Some(diff.into()), kind: None, expanded: false,
    };
    let text = message_text(None, &message);
    assert_eq!(text[0], "    ✎ edit  src/main.rs  +2 −1");
    assert!(text[1..].iter().all(|line| line.starts_with("    │ ")), "{text:?}");
}

#[test]
fn diagnostics_render_as_an_icon_title_with_indented_cause_and_next() {
    let text = message_text(None, &ChatMessage::Error("DeepSeek provider error: 429 rate limit exceeded".into()));
    assert!(text[0].starts_with("    ✗ "), "{text:?}");
    assert!(text[0].ends_with("[provider.failed]"), "{text:?}");
    assert!(text[1].starts_with("      cause  "), "{text:?}");
    assert!(text.iter().any(|line| line.starts_with("      next   ")), "{text:?}");
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib tool_rows tool_id_placeholders flushed_tool_output edit_tool_rows diagnostics_render -- --test-threads=1`

- [ ] **Step 3: 实现**

```rust
/// Short, human tool names for transcript rows. Unknown names pass through;
/// `tool:<id>` placeholders (history without a recorded name) become "tool".
pub(crate) fn tool_display_name(name: &str) -> &str {
    if name.starts_with("tool:") {
        return "tool";
    }
    match name {
        "read_file" => "read",
        "write_file" => "write",
        "list_files" => "ls",
        "git_status" => "git status",
        "ask_user_question" => "ask",
        "update_plan" => "plan",
        "web_search" => "search",
        "task_read_output" => "task output",
        "task_send_input" => "task input",
        "task_wait" => "task wait",
        "task_stop" => "task stop",
        "task_list" => "tasks",
        "subagent_message" => "agent message",
        "WorkflowDraft" | "workflow_draft" => "workflow draft",
        "WorkflowDraftAction" => "workflow action",
        "Workflow" => "workflow",
        other => other,
    }
}

fn diff_summary(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}
```

`ToolCall` 分支：

```rust
ChatMessage::ToolCall { name, target, status, output, diff, kind, expanded, .. } => {
    let neutral_completed =
        status == "completed" && matches!(kind.as_deref(), Some("empty" | "no_matches"));
    let is_edit = diff.is_some() || matches!(name.as_str(), "edit" | "write_file");
    let icon = match status.as_str() {
        "completed" if is_edit => "✎",
        "completed" => "✓",
        "running" | "receiving" => spinner_frame(tick),
        "denied" | "failed" => "✗",
        "cancelled" => "×",
        "indeterminate" => "?",
        _ => "·",
    };
    let color = match status.as_str() {
        "completed" if neutral_completed => theme.muted,
        "completed" if is_edit => theme.warning,
        "completed" => theme.success,
        "running" | "receiving" => theme.warning,
        "denied" | "failed" => theme.error,
        "cancelled" | "indeterminate" => theme.warning,
        _ => theme.muted,
    };
    let status_suffix = match status.as_str() {
        "completed" | "running" | "receiving" => None,
        "cancelled" => Some("interrupted"),
        "indeterminate" => Some("state unknown"),
        other => Some(other),
    };
    let mut spans = vec![
        Span::raw(GUTTER_CONTINUATION),
        Span::styled(format!("{icon} "), Style::default().fg(color)),
        Span::styled(tool_display_name(name).to_string(), Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
    ];
    let used = GUTTER_WIDTH + 2 + UnicodeWidthStr::width(tool_display_name(name));
    if let Some(target) = target {
        let budget = width.saturating_sub(used + 2 + 24).max(8);
        spans.push(Span::styled(format!("  {}", truncate_to_display_width(target, budget)), theme.muted_style()));
    }
    if let Some(diff) = diff {
        let (added, removed) = diff_summary(diff);
        spans.push(Span::styled(format!("  +{added} "), Style::default().fg(theme.diff_add)));
        spans.push(Span::styled(format!("−{removed}"), Style::default().fg(theme.diff_remove)));
    }
    if let Some(suffix) = status_suffix {
        spans.push(Span::styled(format!(" · {suffix}"), Style::default().fg(color)));
    }
    lines.push(Line::from(spans));
    if let Some(out) = output {
        if !is_workflow_draft_tool(name)
            || !append_workflow_draft_preview_lines(lines, out, *expanded, force_expand, theme)
        {
            append_tool_output_lines(lines, out, *expanded, force_expand, theme);
        }
    }
    if let Some(diff) = diff {
        append_diff_lines(lines, diff, theme, refined_diff);
    }
}
```

（`+2 −1` 断言里 `+{added} ` 后接 `−{removed}`，中间正好一个空格。）

```rust
fn append_tool_output_lines(lines: &mut Vec<Line<'static>>, output: &str, expanded: bool, force_expand: bool, theme: &Theme) {
    let total = output.lines().count();
    let shown = if force_expand { usize::MAX } else if expanded { 40 } else { 2 };
    let rail = Span::styled("    │ ".to_string(), theme.dim_style());
    for line in output.lines().take(shown) {
        lines.push(Line::from(vec![rail.clone(), Span::styled(line.to_string(), theme.muted_style())]));
    }
    if force_expand {
        return;
    }
    let hidden = total.saturating_sub(shown);
    let tail = if hidden > 0 {
        format!("+{hidden} lines · e to expand")
    } else if expanded && total > 2 {
        format!("{total} lines · e to collapse")
    } else {
        return;
    };
    lines.push(Line::from(vec![
        Span::styled("    └ ".to_string(), theme.dim_style()),
        Span::styled(tail, theme.muted_style()),
    ]));
}

fn append_diff_lines(lines: &mut Vec<Line<'static>>, diff: &str, theme: &Theme, refined: Option<&crate::diff_highlight::RefinedDiffStyles>) {
    let rail = Span::styled("    │ ".to_string(), theme.dim_style());
    for mut line in crate::diff_highlight::render_unified_diff(diff, theme, refined) {
        line.spans.insert(0, rail.clone());
        lines.push(line);
    }
}

fn append_diagnostic_lines(lines: &mut Vec<Line<'static>>, diagnostic: &TuiDiagnostic, theme: &Theme) {
    let (icon, accent) = match diagnostic.level() {
        DiagnosticLevel::Error => ("✗", theme.error),
        DiagnosticLevel::Warning => ("⚠", theme.warning),
        DiagnosticLevel::Info => ("ℹ", theme.muted),
    };
    lines.push(Line::from(vec![
        Span::raw(GUTTER_CONTINUATION),
        Span::styled(format!("{icon} "), Style::default().fg(accent)),
        Span::styled(diagnostic.title().to_string(), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  [{}]", diagnostic.code()), theme.dim_style()),
    ]));
    append_labeled_diagnostic_text(lines, "cause", diagnostic.detail(), theme);
    if let Some(action) = diagnostic.action() {
        append_labeled_diagnostic_text(lines, "next", action, theme);
    }
}

fn append_labeled_diagnostic_text(lines: &mut Vec<Line<'static>>, label: &str, text: &str, theme: &Theme) {
    for (index, line) in text.lines().enumerate() {
        let label = if index == 0 { format!("      {label:<5}  ") } else { "             ".to_string() };
        lines.push(Line::from(vec![
            Span::styled(label, theme.dim_style()),
            Span::styled(line.to_string(), theme.muted_style()),
        ]));
    }
}
```

`append_workflow_draft_preview_lines` 里的四空格缩进改成 `    │ ` 左栏，其余不变。Task 4 的 `current_tool_label` 里 `name.strip_suffix("_file").unwrap_or(name)` 改为 `tool_display_name(name)`，让活动行和工具行用同一套短名。`ChatMessage::Error` 分支不再自己追加空行（间距由 Task 5 的规则决定）。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`
更新：`terminal_tool_rows_render_interrupted_and_state_unknown_labels`（`(interrupted)` → `· interrupted`）、`long_plan_steps_and_tool_targets_stay_on_single_rows`（`ends_with("(completed)")` → 行首 `"    ✓ "` 且不含 `(completed)`）、`errors_render_cause_next_step_and_stable_diagnostic_code`（code 现在在标题行尾 `[…]`，`Cause:`/`Next:` 改为小写标签）、`workflow_draft_tool_output_renders_a_compact_preview_and_expandable_script`（缩进改为左栏）、`[+N lines]` 相关断言改为 `└ +N lines · e to expand`。`app_integration_tests.rs` 里 `grep -n "(completed)\|\[+" crates/orca-tui/src/app_integration_tests.rs` 找到的同样处理。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): tool rows with short names, output rails, diff summaries and diagnostic cards"
```

---

### Task 7: resume 恢复工具名、跳底胶囊不遮字、setup 按键不泄漏

**Files:**
- Modify: `crates/orca-tui/src/hosted_session.rs:580-585`（调用点）、`:695-775`（`chat_messages_from_history` → `HistoryReplay`）
- Modify: `crates/orca-tui/src/ui.rs:1590-1612`（跳底胶囊）
- Modify: `crates/orca-tui/src/setup_actions.rs`（Enter 完成 setup 后清空 composer）
- Test: `hosted_session.rs` tests、`ui.rs` tests、`setup_actions.rs` tests

**Interfaces:**
- Produces: `pub(crate) struct HistoryReplay { tool_names: HashMap<String, (String, Option<String>)> }` 与 `fn convert(&mut self, message: Message) -> Vec<ChatMessage>`；`chat_messages_from_history(message)` 保留为无状态包装

- [ ] **Step 1: 写失败测试（hosted_session.rs tests）**

```rust
#[test]
fn history_replay_restores_tool_names_and_targets_from_assistant_calls() {
    use orca_core::conversation::{Message, RawToolCall};
    let mut replay = HistoryReplay::default();
    let assistant = Message::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: vec![RawToolCall {
            id: "call_1".into(),
            function_name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        }],
        ..Message::assistant_default_for_tests()
    };
    let _ = replay.convert(assistant);
    let tool = Message::Tool {
        tool_call_id: "call_1".into(),
        content: "ok".into(),
        terminal: None,
        ..Message::tool_default_for_tests()
    };
    let messages = replay.convert(tool);
    let ChatMessage::ToolCall { name, target, .. } = &messages[0] else { panic!("tool call") };
    assert_eq!(name, "bash");
    assert_eq!(target.as_deref(), Some("cargo test"));
}
```

（`Message::Assistant`/`Message::Tool` 的完整字段以 `orca_core::conversation::Message` 定义为准；若没有测试用的默认构造器，就按现有 `chat_message_from_history` 测试里的写法列出全部字段，不要新增 `..default` 语法糖。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib history_replay_restores -- --test-threads=1`

- [ ] **Step 3: 实现 HistoryReplay**

```rust
/// Stateful history → transcript conversion. Assistant messages announce tool
/// calls (id, name, arguments); the matching tool result only carries the id,
/// so the name and target have to be remembered across messages.
#[derive(Default)]
pub(crate) struct HistoryReplay {
    tool_names: HashMap<String, (String, Option<String>)>,
}

fn tool_target_from_arguments(name: &str, arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let key = match name {
        "bash" | "bash_wait" => "command",
        "grep" | "glob" => "pattern",
        "subagent" => "description",
        "ask_user_question" => return None,
        _ => "path",
    };
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

impl HistoryReplay {
    pub(crate) fn convert(&mut self, message: Message) -> Vec<ChatMessage> {
        if let Message::Assistant { tool_calls, .. } = &message {
            for call in tool_calls {
                self.tool_names.insert(
                    call.id.clone(),
                    (call.function_name.clone(), tool_target_from_arguments(&call.function_name, &call.arguments)),
                );
            }
        }
        let tool_call_id = match &message {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        };
        let mut messages = chat_messages_from_history(message);
        if let Some(id) = tool_call_id
            && let Some((name, target)) = self.tool_names.remove(&id)
            && let Some(ChatMessage::ToolCall { name: slot, target: target_slot, .. }) = messages.first_mut()
        {
            *slot = name;
            *target_slot = target;
        }
        messages
    }
}
```

调用点（`hosted_session.rs:580-585`）：

```rust
let mut replay = HistoryReplay::default();
messages = transcript
    .messages
    .into_iter()
    .flat_map(|message| replay.convert(message))
    .collect();
```

`chat_messages_from_history` 本身不变（`Message::Tool` 分支仍先填 `tool:<id>`，由 `convert` 覆盖）。`surface_projection.rs:680` 的 live surface 路径没有工具名可用（`SurfaceItem::ToolResultMessage` 不带名字），这里不改，留待 runtime 在 surface item 上补 `tool_name` 字段；Task 6 的 `tool_display_name` 已把它显示为 `tool`。

- [ ] **Step 4: 跳底胶囊**

`render_live_messages` 里 `frame.render_widget(Paragraph::new(Span::styled(label, …)), pill);` 之前加 `frame.render_widget(Clear, pill);`。测试：

```rust
#[test]
fn jump_to_bottom_pill_clears_the_cells_it_covers() {
    let mut state = test_state();
    for index in 0..40 {
        state.push_message(ChatMessage::Assistant(format!("{} line {index}", "x".repeat(70))));
    }
    state.viewport.auto_scroll = false;
    state.viewport.scroll_offset = 0;
    let theme = Theme::named(ThemeName::Dark);
    let textarea = crate::composer_textarea::make_textarea(&VimState::new(false), &theme);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();
    let pill = state.viewport.jump_to_bottom_area.expect("pill");
    let buffer = terminal.backend().buffer();
    let row: String = (0..80).map(|x| buffer[(x, pill.y)].symbol().to_string()).collect();
    let left = &row[..usize::from(pill.x)];
    assert!(!left.trim().is_empty(), "transcript text should remain left of the pill: {row:?}");
    let inside: String = (pill.x..pill.x + pill.width).map(|x| buffer[(x, pill.y)].symbol().to_string()).collect();
    assert!(inside.contains("Jump to bottom"), "{inside:?}");
    assert!(!inside.contains('x'), "pill must not show text through: {inside:?}");
}
```

- [ ] **Step 5: setup 按键泄漏**

实测：在安全提示页按 `t` 后进入主界面，composer 里出现了 `t`。`handle_setup_key` 第 0 步会消费 `t`，所以泄漏发生在从 setup 切到主界面时 textarea 里残留的内容。修复：`finish_setup`（`setup_actions.rs`）开头重建 textarea：

```rust
*textarea = crate::composer_textarea::make_textarea(vim_state, theme);
```

并加回归测试（`setup_actions.rs` tests，参照本文件现有 `enter_persists_untrusted_selection` 的构造方式）：

```rust
#[test]
fn finishing_setup_starts_with_an_empty_composer() {
    let (mut state, mut config, shared, action_tx, mut textarea, vim, theme) = setup_fixture();
    textarea.insert_str("t");
    state.setup_selection = SETUP_TRUST_SELECTION;
    config.api_key = Some("sk-test".into());
    let flow = handle_setup_key(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state, &mut config, &shared, &action_tx, &mut textarea, &vim, &theme, None,
    ).unwrap();
    assert!(matches!(flow, SetupFlow::Continue));
    assert_eq!(state.status, AppStatus::Idle);
    assert!(textarea.is_empty());
}
```

（`setup_fixture()` 指本文件测试里已有的 state/config 构造助手；若名字不同，用现有测试的同一套构造代码。）

- [ ] **Step 6: 运行 + 提交**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "fix(tui): restore tool names on resume, clear under the jump pill, reset composer after setup"
```

---

### Task 8: 弹窗统一到共享组件

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`：`render_recovery_prompt`、`render_config_dialog`、`config_dialog_row`、`render_full_access_confirmation`、`render_approval_dialog`、`approval_dialog_geometry`、`render_plan_approval_dialog`、`render_setup`、`setup_option_line`、`render_workflows_panel` / `render_agents_panel` 的空态、`render_plan_panel`、`render_goal_banner`
- Test: `ui.rs` tests

**Interfaces:**
- Consumes: `chrome::{panel_block, hint_line, option_line, dialog_rect}`、`theme.accent_style/muted_style/dim_style`

- [ ] **Step 1: 写失败测试**

```rust
fn frame_string(state: &mut AppState, width: u16, height: u16) -> String {
    let theme = Theme::named(ThemeName::Dark);
    let textarea = crate::composer_textarea::make_textarea(&VimState::new(false), &theme);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state, &textarea, &theme)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn approval_dialog_uses_shared_options_and_hints() {
    let mut state = test_state();
    state.status = AppStatus::WaitingApproval;
    state.approval_dialog = Some(ApprovalDialog {
        id: "1".into(), interaction: None, tool: "bash".into(), target: Some("cargo test".into()),
        permission_kind: None, background_task_id: None, selected: 0,
        options: vec![ApprovalOption::Once, ApprovalOption::AlwaysTool, ApprovalOption::AlwaysTarget, ApprovalOption::Deny],
        diff: None,
    });
    let frame = frame_string(&mut state, 100, 30);
    assert!(frame.contains("╭ Approve · bash"), "{frame}");
    assert!(frame.contains("› 1  Allow once"), "{frame}");
    assert!(frame.contains("  4  Deny"), "{frame}");
    assert!(frame.contains("↑↓ move · 1-4 pick · Enter confirm · Esc deny"), "{frame}");
    assert!(!frame.contains("legacy"), "{frame}");
    assert!(!frame.contains("▸"), "{frame}");
}

#[test]
fn config_dialog_height_fits_its_rows() {
    let mut state = test_state();
    state.config_dialog = Some(ConfigDialog {
        selected: 0, model: "deepseek-flash".into(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max, approval_mode: ApprovalMode::AutoEdit,
    });
    let frame = frame_string(&mut state, 100, 30);
    let box_rows = frame.lines().filter(|line| line.contains('│')).count();
    assert_eq!(box_rows, 7, "{frame}");
    assert!(frame.contains("› Model"), "{frame}");
    assert!(frame.contains("↑↓ move · ←→ change · Enter apply · Esc cancel"), "{frame}");
}

#[test]
fn setup_security_notice_uses_theme_colors_and_no_empty_rows() {
    let mut state = test_state();
    state.status = AppStatus::Setup;
    state.setup_step = 0;
    let theme = Theme::named(ThemeName::Dark);
    let textarea = crate::composer_textarea::make_setup_textarea(&theme);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();
    let buffer = terminal.backend().buffer();
    let rendered = format!("{buffer:?}");
    assert!(!rendered.contains("Cyan"), "{rendered}");
    let box_rows = (0..30u16).filter(|y| (0..100u16).any(|x| buffer[(x, *y)].symbol() == "│")).count();
    assert!(box_rows <= 12, "security notice should not pad with empty rows: {box_rows}");
    assert!(rendered.contains("› T  Trust workspace"), "{rendered}");
}

#[test]
fn empty_tasks_panel_is_a_short_centered_notice() {
    let mut state = test_state();
    state.panel_mode = PanelMode::Agents;
    let frame = frame_string(&mut state, 100, 30);
    assert!(frame.contains("No tasks yet"), "{frame}");
    assert!(frame.contains("Esc back"), "{frame}");
    let box_rows = frame.lines().filter(|line| line.contains('│')).count();
    assert!(box_rows <= 5, "{frame}");
}
```

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib approval_dialog_uses_shared config_dialog_height setup_security_notice empty_tasks_panel -- --test-threads=1`

- [ ] **Step 3: 实现**

审批弹窗（方案一阶段仍是居中弹窗）：

```rust
fn render_approval_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(dialog) = &state.approval_dialog else { return };
    let area = frame.area();
    let geometry = approval_dialog_geometry(area, dialog);
    let popup = geometry.popup;
    let inner_width = usize::from(popup.width.saturating_sub(4));
    let mut content: Vec<Line<'static>> = Vec::new();
    if let Some(target) = dialog.target.as_deref() {
        content.push(Line::from(Span::styled(truncate_to_display_width(target, inner_width), Style::default().fg(theme.text))));
    }
    if let Some(diff) = &dialog.diff {
        let rail = Span::styled("│ ".to_string(), theme.dim_style());
        for line in diff.lines().take(geometry.shown_diff_lines) {
            let color = if line.starts_with('+') { theme.diff_add } else if line.starts_with('-') { theme.diff_remove } else if line.starts_with("@@") || line.starts_with('$') { theme.border } else { theme.muted };
            content.push(Line::from(vec![rail.clone(), Span::styled(truncate_to_display_width(line, inner_width.saturating_sub(2)), Style::default().fg(color))]));
        }
        if geometry.diff_truncated {
            content.push(Line::from(Span::styled("… preview truncated", theme.dim_style())));
        }
    }
    content.push(Line::from(""));
    let labels: Vec<String> = dialog.options.iter().map(|option| approval_option_label(*option, &dialog.tool)).collect();
    let label_width = labels.iter().map(|label| UnicodeWidthStr::width(label.as_str())).max().unwrap_or(0);
    for (index, option) in dialog.options.iter().enumerate() {
        content.push(crate::chrome::option_line(
            theme, index == dialog.selected, &option.key().to_string(), &labels[index], label_width,
            approval_option_detail(*option), inner_width,
        ));
    }
    content.push(Line::from(""));
    content.push(crate::chrome::hint_line(theme, inner_width, &[
        ("↑↓", "move"), ("1-4", "pick"), ("Enter", "confirm"), ("Esc", "deny"), ("PgUp/PgDn", "preview"),
    ]));
    frame.render_widget(Clear, popup);
    let block = crate::chrome::panel_block(theme, &format!("Approve · {}", dialog.tool), theme.approval);
    frame.render_widget(Paragraph::new(content).block(block), popup);
}

fn approval_option_label(option: ApprovalOption, tool: &str) -> String {
    match option {
        ApprovalOption::Once => "Allow once".to_string(),
        ApprovalOption::AlwaysTool => format!("Allow {tool} this session"),
        ApprovalOption::AlwaysTarget => "Allow this exact call".to_string(),
        ApprovalOption::Deny => "Deny".to_string(),
    }
}

fn approval_option_detail(option: ApprovalOption) -> &'static str {
    match option {
        ApprovalOption::Once => "run it now",
        ApprovalOption::AlwaysTool => "skip approval for this tool until you quit",
        ApprovalOption::AlwaysTarget => "remember only this exact call",
        ApprovalOption::Deny => "ask Orca for another way",
    }
}
```

`approval_dialog_geometry` 的内容行数计算改为：target 行（0/1）+ diff 行 + 截断行 + 空行 + 选项数 + 空行 + 提示行；宽度 `min(72, area.width - 4)`；用 `chrome::dialog_rect(area, width, rows, area.height - 2)`。`approval_option_hit_index` 用同一行数偏移（第一个选项行 = popup.y + 1 + target 行数 + diff 行数 + 截断行数 + 1）。`ApprovalOption::key()` 保持 `1..4`；`y/A/a/n` 按键仍由 `approval_actions.rs` 处理，只是不再显示。

其余弹窗按同一模式：

- `render_recovery_prompt`：`panel_block(theme, "Recover operation", theme.border)`，两行说明，`option_line` 两项（`1 Continue` / `2 Cancel operation`，选中项 `state.recovery_prompt_selected`），`hint_line(&[("↑↓","move"),("Enter","confirm"),("Esc","cancel")])`，`dialog_rect(area, 60, 6, 12)`。
- `render_config_dialog`：`panel_block(theme, "Runtime settings", theme.border)`；三行用 `option_line(theme, selected, "", label, 18, value, inner)` 但 key 为空时去掉 key 列（`option_line` 对空 key 不占列，Task 1 已实现）；值两侧的 `‹ ›` 只在选中行显示；末行 `hint_line(&[("↑↓","move"),("←→","change"),("Enter","apply"),("Esc","cancel")])`；高度 = 1 说明 + 1 空 + 3 行 + 1 空 + 1 提示 = 7 内容行 → `dialog_rect(area, 72, 7, 12)`（内容 7 行每行都带 `│`，与测试断言 `box_rows == 7` 一致）。
- `render_full_access_confirmation`：`panel_block(theme, "Enable Full Access?", theme.error)`，说明三行，`option_line` 两项，提示行；`dialog_rect(area, 72, 8, 14)`。
- `render_plan_approval_dialog`：`panel_block(theme, "Plan ready", theme.plan_mode)`，两项 `option_line`，提示行 `PgUp/PgDn review plan`。
- `render_setup`：三步都改用 `panel_block` + `theme` 颜色（`Color::White/Cyan/DarkGray/Green/Yellow/Red/Blue` 全部替换为 `theme.text/border/muted/success/warning/error/plan_mode`）；第 0 步内容行数按实际行数给 `dialog_rect`（不再固定 22）；`setup_option_line` 改为 `option_line(theme, selected, key, label, 20, "", inner)`（`❯ [T]` → `› T`）；提示行 `hint_line(&[("↑↓","move"),("Enter","select"),("Esc","exit")])`。
- `render_workflows_panel` / `render_agents_panel` 空态：只渲染 3 行（空行、居中 `No tasks yet · they appear here when background work starts`、`Esc back`）并用 `dialog_rect(area, 60, 3, 5)` 居中，不再撑满整个面板；有任务时保持现有布局，仅把 `Block` 换成 `panel_block`。
- `render_plan_panel`、`render_goal_banner`：`Block` 换成 `panel_block(theme, "Plan", theme.border)` / `panel_block(theme, "Goal", theme.border)`；plan 失败态标题 `"Plan · last update failed"` 用 `theme.warning`。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`
更新 `waiting_approval_renders_numeric_shortcuts_in_semantic_order`、`approval_dialog_keeps_actions_visible_with_long_content`、`welcome_step_renders_three_selectable_actions_with_focus_marker`（`❯` → `›`）、`config_dialog_renders_runtime_settings_and_hides_composer_cursor`、`masked_setup_layout_uses_mask_width_and_never_renders_secret`、`ui.rs:7939` 附近 `contains("legacy y/A/a/n")` 的断言删除；`app_integration_tests.rs` 里对 `[1]`、`allow this once` 的点击/文案断言改为新文案与新几何。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): render every dialog through the shared panel, option and hint components"
```

---

### Task 9: 帮助面板表格化、`/` 菜单对齐、`@` 候选类型列

**Files:**
- Modify: `crates/orca-tui/src/shortcuts.rs:519-553`（`shortcut_lines`）
- Modify: `crates/orca-tui/src/ui.rs`：`render_shortcuts`、`render_slash_menu`、`render_mention_candidates`、`mention_popup_status`
- Test: `shortcuts.rs` tests、`ui.rs` tests

**Interfaces:**
- Produces: `pub fn shortcut_lines(scopes: &[ShortcutScope], theme: &Theme, width: usize) -> Vec<Line<'static>>`

- [ ] **Step 1: 写失败测试**

`shortcuts.rs` tests：

```rust
#[test]
fn shortcut_lines_align_keys_in_one_column_and_wrap_with_hanging_indent() {
    let theme = Theme::named(ThemeName::Dark);
    let lines = shortcut_lines(&[ShortcutScope::Editor], &theme, 40);
    let text: Vec<String> = lines.iter().map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
    assert_eq!(text[0], "Editor");
    assert!(text.iter().all(|row| UnicodeWidthStr::width(row.as_str()) <= 40), "{text:?}");
    assert!(text[1].starts_with("  "), "keys are indented two cells: {:?}", text[1]);
    let action_col = lines[1].spans[0].content.len();
    assert!(
        text.iter().skip(2).any(|row| row.starts_with(&" ".repeat(action_col)) && !row.trim().is_empty()),
        "expected a wrapped continuation row with hanging indent: {text:?}"
    );
    assert_eq!(lines[0].spans[0].style.fg, Some(theme.border));
}
```

`ui.rs` tests：

```rust
#[test]
fn help_panel_is_wide_enough_for_its_longest_key_and_hints_the_alias() {
    let mut state = test_state();
    state.show_shortcuts = true;
    let frame = frame_string(&mut state, 110, 34);
    assert!(frame.contains("╭ Help"), "{frame}");
    assert!(!frame.contains("alt+b/fmove"), "key column must not run into the action: {frame}");
    assert!(frame.contains("? or Esc close"), "{frame}");
}

#[test]
fn slash_menu_aligns_descriptions_in_a_column() {
    let mut state = test_state();
    state.slash_menu = Some(SlashMenu {
        items: vec![
            SlashMenuItem { command: "/new".into(), description: "Start a new conversation".into() },
            SlashMenuItem { command: "/compact".into(), description: "Compress conversation context".into() },
        ],
        selected: 0,
        sub_menu: None,
    });
    let frame = frame_string(&mut state, 100, 30);
    let new_col = frame.lines().find(|l| l.contains("/new")).unwrap().find("Start").unwrap();
    let compact_col = frame.lines().find(|l| l.contains("/compact")).unwrap().find("Compress").unwrap();
    assert_eq!(new_col, compact_col, "{frame}");
    assert!(frame.contains("› /new"), "{frame}");
}

#[test]
fn mention_popup_shows_kind_column_and_truncates_with_an_ellipsis() {
    let mut state = test_state();
    state.mention.phase = Some(SearchPhase::Complete);
    state.mention.sigil = Some(orca_runtime::mentions::MentionSigil::At);
    state.mention.candidates = vec![MentionCandidate {
        id: "1".into(), kind: MentionKind::Skill, display: "brainstorming".into(),
        description: "You MUST use this before any creative work - creating features, building components, adding functionality".into(),
        score: 1, indices: vec![], target: MentionTarget::Skill("brainstorming".into()),
    }];
    let frame = frame_string(&mut state, 80, 20);
    let row = frame.lines().find(|l| l.contains("brainstorming")).unwrap();
    assert!(row.contains("skill   brainstorming"), "{row}");
    assert!(row.trim_end().ends_with("…│"), "{row}");
}
```

（`SlashMenu`、`MentionCandidate`、`MentionTarget` 的构造以 `types.rs` 与 `orca_runtime::mentions` 的实际字段为准；`SearchPhase` 来自 `orca_file_search`。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib shortcut_lines_align help_panel_is slash_menu_aligns mention_popup_shows_kind -- --test-threads=1`

- [ ] **Step 3: 实现**

`shortcuts.rs`：

```rust
pub fn shortcut_lines(scopes: &[ShortcutScope], theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let sections = [
        (ShortcutScope::Global, "Global"),
        (ShortcutScope::Editor, "Editor"),
        (ShortcutScope::Idle, "Composer"),
        (ShortcutScope::Running, "Running"),
        (ShortcutScope::Approval, "Approval"),
    ];
    let active = |scope: ShortcutScope| scopes.is_empty() || scopes.contains(&scope);
    let key_width = shortcut_hints()
        .filter(|hint| active(hint.scope))
        .map(|hint| UnicodeWidthStr::width(hint.keys))
        .max()
        .unwrap_or(0);
    let indent = 2;
    let action_col = indent + key_width + 3;
    let action_width = width.saturating_sub(action_col).max(8);
    let mut lines = Vec::new();
    for (scope, title) in sections {
        if !active(scope) {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(title.to_string(), theme.accent_style().add_modifier(Modifier::BOLD))));
        for hint in shortcut_hints().filter(|hint| hint.scope == scope) {
            let mut rows = wrap_words(hint.action, action_width).into_iter();
            let first = rows.next().unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{}{:<key_width$}   ", " ".repeat(indent), hint.keys), Style::default().fg(theme.text)),
                Span::styled(first, theme.muted_style()),
            ]));
            for row in rows {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(action_col)),
                    Span::styled(row, theme.muted_style()),
                ]));
            }
        }
    }
    lines
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate_width = UnicodeWidthStr::width(current.as_str()) + usize::from(!current.is_empty()) + UnicodeWidthStr::width(word);
        if !current.is_empty() && candidate_width > width {
            rows.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}
```

（`{:<key_width$}` 对 ASCII 键名按字符对齐即可；键名不含宽字符。）`shortcuts.rs` 顶部加 `use ratatui::style::Modifier; use unicode_width::UnicodeWidthStr; use crate::theme::Theme;`，删除 `Color` 引用。`SHORTCUT_HINTS` 里把图片查看器的 `+/- · arrows · 0` 条目从 `Global` 挪到新增的 `ShortcutScope::ImageViewer`（`shortcut_lines` 的 sections 加 `(ShortcutScope::ImageViewer, "Image viewer")`），`active_shortcut_scopes` 只在 `state.image_viewer.is_some()` 时加入它；`scope_has_registered_binding` 为新 scope 返回 `false` 之外的现有约定按文件内其它 scope 的写法补齐。

`ui.rs` `render_shortcuts`：

```rust
fn render_shortcuts(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = frame.area();
    let width = 78u16.min(area.width.saturating_sub(4));
    let inner_width = usize::from(width.saturating_sub(4));
    let scopes = active_shortcut_scopes(state);
    let mut lines = shortcuts::shortcut_lines(&scopes, theme, inner_width);
    lines.push(Line::from(""));
    lines.push(crate::chrome::hint_line(theme, inner_width, &[("?", "or Esc close")]));
    let popup = crate::chrome::dialog_rect(area, width, lines.len() as u16, area.height.saturating_sub(4));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(crate::chrome::panel_block(theme, "Help", theme.border)),
        popup,
    );
}
```

`render_slash_menu`：命令列宽 = 最长命令宽度；行用 `option_line(theme, selected, "", command, command_width, description, inner)`（key 为空时不占列，见 Task 1）；标题 `panel_block(theme, "Commands", theme.border)`；最后一行 `hint_line(&[("↑↓","move"),("Enter","run"),("Esc","close")])`（`popup_geometry` 的 `show_status` 参数传 `true` 以预留这一行）。

`render_mention_candidates`：每行 `[marker][kind 左对齐补到 8 列，file 用 theme.plan_mode，skill 用 theme.warning，其它 theme.approval]display  description`，`display` 里匹配字符仍用 `theme.warning` 粗体；`description` 用 `truncate_to_display_width(description, inner - used)` 保证以 `…` 结尾而不是被边框硬截；状态行文案改为 `⋯ indexing files · <text>` 用 `theme.dim_style()`；标题 `panel_block(theme, if skills_only { "Skills" } else { "Mentions" }, theme.border)`。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib -- --test-threads=1`
更新 `long_slash_and_mention_menus_keep_selection_visible`（`▸` → `›`）、`mention_popup_reports_every_streaming_phase`、`mention_popup_highlights_unicode_character_indices`、`mention_popup_renders_in_a_narrow_terminal`（`[skill]` → `skill   `）、`shortcuts_frame_hides_the_hardware_cursor_without_moving_it`、`shortcuts.rs` 里对 `shortcut_lines(&[...])` 单参数调用的测试改为三参数。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src
git commit -m "feat(tui): tabular help panel, aligned slash menu and typed mention rows"
```

---

### Task 10: 会话选择器按项目分组、隐藏测试会话

**Files:**
- Modify: `crates/orca-tui/src/types.rs`（`AppState` 新字段 `pub session_picker_show_tests: bool`，初始 `false`）
- Modify: `crates/orca-tui/src/session_picker.rs:10-21`（`filtered_session_indices`）、新增 `session_picker_rows`
- Modify: `crates/orca-tui/src/session_picker_actions.rs:153-180`（Browsing 分支加 `Ctrl+T`）
- Modify: `crates/orca-tui/src/ui.rs`：`render_session_picker`、`session_picker_hit_index`
- Test: `session_picker.rs` tests、`ui.rs` tests

**Interfaces:**
- Produces:

```rust
pub(crate) enum SessionPickerRow { Group { label: String, current: bool }, Session(usize) }
impl AppState {
    pub fn filtered_session_indices(&self) -> Vec<usize>;        // 现在也过滤 provider == "mock"
    pub(crate) fn session_picker_rows(&self) -> Vec<SessionPickerRow>;
    pub(crate) fn hidden_test_session_count(&self) -> usize;
}
```

- [ ] **Step 1: 写失败测试（session_picker.rs tests）**

```rust
fn summary(title: &str, cwd: &str, provider: &str, minutes_ago: i64) -> SessionSummary {
    let now = chrono::Utc::now();
    SessionSummary {
        session_id: format!("id-{title}"), title: title.into(), cwd: cwd.into(), provider: provider.into(),
        model: None, created_at: now - chrono::Duration::minutes(minutes_ago),
        updated_at: now - chrono::Duration::minutes(minutes_ago),
        ..summary_defaults()
    }
}

#[test]
fn mock_sessions_are_hidden_until_requested() {
    let mut state = test_state_in("/work/orca");
    state.session_picker_sessions = vec![
        summary("real", "/work/orca", "deepseek", 5),
        summary("schema_ok", "/work/orca", "mock", 1),
    ];
    assert_eq!(state.filtered_session_indices(), vec![0]);
    assert_eq!(state.hidden_test_session_count(), 1);
    state.session_picker_show_tests = true;
    assert_eq!(state.filtered_session_indices(), vec![0, 1]);
}

#[test]
fn picker_rows_group_by_project_with_the_current_project_first() {
    let mut state = test_state_in("/work/orca");
    state.session_picker_sessions = vec![
        summary("other newest", "/work/other", "deepseek", 1),
        summary("orca older", "/work/orca", "deepseek", 60),
        summary("orca newer", "/work/orca", "deepseek", 30),
    ];
    let rows = state.session_picker_rows();
    assert!(matches!(&rows[0], SessionPickerRow::Group { label, current: true } if label == "orca"));
    assert!(matches!(rows[1], SessionPickerRow::Session(2)));
    assert!(matches!(rows[2], SessionPickerRow::Session(1)));
    assert!(matches!(&rows[3], SessionPickerRow::Group { label, current: false } if label == "other"));
    assert!(matches!(rows[4], SessionPickerRow::Session(0)));
}
```

（`summary_defaults()`：按 `SessionSummary` 其余字段——`health`、`storage_identity` 等——写一个本地助手返回默认值；`test_state_in(cwd)` 用 `AppState::new` 并把 `cwd` 设为给定路径。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib mock_sessions_are_hidden picker_rows_group -- --test-threads=1`

- [ ] **Step 3: 实现**

`session_picker.rs`：

```rust
pub(crate) enum SessionPickerRow {
    Group { label: String, current: bool },
    Session(usize),
}

fn is_test_session(session: &SessionSummary) -> bool {
    session.provider == "mock"
}

fn project_label(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

impl AppState {
    pub fn filtered_session_indices(&self) -> Vec<usize> {
        let needle = self.session_picker_query.to_lowercase();
        self.session_picker_sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| self.session_picker_show_tests || !is_test_session(session))
            .filter(|(_, session)| needle.is_empty() || session.title.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn hidden_test_session_count(&self) -> usize {
        if self.session_picker_show_tests {
            return 0;
        }
        self.session_picker_sessions.iter().filter(|session| is_test_session(session)).count()
    }

    /// Filtered sessions grouped by project directory. The current project comes
    /// first; other groups follow by their most recent session; sessions inside a
    /// group are newest first.
    pub(crate) fn session_picker_rows(&self) -> Vec<SessionPickerRow> {
        let mut groups: Vec<(String, bool, Vec<usize>)> = Vec::new();
        for index in self.filtered_session_indices() {
            let session = &self.session_picker_sessions[index];
            let current = session.cwd == self.cwd;
            match groups.iter_mut().find(|(cwd, _, _)| *cwd == session.cwd) {
                Some((_, _, members)) => members.push(index),
                None => groups.push((session.cwd.clone(), current, vec![index])),
            }
        }
        for (_, _, members) in groups.iter_mut() {
            members.sort_by_key(|index| std::cmp::Reverse(self.session_picker_sessions[*index].updated_at));
        }
        groups.sort_by_key(|(_, current, members)| {
            let newest = members
                .iter()
                .map(|index| self.session_picker_sessions[*index].updated_at)
                .max();
            (!current, std::cmp::Reverse(newest))
        });
        let mut rows = Vec::new();
        for (cwd, current, members) in groups {
            rows.push(SessionPickerRow::Group { label: project_label(&cwd), current });
            rows.extend(members.into_iter().map(SessionPickerRow::Session));
        }
        rows
    }
}
```

`session_picker_actions.rs` Browsing 分支加：

```rust
KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
    state.session_picker_show_tests = !state.session_picker_show_tests;
    state.session_picker_selected = 0;
}
```

（放在把普通字符追加到 `session_picker_query` 的分支之前。）

`render_session_picker`：标题 `panel_block(theme, "Resume", theme.border)`；第一行左侧 `⌕ ` + 查询，右侧 `{n} sessions · {hidden} test sessions hidden`（`hidden > 0` 时）；第二行改为 `hint_line`（Browsing：`↑↓ move · Enter resume · Tab actions · Ctrl+T show/hide test sessions · Esc back`；其它 phase 沿用原文案拆成键/动作对）；主体按 `session_picker_rows()` 渲染：Group 行 `label`（`theme.text` 粗体）+ current 时 `   current project`（muted）；Session 行 `option_line(theme, selected, "", &title, title_width, &format!("{} · {}", relative_time(updated_at), session.provider), inner)`，`title_width = min(50, 最长标题宽)`，超宽标题用 `truncate_to_display_width`；`relative_time` 用 `chrono::Utc::now() - updated_at` 换算成 `just now / N min ago / N h ago / yesterday / N d ago`（`chrono` 已在 orca-runtime 依赖里，`orca-tui` 的 `Cargo.toml` dev-dependencies 已有，需要移到 `[dependencies]`）。`session_permission_metadata_label` 与 health 标记保持原样附在标题行后。

`session_picker_hit_index`：遍历 `session_picker_rows()`，Group 行占 1 行不可点，Session 行占 `1 + metadata` 行；表头仍是 3 行（查询、提示、空行）。

- [ ] **Step 4: 运行并更新旧断言**

Run: `cargo test -p orca-tui --lib session_picker -- --test-threads=1`
更新 `session_picker_labels_additional_directories_under_runtime_workspace_roots`、`session_picker_phases_and_terminal_statuses_render_in_bounded_frames`、`session_picker_frame_hides_the_hardware_cursor_without_moving_it`、`current_session_picker_actions_hide_destructive_commands` 的文案/行偏移；`session_picker_actions.rs` 里依赖 `filtered_session_indices` 顺序的测试给会话统一的 `cwd`。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tui/src crates/orca-tui/Cargo.toml Cargo.lock
git commit -m "feat(tui): group the session picker by project and hide test sessions"
```

---

### Task 11: 欢迎页两栏布局

**Files:**
- Modify: `crates/orca-tui/src/ui.rs:2807-2879`（`build_welcome_lines`）、`render_live_messages` 里的调用
- Test: `ui.rs` tests

**Interfaces:**
- Produces: `fn build_welcome_lines(state: &AppState, theme: &Theme, width: usize) -> Vec<Line<'static>>`（新增 `width`）

- [ ] **Step 1: 写失败测试**

```rust
fn welcome_text(state: &AppState, width: usize) -> Vec<String> {
    lines_text(&build_welcome_lines(state, &Theme::named(ThemeName::Dark), width))
}

#[test]
fn wide_welcome_puts_the_wordmark_and_status_beside_the_whale() {
    let mut state = test_state();
    state.model_name = "auto".into();
    state.approval_mode = ApprovalMode::Suggest;
    state.reasoning_effort = orca_core::config::ReasoningEffort::Max;
    let text = welcome_text(&state, 100);
    assert_eq!(text.len(), 16, "one blank, 14 art rows, one blank: {text:?}");
    let wordmark_row = text.iter().find(|row| row.contains("___")).expect("wordmark");
    assert!(wordmark_row.find("___").unwrap() >= 42, "{wordmark_row}");
    assert!(text.iter().any(|row| row.contains("model   deepseek-flash · max · suggest")), "{text:?}");
    assert!(text.iter().any(|row| row.contains("› /resume to continue a saved conversation")), "{text:?}");
    assert!(text.iter().any(|row| row.contains("› ? keys · / commands · @ files · $ skills")), "{text:?}");
    assert!(!text.iter().any(|row| row.contains("Tips")), "{text:?}");
}

#[test]
fn narrow_welcome_stacks_the_whale_above_the_text_and_tiny_drops_it() {
    let state = test_state();
    let stacked = welcome_text(&state, 70);
    assert!(stacked.len() > 16, "{stacked:?}");
    assert!(stacked[1].trim_start().starts_with('⢀'), "{stacked:?}");
    let tiny = welcome_text(&state, 40);
    assert!(!tiny.iter().any(|row| row.contains('⣿')), "{tiny:?}");
    assert!(tiny.iter().any(|row| row.contains("___")), "{tiny:?}");
}

#[test]
fn untrusted_workspace_welcome_points_at_trust() {
    let mut state = test_state();
    state.first_run = Some(orca_runtime::onboarding::FirstRunState {
        workspace_trusted: false,
        ..first_run_fixture()
    });
    let text = welcome_text(&state, 100);
    assert!(text.iter().any(|row| row.contains("› /trust")), "{text:?}");
}
```

（`first_run_fixture()`：按 `FirstRunState` 全部字段写一个测试助手，`diagnostics` 用 `DiagnosticReport::default()` 或本文件 setup 测试已有的构造。）

- [ ] **Step 2: 运行，确认失败**

Run: `cargo test -p orca-tui --lib welcome -- --test-threads=1`

- [ ] **Step 3: 实现**

```rust
const WELCOME_ART_WIDTH: usize = 36;
const WELCOME_COLUMN_GAP: usize = 4;
const WELCOME_TWO_COLUMN_MIN_WIDTH: usize = 80;
const WELCOME_TEXT_ONLY_MAX_WIDTH: usize = 44;

fn welcome_right_column(state: &AppState, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let whale = theme.accent_style();
    let muted = theme.muted_style();
    let text = Style::default().fg(theme.text);
    let mode = Span::styled(state.approval_mode.as_str().to_string(), Style::default().fg(approval_mode_color(state.approval_mode, theme)));
    let branch = state.workspace_git.as_ref().map(GitIdentity::label).map(|git| format!(" · {git}")).unwrap_or_default();
    let first_tip = if state.first_run.as_ref().is_some_and(|first_run| !first_run.workspace_trusted) {
        vec![Span::styled("› ", whale), Span::styled("/trust", muted), Span::styled(" to let Orca read and edit this directory", text)]
    } else {
        vec![Span::styled("› ", whale), Span::styled("/resume", muted), Span::styled(" to continue a saved conversation", text)]
    };
    vec![
        vec![],
        vec![Span::styled("   ___                ", whale)],
        vec![Span::styled("  / _ \\ _ __ ___ __ _ ", whale)],
        vec![Span::styled(" | | | | '__/ __/ _` |", whale)],
        vec![Span::styled(" | |_| | | | (_| (_| |", whale)],
        vec![Span::styled("  \\___/|_|  \\___\\__,_|", whale), Span::styled(format!("  v{}", state.app_version), muted)],
        vec![],
        vec![Span::styled("model   ", muted), Span::styled(displayed_model_name(&state.model_name).to_string(), text), Span::styled(format!(" · {} · ", state.reasoning_effort.as_str()), muted), mode],
        vec![Span::styled("cwd     ", muted), Span::styled(compact_cwd(&state.cwd, 48), text), Span::styled(branch, muted)],
        vec![],
        first_tip,
        vec![Span::styled("› ", whale), Span::styled("? ", muted), Span::styled("keys · ", text), Span::styled("/ ", muted), Span::styled("commands · ", text), Span::styled("@ ", muted), Span::styled("files · ", text), Span::styled("$ ", muted), Span::styled("skills", text)],
        vec![],
        vec![],
    ]
}

fn build_welcome_lines<'a>(state: &AppState, theme: &Theme, width: usize) -> Vec<Line<'a>> {
    let art: [&str; 14] = [ /* 现有 whale_art 的 14 行原样搬过来 */ ];
    let art_style = |index: usize| if index < 2 { Style::default().fg(theme.plan_mode) } else { theme.accent_style() };
    let right = welcome_right_column(state, theme);
    let mut lines = vec![Line::from("")];
    if width >= WELCOME_TWO_COLUMN_MIN_WIDTH {
        let column = 2 + WELCOME_ART_WIDTH + WELCOME_COLUMN_GAP;
        for (index, art_row) in art.iter().enumerate() {
            let padded = format!("  {art_row:<WELCOME_ART_WIDTH$}");
            let mut spans = vec![Span::styled(padded, art_style(index))];
            let art_width = 2 + UnicodeWidthStr::width(art_row.trim_end());
            spans.push(Span::raw(" ".repeat(column.saturating_sub(art_width.max(2 + WELCOME_ART_WIDTH)))));
            spans.extend(right[index].clone());
            lines.push(Line::from(spans));
        }
    } else {
        if width > WELCOME_TEXT_ONLY_MAX_WIDTH {
            for (index, art_row) in art.iter().enumerate() {
                lines.push(Line::from(Span::styled(format!("  {art_row}"), art_style(index))));
            }
        }
        for row in right.into_iter() {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row);
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines
}
```

`format!("{art_row:<WELCOME_ART_WIDTH$}")` 对 braille 行按字符数补齐；braille 全是单格字符，所以字符数等于显示宽度。`render_live_messages` 调用改为 `build_welcome_lines(state, theme, width)`。`welcome_lines_use_configured_app_version`、`welcome_lines_show_a_spraying_whale_mark` 加第三个参数 `100`。

- [ ] **Step 4: 运行 + 提交**

Run: `cargo test -p orca-tui --lib welcome -- --test-threads=1 && cargo test -p orca-tui --lib -- --test-threads=1`

```bash
cargo fmt --all
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): two-column welcome screen with contextual tips"
```

---

### Task 12: 守卫测试与整帧黄金快照

**Files:**
- Create: `crates/orca-tui/src/golden/welcome.txt`、`transcript.txt`、`approval.txt`、`help.txt`（由测试首次生成）
- Modify: `crates/orca-tui/src/ui.rs` tests、`crates/orca-tui/src/shortcuts.rs` tests

- [ ] **Step 1: 裸颜色守卫**

```rust
#[test]
fn ui_sources_do_not_hardcode_ansi_colors() {
    for (name, source) in [
        ("ui.rs", include_str!("ui.rs")),
        ("chrome.rs", include_str!("chrome.rs")),
        ("shortcuts.rs", include_str!("shortcuts.rs")),
    ] {
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap_or(source);
        let offenders: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("Color::") && !line.contains("Color::Reset") && !line.trim_start().starts_with("//"))
            .collect();
        assert!(offenders.is_empty(), "{name} hardcodes colors:\n{}", offenders.join("\n"));
    }
}
```

Run: `cargo test -p orca-tui --lib ui_sources_do_not_hardcode -- --test-threads=1`
Expected: PASS（Task 8/9 已清除；若失败，把列出的行改为 `theme.*`）。`ui.rs` 里 `use ratatui::style::Color` 的 import 行会命中 `Color::`？不会，import 写的是 `Color` 不带 `::`；类型签名 `-> Color` 也不带。若 `ui.rs` 有 `Color::Rgb` 之类残留，改掉它。

- [ ] **Step 2: 黄金帧助手与四个画面**

```rust
fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/golden").join(format!("{name}.txt"));
    if std::env::var_os("ORCA_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(actual, expected.trim_end_matches('\n'), "golden `{name}` differs; review and rerun with ORCA_UPDATE_GOLDEN=1 to accept");
}

fn golden_state() -> AppState {
    let mut state = test_state();
    state.model_name = "auto".into();
    state.cwd = "/Users/dev/Documents/GitHub/blade-deepseek".into();
    state.app_version = "0.4.32".into();
    state
}

#[test]
fn golden_welcome() {
    let mut state = golden_state();
    assert_golden("welcome", &frame_string(&mut state, 100, 34));
}

#[test]
fn golden_transcript_with_tools_thinking_and_notices() {
    let mut state = golden_state();
    state.push_message(ChatMessage::User("你会用 ego-browser skill 吗".into()));
    state.push_message(ChatMessage::Assistant("让我看看有哪些可用的 skill。".into()));
    state.push_message(ChatMessage::Reasoning { text: "The user asks if I can use the skill.\nCheck the list.".into(), expanded: false });
    state.push_message(tool_call("list_skills", None, "completed", Some("baoyu-comic [user] - Knowledge comic creator\nego-browser [user] - Browser automation\nplaywright [user] - Browser automation\nzeta [user] - misc"), false));
    state.push_message(tool_call("read_skill", Some("ego-browser"), "completed", Some("# ego-browser\nsource: user"), false));
    state.push_message(ChatMessage::Assistant("有 ego-browser skill，我读一下它的使用说明。".into()));
    state.push_message(ChatMessage::System("Resumed saved conversation.".into()));
    state.push_message(ChatMessage::Error("DeepSeek provider error: 429 rate limit exceeded".into()));
    assert_golden("transcript", &frame_string(&mut state, 100, 34));
}

#[test]
fn golden_approval_dialog() {
    let mut state = golden_state();
    state.push_message(ChatMessage::User("跑一下测试".into()));
    state.status = AppStatus::WaitingApproval;
    state.approval_dialog = Some(ApprovalDialog {
        id: "1".into(), interaction: None, tool: "bash".into(), target: Some("cargo test -p orca-tui".into()),
        permission_kind: None, background_task_id: None, selected: 0,
        options: vec![ApprovalOption::Once, ApprovalOption::AlwaysTool, ApprovalOption::AlwaysTarget, ApprovalOption::Deny],
        diff: None,
    });
    assert_golden("approval", &frame_string(&mut state, 100, 30));
}

#[test]
fn golden_help_panel() {
    let mut state = golden_state();
    state.show_shortcuts = true;
    assert_golden("help", &frame_string(&mut state, 110, 40));
}
```

第一次运行：`ORCA_UPDATE_GOLDEN=1 cargo test -p orca-tui --lib golden_ -- --test-threads=1`，然后人工打开 `crates/orca-tui/src/golden/*.txt` 逐行对照交互稿（`docs/superpowers/specs/2026-09-20-tui-visual-interaction-redesign-mock.html`）检查：gutter 列对齐、composer 上下横线、状态栏三区、审批弹窗选项行、帮助面板列对齐。不对的先改实现再重新生成。之后 `cargo test -p orca-tui --lib golden_ -- --test-threads=1` 必须在不设环境变量时通过。

- [ ] **Step 3: 全量验证**

Run:
```bash
cargo fmt --all -- --check
cargo clippy -p orca-tui -p orca-core --all-targets -- -D warnings
cargo test -p orca-core
cargo test -p orca-tui --lib -- --test-threads=1
```
Expected: 全部 PASS。

- [ ] **Step 4: 提交**

```bash
git add crates/orca-tui/src/ui.rs crates/orca-tui/src/shortcuts.rs crates/orca-tui/src/golden
git commit -m "test(tui): guard against raw colors and snapshot the key screens"
```

---

## 自检记录

- **Spec 覆盖**：§4.1 token/组件 → Task 1；§4.2 消息样式 → Task 5、6；§4.3 composer/活动行/状态栏 → Task 2、3、4（队列预览行文案不含 Ctrl+Enter，按 §9.5）；§4.4 弹窗/帮助/菜单/会话选择器/Tasks 空态/欢迎页 → Task 8、9、10、11；§4.5 缺陷 1-3 → Task 7，缺陷 4（`?` 别名）→ Task 3；§6 迁移策略中的守卫与黄金帧 → Task 12；§1.6 缺陷 5（braille 回退）**未纳入本计划**，因为 `terminal_capabilities` 目前不探测字体，需要先设计探测方式，留到方案二一起做；缺陷 6（`@` 排序）属于 `mention_search_manager` 的排序策略，不在视觉范围，留待后续；缺陷 7 由 Task 10 隐藏 mock 会话缓解。
- **占位符**：Task 0、7、9、10、11 里对测试构造助手（`summary_defaults`、`first_run_fixture`、`setup_fixture`）标明了"按现有测试的同一构造方式"，实现者需在对应文件里找现成的构造代码复用；其余步骤均给出完整代码。
- **类型一致性**：`build_lines_for_message_after`（Task 5）被 Task 6 的测试复用；`toggle_latest_expandable`（Task 5）替代 `toggle_latest_tool_output`；`option_line` 的空 key 规则在 Task 8 修改后，Task 9 依赖同一规则；`displayed_model_name`（Task 0）在 Task 3、11 使用；`shortcut_lines` 三参数签名在 Task 9 定义并在 `render_shortcuts` 使用。
