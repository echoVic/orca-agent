use std::fs::File;
use std::path::{Component, Path, PathBuf};

use orca_core::tool_types::{ToolRequest, ToolResult, ToolStatus, tool_output_spill_notice};
use sha2::{Digest, Sha256};
use tiktoken_rs::cl100k_base_singleton;

const MAX_INLINE_TOKENS: usize = 12_500;

pub(crate) fn completed_text(
    request: &ToolRequest,
    text: String,
    spill_dir: Option<&Path>,
    fallback: impl FnOnce(String) -> (String, bool),
) -> ToolResult {
    let (output, truncated) = match preview(spill_dir, &request.id, &text) {
        Some(output) => (output, true),
        None => fallback(text),
    };
    ToolResult::completed(request, output, truncated)
}

pub(crate) fn shorten_result(result: &mut ToolResult, spill_dir: Option<&Path>) {
    if result.status == ToolStatus::Completed
        && let Some(text) = &result.output
        && let Some(output) = preview(spill_dir, &result.id, text)
    {
        result.output = Some(output);
        result.set_truncated(true);
    }
}

fn preview(spill_dir: Option<&Path>, id: &str, text: &str) -> Option<String> {
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
    let name = if !id.is_empty()
        && id.len() <= 120
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && orca_platform::fs::PathIdentity::windows(&format!("C:\\{id}.txt")).is_ok()
    {
        id.to_owned()
    } else {
        orca_core::hex::lower(&Sha256::digest(id.as_bytes()))
    };
    let path = dir.join(format!("{name}.txt"));
    let display = path.display().to_string();
    let notice_tokens = tokenizer
        .encode_ordinary(&tool_output_spill_notice(text.len(), &display))
        .len();
    let mut retained = MAX_INLINE_TOKENS.checked_sub(notice_tokens + 4)?;
    let output = loop {
        let head_bytes = tokenizer.decode_bytes(&tokens[..retained / 2]).ok()?.len();
        let tail_bytes = tokenizer
            .decode_bytes(&tokens[tokens.len() - retained.div_ceil(2)..])
            .ok()?
            .len();
        let head_end = text.floor_char_boundary(head_bytes);
        let tail_start = text.ceil_char_boundary(text.len() - tail_bytes);
        let notice = tool_output_spill_notice(tail_start - head_end, &display);
        let output = format!(
            "{}\n\n{}\n\n{notice}",
            &text[..head_end],
            &text[tail_start..]
        );
        let count = tokenizer.encode_ordinary(&output).len();
        if count <= MAX_INLINE_TOKENS {
            break output;
        }
        retained = retained.checked_sub(count - MAX_INLINE_TOKENS)?;
    };
    let saved = orca_core::history_redaction::redact_sensitive_text(text);
    orca_platform::fs::write_confined_private(&path, saved.as_bytes()).ok()?;
    Some(output)
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
