use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutScope {
    Global,
    Editor,
    Idle,
    Running,
    Approval,
    ImageViewer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShortcutHint {
    pub scope: ShortcutScope,
    pub keys: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedShortcutHint {
    pub scope: ShortcutScope,
    pub keys: &'static str,
    pub action: &'static str,
    pub has_registered_binding: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutContext {
    Global,
    Editor,
    Idle,
    Running,
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutAction {
    Global(GlobalShortcut),
    Editor(EditorShortcut),
    Idle(IdleShortcut),
    Running(RunningShortcut),
    Approval(ApprovalShortcut),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    key: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub const fn new(key: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { key, modifiers }
    }

    pub fn is_press(&self, event: KeyEvent) -> bool {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }

        normalize_key_parts(self.key, self.modifiers)
            == normalize_key_parts(event.code, event.modifiers)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlobalShortcut {
    Cancel,
    ToggleSideConversation,
    OpenTranscriptSearch,
    ToggleShortcuts,
    ScrollBottom,
    ScrollTop,
    ClearScreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorShortcut {
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordLeft,
    MoveWordRight,
    MoveLineStart,
    MoveLineEnd,
    DeleteBackward,
    DeleteForward,
    DeleteBackwardWord,
    DeleteForwardWord,
    ClearInput,
    DeleteToLineEnd,
    Yank,
    VimEscape,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdleShortcut {
    Submit,
    Newline,
    EditLatestQueued,
    HistoryPrevious,
    HistoryNext,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Backtrack,
    ExpandToolOutput,
    ExpandAll,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunningShortcut {
    BackgroundCurrentTurn,
    Interrupt,
    SubmitNow,
    SubmitQueued,
    Newline,
    EditLatestQueued,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalShortcut {
    SelectAllow,
    SelectDeny,
    ToggleSelection,
    Confirm,
    Approve,
    Deny,
    PreviewPageUp,
    PreviewPageDown,
}

const GLOBAL_BINDINGS: &[(GlobalShortcut, KeyBinding)] = &[
    (
        GlobalShortcut::Cancel,
        KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::OpenTranscriptSearch,
        KeyBinding::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::ToggleSideConversation,
        KeyBinding::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::ToggleShortcuts,
        KeyBinding::new(KeyCode::F(1), KeyModifiers::NONE),
    ),
    (
        GlobalShortcut::ToggleShortcuts,
        KeyBinding::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::ToggleShortcuts,
        KeyBinding::new(KeyCode::Char('?'), KeyModifiers::NONE),
    ),
    (
        GlobalShortcut::ScrollBottom,
        KeyBinding::new(KeyCode::End, KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::ScrollTop,
        KeyBinding::new(KeyCode::Home, KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::ClearScreen,
        KeyBinding::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
    ),
];

const EDITOR_BINDINGS: &[(EditorShortcut, KeyBinding)] = &[
    (
        EditorShortcut::MoveLeft,
        KeyBinding::new(KeyCode::Left, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveLeft,
        KeyBinding::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveRight,
        KeyBinding::new(KeyCode::Right, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveRight,
        KeyBinding::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveUp,
        KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveUp,
        KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveDown,
        KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveDown,
        KeyBinding::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveWordLeft,
        KeyBinding::new(KeyCode::Char('b'), KeyModifiers::ALT),
    ),
    (
        EditorShortcut::MoveWordLeft,
        KeyBinding::new(KeyCode::Left, KeyModifiers::ALT),
    ),
    (
        EditorShortcut::MoveWordLeft,
        KeyBinding::new(KeyCode::Left, KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveWordRight,
        KeyBinding::new(KeyCode::Char('f'), KeyModifiers::ALT),
    ),
    (
        EditorShortcut::MoveWordRight,
        KeyBinding::new(KeyCode::Right, KeyModifiers::ALT),
    ),
    (
        EditorShortcut::MoveWordRight,
        KeyBinding::new(KeyCode::Right, KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveLineStart,
        KeyBinding::new(KeyCode::Home, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveLineStart,
        KeyBinding::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::MoveLineEnd,
        KeyBinding::new(KeyCode::End, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::MoveLineEnd,
        KeyBinding::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteBackward,
        KeyBinding::new(KeyCode::Backspace, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::DeleteBackward,
        KeyBinding::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteForward,
        KeyBinding::new(KeyCode::Delete, KeyModifiers::NONE),
    ),
    (
        EditorShortcut::DeleteForward,
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteBackwardWord,
        KeyBinding::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteBackwardWord,
        KeyBinding::new(KeyCode::Backspace, KeyModifiers::ALT),
    ),
    (
        EditorShortcut::DeleteBackwardWord,
        KeyBinding::new(KeyCode::Backspace, KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteForwardWord,
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::ALT),
    ),
    (
        EditorShortcut::DeleteForwardWord,
        KeyBinding::new(KeyCode::Delete, KeyModifiers::ALT),
    ),
    (
        EditorShortcut::DeleteForwardWord,
        KeyBinding::new(KeyCode::Delete, KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::ClearInput,
        KeyBinding::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::DeleteToLineEnd,
        KeyBinding::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::Yank,
        KeyBinding::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
    ),
    (
        EditorShortcut::VimEscape,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
];

const IDLE_BINDINGS: &[(IdleShortcut, KeyBinding)] = &[
    (
        IdleShortcut::Submit,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ),
    (
        IdleShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
    ),
    (
        IdleShortcut::Newline,
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::EditLatestQueued,
        KeyBinding::new(KeyCode::Up, KeyModifiers::ALT),
    ),
    (
        IdleShortcut::HistoryPrevious,
        KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::HistoryNext,
        KeyBinding::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::HistoryPrevious,
        KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::HistoryNext,
        KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::PageUp,
        KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::PageDown,
        KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::HalfPageUp,
        KeyBinding::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::HalfPageDown,
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::Backtrack,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::ExpandToolOutput,
        KeyBinding::new(KeyCode::Char('e'), KeyModifiers::NONE),
    ),
    (
        IdleShortcut::ExpandAll,
        KeyBinding::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
    ),
];

const RUNNING_BINDINGS: &[(RunningShortcut, KeyBinding)] = &[
    (
        RunningShortcut::BackgroundCurrentTurn,
        KeyBinding::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::Interrupt,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::Interrupt,
        KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::SubmitNow,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::SubmitQueued,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::EditLatestQueued,
        KeyBinding::new(KeyCode::Up, KeyModifiers::ALT),
    ),
    (
        RunningShortcut::ScrollUp,
        KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::ScrollDown,
        KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::PageUp,
        KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::PageDown,
        KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::HalfPageUp,
        KeyBinding::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::HalfPageDown,
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ),
];

const APPROVAL_BINDINGS: &[(ApprovalShortcut, KeyBinding)] = &[
    (
        ApprovalShortcut::SelectAllow,
        KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::SelectAllow,
        KeyBinding::new(KeyCode::Char('k'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::SelectDeny,
        KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::SelectDeny,
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::ToggleSelection,
        KeyBinding::new(KeyCode::Tab, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::ToggleSelection,
        KeyBinding::new(KeyCode::BackTab, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::ToggleSelection,
        KeyBinding::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    ),
    (
        ApprovalShortcut::Confirm,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::Approve,
        KeyBinding::new(KeyCode::Char('y'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::Approve,
        KeyBinding::new(KeyCode::Char('a'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::Deny,
        KeyBinding::new(KeyCode::Char('n'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::Deny,
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::Deny,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::PreviewPageUp,
        KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
    ),
    (
        ApprovalShortcut::PreviewPageDown,
        KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE),
    ),
];

pub fn resolve_shortcut(context: ShortcutContext, event: KeyEvent) -> Option<ShortcutAction> {
    match context {
        ShortcutContext::Global => global_shortcut(event).map(ShortcutAction::Global),
        ShortcutContext::Editor => editor_shortcut(event).map(ShortcutAction::Editor),
        ShortcutContext::Idle => idle_shortcut(event).map(ShortcutAction::Idle),
        ShortcutContext::Running => running_shortcut(event).map(ShortcutAction::Running),
        ShortcutContext::Approval => approval_shortcut(event).map(ShortcutAction::Approval),
    }
}

pub fn global_shortcut(event: KeyEvent) -> Option<GlobalShortcut> {
    match_binding(event, GLOBAL_BINDINGS)
}

pub fn editor_shortcut(event: KeyEvent) -> Option<EditorShortcut> {
    match_binding(event, EDITOR_BINDINGS)
}

pub fn idle_shortcut(event: KeyEvent) -> Option<IdleShortcut> {
    match_binding(event, IDLE_BINDINGS)
}

pub fn running_shortcut(event: KeyEvent) -> Option<RunningShortcut> {
    match_binding(event, RUNNING_BINDINGS)
}

pub fn approval_shortcut(event: KeyEvent) -> Option<ApprovalShortcut> {
    match_binding(event, APPROVAL_BINDINGS)
}

pub fn shortcut_hints() -> impl Iterator<Item = ResolvedShortcutHint> {
    SHORTCUT_HINTS.iter().map(|hint| ResolvedShortcutHint {
        scope: hint.scope,
        keys: hint.keys,
        action: hint.action,
        has_registered_binding: scope_has_registered_binding(hint.scope),
    })
}

pub fn shortcut_lines(scopes: &[ShortcutScope], theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let sections = [
        (ShortcutScope::Global, "Global"),
        (ShortcutScope::Editor, "Editor"),
        (ShortcutScope::Idle, "Composer"),
        (ShortcutScope::Running, "Running"),
        (ShortcutScope::Approval, "Approval"),
        (ShortcutScope::ImageViewer, "Image viewer"),
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
        lines.push(Line::from(Span::styled(
            title.to_string(),
            theme.accent_style().add_modifier(Modifier::BOLD),
        )));
        for hint in shortcut_hints().filter(|hint| hint.scope == scope) {
            let mut rows = wrap_words(hint.action, action_width).into_iter();
            let first = rows.next().unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}{:<key_width$}   ", " ".repeat(indent), hint.keys),
                    Style::default().fg(theme.text),
                ),
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

/// Wraps `text` on whitespace only, so a single long token (e.g. a hyphenated
/// word) never splits mid-token the way a generic wrapper that also breaks on
/// `-`/`/` would.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate_width = UnicodeWidthStr::width(current.as_str())
            + usize::from(!current.is_empty())
            + UnicodeWidthStr::width(word);
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

pub const SHORTCUT_HINTS: &[ShortcutHint] = &[
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+/",
        action: "open or return from side conversation",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+f",
        action: "find in transcript when input is empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "? / F1 / ctrl+k",
        action: "show this help (input must be empty)",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+c",
        action: "cancel and quit",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+home/end",
        action: "jump to top or bottom",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+l",
        action: "clear screen",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "shift+tab",
        action: "cycle approval mode",
    },
    ShortcutHint {
        scope: ShortcutScope::Editor,
        keys: "ctrl+a/e",
        action: "move to line start or end",
    },
    ShortcutHint {
        scope: ShortcutScope::Editor,
        keys: "ctrl+b/f / alt+b/f",
        action: "move by character or word",
    },
    ShortcutHint {
        scope: ShortcutScope::Editor,
        keys: "ctrl+w/d/k/u",
        action: "delete word, character, line end, or clear input",
    },
    ShortcutHint {
        scope: ShortcutScope::Editor,
        keys: "ctrl+v / win alt+v",
        action: "attach image from clipboard",
    },
    ShortcutHint {
        scope: ShortcutScope::Editor,
        keys: "enter on image",
        action: "open image preview",
    },
    ShortcutHint {
        scope: ShortcutScope::ImageViewer,
        keys: "+/- · arrows · 0",
        action: "zoom, pan, or fit image",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "enter",
        action: "send message",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "alt+enter / shift+enter",
        action: "insert newline",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "alt+up",
        action: "edit latest queued message",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "up/down / ctrl+p/ctrl+n",
        action: "history when empty; otherwise edit input",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "pgup/pgdn",
        action: "scroll one page",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "ctrl+u",
        action: "clear input, or scroll half page when empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "ctrl+d",
        action: "delete forward, or scroll when input is empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "esc",
        action: "backtrack only when input is empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "e · Shift+E",
        action: "expand latest tool output · expand all",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+b",
        action: "move left, or background when input is empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "esc / ctrl+g",
        action: "interrupt current turn",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "enter",
        action: "queue follow-up",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+enter",
        action: "send now: the running turn reads it after its current step (needs a terminal with the kitty keyboard protocol)",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "alt+enter / shift+enter",
        action: "insert newline",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "alt+up",
        action: "edit latest queued message",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "up/down",
        action: "edit multiline input, otherwise scroll",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "pgup/pgdn",
        action: "scroll one page",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+u",
        action: "clear input, or scroll half page when empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+d",
        action: "delete forward, or scroll when input is empty",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "up/down/j/k",
        action: "move selection",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "tab",
        action: "toggle selection",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "enter",
        action: "confirm selected action",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "1/2/3",
        action: "allow options",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "4",
        action: "deny",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "y/A/a/n",
        action: "legacy direct keys",
    },
];

fn scope_has_registered_binding(scope: ShortcutScope) -> bool {
    match scope {
        ShortcutScope::Global => !GLOBAL_BINDINGS.is_empty(),
        ShortcutScope::Editor => !EDITOR_BINDINGS.is_empty(),
        ShortcutScope::Idle => !IDLE_BINDINGS.is_empty(),
        ShortcutScope::Running => !RUNNING_BINDINGS.is_empty(),
        ShortcutScope::Approval => !APPROVAL_BINDINGS.is_empty(),
        // Image viewer keys are handled directly in composer_image_actions,
        // not through one of this file's KeyBinding resolver tables.
        ShortcutScope::ImageViewer => false,
    }
}

fn match_binding<T: Copy>(event: KeyEvent, bindings: &[(T, KeyBinding)]) -> Option<T> {
    bindings
        .iter()
        .find(|(_, binding)| binding.is_press(event))
        .map(|(action, _)| *action)
}

fn normalize_key_parts(key: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(ch) = key else {
        return (key, modifiers);
    };

    if modifiers.is_empty() {
        if let Some(ctrl_char) = c0_control_char_to_ctrl_char(ch) {
            return (KeyCode::Char(ctrl_char), KeyModifiers::CONTROL);
        }
    }

    if ch.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        return (KeyCode::Char(ch.to_ascii_lowercase()), modifiers);
    }

    (key, modifiers)
}

fn c0_control_char_to_ctrl_char(ch: char) -> Option<char> {
    let code = u32::from(ch);
    match code {
        0x00 => Some(' '),
        0x01..=0x1a => char::from_u32(code - 0x01 + u32::from('a')),
        0x1c..=0x1f => char::from_u32(code - 0x1c + u32::from('4')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn control_binding_matches_raw_c0_characters() {
        let binding = KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL);

        assert!(binding.is_press(key(KeyCode::Char('\n'), KeyModifiers::NONE)));
    }

    #[test]
    fn shifted_binding_matches_uppercase_characters() {
        let binding = KeyBinding::new(KeyCode::Char('a'), KeyModifiers::SHIFT);

        assert!(binding.is_press(key(KeyCode::Char('A'), KeyModifiers::NONE)));
        assert!(binding.is_press(key(KeyCode::Char('A'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn release_events_do_not_trigger_shortcuts() {
        let binding = KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        };

        assert!(!binding.is_press(release));
    }

    #[test]
    fn idle_shortcuts_resolve_history_navigation() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(IdleShortcut::HistoryPrevious)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(IdleShortcut::HistoryNext)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(IdleShortcut::HistoryPrevious)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(IdleShortcut::HistoryNext)
        );
    }

    #[test]
    fn idle_shortcuts_distinguish_enter_from_shift_enter() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(IdleShortcut::Submit)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(IdleShortcut::Newline)
        );
    }

    #[test]
    fn idle_shortcuts_resolve_tool_output_expand() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(IdleShortcut::ExpandToolOutput)
        );
    }

    #[test]
    fn idle_shortcuts_resolve_expand_all() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('E'), KeyModifiers::SHIFT)),
            Some(IdleShortcut::ExpandAll)
        );
        // Some terminals report the shifted letter without also setting the
        // modifier bit; `normalize_key_parts` must still resolve it.
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('E'), KeyModifiers::NONE)),
            Some(IdleShortcut::ExpandAll)
        );
    }

    #[test]
    fn expand_hint_documents_both_e_and_shift_e() {
        let hint = shortcut_hints()
            .find(|hint| hint.scope == ShortcutScope::Idle && hint.keys.contains('E'))
            .expect("an Idle-scope hint mentioning E");
        assert_eq!(hint.keys, "e · Shift+E");
        assert_eq!(hint.action, "expand latest tool output · expand all");
    }

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

    #[test]
    fn running_shortcuts_resolve_background_current_turn() {
        assert_eq!(
            running_shortcut(key(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(RunningShortcut::BackgroundCurrentTurn)
        );
    }

    #[test]
    fn queued_message_shortcuts_are_context_specific() {
        assert_eq!(
            resolve_shortcut(ShortcutContext::Idle, key(KeyCode::Up, KeyModifiers::ALT)),
            Some(ShortcutAction::Idle(IdleShortcut::EditLatestQueued))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Up, KeyModifiers::ALT)
            ),
            Some(ShortcutAction::Running(RunningShortcut::EditLatestQueued))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Running(RunningShortcut::SubmitQueued))
        );
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert_eq!(
                resolve_shortcut(ShortcutContext::Running, key(KeyCode::Enter, modifiers)),
                Some(ShortcutAction::Running(RunningShortcut::Newline))
            );
        }
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            Some(ShortcutAction::Running(RunningShortcut::Newline))
        );
    }

    #[test]
    fn global_ctrl_f_opens_transcript_search() {
        assert_eq!(
            global_shortcut(key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(GlobalShortcut::OpenTranscriptSearch)
        );
    }

    #[test]
    fn search_shortcut_hint_is_backed_by_a_binding() {
        assert!(shortcut_hints().any(|hint| {
            hint.scope == ShortcutScope::Global
                && hint.keys == "ctrl+f"
                && hint.has_registered_binding
        }));
    }

    #[test]
    fn question_mark_toggles_help() {
        assert_eq!(
            global_shortcut(key(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(GlobalShortcut::ToggleShortcuts)
        );
    }

    #[test]
    fn shortcut_resolver_keeps_global_and_editor_contexts_separate() {
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Idle,
                key(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            None
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Global,
                key(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            Some(ShortcutAction::Global(GlobalShortcut::ToggleShortcuts))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Editor,
                key(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            Some(ShortcutAction::Editor(EditorShortcut::DeleteToLineEnd))
        );
    }

    #[test]
    fn editor_shortcuts_cover_readline_navigation_and_deletion() {
        for (code, modifiers, expected) in [
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                EditorShortcut::MoveLineStart,
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                EditorShortcut::MoveLineEnd,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                EditorShortcut::MoveLeft,
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
                EditorShortcut::MoveRight,
            ),
            (
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
                EditorShortcut::DeleteBackwardWord,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                EditorShortcut::ClearInput,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                EditorShortcut::DeleteForward,
            ),
        ] {
            assert_eq!(editor_shortcut(key(code, modifiers)), Some(expected));
        }
    }

    #[test]
    fn known_cross_context_collisions_are_explicitly_classified() {
        for (code, modifiers, context) in [
            (
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
                ShortcutContext::Global,
            ),
            (
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
                ShortcutContext::Global,
            ),
            (
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
                ShortcutContext::Idle,
            ),
            (
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
                ShortcutContext::Idle,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                ShortcutContext::Idle,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                ShortcutContext::Idle,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                ShortcutContext::Running,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                ShortcutContext::Running,
            ),
            (
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                ShortcutContext::Running,
            ),
            (KeyCode::Esc, KeyModifiers::NONE, ShortcutContext::Idle),
            (KeyCode::Esc, KeyModifiers::NONE, ShortcutContext::Running),
        ] {
            let event = key(code, modifiers);
            assert!(
                resolve_shortcut(ShortcutContext::Editor, event).is_some(),
                "{event:?} must have an editor meaning"
            );
            assert!(
                resolve_shortcut(context, event).is_some(),
                "{event:?} must keep its {context:?} fallback"
            );
        }
    }

    #[test]
    fn ctrl_slash_toggles_side_conversation() {
        assert_eq!(
            global_shortcut(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL,)),
            Some(GlobalShortcut::ToggleSideConversation)
        );
    }

    #[test]
    fn shortcut_resolver_interprets_same_key_by_context() {
        assert_eq!(
            resolve_shortcut(ShortcutContext::Idle, key(KeyCode::Up, KeyModifiers::NONE)),
            Some(ShortcutAction::Idle(IdleShortcut::HistoryPrevious))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Running(RunningShortcut::ScrollUp))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Approval,
                key(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Approval(ApprovalShortcut::SelectAllow))
        );
    }

    #[test]
    fn esc_denies_an_approval() {
        // The approval panel's hint has always read "Esc deny" (ui.rs's
        // `render_approval_panel`), but until now no binding backed it, so
        // the key was silently swallowed by `handle_approval_dialog_key`'s
        // catch-all arm. Pin the resolver half of the fix here; the
        // approval_dialog_actions test pins that it actually resolves the
        // dialog end to end.
        assert_eq!(
            approval_shortcut(key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(ApprovalShortcut::Deny)
        );
    }

    #[test]
    fn shortcut_hints_are_backed_by_registered_bindings() {
        for hint in shortcut_hints() {
            // Image viewer keys are handled directly in composer_image_actions,
            // not resolved through this file's KeyBinding tables, so they have
            // no resolver binding to point back to.
            if hint.scope == ShortcutScope::ImageViewer {
                continue;
            }
            assert!(
                hint.has_registered_binding,
                "shortcut hint '{}' in {:?} must be backed by a resolver binding",
                hint.keys, hint.scope
            );
        }
    }

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    #[test]
    fn shortcut_lines_align_keys_in_one_column_and_wrap_with_hanging_indent() {
        let theme = theme();
        let lines = shortcut_lines(&[ShortcutScope::Editor], &theme, 40);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "Editor");
        assert!(
            text.iter()
                .all(|row| UnicodeWidthStr::width(row.as_str()) <= 40),
            "{text:?}"
        );
        assert!(
            text[1].starts_with("  "),
            "keys are indented two cells: {:?}",
            text[1]
        );
        let action_col = lines[1].spans[0].content.len();
        assert!(
            text.iter()
                .skip(2)
                .any(|row| row.starts_with(&" ".repeat(action_col)) && !row.trim().is_empty()),
            "expected a wrapped continuation row with hanging indent: {text:?}"
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.border));
    }
}
