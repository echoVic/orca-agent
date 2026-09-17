use super::wire::{WireInputParam, WireParams, WireUserInput};
use crate::mentions::{MentionBinding, MentionBindings};

pub(super) struct TurnStartInput {
    pub(super) prompt: String,
    pub(super) bindings: MentionBindings,
}

pub(super) fn prompt_from_turn_start_params(params: Option<WireParams>) -> String {
    turn_start_input_from_params(params).prompt
}

pub(super) fn turn_start_input_from_params(params: Option<WireParams>) -> TurnStartInput {
    let Some(params) = params else {
        return TurnStartInput {
            prompt: String::new(),
            bindings: MentionBindings::default(),
        };
    };
    // `input` accepts either a block list or the bare-string shorthand; both must deliver the
    // text. Treating the shorthand as "no input" started an empty turn (and billed it) while
    // every other method on this surface (`thread/queue/add`, `command/exec`) honours it.
    let input = match params.input {
        Some(WireInputParam::Items(items)) => items,
        Some(WireInputParam::Text(text)) => vec![WireUserInput::Text { text }],
        None => Vec::new(),
    };
    let mut prompt = String::new();
    let mut bindings = Vec::new();
    for item in input {
        match item {
            WireUserInput::Text { text } => {
                if !prompt.is_empty() {
                    prompt.push('\n');
                }
                prompt.push_str(&text);
            }
            WireUserInput::Mention {
                name,
                target,
                start,
                end,
            } => {
                if let (Some(start), Some(end)) = (start, end) {
                    bindings.push(MentionBinding {
                        start,
                        end,
                        visible: prompt.get(start..end).unwrap_or_default().to_string(),
                        target,
                    });
                    continue;
                }
                if !prompt.is_empty() && !prompt.ends_with(char::is_whitespace) {
                    prompt.push(' ');
                }
                let start = prompt.len();
                let visible = if name.contains(char::is_whitespace) {
                    format!("@\"{name}\"")
                } else {
                    format!("@{name}")
                };
                prompt.push_str(&visible);
                let end = prompt.len();
                bindings.push(MentionBinding {
                    start,
                    end,
                    visible,
                    target,
                });
            }
            WireUserInput::Skill {
                name: Some(name),
                path: Some(path),
            } => {
                if !prompt.is_empty() && !prompt.ends_with(char::is_whitespace) {
                    prompt.push(' ');
                }
                let start = prompt.len();
                let visible = if name.contains(char::is_whitespace) {
                    format!("@\"{name}\"")
                } else {
                    format!("@{name}")
                };
                prompt.push_str(&visible);
                let end = prompt.len();
                bindings.push(MentionBinding {
                    start,
                    end,
                    visible,
                    target: crate::mentions::MentionTarget::Skill { id: name, path },
                });
            }
            WireUserInput::Image {}
            | WireUserInput::LocalImage {}
            | WireUserInput::Skill { .. } => {}
        }
    }
    TurnStartInput {
        bindings: MentionBindings::from_bindings(&prompt, bindings),
        prompt,
    }
}
