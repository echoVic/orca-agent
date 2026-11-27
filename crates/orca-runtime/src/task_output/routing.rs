use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::persistence::{
    ArchiveLimits, ArchivedShell, OutputArchive, SharedArchive, check_components, missing,
};
use crate::tasks::TaskRegistry;

type CursorCache = Mutex<HashMap<PathBuf, HashMap<String, usize>>>;
static LEGACY_CURSORS: OnceLock<CursorCache> = OnceLock::new();

#[derive(Clone, Debug)]
pub(super) struct ArchiveRoute {
    tasks: TaskRegistry,
    own: PathBuf,
    session: PathBuf,
}

struct DeadArchive {
    path: PathBuf,
    newest: Option<u64>,
    shells: usize,
    bytes: usize,
}

impl ArchiveRoute {
    pub(super) fn new(tasks: TaskRegistry, own: PathBuf) -> Self {
        let session = own.ancestors().nth(3).expect("persistent task session");
        let session = fs::canonicalize(session).unwrap_or_else(|_| session.to_owned());
        Self {
            tasks,
            own,
            session,
        }
    }

    pub(super) fn writer(&self) -> Option<SharedArchive> {
        self.tasks
            .own_archive()
            .lock()
            .expect("registry archive poisoned")
            .clone()
    }

    fn siblings(&self) -> &Path {
        self.own
            .parent()
            .expect("registry owner's archive directory")
    }

    fn entries(&self) -> io::Result<Vec<(String, PathBuf)>> {
        let directory = self.siblings();
        check_components(directory)?;
        let entries = match fs::read_dir(directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            result => result?,
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if crate::task_owner::valid_id(&id)
                && entry.file_type()?.is_dir()
                && entry.path() != self.own
            {
                paths.push((id, entry.path()));
            }
        }
        Ok(paths)
    }

    pub(super) fn open_writer(&self) -> io::Result<SharedArchive> {
        let _lock = self.tasks.output_session_lock()?;
        let mut writer = self
            .tasks
            .own_archive()
            .lock()
            .expect("registry archive poisoned");
        if let Some(writer) = writer.as_ref() {
            return Ok(writer.clone());
        }
        self.sweep(ArchiveLimits::default())?;
        let archive = OutputArchive::open(&self.own, self.tasks.session_id())?;
        *writer = Some(archive.clone());
        Ok(archive)
    }

    fn dead_archives(
        &self,
        entries: &[(String, PathBuf)],
        remove_empty: bool,
    ) -> io::Result<Vec<DeadArchive>> {
        let mut dead = Vec::new();
        if entries.is_empty() {
            return Ok(dead);
        }
        let records = self.tasks.output_records_unlocked()?;
        for (id, path) in entries {
            if self.tasks.output_owner_alive(id) {
                continue;
            }
            if remove_empty && OutputArchive::remove_empty_directory(path).unwrap_or(false) {
                continue;
            }
            let Ok(archive) = OutputArchive::existing(path, self.tasks.session_id(), false) else {
                continue;
            };
            let Ok((ids, bytes)) = archive.footprint() else {
                continue;
            };
            let newest = ids
                .iter()
                .filter_map(|id| records.get(id).map(|(sequence, _)| *sequence))
                .max();
            dead.push(DeadArchive {
                path: path.clone(),
                newest,
                shells: ids.len(),
                bytes,
            });
        }
        dead.sort_by_key(|archive| std::cmp::Reverse(archive.newest));
        Ok(dead)
    }

    pub(super) fn sweep(&self, limits: ArchiveLimits) -> io::Result<()> {
        let mut shells = 0usize;
        let mut bytes = 0usize;
        for archive in self.dead_archives(&self.entries()?, true)? {
            if archive.newest.is_some() {
                shells = shells.saturating_add(archive.shells);
                bytes = bytes.saturating_add(archive.bytes);
                if shells <= limits.tasks && bytes <= limits.session_bytes {
                    continue;
                }
            }
            if check_components(&archive.path).is_ok() {
                for file in ["archive.sqlite3", "archive.sqlite3-journal", "owner.lock"] {
                    if let Err(error) = fs::remove_file(archive.path.join(file))
                        && error.kind() != io::ErrorKind::NotFound
                    {
                        break;
                    }
                }
                let _ = fs::remove_dir(archive.path);
            }
        }
        Ok(())
    }

    pub(super) fn find<T>(
        &self,
        mut read: impl FnMut(&OutputArchive, bool) -> io::Result<T>,
    ) -> io::Result<T> {
        let own = self.writer();
        if let Some(own) = own {
            let result = read(&own.lock().expect("output archive poisoned"), false);
            if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::NotFound) {
                return result;
            }
        }
        let _lock = self.tasks.output_session_lock()?;
        let entries = self.entries()?;
        for (_, path) in &entries {
            if let Ok(Some(archive)) = OutputArchive::cached(path) {
                let result = read(&archive.lock().expect("output archive poisoned"), false);
                if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::NotFound) {
                    return result;
                }
            }
        }
        for dead in self.dead_archives(&entries, false)? {
            if let Ok(archive) = OutputArchive::existing(&dead.path, self.tasks.session_id(), false)
            {
                let result = read(&archive, false);
                if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::NotFound) {
                    return result;
                }
            }
        }
        let legacy = self.siblings().parent().expect("session output root");
        let archive = OutputArchive::existing(legacy, self.tasks.session_id(), true)
            .map_err(|_| missing())?;
        read(&archive, true).map_err(|_| missing())
    }

    pub(super) fn shell(&self, id: &str) -> io::Result<ArchivedShell> {
        self.find(|archive, legacy| {
            let mut shell = archive.shell(id)?;
            if legacy {
                if let Some(cursor) = LEGACY_CURSORS
                    .get_or_init(Mutex::default)
                    .lock()
                    .expect("legacy cursors poisoned")
                    .get(&self.session)
                    .and_then(|cursors| cursors.get(id))
                {
                    shell.cursor = *cursor;
                }
                if shell.state == "running"
                    && self
                        .tasks
                        .output_records_unlocked()?
                        .get(&shell.task_id)
                        .is_some_and(|(_, terminal)| *terminal)
                {
                    shell.state = "interrupted".into();
                    shell.exit_code = None;
                }
            }
            Ok(shell)
        })
    }

    pub(super) fn set_cursor(&self, id: &str, cursor: usize) -> io::Result<()> {
        self.find(|archive, legacy| {
            let shell = archive.shell(id)?;
            if legacy {
                archive.read(&shell.task_id, cursor, 0)?;
                LEGACY_CURSORS
                    .get_or_init(Mutex::default)
                    .lock()
                    .expect("legacy cursors poisoned")
                    .entry(self.session.clone())
                    .or_default()
                    .insert(id.into(), cursor);
                Ok(())
            } else {
                archive.set_cursor(id, cursor)
            }
        })
    }
}
