# TUI IME Hardware Cursor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Place the native terminal cursor on the exact visible software-cursor cell for Orca's main composer and masked setup input.

**Architecture:** `ui.rs` owns one display-line layout model that computes rendered spans, cursor visual row, and cursor display column from the same wrap pass. A shared renderer turns that layout into a visible row slice and optional `Frame::set_cursor_position`; top-level UI state decides whether the native cursor is exposed. Test-only recording infrastructure verifies both cursor placement and hiding through completed terminal draws.

**Tech Stack:** Rust 2024, ratatui 0.29, tui-textarea 0.7, unicode-width 0.2, crossterm backend tests.

---

## File Map

- Modify `crates/orca-tui/src/ui.rs`
  - Add the shared textarea visual-layout model.
  - Compute display-width-aware cursor coordinates.
  - Render main and setup inputs through one custom surface.
  - Call `Frame::set_cursor_position` only for editable, unobscured surfaces.
  - Add pure and completed-frame cursor tests.
- Modify `crates/orca-tui/src/composer_textarea.rs`
  - Verify setup textarea masking; no production change is planned.
- Verify `crates/orca-tui/src/app.rs`
  - No direct cursor escape sequence changes should be needed because ratatui `Frame` owns visibility.
- Verify `crates/orca-tui/src/terminal_lifecycle.rs`
  - Existing final `Show` on cleanup remains unchanged.

## Required Working Discipline

- Run every RED command before production changes.
- Confirm failures are caused by missing cursor behavior, not test setup.
- Keep the existing software cursor and all Vim cursor styles.
- Do not introduce direct `Show`, `Hide`, or `MoveTo` commands.
- Every commit ends with exactly:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

---

### Task 1: Shared Visual Layout and Pure Cursor Coordinates

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing layout-shape tests**

Replace tuple-oriented test expectations with the desired layout API:

```rust
#[test]
fn composer_cursor_layout_tracks_ascii_and_cjk_display_columns() {
    let mut textarea = TextArea::from(["ab界c"]);
    textarea.move_cursor(CursorMove::Jump(0, 3));

    let layout = textarea_visual_layout(&textarea, 20);

    assert_eq!(layout.cursor_visual_row, 0);
    assert_eq!(layout.cursor_display_col, 4);
    assert_eq!(layout.lines[0].to_string(), "ab界c");
}

#[test]
fn composer_cursor_layout_tracks_combining_and_emoji_widths() {
    let mut textarea = TextArea::from(["e\u{301}🙂x"]);
    textarea.move_cursor(CursorMove::Jump(0, 3));

    let layout = textarea_visual_layout(&textarea, 20);

    assert_eq!(
        layout.cursor_display_col,
        UnicodeWidthStr::width("e\u{301}🙂")
    );
}

#[test]
fn empty_composer_cursor_starts_at_origin_before_placeholder() {
    let textarea = TextArea::default();

    let layout = textarea_visual_layout(&textarea, 20);

    assert_eq!(layout.cursor_visual_row, 0);
    assert_eq!(layout.cursor_display_col, 0);
    assert_eq!(layout.lines.len(), 1);
    assert_eq!(layout.lines[0].spans[0].content.as_ref(), " ");
}
```

Import `UnicodeWidthStr` and `CursorMove` from the modules already used by the
test module.

- [ ] **Step 2: Run layout tests to verify RED**

Run:

```sh
cargo test -p orca-tui composer_cursor_layout --lib
```

Expected: FAIL because `textarea_visual_layout` and `TextareaVisualLayout` do
not exist.

- [ ] **Step 3: Define the shared layout type**

Add near the current composer helpers:

```rust
struct TextareaVisualLayout {
    lines: Vec<Line<'static>>,
    cursor_visual_row: usize,
    cursor_display_col: usize,
    alignment: ratatui::layout::Alignment,
}
```

Rename:

```rust
composer_visual_lines(...)
```

to:

```rust
fn textarea_visual_layout(textarea: &TextArea, width: usize) -> TextareaVisualLayout
```

For an empty textarea, return one cursor/placeholder row and column zero.

- [ ] **Step 4: Build masked display lines without exposing source text**

Add:

```rust
fn textarea_display_line(textarea: &TextArea, logical_line: &str) -> String {
    match textarea.mask_char() {
        Some(mask) => std::iter::repeat(mask)
            .take(logical_line.chars().count())
            .collect(),
        None => logical_line.to_string(),
    }
}
```

