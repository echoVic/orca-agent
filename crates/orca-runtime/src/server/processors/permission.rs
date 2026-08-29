use std::io::{self, Write};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_permission_operation(op: &ClientOp) -> bool {
    matches!(op, ClientOp::PermissionRespond { .. })
}

pub(in crate::server::router) fn dispatch_permission_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::PermissionRespond {
            request_id,
            decision,
            scope,
            permissions,
            strict_auto_review,
        } => run_permission_respond(
            config,
            state,
            request_id,
            *decision,
            *scope,
            permissions.clone(),
            *strict_auto_review,
            id,
            writer,
        ),
        _ => unreachable!("only permission operations can reach the permission processor"),
    }
}

fn run_permission_respond<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    request_id: &str,
    decision: protocol::PermissionResponseDecision,
    scope: protocol::PermissionGrantScope,
    permissions: protocol::RequestPermissionProfile,
    strict_auto_review: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let response_digest = jsonl_response_digest(&json!({
        "decision": decision,
        "scope": scope,
        "permissions": &permissions,
        "strictAutoReview": strict_auto_review,
    }))?;
    let pending = state.permission_routes.published_route(request_id)?;
    let Some(pending) = pending else {
        return match state
            .permission_routes
            .committed_replay(request_id, response_digest)?
        {
            JsonlCommittedReplay::SameResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::PermissionResolved {
                    request_id: json!(request_id),
                    decision: json!(decision),
                    scope: json!(scope),
                    strict_auto_review: json!(strict_auto_review),
                },
            ),
            JsonlCommittedReplay::ConflictingResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "permission request already resolved with a different response: {request_id}"
                )),
            ),
            JsonlCommittedReplay::NotCommitted => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("unknown permission request: {request_id}")),
            ),
        };
    };
    if decision == protocol::PermissionResponseDecision::Allow
        && scope == protocol::PermissionGrantScope::Session
        && matches!(pending, JsonlPermissionRoute::CommandExec { .. })
    {
        let session_grants = materialize_session_permission_grant(
            &state.threads,
            pending.thread_id(),
            pending.runtime_workspace_roots(),
            &permissions,
        )?;
        state.threads.update_thread_metadata(
            pending.thread_id(),
            ThreadMetadataPatch {
                title: None,
                active_permission_profile: None,
                approval_mode: None,
                runtime_workspace_roots: None,
                permission_rules: None,
                additional_working_directories: Some(session_grants.additional_working_directories),
                metadata_writable_directories: Some(session_grants.metadata_writable_directories),
                network_domain_permissions: Some(session_grants.network_domain_permissions),
            },
        );
    }
    if let JsonlPermissionRoute::Surface {
        client,
        interaction_id,
        target,
        thread_id,
        runtime_workspace_roots,
    } = pending
    {
        let permissions = materialize_surface_permission_profile(
            state,
            &thread_id,
            &runtime_workspace_roots,
            permissions,
        )?;
        let allow = decision == protocol::PermissionResponseDecision::Allow;
        if allow
            && scope == protocol::PermissionGrantScope::Session
            && let Err(error) = state.threads.persist_session_permission_grant(
                &thread_id,
                &client,
                &runtime_workspace_roots,
                &permissions,
            )
        {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "session permission settings did not commit: {error}"
                )),
            );
        }
        let answer = match &target {
            crate::surface::SurfaceInteractionKind::ToolApproval => {
                crate::surface::SurfaceClientInteractionAnswer::ToolApproval {
                    decision: if allow {
                        crate::surface::SurfaceAllowDeny::Allow
                    } else {
                        crate::surface::SurfaceAllowDeny::Deny
                    },
                }
            }
            crate::surface::SurfaceInteractionKind::PermissionRequest => {
                let scope = match scope {
                    protocol::PermissionGrantScope::Turn => {
                        crate::surface::PermissionGrantScope::Turn
                    }
                    protocol::PermissionGrantScope::Session => {
                        crate::surface::PermissionGrantScope::Session
                    }
                };
                let permissions = surface_permission_profile(&permissions);
                let decision = if allow {
                    crate::surface::SurfacePermissionClientDecision::Allow {
                        scope,
                        permissions,
                        strict_auto_review,
                    }
                } else {
                    crate::surface::SurfacePermissionClientDecision::Deny {
                        scope,
                        permissions,
                        strict_auto_review,
                    }
                };
                crate::surface::SurfaceClientInteractionAnswer::PermissionRequest { decision }
            }
            _ => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request has incompatible interaction kind: {request_id}"
                    )),
                );
            }
        };
        let response_request_id = crate::surface::SurfaceRequestId::new();
        match client.respond_interaction_by_id(response_request_id, interaction_id.clone(), answer)
        {
            Ok(crate::surface::MutationReply::Committed { .. }) => {}
            Ok(crate::surface::MutationReply::Deferred { mutation, .. }) => {
                state.permission_routes.mark_committed_pending(
                    request_id,
                    &mutation,
                    response_digest,
                )?;
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission response is awaiting durable reconciliation: {request_id}"
                    )),
                );
            }
            Ok(crate::surface::MutationReply::Uncommitted { .. }) => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request is no longer active: {request_id}"
                    )),
                );
            }
            Err(_) => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request is no longer active: {request_id}"
                    )),
                );
            }
        }
        state.permission_routes.settle(
            request_id,
            JsonlRetiredRequestSettlement::PermissionCommitted { response_digest },
        )?;
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::PermissionResolved {
                request_id: json!(request_id),
                decision: json!(decision),
                scope: json!(scope),
                strict_auto_review: json!(strict_auto_review),
            },
        );
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::PermissionResolved {
            request_id: json!(request_id),
            decision: json!(decision),
            scope: json!(scope),
            strict_auto_review: json!(strict_auto_review),
        },
    )?;
    state.permission_routes.settle(
        request_id,
        JsonlRetiredRequestSettlement::PermissionCommitted { response_digest },
    )?;
    match pending {
        JsonlPermissionRoute::Surface { .. } => unreachable!("surface response returned above"),
        JsonlPermissionRoute::CommandExec { request } => {
            if decision != protocol::PermissionResponseDecision::Allow {
                return protocol::write_server_event(
                    writer,
                    &request.event_id,
                    ServerEvent::error(format!("command/exec permission denied: {request_id}")),
                );
            }
            run_command_exec(
                config,
                state,
                Some(&request.thread_id),
                &request.command,
                request.command_is_argv,
                request.process_id.as_deref(),
                request.cwd.as_ref(),
                &request.env,
                &request.options,
                Some(&permissions),
                request.terminal,
                request.event_id,
                writer,
            )
        }
    }
}

