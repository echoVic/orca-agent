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
    let (Some(parent), Some(fork)) = (directory(parent_id), directory(fork_id)) else {
        return;
    };
    for message in messages {
        let Message::Tool {
            tool_call_id,
            content,
            terminal: Some(terminal),
            ..
        } = message
        else {
            continue;
        };
        let Some(spill) = terminal.spill.take() else {
            continue;
        };
        if !spill.belongs_to(&parent, tool_call_id) {
            *content = spill.unavailable(
                spill.preview(content),
                "The full result could not be copied into this session.",
            );
            continue;
        }
        let inherited = spill.in_directory(fork.clone());
        let preview = spill.preview(content);
        match copy_spill_file(&spill.path(), &inherited.path()) {
            Ok(()) => {
                *content = inherited.render(preview);
                terminal.spill = Some(Box::new(inherited));
            }
            Err(error) => {
                tracing::warn!(%error, "could not copy a saved tool result into the fork");
                *content = spill.unavailable(
                    preview,
                    "The full result could not be copied into this session.",
                );
            }
        }
    }
}

pub(super) fn omit_from_export(record: &mut super::types::SessionRecord) {
    use super::types::{SessionRecord, StoredMessage};
    fn omit(message: &mut StoredMessage) {
        if let StoredMessage::Tool {
            content, terminal, ..
        } = message
            && let Some(spill) = terminal.take_spill()
        {
            *content =
                spill.unavailable(spill.preview(content), "The full result was not exported.");
        }
    }
    match record {
        SessionRecord::Message { message, .. } => omit(message),
        SessionRecord::ManualCompactionSnapshot(snapshot) => {
            snapshot.messages.iter_mut().for_each(omit)
        }
        _ => {}
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