All wrapping and rendered characters use the display line. Cursor and
selection indices remain based on original character indexes.

Change `render_textarea_visual_line` to receive both:

```rust
original_line: &str,
display_line: &str,
```

Use `display_line` for emitted characters and widths. Use original indices for
cursor/selection membership.

- [ ] **Step 5: Compute the cursor display column in the wrap pass**

For the cursor-containing range:

```rust
let cursor_display_col = display_line
    .chars()
    .skip(range.start)
    .take(cursor_col.saturating_sub(range.start))
    .collect::<String>()
    .width();
```

Avoid allocating the temporary string in final code by summing
`UnicodeWidthChar::width` or by slicing with a character-boundary helper.

Store the visual row and display column only when
`cursor_in_visual_range` identifies the cursor's range.

- [ ] **Step 6: Add failing wrap and exact-width tests**

Add:

```rust
#[test]
fn composer_cursor_at_word_wrap_boundary_uses_next_visual_row() {
    let mut textarea = TextArea::from(["alpha bravo"]);
    textarea.move_cursor(CursorMove::Jump(0, 6));

    let layout = textarea_visual_layout(&textarea, 6);

    assert_eq!(layout.cursor_visual_row, 1);
    assert_eq!(layout.cursor_display_col, 0);
}

#[test]
fn exact_width_line_end_creates_a_synthetic_cursor_row() {
    let mut textarea = TextArea::from(["abcdef"]);
    textarea.move_cursor(CursorMove::End);

    let layout = textarea_visual_layout(&textarea, 6);

    assert_eq!(layout.lines.len(), 2);
    assert_eq!(layout.cursor_visual_row, 1);
    assert_eq!(layout.cursor_display_col, 0);
    assert_eq!(layout.lines[1].to_string(), " ");
}

#[test]
fn hard_wrapped_token_cursor_uses_display_width_within_chunk() {
    let mut textarea = TextArea::from(["界界界"]);
    textarea.move_cursor(CursorMove::Jump(0, 2));

    let layout = textarea_visual_layout(&textarea, 4);

    assert_eq!(layout.cursor_visual_row, 1);
    assert_eq!(layout.cursor_display_col, 0);
}
```

Run them before implementing the synthetic-row behavior. Expected: at least
the exact-width test FAILS because the current renderer keeps the cursor on the
full row.

- [ ] **Step 7: Implement exact-width insertion rows**

After processing a logical line, detect:

```rust
let cursor_at_end = row == cursor_row && cursor_col == original_line.chars().count();
let final_range_fills_width = final_range_display_width == width;
```

When both are true:

- remove the software cursor appended to the full previous row if necessary,
- append a new line containing one software cursor space,
- set cursor row to the new visual line,
- set cursor display column to zero.

Do not create the row when `width == 0`.

- [ ] **Step 8: Add the pure visible-coordinate helper**

Add:

```rust
fn textarea_visible_start(layout: &TextareaVisualLayout, visible_height: usize) -> usize {
    if layout.lines.len() <= visible_height {
        0
    } else if layout.cursor_visual_row >= visible_height {
        layout.cursor_visual_row + 1 - visible_height
    } else {
        0
    }
}

fn visible_textarea_cursor(
    layout: &TextareaVisualLayout,
    inner: Rect,
) -> Option<ratatui::layout::Position> {
    if inner.is_empty()
        || layout.alignment != ratatui::layout::Alignment::Left
        || layout.cursor_display_col >= inner.width as usize
    {
        return None;
    }
    let start = textarea_visible_start(layout, inner.height as usize);
    let row = layout.cursor_visual_row.checked_sub(start)?;
    if row >= inner.height as usize {
        return None;
    }
    Some(ratatui::layout::Position::new(
        inner.x.checked_add(layout.cursor_display_col.try_into().ok()?)?,
        inner.y.checked_add(row.try_into().ok()?)?,
    ))
}
```

- [ ] **Step 9: Add scrolling, origin, border, alignment, and mask tests**

Add these direct tests:

