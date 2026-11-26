use std::fs::File;
use std::path::{Component, Path, PathBuf};

use orca_core::tool_types::{ToolOutputSpill, ToolRequest, ToolResult, ToolStatus};
use tiktoken_rs::cl100k_base_singleton;

const MAX_INLINE_TOKENS: usize = 12_500;

pub(crate) fn completed_text(
    request: &ToolRequest,
    text: String,
    spill_dir: Option<&Path>,
    fallback: impl FnOnce(String) -> (String, bool),
) -> ToolResult {
    if let Some((output, spill)) = preview(spill_dir, &request.id, &text) {
        let mut result = ToolResult::completed(request, output, true);
        result.set_spill(spill);
        return result;
    }
    let (output, truncated) = fallback(text);
    ToolResult::completed(request, output, truncated)
}

pub(crate) fn shorten_result(result: &mut ToolResult, spill_dir: Option<&Path>) {
    if result.status == ToolStatus::Completed
        && result.spill.is_none()
        && let Some(text) = &result.output
        && let Some((output, spill)) = preview(spill_dir, &result.id, text)
    {
        result.output = Some(output);
        result.set_spill(spill);
    }
}

fn preview(spill_dir: Option<&Path>, id: &str, text: &str) -> Option<(String, ToolOutputSpill)> {
    let dir = spill_dir?;
    // No ordinary token spans fewer than one byte.
    if text.len() <= MAX_INLINE_TOKENS {
        return None;
    }
    let tokenizer = cl100k_base_singleton();
    let tokens = tokenizer.encode_ordinary(text);
    if tokens.len() <= MAX_INLINE_TOKENS {
        return None;
    }
    let reference = |bytes| ToolOutputSpill::new(dir.to_owned(), id.to_owned(), bytes);
    let notice_tokens = tokenizer
        .encode_ordinary(&reference(text.len())?.notice())
        .len();
    let mut retained = MAX_INLINE_TOKENS.checked_sub(notice_tokens + 4)?;
    let (output, spill) = loop {
        let head_bytes = tokenizer.decode_bytes(&tokens[..retained / 2]).ok()?.len();
        let tail_bytes = tokenizer
            .decode_bytes(&tokens[tokens.len() - retained.div_ceil(2)..])
            .ok()?
            .len();
        let head_end = text.floor_char_boundary(head_bytes);
        let tail_start = text.ceil_char_boundary(text.len() - tail_bytes);
        let spill = reference(tail_start - head_end)?;
        let preview = format!("{}\n\n{}", &text[..head_end], &text[tail_start..]);
        let output = spill.render(&preview);
        let count = tokenizer.encode_ordinary(&output).len();
        if count <= MAX_INLINE_TOKENS {
            break (output, spill);
        }
        retained = retained.checked_sub(count - MAX_INLINE_TOKENS)?;
    };
    let saved = orca_core::history_redaction::redact_sensitive_text(text);
    orca_platform::fs::write_confined_private(&spill.path(), saved.as_bytes()).ok()?;
    Some((output, spill))
}

pub(crate) enum ReadTarget {
    Workspace(PathBuf),
    Spill(PathBuf, File),
}

pub(crate) fn resolve_read_path(
    cwd: &Path,
    target: Option<&str>,
    spill_dir: Option<&Path>,
) -> Result<ReadTarget, String> {
    if let Some((target, dir)) = target.zip(spill_dir) {
        let path = Path::new(target);
        if let Ok(relative) = path.strip_prefix(dir) {
            let mut components = relative.components();
            if matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none()
            {
                return orca_platform::fs::open_confined_regular(path)
                    .map(|file| ReadTarget::Spill(path.to_owned(), file))
                    .map_err(|error| format!("cannot open spill file {} (path escapes workspace or is unavailable): {error}", path.display()));
            }
            return Err(format!("invalid spill file path: {}", path.display()));
        }
    }
    crate::resolve_workspace_path(cwd, target).map(ReadTarget::Workspace)
}
