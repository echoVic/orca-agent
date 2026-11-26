use std::fs;
use std::io;
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
        let replacement = match copy_spill_file(&parent, file, &fork, &target) {
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

fn copy_spill_file(parent: &Path, file: &Path, fork: &Path, target: &Path) -> io::Result<()> {
    existing(Some(parent))?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "parent spill directory is missing")
    })?;
    let mut source = super::writer::open_regular_history_file(file)?;
    fs::create_dir_all(fork)?;
    existing(Some(fork))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(orca_platform::fs::extended_length_path(fork))?;
    io::copy(&mut source, &mut temporary)?;
    temporary
        .persist_noclobber(orca_platform::fs::extended_length_path(target))
        .map_err(|error| error.error)?;
    Ok(())
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

fn existing(path: Option<&Path>) -> io::Result<Option<&Path>> {
    let Some(path) = path else { return Ok(None) };
    if let Some(parent) = path.parent() {
        match fs::symlink_metadata(parent) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(io::Error::other("spill session path must be a directory"));
            }
            _ => {}
        }
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(path)),
        Ok(_) => Err(io::Error::other("spill path must be a directory")),
    }
}

pub(crate) fn remove(path: Option<&Path>) -> io::Result<()> {
    if let Some(path) = existing(path)? {
        fs::remove_dir_all(path)?;
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    Ok(())
}

pub(super) fn size(path: Option<&Path>) -> io::Result<u64> {
    let Some(path) = existing(path)? else {
        return Ok(0);
    };
    fs::read_dir(path)?.try_fold(0u64, |bytes, entry| {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(io::Error::other("retention requires regular spill files"));
        }
        Ok(bytes.saturating_add(entry.metadata()?.len()))
    })
}