```rust
#[test]
fn visible_composer_cursor_includes_origin_border_and_scroll() {
    let mut textarea = TextArea::from(["one", "two", "three", "four"]);
    textarea.set_block(Block::default().borders(Borders::ALL));
    textarea.move_cursor(CursorMove::Bottom);
    textarea.move_cursor(CursorMove::End);
    let area = Rect::new(10, 5, 12, 4);
    let inner = textarea.block().unwrap().inner(area);
    let layout = textarea_visual_layout(&textarea, inner.width as usize);

    assert_eq!(
        visible_textarea_cursor(&layout, inner),
        Some(ratatui::layout::Position::new(inner.x + 4, inner.y + 1))
    );
}

#[test]
fn masked_setup_layout_uses_mask_width_and_never_renders_secret() {
    let mut textarea = crate::composer_textarea::make_setup_textarea(&dark_theme());
    textarea.insert_str("密钥abc");
    let layout = textarea_visual_layout(&textarea, 20);
    let rendered = layout.lines.iter().map(Line::to_string).collect::<String>();

    assert!(!rendered.contains("密钥abc"));
    assert!(rendered.contains("*****"));
    assert_eq!(layout.cursor_display_col, 5);
}
```

Add:

```rust
#[test]
fn hardware_cursor_rejects_empty_and_non_left_aligned_surfaces() {
    let textarea = TextArea::default();
    let layout = textarea_visual_layout(&textarea, 10);
    assert_eq!(visible_textarea_cursor(&layout, Rect::ZERO), None);

    let mut centered = TextArea::default();
    centered.set_alignment(ratatui::layout::Alignment::Center);
    let layout = textarea_visual_layout(&centered, 10);
    assert_eq!(
        visible_textarea_cursor(&layout, Rect::new(3, 4, 10, 1)),
        None
    );
}

#[test]
fn hardware_cursor_includes_nonzero_origin_without_block() {
    let mut textarea = TextArea::from(["abc"]);
    textarea.move_cursor(CursorMove::End);
    let layout = textarea_visual_layout(&textarea, 10);

    assert_eq!(
        visible_textarea_cursor(&layout, Rect::new(7, 9, 10, 1)),
        Some(ratatui::layout::Position::new(10, 9))
    );
}
```

The exact-width synthetic-row test already proves that a cursor which would
otherwise equal `inner.width` moves to column zero of the next row.

- [ ] **Step 10: Update existing composer layout callers**

Change:

```rust
composer_visual_line_count
```

to count:

```rust
textarea_visual_layout(textarea, inner_width).lines.len()
```

Change `composer_click_target` to use the same wrap/display-line helpers, but
keep its logical character result. Do not use display columns as character
indexes.

Update existing tests:

```rust
composer_cursor_at_wrap_boundary_belongs_to_next_visual_line
```

to inspect `TextareaVisualLayout`.

- [ ] **Step 11: Run Task 1 GREEN checks**

Run:

```sh
cargo test -p orca-tui composer_cursor_layout --lib
cargo test -p orca-tui exact_width_line_end --lib
cargo test -p orca-tui masked_setup_layout --lib
cargo test -p orca-tui composer_layout --lib
cargo test -p orca-tui composer_click --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Expected: PASS.

- [ ] **Step 12: Commit the layout model**

```sh
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): compute display-aware composer cursor" \
  -m "Derive software and native cursor coordinates from one wrapped textarea layout, including masked and wide-character input." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Main Composer Hardware Cursor

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing completed-frame cursor tests**

Add:

```rust
#[test]
fn hardware_cursor_matches_idle_composer_software_cursor() {
    let mut state = test_state();
    let theme = dark_theme();
    let mut textarea = crate::composer_textarea::make_textarea(
        &crate::vim::VimState::new(false),
        &theme,
    );
    textarea.insert_str("ab界");
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

    terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();

    terminal
        .backend_mut()
        .assert_cursor_position(ratatui::layout::Position::new(5, 7));
    assert!(
        terminal.backend().buffer()[(5, 7)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
}
```

The concrete expected position is derived independently:

- status row is last row,
- the bordered three-row input occupies rows 6–8 in a 10-row terminal,
- its inner row is 7,
- inner x is 1,
- cursor display width for `ab界` is 4.

Therefore the position is `(1 + 4, 7)`, not a value returned by the helper.

Add a CJK wrapped integration case with a narrow terminal.

- [ ] **Step 2: Run the composer frame tests to verify RED**

Run:

```sh
cargo test -p orca-tui hardware_cursor_matches_idle_composer --lib
cargo test -p orca-tui hardware_cursor_matches_wrapped_cjk --lib
```

Expected: FAIL because the backend cursor remains at its prior/default position
and the frame never requests a cursor.

- [ ] **Step 3: Refactor input rendering into a shared surface**

Introduce:

```rust
fn render_textarea_surface(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    notice: Option<crate::viewport_state::CopyNotice>,
    theme: &Theme,
    show_hardware_cursor: bool,
) {
    let inner = render_textarea_block_and_notice(frame, area, textarea, notice, theme);
    if inner.is_empty() {
        return;
    }
    let layout = textarea_visual_layout(textarea, inner.width as usize);
    let start = textarea_visible_start(&layout, inner.height as usize);
    let end = (start + inner.height as usize).min(layout.lines.len());
    frame.render_widget(
        Paragraph::new(layout.lines[start..end].to_vec())
            .style(textarea.style())
            .alignment(textarea.alignment()),
        inner,
    );
    if show_hardware_cursor
        && let Some(position) = visible_textarea_cursor(&layout, inner)
    {
        frame.set_cursor_position(position);
    }
}
```

The surface must not call `textarea_visual_layout` twice.

Extract the current block and copy-notice behavior into:

```rust
fn render_textarea_block_and_notice(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    notice: Option<crate::viewport_state::CopyNotice>,
    theme: &Theme,
) -> Rect {
    let inner = if let Some(block) = textarea.block() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    if let Some(notice) = notice {
        let text = if notice.local_only {
            format!(" copied {} chars (local clipboard only) ", notice.chars)
        } else {
            format!(" copied {} chars to clipboard ", notice.chars)
        };
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        if area.height > 0 && text_width + 2 < area.width {
            let overlay = Rect::new(
                area.x + area.width - text_width - 2,
                area.y,
                text_width,
                1,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    text,
                    Style::default().fg(theme.approval),
                )),
                overlay,
            );
        }
    }
    inner
}
```

`render_input` passes `state.copy_notice_at(Instant::now())`; setup passes
`None`.

- [ ] **Step 4: Add the top-level visibility predicate**

Add:

```rust
fn main_composer_hardware_cursor_visible(state: &AppState) -> bool {
    composer_visible(state) && !state.show_shortcuts
}
```

The top-level `render` passes this value into `render_input`.

Slash and mention popup state does not change the result.

- [ ] **Step 5: Set the frame cursor after rendering the paragraph**

Inside the shared surface:

```rust
if show_hardware_cursor
    && let Some(position) = visible_textarea_cursor(&layout, inner)
{
    frame.set_cursor_position(position);
}
```

Do not emit crossterm cursor commands directly.

- [ ] **Step 6: Add state visibility integration tests**

Use one table-driven test for visible states:

```rust
#[test]
fn editable_conversation_states_expose_the_hardware_cursor() {
    let theme = dark_theme();
    let textarea = crate::composer_textarea::make_textarea(
        &crate::vim::VimState::new(false),
        &theme,
    );
    for status in [
        AppStatus::Idle,
        AppStatus::Running,
        AppStatus::Compacting,
        AppStatus::WaitingUserInput,
    ] {
        let mut state = test_state();
        state.status = status;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();
        terminal
            .backend_mut()
            .assert_cursor_position(ratatui::layout::Position::new(1, 7));
    }
}
```

Use full `render`, not `render_input`.

Add:

```rust
#[test]
fn composer_popups_and_vim_modes_keep_the_hardware_cursor() {
    let theme = dark_theme();
    for popup in ["slash", "mention"] {
        let mut state = test_state();
        match popup {
            "slash" => {
                state.slash_menu = Some(SlashMenu {
                    items: vec![SlashMenuItem {
                        command: "/help".to_string(),
                        description: "show help".to_string(),
                    }],
                    selected: 0,
                    sub_menu: None,
                });
            }
            "mention" => {
                state.mention.phase = Some(orca_file_search::SearchPhase::Searching);
            }
            _ => unreachable!(),
        }
        let textarea = crate::composer_textarea::make_textarea(
            &crate::vim::VimState::new(false),
            &theme,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();
        terminal
            .backend_mut()
            .assert_cursor_position(ratatui::layout::Position::new(1, 7));
    }

    for mode in [
        crate::vim::VimMode::Insert,
        crate::vim::VimMode::Normal,
        crate::vim::VimMode::Visual,
    ] {
        let mut state = test_state();
        let vim = crate::vim::VimState {
            enabled: true,
            mode,
        };
        let textarea = crate::composer_textarea::make_textarea(&vim, &theme);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

        terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(ratatui::layout::Position::new(1, 7));
    }
}
```

