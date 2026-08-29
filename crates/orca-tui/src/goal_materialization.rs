//! Materializes oversized TUI goal objectives and active paste chips as local files.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::composer_textarea::{expand_pending_pastes, locate_pending_pastes};
use crate::protocol::GoalDraft;

pub(crate) const MAX_GOAL_OBJECTIVE_CHARS: usize = 4_000;

const GOAL_ATTACHMENT_DIR: &str = "attachments";
const GOAL_FILE_PREFIX: &str = "Read the Orca goal objective file at ";
const GOAL_FILE_SUFFIX: &str = " before continuing.";
const GOAL_FILE_NAME: &str = "goal-objective.md";

#[derive(Debug)]
pub(crate) struct MaterializedGoal {
    objective: String,
    output_dir: Option<PathBuf>,
    retained: bool,
}

impl MaterializedGoal {
    pub(crate) fn objective(&self) -> &str {
        &self.objective
    }

    pub(crate) fn retain(mut self) {
        self.retained = true;
    }

    #[cfg(test)]
    fn output_dir(&self) -> Option<&Path> {
        self.output_dir.as_deref()
    }
}

impl Drop for MaterializedGoal {
    fn drop(&mut self) {
        if !self.retained
            && let Some(output_dir) = self.output_dir.as_deref()
        {
            let _ = fs::remove_dir_all(output_dir);
        }
    }
}

/// Materialize the active paste chips in a Goal draft under the current ORCA_HOME.
///
/// The returned value owns a rollback guard. Call [`MaterializedGoal::retain`] only
/// after the runtime Goal update succeeds; otherwise dropping it removes all files
/// created for this attempt.
pub(crate) fn materialize_goal_draft(draft: GoalDraft) -> io::Result<MaterializedGoal> {
    let home = orca_home()?;
    materialize_goal_draft_with_id(&home, Uuid::new_v4(), draft)
}

fn materialize_goal_draft_with_id(
    home: &Path,
    attachment_id: Uuid,
    draft: GoalDraft,
) -> io::Result<MaterializedGoal> {
    materialize_goal_draft_with_id_and_writer(
        home,
        attachment_id,
        draft,
        write_goal_attachment_file,
    )
}

fn write_goal_attachment_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)
    }
}

fn materialize_goal_draft_with_id_and_writer(
    home: &Path,
    attachment_id: Uuid,
    draft: GoalDraft,
    mut write_file: impl FnMut(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<MaterializedGoal> {
    let expanded = expand_pending_pastes(&draft.objective, &draft.pending_pastes);
    if expanded.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Goal objective must not be empty",
        ));
    }

    let mut located = locate_pending_pastes(&draft.objective, &draft.pending_pastes);
    located.sort_unstable_by_key(|(start, _, _)| *start);

    let mut output_dir = None;
    let result = (|| {
        let mut replacements = Vec::with_capacity(located.len());
        for (number, (start, end, index)) in located.into_iter().enumerate() {
            let dir = ensure_goal_output_dir(home, attachment_id, &mut output_dir)?;
            let file_name = format!("pasted-text-{}.txt", number + 1);
            let path = checked_goal_file_path(&dir, &file_name)?;
            write_file(&path, draft.pending_pastes[index].1.as_bytes())?;
            replacements.push((
                start,
                end,
                format!(
                    "pasted text file: {}. Read this file before continuing.",
                    path.display()
                ),
            ));
        }

        let mut objective = draft.objective;
        for (start, end, reference) in replacements.into_iter().rev() {
            objective.replace_range(start..end, &reference);
        }
        objective = objective.trim().to_string();

        if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
            let dir = ensure_goal_output_dir(home, attachment_id, &mut output_dir)?;
            let path = checked_goal_file_path(&dir, GOAL_FILE_NAME)?;
            let reference = objective_file_reference(home, &path)?;
            write_file(&path, objective.as_bytes())?;
            objective = reference;
        }

        Ok(objective)
    })();

    match result {
        Ok(objective) => Ok(MaterializedGoal {
            objective,
            output_dir,
            retained: false,
        }),
        Err(error) => {
            if let Some(output_dir) = output_dir.as_deref() {
                let _ = fs::remove_dir_all(output_dir);
            }
            Err(error)
        }
    }
}

fn managed_goal_objective_path_in(home: &Path, objective: &str) -> Option<PathBuf> {
    let raw_path = objective
        .strip_prefix(GOAL_FILE_PREFIX)?
        .strip_suffix(GOAL_FILE_SUFFIX)?;
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return None;
    }
    if path.file_name()? != OsStr::new(GOAL_FILE_NAME) {
        return None;
    }
    let parent = path.parent()?;
    let attachment_id = parent.file_name()?.to_str()?;
    Uuid::parse_str(attachment_id).ok()?;
    let canonical_home = fs::canonicalize(absolute_path(home).ok()?).ok()?;
    let canonical_attachments = fs::canonicalize(canonical_home.join(GOAL_ATTACHMENT_DIR)).ok()?;
    if !canonical_attachments.starts_with(&canonical_home) {
        return None;
    }
    let canonical_parent = fs::canonicalize(parent).ok()?;
    let expected_parent = fs::canonicalize(canonical_attachments.join(attachment_id)).ok()?;
    if expected_parent.parent() != Some(canonical_attachments.as_path()) {
        return None;
    }
    (canonical_parent == expected_parent).then_some(path)
}

