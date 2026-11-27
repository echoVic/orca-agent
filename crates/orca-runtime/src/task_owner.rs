use std::fs;
use std::io;
use std::path::Path;

use orca_platform::PlatformError;
use orca_platform::fs::{ExclusiveFileLock, open_nofollow_nonblocking};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct OwnerStamp {
    pub(crate) session: String,
    pub(crate) id: String,
}

pub(crate) struct RegistryOwner {
    pub(crate) stamp: OwnerStamp,
    _lock: ExclusiveFileLock,
}

impl std::fmt::Debug for RegistryOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.stamp.fmt(formatter)
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.split_once('-').is_some_and(|(pid, uuid)| {
        pid.parse::<u32>().is_ok()
            && uuid.len() == 32
            && uuid.bytes().all(|ch| ch.is_ascii_hexdigit())
    })
}

impl RegistryOwner {
    pub(crate) fn acquire(root: &Path, session: &str) -> io::Result<Self> {
        let stamp = OwnerStamp {
            session: crate::tasks::safe_path_component(session),
            id: format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()),
        };
        let directory = root.join(&stamp.session).join("owners");
        crate::task_output::prepare_directory(&directory)?;
        let path = directory.join(format!("{}.lock", stamp.id));
        let file = fs::File::create_new(&path)?;
        let lock = ExclusiveFileLock::try_acquire_file(&path, file).map_err(io::Error::other)?;
        Ok(Self { stamp, _lock: lock })
    }

    pub(crate) fn alive(caller: Option<&Self>, root: &Path, stamp: &OwnerStamp) -> bool {
        if caller.is_some_and(|owner| owner.stamp == *stamp)
            || !valid_id(&stamp.id)
            || crate::tasks::safe_path_component(&stamp.session) != stamp.session
        {
            return true;
        }
        // A stamp is published only after its one lock is held. Only its own session's
        // sweep unlinks it, after death, and IDs are never reused. Thus even a foreign
        // probe cannot see create-before-lock; it never takes a second session lock.
        let directory = root.join(&stamp.session).join("owners");
        if crate::task_output::check_components(&directory).is_err() {
            return true;
        }
        let path = directory.join(format!("{}.lock", stamp.id));
        match open_nofollow_nonblocking(&path) {
            Ok(file) if file.metadata().is_ok_and(|meta| meta.is_file()) => {
                ExclusiveFileLock::try_acquire_file(&path, file).is_err()
            }
            Err(PlatformError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            }) => false,
            _ => true,
        }
    }

    pub(crate) fn sweep_locks(&self, root: &Path) {
        let directory = root.join(&self.stamp.session).join("owners");
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
                continue;
            };
            let stamp = OwnerStamp {
                session: self.stamp.session.clone(),
                id: id.into(),
            };
            if !Self::alive(Some(self), root, &stamp) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_distinguishes_self_live_released_and_missing_without_creating() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let caller = RegistryOwner::acquire(root, "caller").unwrap();
        let foreign = RegistryOwner::acquire(root, "foreign").unwrap();
        let alive = |stamp: &OwnerStamp| RegistryOwner::alive(Some(&caller), root, stamp);
        assert!(alive(&caller.stamp));
        let stamp = foreign.stamp.clone();
        assert!(alive(&stamp));
        drop(foreign);
        assert!(!alive(&stamp));
        let missing = OwnerStamp {
            session: "missing".into(),
            ..stamp
        };
        assert!(!alive(&missing));
        assert!(!root.join("missing").exists());
    }
}