This directly covers:

- slash menu open: cursor remains visible,
- mention menu open: cursor remains visible,
- Vim Normal and Visual mode: cursor remains visible at the software block.

- [ ] **Step 7: Add hidden-frame recording backend**

Inside the `ui.rs` test module, define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorEvent {
    Show,
    Hide,
    Move(Position),
}

struct RecordingBackend {
    inner: ratatui::backend::TestBackend,
    events: Arc<Mutex<Vec<CursorEvent>>>,
}
```

Implement a `RecordingBackend` that wraps `TestBackend`. The complete required
`Backend` implementation is:

```rust
impl ratatui::backend::Backend for RecordingBackend {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.events.lock().unwrap().push(CursorEvent::Hide);
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.events.lock().unwrap().push(CursorEvent::Show);
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        let position = position.into();
        self.events.lock().unwrap().push(CursorEvent::Move(position));
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(
        &mut self,
        clear_type: ratatui::backend::ClearType,
    ) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, line_count: u16) -> std::io::Result<()> {
        self.inner.append_lines(line_count)
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }

    fn scroll_region_up(
        &mut self,
        region: std::ops::Range<u16>,
        line_count: u16,
    ) -> std::io::Result<()> {
        self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        line_count: u16,
    ) -> std::io::Result<()> {
        self.inner.scroll_region_down(region, line_count)
    }
}
```

Bring the `Backend` trait into scope so delegation methods resolve. Scrolling
region methods are delegated because `orca-tui` enables ratatui's
`scrolling-regions` feature. Keep this backend test-only.

- [ ] **Step 8: Add modal hidden-state tests**

With `RecordingBackend`, draw:

- `WaitingApproval`,
- `SessionPicker`,
- shortcuts overlay.

For each completed frame assert:

```rust
assert!(events.contains(&CursorEvent::Hide));
assert!(!events.iter().any(|event| matches!(event, CursorEvent::Move(_))));
```

Clear events between frames.

Use:

```rust
fn take_cursor_events(events: &Arc<Mutex<Vec<CursorEvent>>>) -> Vec<CursorEvent> {
    std::mem::take(&mut *events.lock().unwrap())
}
```

between draws so assertions cannot observe prior-frame commands.

- [ ] **Step 9: Run Task 2 GREEN checks**

Run:

```sh
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui composer_cursor --lib
cargo test -p orca-tui waiting_approval_does_not_render_composer --lib
cargo test -p orca-tui session_picker --lib
cargo test -p orca-tui shortcuts --lib
cargo test -p orca-tui ui::tests --lib
cargo fmt --all -- --check
git diff --check
```

Expected: PASS.

- [ ] **Step 10: Commit main-composer cursor integration**

```sh
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): expose the composer hardware cursor" \
  -m "Position the native terminal cursor on the custom composer layout while modal frames keep it hidden." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Setup Cursor, Final Verification, and Delivery

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Verify: `crates/orca-tui/src/composer_textarea.rs`
- Verify: `crates/orca-tui/src/app.rs`
- Verify: `crates/orca-tui/src/terminal_lifecycle.rs`

- [ ] **Step 1: Add failing setup cursor integration test**

Add:

```rust
#[test]
fn setup_cursor_uses_masked_api_key_cell() {
    let mut state = test_state();
    state.status = AppStatus::Setup;
    state.setup_step = 1;
    let theme = dark_theme();
    let mut textarea = crate::composer_textarea::make_setup_textarea(&theme);
    textarea.insert_str("密钥abc");
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20)).unwrap();

    terminal.draw(|frame| render(frame, &mut state, &textarea, &theme)).unwrap();

    terminal
        .backend_mut()
        .assert_cursor_position(ratatui::layout::Position::new(12, 14));
    let buffer = terminal.backend().buffer().to_string();
    assert!(!buffer.contains("密钥abc"));
    assert!(buffer.contains("*****"));
    assert!(
        terminal.backend().buffer()[(12, 14)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
}
```

