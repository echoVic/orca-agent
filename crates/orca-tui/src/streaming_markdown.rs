#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamingMarkdownAction {
    UpdateTail(String),
    ExtendTail(String),
    FreezeTail { text: String, trailing_blank: bool },
    AppendFrozen { text: String, trailing_blank: bool },
    ClearTail,
    FinishTail(String),
}

#[derive(Default)]
pub(crate) struct StreamingMarkdownAssembler {
    partial_line: String,
    current_block: String,
    pipe_candidate: Option<String>,
    active_table: Option<String>,
    fence: Option<FenceState>,
    finished: bool,
    restored_reply: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FenceState {
    marker: char,
    run_len: usize,
}

impl StreamingMarkdownAssembler {
    pub(crate) fn restore_open_reply(&mut self) {
        self.restored_reply = true;
    }

    pub(crate) fn push(&mut self, text: &str) -> Vec<StreamingMarkdownAction> {
        if self.finished || text.is_empty() {
            return Vec::new();
        }
        if self.restored_reply {
            return vec![StreamingMarkdownAction::ExtendTail(text.to_owned())];
        }
        self.partial_line.push_str(text);
        let Some(last_newline) = self.partial_line.rfind('\n') else {
            return Vec::new();
        };
        let complete = self.partial_line[..=last_newline].to_owned();
        self.partial_line.drain(..=last_newline);
        let mut actions = Vec::new();
        let mut current_block_changed = false;
        for line in complete.split_inclusive('\n') {
            self.process_complete_line(line, &mut actions, &mut current_block_changed);
        }
        if current_block_changed && !self.current_block.is_empty() {
            actions.push(StreamingMarkdownAction::UpdateTail(
                self.current_block.clone(),
            ));
        }
        actions
    }

    pub(crate) fn finish(&mut self) -> Vec<StreamingMarkdownAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if let Some(mut table) = self.active_table.take() {
            let partial_line = std::mem::take(&mut self.partial_line);
            if table_row(&partial_line) {
                table.push_str(&partial_line);
            }
            let mut actions = vec![StreamingMarkdownAction::AppendFrozen {
                text: table,
                trailing_blank: false,
            }];
            if !partial_line.is_empty() && !table_row(&partial_line) {
                actions.push(StreamingMarkdownAction::FinishTail(partial_line));
            }
            return actions;
        }
        let mut hidden_suffix = self.pipe_candidate.take().unwrap_or_default();
        hidden_suffix.push_str(&self.partial_line);
        self.partial_line.clear();
        if self.current_block.is_empty() && hidden_suffix.is_empty() {
            return Vec::new();
        }
        self.current_block.clear();
        vec![StreamingMarkdownAction::FinishTail(hidden_suffix)]
    }

    #[cfg(test)]
    fn held_text_for_test(&self) -> String {
        let mut held = self
            .active_table
            .as_ref()
            .or(self.pipe_candidate.as_ref())
            .cloned()
            .unwrap_or_default();
        held.push_str(&self.partial_line);
        held
    }

    fn process_complete_line(
        &mut self,
        line: &str,
        actions: &mut Vec<StreamingMarkdownAction>,
        current_block_changed: &mut bool,
    ) {
        if let Some(table) = self.active_table.as_mut() {
            if line_is_blank(line) {
                table.push_str(line);
                self.release_table(actions, true);
                return;
            }
            if table_row(line) {
                table.push_str(line);
                return;
            }
            self.release_table(actions, false);
        }

        if let Some(candidate) = self.pipe_candidate.take() {
            if table_delimiter(line) {
                if !self.current_block.is_empty() {
                    self.freeze_current_block(actions, false);
                    *current_block_changed = false;
                }
                let mut table = candidate;
                table.push_str(line);
                self.active_table = Some(table);
                return;
            }
            self.current_block.push_str(&candidate);
            *current_block_changed = true;
        }

        if let Some(fence) = self.fence {
            self.current_block.push_str(line);
            *current_block_changed = true;
            if fence_closes(line, fence) {
                self.fence = None;
                self.freeze_current_block(actions, false);
                *current_block_changed = false;
            }
            return;
        }

        if let Some(fence) = fence_open(line) {
            if !self.current_block.is_empty() {
                self.freeze_current_block(actions, false);
                *current_block_changed = false;
            }
            self.current_block.push_str(line);
            *current_block_changed = true;
            self.fence = Some(fence);
        } else if plausible_table_header(line) {
            self.pipe_candidate = Some(line.to_owned());
        } else {
            let had_content = !self.current_block.trim().is_empty();
            self.current_block.push_str(line);
            *current_block_changed = true;
            if line_is_blank(line) && had_content {
                self.freeze_current_block(actions, true);
                *current_block_changed = false;
            }
        }
    }

