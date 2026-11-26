use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) fn directory(id: &str) -> Option<PathBuf> {
    // Session ids are minted as UUIDs and also restored from transcripts.
    let id = uuid::Uuid::parse_str(id).ok()?;
    let home = super::orca_home();
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir().ok()?.join(home)
    };
    Some(
        home.join("session-results")
            .join(id.to_string())
            .join("spill"),
    )
}

pub(crate) fn carry_into_fork(
    parent_id: &str,
    fork_id: &str,
    messages: &mut [orca_core::conversation::Message],
) {
    use orca_core::conversation::Message;
    use orca_core::tool_types::{
        parse_tool_output_spill_notice, tool_output_spill_notice, trailing_tool_output_spill_notice,
    };
    let (Some(parent), Some(fork)) = (directory(parent_id), directory(fork_id)) else {
        return;
    };
    for message in messages {
        let Message::Tool { content, .. } = message else {
            continue;
        };
        let Some(notice) = trailing_tool_output_spill_notice(content) else {
            continue;
        };
        let Some((omitted, file)) = parse_tool_output_spill_notice(notice) else {
            continue;
        };
        let Some(name) = file
            .file_name()
            .filter(|_| file.parent() == Some(parent.as_path()))
        else {
            continue;
        };
        let target = fork.join(name);
        let replacement = match copy_spill_file(file, &target) {
            Ok(()) => tool_output_spill_notice(omitted, &target.display().to_string()),
            Err(error) => {
                tracing::warn!(%error, "could not copy a saved tool result into the fork");
                format!(
                    "[{omitted} bytes omitted. The full result could not be copied into this session.]"
                )
            }
        };
        content.truncate(content.len() - notice.len());
        content.push_str(&replacement);
    }
}

fn copy_spill_file(file: &Path, target: &Path) -> io::Result<()> {
    let mut source = orca_platform::fs::open_confined_regular(file)?;
    let mut contents = Vec::new();
    source.read_to_end(&mut contents)?;
    orca_platform::fs::write_confined_private(target, &contents)
}

pub(super) fn directory_for_transcript(path: &Path) -> Option<PathBuf> {
    // The id in Orca's standard filename survives archiving and compression.
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix(".zst")
        .unwrap_or(name)
        .strip_suffix(".jsonl")?;
    let id = name.get(name.len().checked_sub(36)?..)?;
    directory(id).or_else(|| directory(&super::writer::read_session_meta(path).ok()?.session_id))
}

pub(crate) fn remove(path: Option<&Path>) -> io::Result<()> {
    path.map_or(Ok(()), orca_platform::fs::remove_confined_files)
}

pub(super) fn size(path: Option<&Path>) -> io::Result<u64> {
    path.map_or(Ok(0), orca_platform::fs::confined_files_size)
}