For a `70×20` terminal, the `60×14` popup begins at `(5, 3)`. The input outer
area is `(6, 13, 58, 3)`, its bordered inner area begins at `(7, 14)`, and five
mask cells place the cursor at `(12, 14)`. The test does not call
`visible_textarea_cursor` to derive it.

- [ ] **Step 2: Run setup test to verify RED**

Run:

```sh
cargo test -p orca-tui setup_cursor_uses_masked_api_key_cell --lib
```

Expected: FAIL because setup step 1 still renders the upstream widget without
calling `Frame::set_cursor_position`.

- [ ] **Step 3: Render setup input through the shared surface**

Replace:

```rust
frame.render_widget(textarea, inner[1]);
```

with:

```rust
render_textarea_surface(
    frame,
    inner[1],
    textarea,
    None,
    theme,
    true,
);
```

Rename `_theme` to `theme` in `render_setup`.

Do not set cursor position for setup steps 0 or 2.

- [ ] **Step 4: Add setup hidden-state tests**

Using `RecordingBackend`, draw setup steps 0 and 2 and assert:

- one `Hide` event,
- no `Move` event.

Draw step 1 and assert:

- one `Show`,
- one `Move(expected)`,
- no source API-key text in the buffer.

- [ ] **Step 5: Add two-frame visibility transition test**

Using the same terminal/backend:

1. Draw Idle composer; assert Show + Move.
2. Clear events.
3. Change state to WaitingApproval and draw; assert Hide and no Move.
4. Clear events.
5. Return to Idle and draw; assert Show + Move again.

This proves ratatui frame ownership actually hides and restores the cursor.

- [ ] **Step 6: Run all focused tests**

Run:

```sh
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui composer_cursor --lib
cargo test -p orca-tui setup_cursor --lib
cargo test -p orca-tui masked_setup_layout --lib
```

Expected: PASS with nonzero selected tests.

- [ ] **Step 7: Run complete verification**

Run:

```sh
cargo test -p orca-tui
cargo check -p orca-tui
cargo fmt --all
cargo fmt --all -- --check
git diff --check
```

Expected: PASS.

Run:

```sh
cargo test --workspace --all-targets -- --test-threads=1
```

When a known process/timing test fails, rerun the exact test and compare against
the existing base/feature evidence. Do not change unrelated process code.

- [ ] **Step 8: Perform the prompt-to-artifact audit**

Verify this table from current source and fresh command output:

| Requirement | Direct evidence |
|---|---|
| Main composer native cursor | Completed TestBackend draw at known position |
| Software cursor retained | Buffer style/span assertion at same cell |
| CJK/emoji/zero-width columns | Pure layout tests with explicit display columns |
| Soft/hard wrapping | Wrapped-row coordinate tests |
| Exact-width line end | Synthetic next-row test and completed draw |
| Composer scrolling | Nonzero-origin/bordered visible-row test |
| Editable state visibility | Idle/Running/Compacting/WaitingUserInput frame tests |
| Modal hiding | Recording backend Hide/no-Move tests |
| Slash/mention behavior | Cursor remains visible with popup tests |
| Vim modes | Insert/Normal/Visual frame tests |
| Setup API-key cursor | Completed masked setup draw |
| Secret not exposed | Buffer assertion contains masks, not source |
| Setup non-input hiding | Recording backend tests for steps 0/2 |
| No direct cursor escapes | Source inspection: only Frame API added |
| Existing cursor cleanup | `terminal_lifecycle.rs` unchanged |

Treat any missing direct evidence as incomplete.

- [ ] **Step 9: Commit final setup/tests if uncommitted**

Task 3 changes `ui.rs`; commit:

```sh
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): position the setup hardware cursor" \
  -m "Reuse the display-aware textarea surface for masked API-key input and verify cursor visibility transitions." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 10: Final branch review and push**

Run:

```sh
git status --short
git log --format='%h %s%n%(trailers:key=Co-authored-by,valueonly)' dde75e9a..HEAD
git diff --check dde75e9a..HEAD
```

Request final holistic review of the IME commits against:

```text
docs/superpowers/specs/2026-07-27-tui-ime-hardware-cursor-design.md
```

After approval:

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test "$local_sha" = "$remote_sha"
```

Keep the worktree for subsequent P0 tasks.
