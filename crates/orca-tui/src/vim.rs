use std::time::{Duration, Instant};

use orca_core::config::VimInsertEscapeSequence;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Block;
use tui_textarea::{CursorMove, Input, Key, TextArea};

use crate::theme::Theme;
use crate::vim_command::{
    VimCommand, VimCommandParser, VimCommandResolution, VimMotion, VimRegisterSelector,
    VimVisualCommand, VimVisualResolution,
};

const MAX_VIM_PASTE_BYTES: usize = 1024 * 1024;
const VIM_INSERT_ESCAPE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingInsertEscape {
    character: char,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInsertEscapeFlow {
    NoPending,
    Flushed,
    Consumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimRegisterKind {
    Characterwise,
    Linewise,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VimRegisterValue {
    text: String,
    kind: VimRegisterKind,
}

#[derive(Clone, Debug)]
struct VimRegisterBank {
    unnamed: Option<VimRegisterValue>,
    named: [Option<VimRegisterValue>; 26],
}

impl Default for VimRegisterBank {
    fn default() -> Self {
        Self {
            unnamed: None,
            named: std::array::from_fn(|_| None),
        }
    }
}

impl VimRegisterBank {
    fn read(&self, selector: VimRegisterSelector) -> Option<&VimRegisterValue> {
        match selector {
            VimRegisterSelector::Unnamed => self.unnamed.as_ref(),
            VimRegisterSelector::Named(index) => {
                self.named.get(index as usize).and_then(Option::as_ref)
            }
        }
    }

    fn write(&mut self, selector: VimRegisterSelector, value: VimRegisterValue) {
        self.unnamed = Some(value.clone());
        if let VimRegisterSelector::Named(index) = selector
            && let Some(slot) = self.named.get_mut(index as usize)
        {
            *slot = Some(value);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VimCommandOutcome {
    redraw: bool,
    text_changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepeatableChange {
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimTranscriptSearchIntent {
    Open,
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
}

#[derive(Clone, Debug)]
pub struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
    parser: VimCommandParser,
    registers: VimRegisterBank,
    last_change: Option<RepeatableChange>,
    insert_escape: Option<VimInsertEscapeSequence>,
    pending_insert_escape: Option<PendingInsertEscape>,
}

impl VimState {
    pub fn new(enabled: bool) -> Self {
        Self::with_insert_escape(enabled, None)
    }

    pub fn with_insert_escape(
        enabled: bool,
        insert_escape: Option<VimInsertEscapeSequence>,
    ) -> Self {
        Self {
            enabled,
            mode: if enabled {
                VimMode::Normal
            } else {
                VimMode::Insert
            },
            parser: VimCommandParser::default(),
            registers: VimRegisterBank::default(),
            last_change: None,
            insert_escape,
            pending_insert_escape: None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn title(&self) -> &'static str {
        if !self.enabled {
            " Input "
        } else {
            match self.mode {
                VimMode::Insert => " Input [vi insert] ",
                VimMode::Normal => " Input [vi normal] ",
                VimMode::Visual => " Input [vi visual] ",
            }
        }
    }

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

    pub(crate) fn transcript_search_intent(
        &self,
        key: crossterm::event::KeyCode,
    ) -> Option<VimTranscriptSearchIntent> {
        if !self.enabled || self.mode != VimMode::Normal {
            return None;
        }
        match key {
            crossterm::event::KeyCode::Char('/') => Some(VimTranscriptSearchIntent::Open),
            crossterm::event::KeyCode::Char('n') => Some(VimTranscriptSearchIntent::Next),
            crossterm::event::KeyCode::Char('N') => Some(VimTranscriptSearchIntent::Previous),
            _ => None,
        }
    }

    pub fn reset_insert(&mut self, textarea: &mut TextArea<'_>, theme: &Theme) {
        self.flush_pending_insert_escape(textarea);
        self.cancel_pending_command();
        self.mode = if self.enabled {
            VimMode::Normal
        } else {
            VimMode::Insert
        };
        textarea.cancel_selection();
        self.configure_textarea(textarea, theme);
    }

    pub fn handle(&mut self, input: Input, textarea: &mut TextArea<'_>, theme: &Theme) -> bool {
        self.handle_at(input, textarea, theme, Instant::now())
    }

    pub fn handle_at(
        &mut self,
        input: Input,
        textarea: &mut TextArea<'_>,
        theme: &Theme,
        now: Instant,
    ) -> bool {
        if !self.enabled {
            return textarea.input(input);
        }

        let changed = match self.mode {
            VimMode::Insert => self.handle_insert_at(input, textarea, now),
            VimMode::Normal | VimMode::Visual => self.handle_command(input, textarea),
        };
        self.configure_textarea(textarea, theme);
        changed
    }

    pub(crate) fn cancel_pending_command(&mut self) {
        self.parser.reset();
    }

    pub(crate) fn resolve_pending_insert_escape(
        &mut self,
        input: &Input,
        now: Instant,
        textarea: &mut TextArea<'_>,
    ) -> PendingInsertEscapeFlow {
        let Some(pending) = self.pending_insert_escape.take() else {
            return PendingInsertEscapeFlow::NoPending;
        };
        if self.mode == VimMode::Insert
            && now <= pending.deadline
            && !input.ctrl
            && !input.alt
            && self
                .insert_escape
                .as_ref()
                .is_some_and(|sequence| input.key == Key::Char(sequence.second()))
        {
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return PendingInsertEscapeFlow::Consumed;
        }
        textarea.insert_char(pending.character);
        PendingInsertEscapeFlow::Flushed
    }

    pub(crate) fn flush_pending_insert_escape(&mut self, textarea: &mut TextArea<'_>) -> bool {
        let Some(pending) = self.pending_insert_escape.take() else {
            return false;
        };
        textarea.insert_char(pending.character);
        true
    }

    pub(crate) fn flush_expired_insert_escape(
        &mut self,
        now: Instant,
        textarea: &mut TextArea<'_>,
    ) -> bool {
        if self
            .pending_insert_escape
            .is_some_and(|pending| now > pending.deadline)
        {
            return self.flush_pending_insert_escape(textarea);
        }
        false
    }

    fn handle_insert_at(
        &mut self,
        input: Input,
        textarea: &mut TextArea<'_>,
        now: Instant,
    ) -> bool {
        match self.resolve_pending_insert_escape(&input, now, textarea) {
            PendingInsertEscapeFlow::Consumed => return true,
            PendingInsertEscapeFlow::Flushed | PendingInsertEscapeFlow::NoPending => {}
        }

        if input.key == Key::Esc {
            self.cancel_pending_command();
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return true;
        }

        if !input.ctrl
            && !input.alt
            && let Some(sequence) = self.insert_escape.as_ref()
            && input.key == Key::Char(sequence.first())
        {
            self.pending_insert_escape = Some(PendingInsertEscape {
                character: sequence.first(),
                deadline: now + VIM_INSERT_ESCAPE_WINDOW,
            });
            return true;
        }
        textarea.input(input)
    }

    fn handle_command(&mut self, input: Input, textarea: &mut TextArea<'_>) -> bool {
        if input.key == Key::Esc {
            self.cancel_pending_command();
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return true;
        }

        let outcome = match self.mode {
            VimMode::Normal => self.handle_normal(input, textarea),
            VimMode::Visual => self.handle_visual(input, textarea),
            VimMode::Insert => unreachable!(),
        };
        outcome.redraw
    }

    fn handle_normal(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimCommandOutcome {
        match self.parser.resolve_normal(input.clone()) {
            VimCommandResolution::Pending | VimCommandResolution::Consumed => {
                return VimCommandOutcome::default();
            }
            VimCommandResolution::Execute(command) => {
                return self.execute_command(command, textarea);
            }
            VimCommandResolution::Unhandled => {}
        }

        let redraw = match input {
            Input {
                key: Key::Char('^'),
                ..
            } => move_cursor(textarea, CursorMove::Head, false),
            Input {
                key: Key::Char('$'),
                ..
            } => move_cursor(textarea, CursorMove::End, false),
            Input {
                key: Key::Char('i'),
                ..
            } => {
                textarea.cancel_selection();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('a'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::Forward);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('A'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::End);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('o'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::End);
                textarea.insert_newline();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('O'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::Head);
                textarea.insert_newline();
                textarea.move_cursor(CursorMove::Up);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('v'),
                ..
            } => {
                textarea.start_selection();
                self.mode = VimMode::Visual;
                true
            }
            Input {
                key: Key::Char('u'),
                ctrl: false,
                ..
            } => textarea.undo(),
            Input {
                key: Key::Char('r'),
                ctrl: true,
                ..
            } => textarea.redo(),
            _ => false,
        };
        VimCommandOutcome {
            redraw,
            text_changed: false,
        }
    }

    fn handle_visual(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimCommandOutcome {
        match self.parser.resolve_visual(input.clone()) {
            VimVisualResolution::Pending | VimVisualResolution::Consumed => {
                return VimCommandOutcome::default();
            }
            VimVisualResolution::Execute(command) => {
                let (register, change, delete) = match command {
                    VimVisualCommand::Yank(register) => (register, false, false),
                    VimVisualCommand::Delete(register) => (register, false, true),
                    VimVisualCommand::Change(register) => (register, true, true),
                };
                let has_selection = textarea
                    .selection_range()
                    .is_some_and(|(start, end)| start != end);
                let changed = if !has_selection {
                    false
                } else if delete {
                    textarea.cut()
                } else {
                    textarea.copy();
                    false
                };
                if has_selection {
                    let text = textarea.yank_text();
                    if !text.is_empty() {
                        self.registers.write(
                            register,
                            VimRegisterValue {
                                text,
                                kind: VimRegisterKind::Characterwise,
                            },
                        );
                    }
                }
                textarea.cancel_selection();
                self.mode = if change {
                    VimMode::Insert
                } else {
                    VimMode::Normal
                };
                return VimCommandOutcome {
                    redraw: true,
                    text_changed: changed,
                };
            }
            VimVisualResolution::Unhandled => {}
        }

        let redraw = match input {
            Input {
                key: Key::Char('h'),
                ..
            } => move_cursor(textarea, CursorMove::Back, true),
            Input {
                key: Key::Char('j'),
                ..
            } => move_cursor(textarea, CursorMove::Down, true),
            Input {
                key: Key::Char('k'),
                ..
            } => move_cursor(textarea, CursorMove::Up, true),
            Input {
                key: Key::Char('l'),
                ..
            } => move_cursor(textarea, CursorMove::Forward, true),
            Input {
                key: Key::Char('w'),
                ..
            } => move_cursor(textarea, CursorMove::WordForward, true),
            Input {
                key: Key::Char('e'),
                ..
            } => move_cursor(textarea, CursorMove::WordEnd, true),
            Input {
                key: Key::Char('b'),
                ctrl: false,
                ..
            } => move_cursor(textarea, CursorMove::WordBack, true),
            Input {
                key: Key::Char('^'),
                ..
            } => move_cursor(textarea, CursorMove::Head, true),
            Input {
                key: Key::Char('$'),
                ..
            } => move_cursor(textarea, CursorMove::End, true),
            Input {
                key: Key::Char('v'),
                ..
            } => {
                textarea.cancel_selection();
                self.mode = VimMode::Normal;
                true
            }
            _ => false,
        };
        VimCommandOutcome {
            redraw,
            text_changed: false,
        }
    }

    fn execute_command(
        &mut self,
        command: VimCommand,
        textarea: &mut TextArea<'_>,
    ) -> VimCommandOutcome {
        if let VimCommand::Repeat { count } = command {
            return self.repeat_change(textarea, count);
        }
        let repeatable = repeatable_change(&command);
        let outcome = self.execute_command_without_repeat(command, textarea);
        if outcome.text_changed
            && let Some(change) = repeatable
        {
            self.last_change = Some(change);
        }
        outcome
    }

    fn execute_command_without_repeat(
        &mut self,
        command: VimCommand,
        textarea: &mut TextArea<'_>,
    ) -> VimCommandOutcome {
        match command {
            VimCommand::Motion { motion, count } => VimCommandOutcome {
                redraw: execute_motion(textarea, motion, count, false),
                text_changed: false,
            },
            VimCommand::GotoLine { one_based } => {
                match one_based {
                    Some(line) => move_to_row_head(textarea, line.saturating_sub(1)),
                    None => textarea.move_cursor(CursorMove::Bottom),
                }
                VimCommandOutcome {
                    redraw: true,
                    text_changed: false,
                }
            }
            VimCommand::DeleteChars { count, register } => {
                let deleted = delete_chars(textarea, count);
                if let Some(text) = deleted {
                    self.registers.write(
                        register,
                        VimRegisterValue {
                            text,
                            kind: VimRegisterKind::Characterwise,
                        },
                    );
                    VimCommandOutcome {
                        redraw: true,
                        text_changed: true,
                    }
                } else {
                    VimCommandOutcome::default()
                }
            }
            VimCommand::DeleteToEnd { register } => {
                let deleted = delete_to_end(textarea);
                if let Some(text) = deleted.as_ref() {
                    self.registers.write(
                        register,
                        VimRegisterValue {
                            text: text.clone(),
                            kind: VimRegisterKind::Characterwise,
                        },
                    );
                }
                VimCommandOutcome {
                    redraw: deleted.is_some(),
                    text_changed: deleted.is_some(),
                }
            }
            VimCommand::ChangeToEnd { register } => {
                let deleted = delete_to_end(textarea);
                if let Some(text) = deleted.as_ref() {
                    self.registers.write(
                        register,
                        VimRegisterValue {
                            text: text.clone(),
                            kind: VimRegisterKind::Characterwise,
                        },
                    );
                }
                textarea.cancel_selection();
                self.mode = VimMode::Insert;
                VimCommandOutcome {
                    redraw: true,
                    text_changed: deleted.is_some(),
                }
            }
            VimCommand::DeleteLines { count, register } => {
                let deleted = delete_lines(textarea, count);
                if let Some(text) = deleted {
                    self.registers.write(
                        register,
                        VimRegisterValue {
                            text,
                            kind: VimRegisterKind::Linewise,
                        },
                    );
                    VimCommandOutcome {
                        redraw: true,
                        text_changed: true,
                    }
                } else {
                    VimCommandOutcome::default()
                }
            }
            VimCommand::YankLines { count, register } => {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text: yank_lines(textarea, count),
                        kind: VimRegisterKind::Linewise,
                    },
                );
                VimCommandOutcome {
                    redraw: true,
                    text_changed: false,
                }
            }
            VimCommand::Paste { count, register } => {
                let changed = self
                    .registers
                    .read(register)
                    .cloned()
                    .is_some_and(|value| paste_register(textarea, &value, count));
                VimCommandOutcome {
                    redraw: changed,
                    text_changed: changed,
                }
            }
            VimCommand::Repeat { .. } => {
                unreachable!("repeat is handled by execute_command")
            }
        }
    }

    fn repeat_change(
        &mut self,
        textarea: &mut TextArea<'_>,
        multiplier: usize,
    ) -> VimCommandOutcome {
        let Some(change) = self.last_change.clone() else {
            return VimCommandOutcome::default();
        };
        match change {
            RepeatableChange::DeleteChars { count, register } => self
                .execute_command_without_repeat(
                    VimCommand::DeleteChars {
                        count: multiplied_count(count, multiplier),
                        register,
                    },
                    textarea,
                ),
            RepeatableChange::DeleteToEnd { register } => {
                let mut outcome = VimCommandOutcome::default();
                for _ in 0..multiplier.min(crate::vim_command::MAX_VIM_COUNT) {
                    let next = self.execute_command_without_repeat(
                        VimCommand::DeleteToEnd { register },
                        textarea,
                    );
                    outcome.redraw |= next.redraw;
                    outcome.text_changed |= next.text_changed;
                    if !next.text_changed {
                        break;
                    }
                }
                outcome
            }
            RepeatableChange::DeleteLines { count, register } => self
                .execute_command_without_repeat(
                    VimCommand::DeleteLines {
                        count: multiplied_count(count, multiplier),
                        register,
                    },
                    textarea,
                ),
            RepeatableChange::Paste { count, register } => self.execute_command_without_repeat(
                VimCommand::Paste {
                    count: multiplied_count(count, multiplier),
                    register,
                },
                textarea,
            ),
        }
    }
}

fn repeatable_change(command: &VimCommand) -> Option<RepeatableChange> {
    match *command {
        VimCommand::DeleteChars { count, register } => {
            Some(RepeatableChange::DeleteChars { count, register })
        }
        VimCommand::DeleteToEnd { register } => Some(RepeatableChange::DeleteToEnd { register }),
        VimCommand::DeleteLines { count, register } => {
            Some(RepeatableChange::DeleteLines { count, register })
        }
        VimCommand::Paste { count, register } => Some(RepeatableChange::Paste { count, register }),
        _ => None,
    }
}

fn multiplied_count(base: usize, multiplier: usize) -> usize {
    base.saturating_mul(multiplier)
        .min(crate::vim_command::MAX_VIM_COUNT)
}

fn move_to_line_head(textarea: &mut TextArea<'_>) {
    textarea.move_cursor(CursorMove::Head);
}

fn move_to_line_end(textarea: &mut TextArea<'_>) {
    textarea.move_cursor(CursorMove::End);
}

fn move_to_row_head(textarea: &mut TextArea<'_>, row: usize) {
    textarea.move_cursor(CursorMove::Top);
    for _ in 0..row.min(textarea.lines().len().saturating_sub(1)) {
        textarea.move_cursor(CursorMove::Down);
    }
    move_to_line_head(textarea);
}

fn execute_motion(
    textarea: &mut TextArea<'_>,
    motion: VimMotion,
    count: usize,
    visual: bool,
) -> bool {
    if motion == VimMotion::LineHead {
        if visual && textarea.selection_range().is_none() {
            textarea.start_selection();
        }
        move_to_line_head(textarea);
        return true;
    }
    let movement = match motion {
        VimMotion::Back => CursorMove::Back,
        VimMotion::Down => CursorMove::Down,
        VimMotion::Up => CursorMove::Up,
        VimMotion::Forward => CursorMove::Forward,
        VimMotion::WordForward => CursorMove::WordForward,
        VimMotion::WordEnd => CursorMove::WordEnd,
        VimMotion::WordBack => CursorMove::WordBack,
        VimMotion::LineHead => unreachable!(),
    };
    for _ in 0..count {
        move_cursor(textarea, movement, visual);
    }
    true
}

fn selected_line_text(textarea: &TextArea<'_>, count: usize) -> (usize, String) {
    let start = textarea.cursor().0;
    let available = textarea.lines().len().saturating_sub(start);
    let count = count.max(1).min(available);
    let text = textarea.lines()[start..start + count].join("\n");
    (count, text)
}

fn delete_chars(textarea: &mut TextArea<'_>, count: usize) -> Option<String> {
    let start = textarea.cursor();
    textarea.start_selection();
    for _ in 0..count {
        let before = textarea.cursor();
        textarea.move_cursor(CursorMove::Forward);
        if textarea.cursor() == before {
            break;
        }
    }
    if textarea.cursor() == start {
        textarea.cancel_selection();
        return None;
    }
    textarea.cut().then(|| textarea.yank_text())
}

fn delete_to_end(textarea: &mut TextArea<'_>) -> Option<String> {
    let (row, col) = textarea.cursor();
    let suffix = textarea.lines()[row].chars().skip(col).collect::<String>();
    let deleted = if !suffix.is_empty() {
        suffix
    } else if row + 1 < textarea.lines().len() {
        "\n".to_string()
    } else {
        return None;
    };
    textarea.delete_line_by_end().then_some(deleted)
}

fn delete_lines(textarea: &mut TextArea<'_>, count: usize) -> Option<String> {
    let start_row = textarea.cursor().0;
    let (count, register_text) = selected_line_text(textarea, count);
    let reaches_end = start_row + count == textarea.lines().len();

    if start_row == 0 && reaches_end {
        textarea.move_cursor(CursorMove::Head);
        textarea.start_selection();
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
    } else if reaches_end {
        textarea.move_cursor(CursorMove::Head);
        textarea.move_cursor(CursorMove::Up);
        textarea.move_cursor(CursorMove::End);
        textarea.start_selection();
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
    } else {
        textarea.move_cursor(CursorMove::Head);
        textarea.start_selection();
        for _ in 0..count {
            textarea.move_cursor(CursorMove::Down);
        }
        textarea.move_cursor(CursorMove::Head);
    }

    textarea.cut().then_some(register_text)
}

fn yank_lines(textarea: &TextArea<'_>, count: usize) -> String {
    selected_line_text(textarea, count).1
}

fn repeat_text(text: &str, separator: &str, count: usize) -> Option<String> {
    let count = count.max(1).min(crate::vim_command::MAX_VIM_COUNT);
    let content_bytes = text.len().checked_mul(count)?;
    let separator_bytes = separator.len().checked_mul(count.saturating_sub(1))?;
    let total = content_bytes.checked_add(separator_bytes)?;
    if total > MAX_VIM_PASTE_BYTES {
        return None;
    }
    Some(
        std::iter::repeat_n(text, count)
            .collect::<Vec<_>>()
            .join(separator),
    )
}

fn paste_register(textarea: &mut TextArea<'_>, value: &VimRegisterValue, count: usize) -> bool {
    let payload = match value.kind {
        VimRegisterKind::Characterwise => repeat_text(&value.text, "", count),
        VimRegisterKind::Linewise => repeat_text(&value.text, "\n", count).and_then(|text| {
            let total = text.len().checked_add(1)?;
            if total > MAX_VIM_PASTE_BYTES {
                return None;
            }
            let mut payload = String::with_capacity(total);
            payload.push('\n');
            payload.push_str(&text);
            Some(payload)
        }),
    };
    let Some(payload) = payload.filter(|payload| !payload.is_empty()) else {
        return false;
    };
    if value.kind == VimRegisterKind::Linewise {
        move_to_line_end(textarea);
    }
    textarea.set_yank_text(payload);
    textarea.paste()
}

fn move_cursor(textarea: &mut TextArea<'_>, movement: CursorMove, visual: bool) -> bool {
    if visual && textarea.selection_range().is_none() {
        textarea.start_selection();
    }
    if visual {
        textarea.move_cursor(movement);
    } else {
        textarea.move_cursor(movement);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::ThemeName;
    use orca_core::config::VimInsertEscapeSequence;
    use std::time::{Duration, Instant};

    impl VimState {
        fn register_for_test(
            &self,
            selector: VimRegisterSelector,
        ) -> Option<(&str, VimRegisterKind)> {
            let value = match selector {
                VimRegisterSelector::Unnamed => self.registers.unnamed.as_ref(),
                VimRegisterSelector::Named(index) => self
                    .registers
                    .named
                    .get(index as usize)
                    .and_then(Option::as_ref),
            }?;
            Some((value.text.as_str(), value.kind))
        }

        pub(crate) fn has_pending_command_for_test(&self) -> bool {
            self.parser.has_pending()
        }

        pub(crate) fn named_register_for_test(&self, name: u8) -> Option<(&str, bool)> {
            self.registers
                .read(VimRegisterSelector::Named(name))
                .map(|value| (value.text.as_str(), value.kind == VimRegisterKind::Linewise))
        }

        pub(crate) fn seed_pending_count_for_test(&mut self) {
            let _ = self.parser.resolve_normal(Input {
                key: Key::Char('2'),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }

        pub(crate) fn set_named_register_for_test(&mut self, name: u8, text: &str) {
            self.registers.write(
                VimRegisterSelector::Named(name),
                VimRegisterValue {
                    text: text.to_string(),
                    kind: VimRegisterKind::Characterwise,
                },
            );
        }

        pub(crate) fn set_repeat_for_test(&mut self) {
            self.last_change = Some(RepeatableChange::DeleteChars {
                count: 1,
                register: VimRegisterSelector::Unnamed,
            });
        }

        pub(crate) fn has_repeat_for_test(&self) -> bool {
            self.last_change.is_some()
        }

        pub(crate) fn has_pending_insert_escape_for_test(&self) -> bool {
            self.pending_insert_escape.is_some()
        }
    }

    fn input(ch: char) -> Input {
        Input {
            key: Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn insert_escape_state(value: &str) -> VimState {
        let mut state = VimState::with_insert_escape(
            true,
            Some(VimInsertEscapeSequence::parse(value).unwrap()),
        );
        state.mode = VimMode::Insert;
        state
    }

    #[test]
    fn vim_insert_escape_exact_pair_exits_without_text_or_history() {
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();
        let mut state = insert_escape_state("jj");
        let mut textarea = TextArea::from(["draft"]);
        textarea.move_cursor(CursorMove::End);

        assert!(state.handle_at(input('j'), &mut textarea, &theme, started));
        assert_eq!(textarea.lines(), &["draft"]);
        assert_eq!(state.mode, VimMode::Insert);
        assert!(state.has_pending_insert_escape_for_test());

        assert!(state.handle_at(
            input('j'),
            &mut textarea,
            &theme,
            started + Duration::from_millis(500),
        ));
        assert_eq!(textarea.lines(), &["draft"]);
        assert_eq!(state.mode, VimMode::Normal);
        assert!(!state.has_pending_insert_escape_for_test());
        assert!(!textarea.undo());
    }

    #[test]
    fn vim_insert_escape_mismatch_overlap_and_expiry_preserve_text_once() {
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();

        let mut mismatch = insert_escape_state("jk");
        let mut mismatch_area = TextArea::default();
        mismatch.handle_at(input('j'), &mut mismatch_area, &theme, started);
        mismatch.handle_at(
            input('x'),
            &mut mismatch_area,
            &theme,
            started + Duration::from_millis(1),
        );
        assert_eq!(mismatch_area.lines(), &["jx"]);

        let mut overlap = insert_escape_state("jk");
        let mut overlap_area = TextArea::default();
        for (character, millis) in [('j', 0), ('j', 1), ('k', 2)] {
            overlap.handle_at(
                input(character),
                &mut overlap_area,
                &theme,
                started + Duration::from_millis(millis),
            );
        }
        assert_eq!(overlap_area.lines(), &["j"]);
        assert_eq!(overlap.mode, VimMode::Normal);

        let mut expired = insert_escape_state("jj");
        let mut expired_area = TextArea::default();
        expired.handle_at(input('j'), &mut expired_area, &theme, started);
        assert!(
            expired.flush_expired_insert_escape(
                started + Duration::from_millis(501),
                &mut expired_area,
            )
        );
        assert_eq!(expired_area.lines(), &["j"]);
        assert!(
            !expired
                .flush_expired_insert_escape(started + Duration::from_secs(1), &mut expired_area,)
        );
    }

    #[test]
    fn vim_insert_escape_disabled_modified_and_direct_esc_paths_preserve_behavior() {
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();

        let mut disabled = VimState::new(true);
        disabled.mode = VimMode::Insert;
        let mut disabled_area = TextArea::default();
        disabled.handle_at(input('j'), &mut disabled_area, &theme, started);
        disabled.handle_at(input('j'), &mut disabled_area, &theme, started);
        assert_eq!(disabled_area.lines(), &["jj"]);

        let mut modified = insert_escape_state("jj");
        let mut modified_area = TextArea::default();
        modified.handle_at(input('j'), &mut modified_area, &theme, started);
        modified.handle_at(
            Input {
                key: Key::Char('j'),
                ctrl: false,
                alt: true,
                shift: false,
            },
            &mut modified_area,
            &theme,
            started + Duration::from_millis(1),
        );
        assert_eq!(modified_area.lines(), &["j"]);
        assert_eq!(modified.mode, VimMode::Insert);

        let mut escaped = insert_escape_state("jj");
        let mut escaped_area = TextArea::default();
        escaped.handle_at(input('j'), &mut escaped_area, &theme, started);
        escaped.handle_at(
            Input {
                key: Key::Esc,
                ctrl: false,
                alt: false,
                shift: false,
            },
            &mut escaped_area,
            &theme,
            started + Duration::from_millis(1),
        );
        assert_eq!(escaped_area.lines(), &["j"]);
        assert_eq!(escaped.mode, VimMode::Normal);
    }

    #[test]
    fn vim_insert_escape_cannot_complete_after_leaving_insert_mode() {
        let theme = Theme::named(ThemeName::Dark);
        let started = Instant::now();
        let mut state = insert_escape_state("jj");
        let mut textarea = TextArea::default();
        state.handle_at(input('j'), &mut textarea, &theme, started);
        state.mode = VimMode::Normal;

        assert_eq!(
            state.resolve_pending_insert_escape(
                &input('j'),
                started + Duration::from_millis(1),
                &mut textarea,
            ),
            PendingInsertEscapeFlow::Flushed
        );
        assert_eq!(textarea.lines(), &["j"]);
        assert_eq!(state.mode, VimMode::Normal);
    }

    fn handle_sequence(
        state: &mut VimState,
        textarea: &mut TextArea<'_>,
        theme: &Theme,
        sequence: &str,
    ) {
        for character in sequence.chars() {
            state.handle(input(character), textarea, theme);
        }
    }

    #[test]
    fn counted_motions_and_goto_commands_land_on_exact_positions() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["zero one two", "one", "two", "three"]);

        handle_sequence(&mut state, &mut textarea, &theme, "3l");
        assert_eq!(textarea.cursor(), (0, 3));

        handle_sequence(&mut state, &mut textarea, &theme, "3G");
        assert_eq!(textarea.cursor(), (2, 0));

        handle_sequence(&mut state, &mut textarea, &theme, "gg");
        assert_eq!(textarea.cursor(), (0, 0));

        state.handle(input('G'), &mut textarea, &theme);
        assert_eq!(textarea.cursor().0, 3);
    }

    #[test]
    fn bare_zero_moves_to_current_line_head_without_crossing_lines() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["first", "second"]);
        textarea.move_cursor(CursorMove::Down);
        textarea.move_cursor(CursorMove::Forward);
        textarea.move_cursor(CursorMove::Forward);

        state.handle(input('0'), &mut textarea, &theme);

        assert_eq!(textarea.cursor(), (1, 0));
    }

    #[test]
    fn line_commands_handle_twenty_thousand_columns_and_rows() {
        let theme = Theme::named(ThemeName::Dark);
        let mut line_state = VimState::new(true);
        let long_line = "x".repeat(20_000);
        let mut long_line_area = TextArea::from([long_line.as_str()]);
        long_line_area.move_cursor(CursorMove::End);

        line_state.handle(input('0'), &mut long_line_area, &theme);

        assert_eq!(long_line_area.cursor(), (0, 0));

        let mut row_state = VimState::new(true);
        let lines = (0..20_001)
            .map(|row| format!("line-{row}"))
            .collect::<Vec<_>>();
        let mut many_rows_area = TextArea::from(lines);
        many_rows_area.move_cursor(CursorMove::Bottom);

        handle_sequence(&mut row_state, &mut many_rows_area, &theme, "dd");

        assert_eq!(many_rows_area.lines().len(), 20_000);
        assert_eq!(
            many_rows_area.lines().last().map(String::as_str),
            Some("line-19999")
        );
        assert!(many_rows_area.undo());
        assert_eq!(many_rows_area.lines().len(), 20_001);
    }

    #[test]
    fn dd_deletes_whole_lines_as_one_undoable_change() {
        let theme = Theme::named(ThemeName::Dark);
        for (start_row, sequence, expected, expected_cursor, expected_yank) in [
            (0, "dd", vec!["one", "two"], (0, 0), "zero"),
            (1, "dd", vec!["zero", "two"], (1, 0), "one"),
            (1, "2dd", vec!["zero"], (0, 4), "one\ntwo"),
        ] {
            let mut state = VimState::new(true);
            let mut textarea = TextArea::from(["zero", "one", "two"]);
            for _ in 0..start_row {
                textarea.move_cursor(CursorMove::Down);
            }
            handle_sequence(&mut state, &mut textarea, &theme, sequence);
            assert_eq!(textarea.lines(), expected.as_slice(), "{sequence}");
            assert_eq!(textarea.cursor(), expected_cursor, "{sequence}");
            assert_eq!(
                state.register_for_test(VimRegisterSelector::Unnamed),
                Some((expected_yank, VimRegisterKind::Linewise)),
                "{sequence}"
            );
            assert!(textarea.undo(), "{sequence}");
            assert_eq!(textarea.lines(), &["zero", "one", "two"], "{sequence}");
        }
    }

    #[test]
    fn dd_on_the_only_line_leaves_one_empty_line_and_is_undoable() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["only"]);

        handle_sequence(&mut state, &mut textarea, &theme, "dd");

        assert_eq!(textarea.lines(), &[""]);
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Unnamed),
            Some(("only", VimRegisterKind::Linewise))
        );
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &["only"]);
    }

    #[test]
    fn dd_deletes_a_seventy_thousand_character_only_line_atomically() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let line = "x".repeat(70_000);
        let mut textarea = TextArea::from([line.as_str()]);

        handle_sequence(&mut state, &mut textarea, &theme, "dd");

        assert_eq!(textarea.lines(), &[""]);
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Unnamed),
            Some((line.as_str(), VimRegisterKind::Linewise))
        );
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &[line]);
    }

    #[test]
    fn yy_and_named_registers_preserve_linewise_text() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["zero", "one", "two"]);

        handle_sequence(&mut state, &mut textarea, &theme, "\"a2yy");

        assert_eq!(textarea.lines(), &["zero", "one", "two"]);
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Named(0)),
            Some(("zero\none", VimRegisterKind::Linewise))
        );
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Unnamed),
            Some(("zero\none", VimRegisterKind::Linewise))
        );
    }

    #[test]
    fn named_character_delete_and_paste_use_one_shot_register_selection() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["abcd"]);

        handle_sequence(&mut state, &mut textarea, &theme, "\"a2x");
        assert_eq!(textarea.lines(), &["cd"]);
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Named(0)),
            Some(("ab", VimRegisterKind::Characterwise))
        );

        textarea.move_cursor(CursorMove::End);
        handle_sequence(&mut state, &mut textarea, &theme, "\"ap");
        assert_eq!(textarea.lines(), &["cdab"]);
        assert!(!state.has_pending_command_for_test());
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &["cd"]);
    }

    #[test]
    fn named_delete_and_change_to_end_write_register_and_clear_count() {
        let theme = Theme::named(ThemeName::Dark);

        let mut delete_state = VimState::new(true);
        let mut delete_area = TextArea::from(["abcd"]);
        delete_area.move_cursor(CursorMove::Forward);
        handle_sequence(&mut delete_state, &mut delete_area, &theme, "2\"aD");
        assert_eq!(delete_area.lines(), &["a"]);
        assert_eq!(
            delete_state.register_for_test(VimRegisterSelector::Named(0)),
            Some(("bcd", VimRegisterKind::Characterwise))
        );
        assert!(!delete_state.has_pending_command_for_test());

        let mut change_state = VimState::new(true);
        let mut change_area = TextArea::from(["wxyz"]);
        change_area.move_cursor(CursorMove::Forward);
        handle_sequence(&mut change_state, &mut change_area, &theme, "\"bC");
        assert_eq!(change_area.lines(), &["w"]);
        assert_eq!(change_state.mode, VimMode::Insert);
        assert_eq!(
            change_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("xyz", VimRegisterKind::Characterwise))
        );

        let mut newline_state = VimState::new(true);
        let mut newline_area = TextArea::from(["left", "right"]);
        newline_area.move_cursor(CursorMove::End);
        handle_sequence(&mut newline_state, &mut newline_area, &theme, "\"cD");
        assert_eq!(newline_area.lines(), &["leftright"]);
        assert_eq!(
            newline_state.register_for_test(VimRegisterSelector::Named(2)),
            Some(("\n", VimRegisterKind::Characterwise))
        );
    }

    #[test]
    fn linewise_counted_paste_inserts_below_as_one_undoable_change() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["zero", "one"]);
        handle_sequence(&mut state, &mut textarea, &theme, "yy");

        handle_sequence(&mut state, &mut textarea, &theme, "2p");

        assert_eq!(textarea.lines(), &["zero", "zero", "zero", "one"]);
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &["zero", "one"]);
    }

    #[test]
    fn linewise_paste_preserves_a_selected_trailing_empty_line() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["a", ""]);

        handle_sequence(&mut state, &mut textarea, &theme, "2yy");
        handle_sequence(&mut state, &mut textarea, &theme, "p");

        assert_eq!(textarea.lines(), &["a", "a", "", ""]);
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &["a", ""]);
    }

    #[test]
    fn counted_paste_above_one_mib_is_a_handled_noop() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        state.registers.unnamed = Some(VimRegisterValue {
            text: "x".repeat(1024),
            kind: VimRegisterKind::Characterwise,
        });
        let mut textarea = TextArea::from(["keep"]);

        handle_sequence(&mut state, &mut textarea, &theme, "9999p");

        assert_eq!(textarea.lines(), &["keep"]);
        assert!(!textarea.undo());
    }

    #[test]
    fn linewise_paste_counts_the_leading_newline_in_the_one_mib_bound() {
        let theme = Theme::named(ThemeName::Dark);
        for (body_bytes, should_paste) in [
            (MAX_VIM_PASTE_BYTES - 1, true),
            (MAX_VIM_PASTE_BYTES, false),
        ] {
            let mut state = VimState::new(true);
            state.registers.unnamed = Some(VimRegisterValue {
                text: "x".repeat(body_bytes),
                kind: VimRegisterKind::Linewise,
            });
            let mut textarea = TextArea::from(["keep"]);

            state.handle(input('p'), &mut textarea, &theme);

            if should_paste {
                assert_eq!(
                    textarea.lines().join("\n").len(),
                    "keep".len() + MAX_VIM_PASTE_BYTES
                );
                assert!(textarea.undo());
                assert_eq!(textarea.lines(), &["keep"]);
            } else {
                assert_eq!(textarea.lines(), &["keep"]);
                assert!(!textarea.undo());
            }
        }
    }

    #[test]
    fn visual_yank_and_delete_write_selected_named_register() {
        let theme = Theme::named(ThemeName::Dark);
        let mut yank_state = VimState::new(true);
        let mut yank_area = TextArea::from(["abcd"]);
        yank_state.handle(input('v'), &mut yank_area, &theme);
        yank_state.handle(input('l'), &mut yank_area, &theme);
        handle_sequence(&mut yank_state, &mut yank_area, &theme, "\"by");
        assert_eq!(
            yank_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("a", VimRegisterKind::Characterwise))
        );
        assert_eq!(yank_area.lines(), &["abcd"]);

        let mut delete_state = VimState::new(true);
        let mut delete_area = TextArea::from(["abcd"]);
        delete_state.handle(input('v'), &mut delete_area, &theme);
        delete_state.handle(input('l'), &mut delete_area, &theme);
        handle_sequence(&mut delete_state, &mut delete_area, &theme, "\"bd");
        assert_eq!(delete_area.lines(), &["bcd"]);
        assert_eq!(
            delete_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("a", VimRegisterKind::Characterwise))
        );
    }

    #[test]
    fn zero_width_visual_commands_do_not_copy_stale_yank_text() {
        let theme = Theme::named(ThemeName::Dark);
        for command in ['y', 'd', 'c'] {
            let mut state = VimState::new(true);
            state.registers.write(
                VimRegisterSelector::Named(1),
                VimRegisterValue {
                    text: "saved".to_string(),
                    kind: VimRegisterKind::Characterwise,
                },
            );
            let mut textarea = TextArea::from(["abcd"]);
            textarea.set_yank_text("stale".to_string());

            state.handle(input('v'), &mut textarea, &theme);
            handle_sequence(&mut state, &mut textarea, &theme, &format!("\"b{command}"));

            assert_eq!(
                state.register_for_test(VimRegisterSelector::Named(1)),
                Some(("saved", VimRegisterKind::Characterwise)),
                "{command}"
            );
            assert_eq!(textarea.lines(), &["abcd"], "{command}");
        }
    }

    #[test]
    fn dot_repeats_x_and_counted_line_delete() {
        let theme = Theme::named(ThemeName::Dark);

        let mut x_state = VimState::new(true);
        let mut x_area = TextArea::from(["abcd"]);
        x_state.handle(input('x'), &mut x_area, &theme);
        x_state.handle(input('.'), &mut x_area, &theme);
        assert_eq!(x_area.lines(), &["cd"]);

        let mut dd_state = VimState::new(true);
        let mut dd_area = TextArea::from(["zero", "one", "two", "three", "four"]);
        handle_sequence(&mut dd_state, &mut dd_area, &theme, "2dd.");
        assert_eq!(dd_area.lines(), &["four"]);
    }

    #[test]
    fn count_before_dot_multiplies_the_stored_count_with_a_bound() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["abcdefgh"]);
        state.handle(input('x'), &mut textarea, &theme);
        handle_sequence(&mut state, &mut textarea, &theme, "3.");
        assert_eq!(textarea.lines(), &["efgh"]);
    }

    #[test]
    fn failed_change_movement_yank_and_undo_do_not_replace_repeat() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["abcd"]);
        state.handle(input('x'), &mut textarea, &theme);
        handle_sequence(&mut state, &mut textarea, &theme, "yy");
        state.handle(input('l'), &mut textarea, &theme);
        move_to_line_end(&mut textarea);
        state.handle(input('x'), &mut textarea, &theme);
        textarea.move_cursor(CursorMove::Back);
        state.handle(input('.'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bc"]);

        state.handle(input('u'), &mut textarea, &theme);
        textarea.move_cursor(CursorMove::Back);
        state.handle(input('.'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bc"]);
    }

    #[test]
    fn named_paste_repeat_reads_the_registers_current_value() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["abc"]);
        handle_sequence(&mut state, &mut textarea, &theme, "\"ax");
        textarea.move_cursor(CursorMove::End);
        handle_sequence(&mut state, &mut textarea, &theme, "\"ap");
        state.registers.write(
            VimRegisterSelector::Named(0),
            VimRegisterValue {
                text: "Z".to_string(),
                kind: VimRegisterKind::Characterwise,
            },
        );
        state.handle(input('.'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bcaZ"]);
    }

    #[test]
    fn dot_without_a_previous_change_is_a_safe_noop() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["abc"]);
        assert!(!state.handle(input('.'), &mut textarea, &theme));
        assert_eq!(textarea.lines(), &["abc"]);
    }

    #[test]
    fn vi_insert_esc_returns_to_normal() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::default();
        state.handle(input('i'), &mut textarea, &theme);
        assert_eq!(state.mode, VimMode::Insert);
        state.handle(
            Input {
                key: Key::Esc,
                ctrl: false,
                alt: false,
                shift: false,
            },
            &mut textarea,
            &theme,
        );
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn vi_normal_x_deletes_character() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(vec!["abc".to_string()]);
        state.handle(input('x'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bc".to_string()]);
    }

    #[test]
    fn vim_normal_mode_resolves_transcript_search_intents() {
        let state = VimState::new(true);
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('/')),
            Some(VimTranscriptSearchIntent::Open)
        );
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('n')),
            Some(VimTranscriptSearchIntent::Next)
        );
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('N')),
            Some(VimTranscriptSearchIntent::Previous)
        );
    }

    #[test]
    fn vim_insert_and_visual_modes_do_not_resolve_search_intents() {
        let mut state = VimState::new(true);
        state.mode = VimMode::Insert;
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('/')),
            None
        );
        state.mode = VimMode::Visual;
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('n')),
            None
        );
    }
}
