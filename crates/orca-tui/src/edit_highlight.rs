use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::diff_highlight::{ParsedDiff, RefinedDiffStyles, parse_unified_diff};
use crate::edit_highlight_worker::{
    DrainResults, EditHighlightJob, EditHighlightOutcome, EditHighlightResult, EditHighlightRuntime,
};
use crate::syntax_highlight::{SyntaxTheme, highlighter_for_path};
use crate::terminal_capabilities::{TerminalColorLevel, syntax_style_revision};
use crate::transcript_state::ChatMessage;
use crate::types::AppState;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AppliedDiffHighlight {
    pub(crate) tool_id: String,
    pub(crate) display_path: String,
    pub(crate) styles: Arc<RefinedDiffStyles>,
}

#[cfg(test)]
type EditHighlightRuntimeFactory = fn() -> std::io::Result<EditHighlightRuntime>;

#[cfg(test)]
type EditHighlightDrain = fn(&mut EditHighlightRuntime) -> DrainResults;

pub(crate) struct EditHighlightState {
    workspace_root: Option<PathBuf>,
    syntax_theme: SyntaxTheme,
    syntax_color_level: TerminalColorLevel,
    runtime: Option<EditHighlightRuntime>,
    applied: HashMap<u64, AppliedDiffHighlight>,
    #[cfg(test)]
    runtime_factory: EditHighlightRuntimeFactory,
    #[cfg(test)]
    drain: Option<EditHighlightDrain>,
}

impl Default for EditHighlightState {
    fn default() -> Self {
        Self {
            workspace_root: None,
            syntax_theme: SyntaxTheme::OneHalfDark,
            syntax_color_level: TerminalColorLevel::TrueColor,
            runtime: None,
            applied: HashMap::new(),
            #[cfg(test)]
            runtime_factory: EditHighlightRuntime::new,
            #[cfg(test)]
            drain: None,
        }
    }
}

impl EditHighlightState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn reconfigure_edit_highlighting(
        &mut self,
        workspace_root: PathBuf,
        syntax_theme: SyntaxTheme,
        syntax_color_level: TerminalColorLevel,
    ) {
        self.workspace_root = Some(workspace_root);
        self.syntax_theme = syntax_theme;
        self.syntax_color_level = syntax_color_level;
        self.runtime = None;
        self.applied.clear();
    }

    pub(crate) fn applied(&self) -> &HashMap<u64, AppliedDiffHighlight> {
        &self.applied
    }

    fn new_runtime(&self) -> std::io::Result<EditHighlightRuntime> {
        #[cfg(test)]
        {
            (self.runtime_factory)()
        }
        #[cfg(not(test))]
        {
            EditHighlightRuntime::new()
        }
    }

    fn drain_results(&mut self) -> Option<DrainResults> {
        let runtime = self.runtime.as_mut()?;
        #[cfg(test)]
        {
            Some(match self.drain {
                Some(drain) => drain(runtime),
                None => runtime.drain_results(),
            })
        }
        #[cfg(not(test))]
        {
            Some(runtime.drain_results())
        }
    }

    fn clear_applied(&mut self) {
        self.applied.clear();
    }

    fn remove_applied_revision(&mut self, revision: u64) {
        self.applied.remove(&revision);
    }

    fn cancel_pending_for_message(&mut self, message_index: usize, message_revision: u64) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.cancel_pending_for_message(message_index, message_revision);
        }
    }
}