fn objective_file_reference(home: &Path, path: &Path) -> io::Result<String> {
    let reference = format!("{GOAL_FILE_PREFIX}{}{GOAL_FILE_SUFFIX}", path.display());
    let actual_chars = reference.chars().count();
    if actual_chars > MAX_GOAL_OBJECTIVE_CHARS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Goal objective file reference is too long: {actual_chars} characters; limit: {MAX_GOAL_OBJECTIVE_CHARS}"
            ),
        ));
    }
    if managed_goal_objective_path_in(home, &reference) != Some(path.to_path_buf()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Goal objective file reference escapes ORCA_HOME attachments",
        ));
    }
    Ok(reference)
}

fn ensure_goal_output_dir(
    home: &Path,
    attachment_id: Uuid,
    output_dir: &mut Option<PathBuf>,
) -> io::Result<PathBuf> {
    if let Some(output_dir) = output_dir {
        return Ok(output_dir.clone());
    }

    let home = absolute_path(home)?;
    fs::create_dir_all(&home)?;
    let canonical_home = fs::canonicalize(&home)?;
    let attachments = home.join(GOAL_ATTACHMENT_DIR);
    create_goal_attachments_dir(&attachments)?;
    let canonical_attachments = fs::canonicalize(&attachments)?;
    if !canonical_attachments.starts_with(&canonical_home) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Goal attachments directory escapes ORCA_HOME",
        ));
    }
    set_private_goal_dir_permissions(&canonical_attachments)?;

    let path = attachments.join(attachment_id.to_string());
    create_goal_output_dir(&path)?;
    let canonical_path = match fs::canonicalize(&path) {
        Ok(path) if path.parent() == Some(canonical_attachments.as_path()) => path,
        Ok(_) => {
            let _ = fs::remove_dir_all(&path);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Goal attachment directory escapes the attachments root",
            ));
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&path);
            return Err(error);
        }
    };
    set_private_goal_dir_permissions(&canonical_path)?;
    *output_dir = Some(canonical_path.clone());
    Ok(canonical_path)
}

fn create_goal_attachments_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        match create_unix_goal_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn create_goal_output_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        create_unix_goal_dir(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

#[cfg(unix)]
fn create_unix_goal_dir(path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Goal attachment directory path contains a null byte",
        )
    })?;
    // SAFETY: `path` is a live, null-terminated C string for the duration of the call.
    if unsafe { libc::mkdir(path.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_private_goal_dir_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn checked_goal_file_path(output_dir: &Path, file_name: &str) -> io::Result<PathBuf> {
    let file_name = Path::new(file_name);
    if file_name.components().count() != 1
        || file_name.file_name() != Some(file_name.as_os_str())
        || file_name.as_os_str() == OsStr::new("")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Goal attachment file name",
        ));
    }
    let canonical_dir = fs::canonicalize(output_dir)?;
    let path = canonical_dir.join(file_name);
    if path.parent() != Some(canonical_dir.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Goal attachment file escapes its output directory",
        ));
    }
    Ok(path)
}

