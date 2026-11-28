use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::shell_session::{
    RuntimeShellSessionManager, ShellSessionCommand, ShellSessionHandle, ShellSessionOutput,
    ShellSessionSnapshot,
};
use crate::tasks::TaskRegistry;

#[derive(Default)]
pub(super) struct ServerShellManager {
    sessions: Option<RuntimeShellSessionManager>,
}

impl ServerShellManager {
    fn manager_for_cwd(&mut self, cwd: &Path) -> &mut RuntimeShellSessionManager {
        self.sessions.get_or_insert_with(|| {
            RuntimeShellSessionManager::new(TaskRegistry::new_for_cwd(
                "server-shell".to_string(),
                cwd,
            ))
        })
    }

    pub(super) fn sessions_mut(&mut self) -> Option<&mut RuntimeShellSessionManager> {
        self.sessions.as_mut()
    }

    pub(super) fn terminate_all(&mut self) {
        if let Some(sessions) = self.sessions.as_mut() {
            sessions.terminate_all();
        }
    }

    pub(super) fn spawn(
        &mut self,
        cwd: &Path,
        command: ShellSessionCommand,
        task_registry: Option<TaskRegistry>,
    ) -> io::Result<ShellSessionHandle> {
        self.spawn_with_metadata_roots(cwd, command, Vec::new(), task_registry)
    }

    pub(super) fn spawn_with_metadata_roots(
        &mut self,
        cwd: &Path,
        command: ShellSessionCommand,
        metadata_writable_directories: Vec<std::path::PathBuf>,
        task_registry: Option<TaskRegistry>,
    ) -> io::Result<ShellSessionHandle> {
        let manager = self.manager_for_cwd(cwd);
        match task_registry {
            Some(task_registry) => manager.spawn_with_task_registry_and_metadata_roots(
                command,
                metadata_writable_directories,
                task_registry,
            ),
            None => manager.spawn_with_metadata_roots(command, metadata_writable_directories),
        }
    }

    pub(super) fn write_stdin(&mut self, id: &str, input: &str) -> Option<io::Result<()>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.write_stdin(id, input))
    }

    pub(super) fn close_stdin(&mut self, id: &str) -> Option<io::Result<()>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.close_stdin(id))
    }

    pub(super) fn update_description(
        &mut self,
        id: &str,
        description: &str,
    ) -> Option<io::Result<()>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.update_description(id, description))
    }

    pub(super) fn resize(&mut self, id: &str, cols: u16, rows: u16) -> Option<io::Result<()>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.resize(id, cols, rows))
    }

    pub(super) fn list(&mut self) -> Vec<ShellSessionSnapshot> {
        self.sessions
            .as_mut()
            .map(RuntimeShellSessionManager::list)
            .unwrap_or_default()
    }

    pub(super) fn reap_completed_except(
        &mut self,
        protected_ids: &HashSet<String>,
    ) -> io::Result<Vec<io::Result<ShellSessionOutput>>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.reap_completed_results_except(protected_ids))
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    pub(super) fn reap_requested_stops(&mut self) -> Vec<io::Result<ShellSessionOutput>> {
        self.sessions
            .as_mut()
            .map(RuntimeShellSessionManager::reap_requested_stops_results)
            .unwrap_or_default()
    }

    pub(super) fn read(
        &mut self,
        id: &str,
        timeout: Duration,
    ) -> Option<io::Result<ShellSessionOutput>> {
        self.sessions
            .as_mut()
            .map(|manager| manager.read(id, timeout))
    }

    pub(super) fn wait(&mut self, id: &str, timeout: Duration) -> io::Result<ShellSessionOutput> {
        let Some(manager) = self.sessions.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown shell: {id}"),
            ));
        };
        manager.wait(id, timeout)
    }

    pub(super) fn kill(&mut self, id: &str) -> Option<io::Result<ShellSessionOutput>> {
        self.sessions.as_mut().map(|manager| manager.kill(id))
    }
}