pub(crate) fn normalize_diff_relative_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn unified_diff_header_path(header: &str) -> &str {
    let path = header.split_once('\t').map_or(header, |(path, _)| path);
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

pub(crate) fn parsed_diff_structure_matches_target(
    parsed: &ParsedDiff,
    diff: &str,
    target: &Path,
) -> bool {
    if !parsed.is_structurally_valid() || parsed.has_multiple_files {
        return false;
    }
    let mut lines = diff.lines();
    let Some(old_header) = lines.next().and_then(|line| line.strip_prefix("--- ")) else {
        return false;
    };
    let Some(new_header) = lines.next().and_then(|line| line.strip_prefix("+++ ")) else {
        return false;
    };
    let old_path = unified_diff_header_path(old_header);
    let new_path = unified_diff_header_path(new_header);
    let old_is_null = old_path == "/dev/null";
    let new_is_null = new_path == "/dev/null";
    if old_is_null && new_is_null {
        return false;
    }
    if (!old_is_null && normalize_diff_relative_path(old_path).is_none())
        || (!new_is_null && normalize_diff_relative_path(new_path).as_deref() != Some(target))
        || (new_is_null && normalize_diff_relative_path(old_path).as_deref() != Some(target))
    {
        return false;
    }
    parsed
        .destination_path
        .as_deref()
        .and_then(normalize_diff_relative_path)
        .as_deref()
        == Some(target)
}

fn parsed_diff_has_valid_new_side(parsed: &ParsedDiff) -> bool {
    let mut new_side_lines = parsed
        .hunks
        .iter()
        .flat_map(|hunk| hunk.source_lines())
        .filter(|line| {
            matches!(
                line.kind,
                crate::diff_highlight::DiffLineKind::Context
                    | crate::diff_highlight::DiffLineKind::Insert
            )
        });
    new_side_lines
        .next()
        .is_some_and(|line| line.new_line.is_some_and(|line_number| line_number > 0))
        && new_side_lines.all(|line| line.new_line.is_some_and(|line_number| line_number > 0))
}

impl AppState {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn configure_syntax_highlighting(
        &mut self,
        workspace_root: PathBuf,
        syntax_theme: SyntaxTheme,
        syntax_color_level: TerminalColorLevel,
    ) {
        self.edit_highlights.reconfigure_edit_highlighting(
            workspace_root,
            syntax_theme,
            syntax_color_level,
        );
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn edit_highlight_needs_tick(&self) -> bool {
        self.edit_highlights
            .runtime
            .as_ref()
            .is_some_and(EditHighlightRuntime::has_pending)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn poll_edit_highlight_results(&mut self) -> bool {
        let Some(drained) = self.edit_highlights.drain_results() else {
            return false;
        };
        let disconnected = drained.disconnected;
        let mut redraw = false;
        for result in drained.results {
            redraw |= self.apply_edit_highlight_result(result);
        }
        if disconnected {
            self.edit_highlights.runtime = None;
        }
        redraw
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn refined_diff_styles(
        &self,
        message_index: usize,
        tool_id: &str,
    ) -> Option<&RefinedDiffStyles> {
        let message = self.transcript.messages.get(message_index)?;
        let ChatMessage::ToolCall { id, .. } = message else {
            return None;
        };
        (id == tool_id)
            .then(|| {
                Self::refined_diff_styles_for_message(
                    &self.transcript.message_revisions,
                    self.edit_highlights.applied(),
                    message_index,
                    message,
                )
            })
            .flatten()
    }

    pub(crate) fn refined_diff_styles_for_message<'a>(
        revisions: &[u64],
        highlights: &'a HashMap<u64, AppliedDiffHighlight>,
        message_index: usize,
        message: &ChatMessage,
    ) -> Option<&'a RefinedDiffStyles> {
        let revision = *revisions.get(message_index)?;
        let ChatMessage::ToolCall { id, .. } = message else {
            return None;
        };
        let highlight = highlights.get(&revision)?;
        (highlight.tool_id == *id).then_some(highlight.styles.as_ref())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resolve_edit_target(&self, target: &str) -> Option<(PathBuf, String)> {
        let target_path = Path::new(target);
        if target_path.as_os_str().is_empty() || target_path.is_absolute() {
            return None;
        }
        let configured_workspace = self.edit_highlights.workspace_root.as_ref()?;
        let resolved_path =
            orca_tools::resolve_workspace_path(configured_workspace, Some(target)).ok()?;
        let display_path = resolved_path
            .strip_prefix(configured_workspace)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        if display_path.is_empty() {
            return None;
        }
        let workspace_root = configured_workspace.canonicalize().ok()?;
        if !workspace_root.is_dir() {
            return None;
        }
        let absolute_path = resolved_path.canonicalize().ok()?;
        if !absolute_path.starts_with(&workspace_root) || !absolute_path.is_file() {
            return None;
        }
        Some((absolute_path, display_path))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn edit_target_matches_job(&self, target: &str, job: &EditHighlightJob) -> bool {
        self.resolve_edit_target(target)
            .is_some_and(|(absolute_path, display_path)| {
                absolute_path == job.absolute_path && display_path == job.display_path
            })
    }

    pub(crate) fn submit_edit_highlight_for_message(&mut self, message_index: usize) {
        self.reconcile_message_tracking();
        let Some(ChatMessage::ToolCall {
            id,
            target: Some(target),
            status,
            diff: Some(diff),
            ..
        }) = self.transcript.messages.get(message_index)
        else {
            return;
        };
        if status != "completed" || diff.trim().is_empty() {
            return;
        }

        let parsed = parse_unified_diff(diff);
        let Some(destination_path) = parsed.destination_path.as_deref() else {
            return;
        };
        let Some((absolute_path, display_path)) = self.resolve_edit_target(target) else {
            return;
        };
        let Some(normalized_destination) = normalize_diff_relative_path(destination_path) else {
            return;
        };
        if !parsed_diff_structure_matches_target(&parsed, diff, Path::new(&display_path))
            || normalized_destination != Path::new(&display_path)
            || !parsed_diff_has_valid_new_side(&parsed)
            || highlighter_for_path(
                &normalized_destination,
                self.edit_highlights.syntax_theme,
                self.edit_highlights.syntax_color_level,
            )
            .is_none()
        {
            return;
        }

        let Some(message_revision) = self
            .transcript
            .message_revisions
            .get(message_index)
            .copied()
        else {
            return;
        };
        let tool_id = id.clone();

        if self.edit_highlights.runtime.is_none() {
            let Ok(runtime) = self.edit_highlights.new_runtime() else {
                return;
            };
            self.edit_highlights.runtime = Some(runtime);
        }
        let runtime = self
            .edit_highlights
            .runtime
            .as_mut()
            .expect("edit highlight runtime initialized");
        let job = EditHighlightJob {
            job_id: runtime.allocate_job_id(),
            tool_id,
            message_index,
            message_revision,
            syntax_theme_revision: syntax_style_revision(
                self.edit_highlights.syntax_theme,
                self.edit_highlights.syntax_color_level,
            ),
            syntax_theme: self.edit_highlights.syntax_theme,
            syntax_color_level: self.edit_highlights.syntax_color_level,
            absolute_path,
            display_path,
            parsed,
        };
        if !runtime.submit(job) {
            self.edit_highlights.runtime = None;
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn apply_edit_highlight_result(&mut self, result: EditHighlightResult) -> bool {
        let Some(runtime) = self.edit_highlights.runtime.as_mut() else {
            return false;
        };
        if !runtime.pending_matches(&result.job) || !runtime.finish_pending(&result.job) {
            return false;
        }
        let EditHighlightOutcome::Ready { styles } = result.outcome else {
            return false;
        };

        let job = result.job;
        if self.edit_highlights.syntax_theme != job.syntax_theme
            || self.edit_highlights.syntax_color_level != job.syntax_color_level
            || syntax_style_revision(
                self.edit_highlights.syntax_theme,
                self.edit_highlights.syntax_color_level,
            ) != job.syntax_theme_revision
            || self
                .transcript
                .message_revisions
                .get(job.message_index)
                .copied()
                != Some(job.message_revision)
        {
            return false;
        }
        let Some(ChatMessage::ToolCall {
            id,
            target: Some(target),
            status,
            diff: Some(diff),
            ..
        }) = self.transcript.messages.get(job.message_index)
        else {
            return false;
        };
        if id != &job.tool_id || status != "completed" {
            return false;
        }
        let current_parsed = parse_unified_diff(diff);
        let Some(current_destination) = current_parsed.destination_path.as_deref() else {
            return false;
        };
        let Some(normalized_destination) = normalize_diff_relative_path(current_destination) else {
            return false;
        };
        if current_parsed != job.parsed
            || normalized_destination.to_string_lossy().replace('\\', "/") != job.display_path
            || !self.edit_target_matches_job(target, &job)
        {
            return false;
        }

        if !self.touch_message(job.message_index) {
            return false;
        }
        let Some(applied_revision) = self
            .transcript
            .message_revisions
            .get(job.message_index)
            .copied()
        else {
            return false;
        };
        self.edit_highlights.applied.insert(
            applied_revision,
            AppliedDiffHighlight {
                tool_id: job.tool_id,
                display_path: job.display_path,
                styles,
            },
        );
        true
    }

    pub(crate) fn clear_pending_edit_highlights(&mut self) {
        if let Some(runtime) = self.edit_highlights.runtime.as_mut() {
            runtime.clear_pending();
        }
    }

    pub(crate) fn clear_applied_edit_highlights(&mut self) {
        self.edit_highlights.clear_applied();
    }

    pub(crate) fn remove_applied_highlight_revision(&mut self, revision: u64) {
        self.edit_highlights.remove_applied_revision(revision);
    }

    pub(crate) fn cancel_pending_edit_highlight_for_message(
        &mut self,
        index: usize,
        revision: u64,
    ) {
        self.edit_highlights
            .cancel_pending_for_message(index, revision);
    }

    pub(crate) fn remove_applied_highlight_for_message(&mut self, index: usize) {
        if matches!(
            self.transcript.messages.get(index),
            Some(ChatMessage::ToolCall { .. })
        ) && let Some(revision) = self.transcript.message_revisions.get(index)
        {
            self.edit_highlights.applied.remove(revision);
        }
    }

    pub(crate) fn remove_applied_highlights_for_tool_id(&mut self, tool_id: &str) {
        self.edit_highlights
            .applied
            .retain(|_, highlight| highlight.tool_id != tool_id);
    }

    pub(crate) fn prune_applied_diff_highlights(&mut self) {
        let present_revisions = self
            .transcript
            .messages
            .iter()
            .zip(&self.transcript.message_revisions)
            .filter_map(|(message, revision)| match message {
                ChatMessage::ToolCall { id, .. } => Some((*revision, id.as_str())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        self.edit_highlights.applied.retain(|revision, highlight| {
            present_revisions
                .get(revision)
                .is_some_and(|tool_id| *tool_id == highlight.tool_id)
        });
    }

    #[cfg(test)]
    pub(crate) fn pending_edit_highlight_count(&self) -> usize {
        self.edit_highlights
            .runtime
            .as_ref()
            .map_or(0, EditHighlightRuntime::pending_count)
    }

    #[cfg(test)]
    pub(crate) fn successful_edit_highlight_submit_count(&self) -> usize {
        self.edit_highlights
            .runtime
            .as_ref()
            .map_or(0, EditHighlightRuntime::successful_submit_count)
    }

    #[cfg(test)]
    pub(crate) fn pending_edit_highlight_job(&self, tool_id: &str) -> Option<EditHighlightJob> {
        self.edit_highlights.runtime.as_ref()?.pending_job(tool_id)
    }

    #[cfg(test)]
    pub(crate) fn edit_highlight_runtime_started(&self) -> bool {
        self.edit_highlights.runtime.is_some()
    }

    #[cfg(test)]
    pub(crate) fn syntax_workspace_root_for_test(&self) -> Option<&Path> {
        self.edit_highlights.workspace_root.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn syntax_theme_for_test(&self) -> SyntaxTheme {
        self.edit_highlights.syntax_theme
    }

    #[cfg(test)]
    pub(crate) fn syntax_color_level_for_test(&self) -> TerminalColorLevel {
        self.edit_highlights.syntax_color_level
    }

    #[cfg(test)]
    pub(crate) fn edit_highlight_runtime_started_for_test(&self) -> bool {
        self.edit_highlights.runtime.is_some()
    }

    #[cfg(test)]
    pub(crate) fn pending_edit_highlight_count_for_test(&self) -> usize {
        self.pending_edit_highlight_count()
    }

    #[cfg(test)]
    pub(crate) fn set_edit_highlight_runtime_factory_for_test(
        &mut self,
        factory: EditHighlightRuntimeFactory,
    ) {
        self.edit_highlights.runtime_factory = factory;
    }

    #[cfg(test)]
    pub(crate) fn set_edit_highlight_drain_for_test(&mut self, drain: Option<EditHighlightDrain>) {
        self.edit_highlights.drain = drain;
    }

    #[cfg(test)]
    pub(crate) fn set_syntax_theme_for_test(&mut self, syntax_theme: SyntaxTheme) {
        self.edit_highlights.syntax_theme = syntax_theme;
    }

    #[cfg(test)]
    pub(crate) fn set_syntax_color_level_for_test(
        &mut self,
        syntax_color_level: TerminalColorLevel,
    ) {
        self.edit_highlights.syntax_color_level = syntax_color_level;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn reconfigure_retires_runtime_and_clears_applied_styles_atomically() {
        let workspace = tempfile::tempdir().unwrap();
        let mut state = EditHighlightState::default();
        state.runtime = Some(EditHighlightRuntime::new().unwrap());
        state.applied.insert(
            7,
            AppliedDiffHighlight {
                tool_id: "edit-1".to_string(),
                display_path: "src/lib.rs".to_string(),
                styles: Arc::new(HashMap::new()),
            },
        );

        state.reconfigure_edit_highlighting(
            workspace.path().to_path_buf(),
            SyntaxTheme::OneHalfLight,
            TerminalColorLevel::Ansi256,
        );

        assert!(state.runtime.is_none());
        assert!(state.applied.is_empty());
        assert_eq!(state.workspace_root.as_deref(), Some(workspace.path()));
        assert_eq!(state.syntax_theme, SyntaxTheme::OneHalfLight);
        assert_eq!(state.syntax_color_level, TerminalColorLevel::Ansi256);
    }
}