fn orca_home() -> io::Result<PathBuf> {
    let home = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine ORCA_HOME"))?;
    absolute_path(&home)
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTACHMENT_ID: &str = "00000000-0000-4000-8000-000000000000";

    fn fixed_id() -> Uuid {
        Uuid::parse_str(ATTACHMENT_ID).unwrap()
    }

    #[test]
    fn active_pastes_are_written_in_objective_order_and_replaced() {
        let home = tempfile::tempdir().unwrap();
        let first = "[Pasted Content 1001 chars]".to_string();
        let second = "[Pasted Content 1002 chars]".to_string();
        let materialized = materialize_goal_draft_with_id(
            home.path(),
            fixed_id(),
            GoalDraft {
                objective: format!("before {second} middle {first} after"),
                pending_pastes: vec![
                    (first, "first body".to_string()),
                    (second, "second body".to_string()),
                ],
            },
        )
        .unwrap();
        let output_dir = materialized.output_dir().unwrap();
        assert_eq!(
            fs::read_to_string(output_dir.join("pasted-text-1.txt")).unwrap(),
            "second body"
        );
        assert_eq!(
            fs::read_to_string(output_dir.join("pasted-text-2.txt")).unwrap(),
            "first body"
        );
        assert!(materialized.objective().contains("pasted-text-1.txt"));
        assert!(materialized.objective().contains("pasted-text-2.txt"));
        materialized.retain();
    }

    #[test]
    fn oversized_objective_is_written_to_goal_file() {
        let home = tempfile::tempdir().unwrap();
        let objective = "x".repeat(MAX_GOAL_OBJECTIVE_CHARS + 1);
        let materialized = materialize_goal_draft_with_id(
            home.path(),
            fixed_id(),
            GoalDraft {
                objective: objective.clone(),
                pending_pastes: Vec::new(),
            },
        )
        .unwrap();
        let output_dir = materialized.output_dir().unwrap();
        assert_eq!(
            fs::read_to_string(output_dir.join(GOAL_FILE_NAME)).unwrap(),
            objective
        );
        assert_eq!(
            managed_goal_objective_path_in(home.path(), materialized.objective()),
            Some(output_dir.join(GOAL_FILE_NAME))
        );
        materialized.retain();
    }

    #[cfg(unix)]
    #[test]
    fn unix_goal_attachments_use_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let materialized = materialize_goal_draft_with_id(
            home.path(),
            fixed_id(),
            GoalDraft {
                objective: "[Pasted Content 1001 chars]".to_string(),
                pending_pastes: vec![(
                    "[Pasted Content 1001 chars]".to_string(),
                    "secret body".to_string(),
                )],
            },
        )
        .unwrap();
        let attachments = home.path().join(GOAL_ATTACHMENT_DIR);
        let output_dir = materialized.output_dir().unwrap();
        let attachment = output_dir.join("pasted-text-1.txt");

        assert_eq!(
            fs::metadata(attachments).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(output_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(attachment).unwrap().permissions().mode() & 0o777,
            0o600
        );
        materialized.retain();
    }

    #[test]
    fn removed_paste_is_not_written() {
        let home = tempfile::tempdir().unwrap();
        let materialized = materialize_goal_draft_with_id(
            home.path(),
            fixed_id(),
            GoalDraft {
                objective: "keep this goal".to_string(),
                pending_pastes: vec![(
                    "[Pasted Content 1001 chars]".to_string(),
                    "removed body".to_string(),
                )],
            },
        )
        .unwrap();
        assert_eq!(materialized.objective(), "keep this goal");
        assert!(materialized.output_dir().is_none());
    }

    #[test]
    fn write_failure_cleans_attachment_directory() {
        let home = tempfile::tempdir().unwrap();
        let output_dir = home.path().join(GOAL_ATTACHMENT_DIR).join(ATTACHMENT_ID);

        let error = materialize_goal_draft_with_id_and_writer(
            home.path(),
            fixed_id(),
            GoalDraft {
                objective: "[Pasted Content 1001 chars]".to_string(),
                pending_pastes: vec![(
                    "[Pasted Content 1001 chars]".to_string(),
                    "body".to_string(),
                )],
            },
            |_path, _bytes| Err(io::Error::other("injected write failure")),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!output_dir.exists());
    }

    #[test]
    fn dropping_unretained_materialization_cleans_goal_update_failure() {
        let home = tempfile::tempdir().unwrap();
        let output_dir = home.path().join(GOAL_ATTACHMENT_DIR).join(ATTACHMENT_ID);
        {
            let materialized = materialize_goal_draft_with_id(
                home.path(),
                fixed_id(),
                GoalDraft {
                    objective: "[Pasted Content 1001 chars]".to_string(),
                    pending_pastes: vec![(
                        "[Pasted Content 1001 chars]".to_string(),
                        "body".to_string(),
                    )],
                },
            )
            .unwrap();
            assert!(materialized.output_dir().unwrap().exists());
        }
        assert!(!output_dir.exists());
    }

    #[test]
    fn managed_goal_path_is_confined_to_current_orca_home() {
        let home = tempfile::tempdir().unwrap();
        let expected = home
            .path()
            .join(GOAL_ATTACHMENT_DIR)
            .join(ATTACHMENT_ID)
            .join(GOAL_FILE_NAME);
        fs::create_dir_all(expected.parent().unwrap()).unwrap();
        let valid = objective_file_reference(home.path(), &expected).unwrap();
        assert_eq!(
            managed_goal_objective_path_in(home.path(), &valid),
            Some(expected)
        );

        let equivalent = home
            .path()
            .join(".")
            .join(GOAL_ATTACHMENT_DIR)
            .join(ATTACHMENT_ID)
            .join(GOAL_FILE_NAME);
        let equivalent_reference = objective_file_reference(home.path(), &equivalent).unwrap();
        assert_eq!(
            managed_goal_objective_path_in(home.path(), &equivalent_reference),
            Some(equivalent)
        );

        for invalid in [
            home.path()
                .join(GOAL_ATTACHMENT_DIR)
                .join("not-a-uuid")
                .join(GOAL_FILE_NAME),
            home.path()
                .join(GOAL_ATTACHMENT_DIR)
                .join(ATTACHMENT_ID)
                .join("other.md"),
            home.path()
                .join("other")
                .join(ATTACHMENT_ID)
                .join(GOAL_FILE_NAME),
            std::env::temp_dir()
                .join(GOAL_ATTACHMENT_DIR)
                .join(ATTACHMENT_ID)
                .join(GOAL_FILE_NAME),
        ] {
            let reference = format!("{GOAL_FILE_PREFIX}{}{GOAL_FILE_SUFFIX}", invalid.display());
            assert_eq!(
                managed_goal_objective_path_in(home.path(), &reference),
                None
            );
        }
    }
}
