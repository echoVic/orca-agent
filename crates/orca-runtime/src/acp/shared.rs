//! Daemon-owned session lookup and connection-independent turn leases.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::Error;
use orca_core::config::RunConfig;
use tokio::sync::Mutex;

use crate::surface::{RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, RuntimeSurfaceThreadHandle};

#[derive(Clone)]
pub(super) struct SharedThread {
    pub thread: RuntimeSurfaceThreadHandle,
    pub busy: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
pub(super) struct SharedSessions {
    threads: Arc<Mutex<HashMap<String, SharedThread>>>,
}

pub(super) struct TurnLease(Arc<AtomicBool>);

impl TurnLease {
    pub fn acquire(busy: &Arc<AtomicBool>) -> Result<Self, Error> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::invalid_request().data("session already has an active prompt"))?;
        Ok(Self(Arc::clone(busy)))
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SharedSessions {
    pub async fn open(
        &self,
        host: RuntimeSurfaceHostHandle,
        mut config: RunConfig,
        selector: Option<String>,
    ) -> Result<(String, SharedThread, RuntimeSurfaceHandle), Error> {
        if let Some(id) = &selector {
            let parsed = uuid::Uuid::parse_str(id).map_err(|_| {
                Error::invalid_params().data("daemon load requires an exact session UUID")
            })?;
            if parsed.to_string() != *id {
                return Err(
                    Error::invalid_params().data("daemon load requires a canonical session UUID")
                );
            }
        }
        // Serialize lookup plus hydration: two simultaneous loads must never
        // try to open a second runtime actor for the same persisted session.
        let mut threads = self.threads.lock().await;
        let entry = if let Some(entry) = selector.as_ref().and_then(|id| threads.get(id)) {
            entry.clone()
        } else {
            if threads.len() >= 64 {
                return Err(Error::invalid_request().data("daemon session limit reached"));
            }
            let load_host = host.clone();
            let thread = tokio::task::spawn_blocking(move || match selector {
                Some(id) => {
                    let transcript = RuntimeSurfaceHostHandle::load_saved_session(&id)
                        .map_err(Error::into_internal_error)?;
                    validate_saved_policy(&config, &transcript.meta)?;
                    if transcript.meta.session_id != id {
                        return Err(Error::invalid_params().data("saved session identity mismatch"));
                    }
                    config.history_mode = orca_core::config::HistoryMode::Resume(id);
                    load_host
                        .start_thread_with_request(
                            crate::runtime_host::RuntimeThreadStartRequest::new(
                                config,
                                "ACP session",
                            )
                            .with_preloaded(transcript),
                        )
                        .map_err(Error::into_internal_error)
                }
                None => load_host
                    .start_thread(config, "ACP session")
                    .map_err(Error::into_internal_error),
            })
            .await
            .map_err(Error::into_internal_error)??;
            let entry = SharedThread {
                thread,
                busy: Arc::new(AtomicBool::new(false)),
            };
            let id = entry
                .thread
                .session_id()
                .ok_or_else(Error::internal_error)?
                .to_string();
            threads.insert(id, entry.clone());
            entry
        };
        let id = entry
            .thread
            .session_id()
            .ok_or_else(Error::internal_error)?
            .to_string();
        let thread_id = entry.thread.thread_id().to_string();
        // Never reuse the first client's connection-bound surface.
        let surface = tokio::task::spawn_blocking(move || {
            host.runtime
                .as_ref()
                .ok_or_else(Error::internal_error)?
                .resolve_live_thread(&thread_id)
                .map_err(Error::into_internal_error)?
                .acp_surface_for_connection(
                    host.connection_id()
                        .cloned()
                        .ok_or_else(Error::internal_error)?,
                )
                .ok_or_else(Error::internal_error)
        })
        .await
        .map_err(Error::into_internal_error)??;
        Ok((id, entry, surface))
    }
}

fn validate_saved_policy(
    config: &RunConfig,
    meta: &crate::history::SessionMeta,
) -> Result<(), Error> {
    let roots = config
        .runtime_workspace_roots
        .clone()
        .unwrap_or_else(|| config.cwd.clone().into_iter().collect());
    if config
        .cwd
        .as_ref()
        .is_none_or(|cwd| Path::new(&meta.cwd) != cwd)
        || meta.approval_mode.is_some_and(|mode| {
            !super::settings::allowed_modes(config.approval_mode).contains(&mode.as_str())
        })
        || meta.active_permission_profile != config.active_permission_profile
        || meta.permission_rules != config.permission_rules
        || meta
            .runtime_workspace_roots
            .iter()
            .any(|root| !roots.contains(root))
        || meta
            .additional_working_directories
            .iter()
            .any(|directory| !config.additional_working_directories.contains(directory))
        || !meta.network_domain_permissions.is_empty()
        || meta
            .metadata_writable_directories
            .iter()
            .any(|path| !roots.iter().any(|root| path.starts_with(root)))
    {
        return Err(Error::invalid_params().data(
            "saved workspace or permission policy differs from this daemon; refusing scope escalation",
        ));
    }
    Ok(())
}

pub(super) fn validate_workspace(base: &RunConfig, cwd: &Path) -> Result<(), String> {
    if !cwd.is_absolute() {
        return Err("daemon workspace must be absolute".into());
    }
    let expected = base
        .cwd
        .as_deref()
        .ok_or("daemon workspace is not configured")?;
    let expected = expected.canonicalize().map_err(|error| error.to_string())?;
    let actual = cwd.canonicalize().map_err(|error| error.to_string())?;
    if actual != expected {
        return Err("ACP workspace does not match the daemon workspace".into());
    }
    Ok(())
}