    fn freeze_current_block(
        &mut self,
        actions: &mut Vec<StreamingMarkdownAction>,
        trailing_blank: bool,
    ) {
        let text = std::mem::take(&mut self.current_block);
        actions.push(StreamingMarkdownAction::UpdateTail(text.clone()));
        actions.push(StreamingMarkdownAction::FreezeTail {
            text,
            trailing_blank,
        });
        actions.push(StreamingMarkdownAction::ClearTail);
    }

    fn release_table(&mut self, actions: &mut Vec<StreamingMarkdownAction>, trailing_blank: bool) {
        if let Some(text) = self.active_table.take() {
            actions.push(StreamingMarkdownAction::AppendFrozen {
                text,
                trailing_blank,
            });
        }
    }
}

fn fence_open(line: &str) -> Option<FenceState> {
    let content = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = content.trim_start_matches(' ');
    if content.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run_len = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (run_len >= 3).then_some(FenceState { marker, run_len })
}

fn fence_closes(line: &str, fence: FenceState) -> bool {
    let content = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = content.trim_start_matches(' ');
    if content.len() - trimmed.len() > 3 {
        return false;
    }
    let run_len = trimmed
        .chars()
        .take_while(|character| *character == fence.marker)
        .count();
    run_len >= fence.run_len && trimmed[run_len..].trim().is_empty()
}

fn line_is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn unescaped_pipe_cells(line: &str) -> Option<Vec<&str>> {
    let content = line.strip_suffix('\n').unwrap_or(line);
    let mut separators = Vec::new();
    let mut preceding_backslashes = 0;
    for (index, character) in content.char_indices() {
        if character == '\\' {
            preceding_backslashes += 1;
            continue;
        }
        if character == '|' && preceding_backslashes % 2 == 0 {
            separators.push(index);
        }
        preceding_backslashes = 0;
    }
    if separators.is_empty() {
        return None;
    }

    let mut cells = Vec::with_capacity(separators.len() + 1);
    let mut start = 0;
    for separator in separators {
        cells.push(&content[start..separator]);
        start = separator + 1;
    }
    cells.push(&content[start..]);
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    Some(cells)
}

fn plausible_table_header(line: &str) -> bool {
    let Some(cells) = unescaped_pipe_cells(line) else {
        return false;
    };
    !cells.is_empty() && cells.iter().any(|cell| !cell.trim().is_empty()) && !table_delimiter(line)
}

fn table_delimiter(line: &str) -> bool {
    let Some(cells) = unescaped_pipe_cells(line) else {
        return false;
    };
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let trimmed = cell.trim();
            let without_left = trimmed.strip_prefix(':').unwrap_or(trimmed);
            let without_colons = without_left.strip_suffix(':').unwrap_or(without_left);
            !without_colons.is_empty() && without_colons.chars().all(|character| character == '-')
        })
}