fn materialize_surface_permission_profile(
    state: &ServerState,
    thread_id: &str,
    runtime_workspace_roots: &[std::path::PathBuf],
    mut permissions: protocol::RequestPermissionProfile,
) -> io::Result<protocol::RequestPermissionProfile> {
    let cwd = state
        .threads
        .thread(thread_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "permission thread is missing"))?
        .cwd()
        .to_string();
    if let Some(file_system) = permissions.file_system.as_mut() {
        for paths in [&mut file_system.read, &mut file_system.write]
            .into_iter()
            .flatten()
        {
            let mut materialized = Vec::new();
            for path in std::mem::take(paths) {
                for path in materialize_workspace_roots_paths(&cwd, runtime_workspace_roots, &path)
                {
                    if !materialized.contains(&path) {
                        materialized.push(path);
                    }
                }
            }
            *paths = materialized;
        }
    }
    Ok(permissions)
}

fn surface_permission_profile(
    permissions: &protocol::RequestPermissionProfile,
) -> crate::surface::SurfacePermissionProfile {
    crate::surface::SurfacePermissionProfile {
        file_system: permissions.file_system.as_ref().map(|file_system| {
            crate::surface::SurfaceFileSystemPermissionProfile {
                read: file_system.read.as_ref().map(|paths| {
                    paths
                        .iter()
                        .map(|path| {
                            crate::surface::SurfacePermissionPathLabel(
                                crate::surface::DisplayText::new(
                                    path.to_string_lossy().to_string(),
                                ),
                            )
                        })
                        .collect()
                }),
                write: file_system.write.as_ref().map(|paths| {
                    paths
                        .iter()
                        .map(|path| {
                            crate::surface::SurfacePermissionPathLabel(
                                crate::surface::DisplayText::new(
                                    path.to_string_lossy().to_string(),
                                ),
                            )
                        })
                        .collect()
                }),
            }
        }),
        network: permissions.network.as_ref().map(|network| {
            crate::surface::SurfacePermissionNetworkProfile {
                enabled: network.enabled,
                domains: network
                    .domains
                    .iter()
                    .map(|(domain, access)| {
                        (
                            crate::surface::SurfacePermissionDomainPattern(
                                crate::surface::DisplayText::new(domain.clone()),
                            ),
                            match access {
                                orca_core::config::PermissionProfileNetworkAccess::Allow => {
                                    crate::surface::SurfaceAllowDeny::Allow
                                }
                                orca_core::config::PermissionProfileNetworkAccess::Deny => {
                                    crate::surface::SurfaceAllowDeny::Deny
                                }
                            },
                        )
                    })
                    .collect(),
            }
        }),
    }
}
