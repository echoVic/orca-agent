//! Restricted local ACP endpoint. No TCP listener or unauthenticated remote fallback.

use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::RunConfig;

pub fn default_socket_path() -> io::Result<PathBuf> {
    orca_core::config::file::config_dir()
        .map(|home| home.join("acp").join("daemon.sock"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ORCA_HOME is unavailable"))
}

#[cfg(not(unix))]
pub fn run(_config: RunConfig, _socket: PathBuf) -> io::Result<()> {
    Err(unsupported())
}

#[cfg(not(unix))]
pub fn bridge(_socket: &Path) -> io::Result<()> {
    Err(unsupported())
}

#[cfg(not(unix))]
pub(crate) fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "local ACP daemon requires Unix; standalone --mode=acp remains available",
    )
}

#[cfg(unix)]
pub use unix::{bridge, connect, run};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{
        DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
    };
    use std::time::Duration;

    use orca_platform::fs::ExclusiveFileLock;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::task::JoinSet;

    struct Endpoint {
        _lock: ExclusiveFileLock,
        path: PathBuf,
        identity: (u64, u64),
        listener: UnixListener,
    }

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn private_parent(path: &Path, create: bool) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ACP socket path must be absolute",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("socket parent missing"))?;
        if create {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let meta = fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ACP socket directory must be owned by this user, non-symlink, and mode 0700",
            ));
        }
        Ok(())
    }

    fn validate_socket(path: &Path) -> io::Result<fs::Metadata> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe ACP socket type, owner, or permissions",
            ));
        }
        Ok(meta)
    }

    impl Endpoint {
        fn bind(path: PathBuf) -> io::Result<Self> {
            private_parent(&path, true)?;
            let lock_path = path.with_extension("lock");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&lock_path)?;
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.uid() != uid()
                || meta.mode() & 0o077 != 0
                || meta.nlink() != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe ACP lock file",
                ));
            }
            let mut lock = ExclusiveFileLock::try_acquire_file(&lock_path, file).map_err(
                |error| match error {
                    orca_platform::PlatformError::LockContended { .. } => io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "ACP daemon singleton lock is held",
                    ),
                    error => io::Error::other(error),
                },
            )?;
            let named = fs::symlink_metadata(&lock_path)?;
            if (named.dev(), named.ino()) != (meta.dev(), meta.ino()) {
                return Err(io::Error::other("ACP lock changed while acquiring it"));
            }
            match validate_socket(&path) {
                Ok(stale) => match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "ACP socket is live",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        let current = validate_socket(&path)?;
                        if (current.dev(), current.ino()) != (stale.dev(), stale.ino()) {
                            return Err(io::Error::other("ACP socket changed during stale check"));
                        }
                        fs::remove_file(&path)?;
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let meta = validate_socket(&path)?;
            lock.file_mut().set_len(0)?;
            writeln!(lock.file_mut(), "{}", std::process::id())?;
            lock.file_mut().sync_all()?;
            Ok(Self {
                _lock: lock,
                path,
                identity: (meta.dev(), meta.ino()),
                listener,
            })
        }
    }

    impl Drop for Endpoint {
        fn drop(&mut self) {
            if let Ok(meta) = fs::symlink_metadata(&self.path)
                && (meta.dev(), meta.ino()) == self.identity
            {
                let _ = fs::remove_file(&self.path);
            }
            // Retain the lock inode. Unlinking it allows contenders to lock
            // different inodes and breaks singleton ownership.
        }
    }

    pub async fn connect(path: &Path) -> io::Result<UnixStream> {
        private_parent(path, false)?;
        validate_socket(path)?;
        let stream =
            tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(path)).await??;
        if stream.peer_cred()?.uid() != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ACP daemon uid mismatch",
            ));
        }
        Ok(stream)
    }

    pub fn bridge(socket: &Path) -> io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            let stream = connect(socket).await?;
            let (mut read, mut write) = stream.into_split();
            let mut stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();
            // EOF detaches this bridge only. Never shut down the daemon.
            tokio::select! {
                result = tokio::io::copy(&mut stdin, &mut write) => result.map(|_| ()),
                result = tokio::io::copy(&mut read, &mut stdout) => result.map(|_| ()),
            }
        });
        // Tokio stdin uses a blocking reader. A daemon EOF must not wait for
        // the editor to type another byte before the bridge process can exit.
        runtime.shutdown_timeout(Duration::from_millis(100));
        result
    }

    pub fn run(mut config: RunConfig, socket: PathBuf) -> io::Result<()> {
        let cwd = config
            .cwd
            .as_ref()
            .ok_or_else(|| io::Error::other("daemon cwd missing"))?
            .canonicalize()?;
        config.cwd = Some(cwd);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async move {
            let endpoint = Endpoint::bind(socket)?;
            let host = crate::runtime_host::RuntimeHost::start().map_err(io::Error::other)?;
            let shared = super::super::shared::SharedSessions::default();
            let mut clients = JoinSet::new();
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            eprintln!("orca: ACP daemon listening on {}", endpoint.path.display());
            loop {
                tokio::select! {
                    _ = term.recv() => break,
                    _ = interrupt.recv() => break,
                    Some(_) = clients.join_next(), if !clients.is_empty() => {}
                    accepted = endpoint.listener.accept() => {
                        let (stream, _) = accepted?;
                        // A peer may close before credentials are queried
                        // (ENOTCONN on macOS). Reject it, not the listener.
                        if !stream.peer_cred().is_ok_and(|peer| peer.uid() == uid())
                            || clients.len() >= 64
                        {
                            continue;
                        }
                        let (reader, writer) = stream.into_split();
                        let surface = host.surface_handle();
                        let config = config.clone();
                        let shared = shared.clone();
                        clients.spawn_local(async move {
                            super::super::supervisor::run_shared_connection(
                                surface, config, reader, writer, shared,
                            ).await
                        });
                    }
                }
            }
            // Runtime commits shutdown terminals before transports disappear.
            let result = tokio::task::spawn_blocking(move || host.shutdown())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other);
            clients.abort_all();
            while clients.join_next().await.is_some() {}
            drop(endpoint);
            result
        })
    }
}