fn table_row(line: &str) -> bool {
    unescaped_pipe_cells(line)
        .is_some_and(|cells| !cells.is_empty() && cells.iter().any(|cell| !cell.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::{StreamingMarkdownAction, StreamingMarkdownAssembler};

    fn reconstructed_action_text(actions: &[StreamingMarkdownAction]) -> String {
        let mut frozen = String::new();
        let mut tail = String::new();
        for action in actions {
            match action {
                StreamingMarkdownAction::UpdateTail(text) => tail.clone_from(text),
                StreamingMarkdownAction::ExtendTail(text) => tail.push_str(text),
                StreamingMarkdownAction::FreezeTail { text, .. } => {
                    frozen.push_str(text);
                    tail.clear();
                }
                StreamingMarkdownAction::AppendFrozen { text, .. } => frozen.push_str(text),
                StreamingMarkdownAction::ClearTail => tail.clear(),
                StreamingMarkdownAction::FinishTail(text) => tail.push_str(text),
            }
        }
        frozen.push_str(&tail);
        frozen
    }

    #[test]
    fn partial_source_line_stays_hidden_until_newline_or_finish() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(assembler.push("hello").is_empty());
        assert_eq!(assembler.held_text_for_test(), "hello");
        assert_eq!(
            assembler.push(" world\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "hello world\n".to_string()
            )]
        );
        assert_eq!(assembler.held_text_for_test(), "");

        assert!(assembler.push("final").is_empty());
        assert_eq!(
            assembler.finish(),
            vec![StreamingMarkdownAction::FinishTail("final".to_string())]
        );
        assert!(assembler.finish().is_empty());
    }

    #[test]
    fn newline_gate_reconstructs_cjk_emoji_and_combining_text_exactly() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let input = ["中", "文👍🏽e\u{301}", "\n尾", "行"];
        let mut actions = Vec::new();
        for piece in input {
            actions.extend(assembler.push(piece));
        }
        actions.extend(assembler.finish());
        assert_eq!(reconstructed_action_text(&actions), "中文👍🏽e\u{301}\n尾行");
    }

    #[test]
    fn blank_line_freezes_the_visible_tail_and_starts_a_fresh_block() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("first paragraph\n\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("first paragraph\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "first paragraph\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
        assert_eq!(
            assembler.push("second paragraph\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "second paragraph\n".to_string()
            )]
        );
    }

    #[test]
    fn consecutive_blank_lines_preserve_source_with_one_display_blank() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let actions = assembler.push("paragraph\n\n\n");
        assert_eq!(
            actions,
            vec![
                StreamingMarkdownAction::UpdateTail("paragraph\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "paragraph\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
                StreamingMarkdownAction::UpdateTail("\n".to_string()),
            ]
        );
        assert_eq!(reconstructed_action_text(&actions), "paragraph\n\n\n");
    }

    #[test]
    fn leading_blank_line_stays_with_following_tail_content() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("\nOutro"),
            vec![StreamingMarkdownAction::UpdateTail("\n".to_string())]
        );
        assert_eq!(
            assembler.finish(),
            vec![StreamingMarkdownAction::FinishTail("Outro".to_string())]
        );
    }

    #[test]
    fn fenced_block_freezes_only_after_matching_close() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("before\n\n```rust\nfn main() {\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("before\n\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "before\n\n".to_string(),
                    trailing_blank: true,
                },
                StreamingMarkdownAction::ClearTail,
                StreamingMarkdownAction::UpdateTail("```rust\nfn main() {\n".to_string()),
            ]
        );
        assert_eq!(
            assembler.push("}\n```\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("```rust\nfn main() {\n}\n```\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "```rust\nfn main() {\n}\n```\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
    }

    #[test]
    fn fence_opener_freezes_preceding_paragraph_without_blank_line() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("before\n```rust\ncode\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("before\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "before\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::ClearTail,
                StreamingMarkdownAction::UpdateTail("```rust\ncode\n".to_string()),
            ]
        );
    }

    #[test]
    fn fences_require_matching_marker_and_sufficient_closing_run() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert_eq!(
            assembler.push("   ~~~~text\n~~~\n````\n~~~~\n"),
            vec![
                StreamingMarkdownAction::UpdateTail("   ~~~~text\n~~~\n````\n~~~~\n".to_string()),
                StreamingMarkdownAction::FreezeTail {
                    text: "   ~~~~text\n~~~\n````\n~~~~\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::ClearTail,
            ]
        );
    }

    #[test]
    fn unfinished_fence_is_emitted_by_finish() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let mut actions = assembler.push("```rust\nlet value = 1;\n");
        actions.extend(assembler.push("unfinished"));
        actions.extend(assembler.finish());
        assert_eq!(
            reconstructed_action_text(&actions),
            "```rust\nlet value = 1;\nunfinished"
        );
    }

    #[test]
    fn action_stream_and_held_suffix_reconstruct_every_input_prefix() {
        let fixtures = [
            vec!["# Head", "ing\n\n- one\n", "- two"],
            vec!["前文\n\n", "~~~text\n中", "文\n~~~\n", "尾"],
            vec!["e\u{301}", "moji 👍🏽\n", "\n````\n", "code\n```", "`\n"],
        ];

        for pieces in fixtures {
            let mut assembler = StreamingMarkdownAssembler::default();
            let mut actions = Vec::new();
            let mut prefix = String::new();
            for piece in pieces {
                prefix.push_str(piece);
                actions.extend(assembler.push(piece));
                let mut reconstructed = reconstructed_action_text(&actions);
                reconstructed.push_str(&assembler.held_text_for_test());
                assert_eq!(reconstructed, prefix);
            }
            actions.extend(assembler.finish());
            assert_eq!(reconstructed_action_text(&actions), prefix);
        }
    }

    #[test]
    fn pipe_header_candidate_is_hidden_until_confirmed_or_rejected() {
        let mut confirmed = StreamingMarkdownAssembler::default();
        assert!(confirmed.push("| Name | Value |\n").is_empty());
        assert_eq!(confirmed.held_text_for_test(), "| Name | Value |\n");
        assert!(confirmed.push("|---|---|\n").is_empty());
        assert_eq!(
            confirmed.held_text_for_test(),
            "| Name | Value |\n|---|---|\n"
        );

        let mut rejected = StreamingMarkdownAssembler::default();
        assert!(rejected.push("A | B\n").is_empty());
        assert_eq!(
            rejected.push("ordinary next line\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "A | B\nordinary next line\n".to_string()
            )]
        );
    }

    #[test]
    fn table_detection_handles_escaped_pipes_empty_cells_and_alignment() {
        let mut escaped = StreamingMarkdownAssembler::default();
        assert_eq!(
            escaped.push(
                r"escaped \| pipe
ordinary
"
            ),
            vec![StreamingMarkdownAction::UpdateTail(
                r"escaped \| pipe
ordinary
"
                .to_string()
            )]
        );

        let mut empty = StreamingMarkdownAssembler::default();
        assert_eq!(
            empty.push("| | |\n|---|---|\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "| | |\n|---|---|\n".to_string()
            )]
        );

        let mut aligned = StreamingMarkdownAssembler::default();
        assert!(
            aligned
                .push("| Left | Center | Right |\n|:---|:---:|---:|\n")
                .is_empty()
        );
    }

    #[test]
    fn single_column_table_with_structural_pipes_is_held_and_released() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(assembler.push("| Name |\n|---|\n| A |\n").is_empty());
        assert_eq!(
            assembler.finish(),
            vec![StreamingMarkdownAction::AppendFrozen {
                text: "| Name |\n|---|\n| A |\n".to_string(),
                trailing_blank: false,
            }]
        );
    }

    #[test]
    fn confirmed_table_remains_hidden_until_boundary_then_emits_once() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(assembler.push("| Name | Value |\n|---|---|\n").is_empty());
        assert!(assembler.push("| A | 1 |\n").is_empty());
        assert_eq!(
            assembler.push("\n"),
            vec![StreamingMarkdownAction::AppendFrozen {
                text: "| Name | Value |\n|---|---|\n| A | 1 |\n\n".to_string(),
                trailing_blank: true,
            }]
        );
        assert!(assembler.finish().is_empty());
    }

    #[test]
    fn table_releases_before_non_table_terminator_and_at_finish() {
        let mut terminated = StreamingMarkdownAssembler::default();
        assert!(
            terminated
                .push("| Name | Value |\n|---|---|\n| A | 1 |\n")
                .is_empty()
        );
        assert_eq!(
            terminated.push("after\n"),
            vec![
                StreamingMarkdownAction::AppendFrozen {
                    text: "| Name | Value |\n|---|---|\n| A | 1 |\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::UpdateTail("after\n".to_string()),
            ]
        );

        let mut finished = StreamingMarkdownAssembler::default();
        assert!(
            finished
                .push("| Name | Value |\n|---|---|\n| A | 1 |\n")
                .is_empty()
        );
        assert_eq!(
            finished.finish(),
            vec![StreamingMarkdownAction::AppendFrozen {
                text: "| Name | Value |\n|---|---|\n| A | 1 |\n".to_string(),
                trailing_blank: false,
            }]
        );
    }

    #[test]
    fn table_finish_keeps_partial_non_table_tail_separate() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(
            assembler
                .push("| Name | Value |\n|---|---|\n| A | 1 |\n")
                .is_empty()
        );
        assert!(assembler.push("after").is_empty());
        assert_eq!(
            assembler.finish(),
            vec![
                StreamingMarkdownAction::AppendFrozen {
                    text: "| Name | Value |\n|---|---|\n| A | 1 |\n".to_string(),
                    trailing_blank: false,
                },
                StreamingMarkdownAction::FinishTail("after".to_string()),
            ]
        );
    }
}
