//! Snapshot-and-subscribe projection. Observer loss never cancels a turn.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{
    ContentBlock, ContentChunk, Error, SessionId, SessionInfoUpdate, SessionNotification,
    SessionUpdate,
};
use serde_json::{Map, Value, json};

use super::agent::{
    AcpNotificationSender, emit_surface_event, replay_snapshot, replay_user_update,
};
use crate::runtime_surface::{
    AssistantChannel, ItemPatch, OperationPatch, SurfaceAssistantStream,
    SurfaceAssistantStreamState, SurfaceItem,
};
use crate::surface::{
    AssistantPatch, AttachResult, DetachRequest, FreshAttachRequest, RuntimeSurfaceHandle,
    SurfaceAttachmentRole, SurfaceCapability, SurfaceEvent, SurfaceRequestId,
    SurfaceSubscriptionItem,
};

pub const PROJECTION_META: &str = "orca.dev/projection";

pub(super) fn metadata(value: Value) -> Map<String, Value> {
    Map::from_iter([(PROJECTION_META.into(), value)])
}

pub(super) fn item_meta(item: &SurfaceItem, offset: usize) -> Map<String, Value> {
    let id = match item {
        SurfaceItem::UserMessage { id, .. }
        | SurfaceItem::SystemMessage { id, .. }
        | SurfaceItem::AssistantMessage { id, .. }
        | SurfaceItem::AssistantReasoning { id, .. }
        | SurfaceItem::AssistantPlan { id, .. }
        | SurfaceItem::ToolResultMessage { id, .. } => id,
    };
    metadata(json!({"version": 1, "itemId": id.as_str(), "offset": offset}))
}

#[derive(Default)]
pub(super) struct DeliveryState {
    stop: AtomicBool,
    flushed: Mutex<VecDeque<crate::surface::SurfaceOperationId>>,
}

impl DeliveryState {
    pub async fn wait_terminal(
        &self,
        id: &crate::surface::SurfaceOperationId,
    ) -> Result<(), Error> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if self
                .flushed
                .lock()
                .expect("observer delivery lock")
                .contains(id)
            {
                return Ok(());
            }
            if self.stop.load(Ordering::Acquire) || tokio::time::Instant::now() >= deadline {
                return Err(
                    Error::internal_error().data("ACP observer delivery lost; reload session")
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

pub(super) struct Observer(Arc<DeliveryState>);

impl Drop for Observer {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
    }
}

fn state(
    sender: &AcpNotificationSender,
    id: &SessionId,
    phase: &str,
    active: bool,
) -> Result<(), ()> {
    if matches!(sender, AcpNotificationSender::Standard(_)) {
        return Ok(());
    }
    sender.send(
        SessionNotification::new(
            id.clone(),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
        )
        .meta(metadata(
            json!({"version": 1, "phase": phase, "active": active}),
        )),
    )
}

impl Observer {
    pub fn delivery(&self) -> Arc<DeliveryState> {
        self.0.clone()
    }

    pub async fn start(
        surface: RuntimeSurfaceHandle,
        id: SessionId,
        sender: AcpNotificationSender,
        mode_ceiling: orca_core::approval_types::ApprovalMode,
        readiness_config: orca_core::config::RunConfig,
    ) -> Result<Self, Error> {
        let stop = Arc::new(DeliveryState::default());
        let stopped = stop.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let attachment = match surface.attach_fresh(FreshAttachRequest {
                request_id: SurfaceRequestId::new(),
                role: SurfaceAttachmentRole::Acp,
                requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
                interaction_capabilities: BTreeSet::new(),
            }) {
                AttachResult::FreshAttached { attachment } => attachment,
                _ => {
                    let _ = ready_tx.send(Err("observer attachment unavailable"));
                    return;
                }
            };
            let run = || {
                let Some(mut subscription) = surface.claim_subscription(&attachment.subscription)
                else {
                    let _ = ready_tx.send(Err("observer subscription unavailable"));
                    return;
                };
                let snapshot = &attachment.baseline.snapshot;
                let active = snapshot
                    .foreground_operation
                    .as_ref()
                    .is_some_and(|operation| operation.terminal.is_none());
                if state(&sender, &id, "reset", active).is_err() {
                    let _ = ready_tx.send(Err("observer disconnected"));
                    return;
                }
                replay_snapshot(snapshot, &id, &sender);
                let mut context = snapshot.context.clone();
                let mut usage = snapshot.usage.thread_total.clone();
                let _ = emit_usage(&sender, &id, &context, &usage);
                let _ = emit_settings(
                    &sender,
                    &id,
                    &snapshot.settings.effective,
                    mode_ceiling,
                    &readiness_config,
                );
                let mut streams =
                    HashMap::<crate::surface::SurfaceStreamId, SurfaceAssistantStream>::new();
                for stream in &snapshot.assistant_streams {
                    if stream.state == SurfaceAssistantStreamState::Open {
                        let _ = emit_text(&sender, &id, stream, 0, stream.text.as_str());
                        streams.insert(stream.stream_id.clone(), stream.clone());
                    }
                }
                if state(&sender, &id, "ready", active).is_err() {
                    let _ = ready_tx.send(Err("observer disconnected"));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                let mut tools = HashMap::new();
                while !stopped.stop.load(Ordering::Acquire) {
                    let Some(item) = subscription.recv_timeout(Duration::from_millis(100)) else {
                        continue;
                    };
                    let SurfaceSubscriptionItem::Batch { batch } = item else {
                        let _ = state(&sender, &id, "reload_required", false);
                        break;
                    };
                    let mut terminal = None;
                    for envelope in batch.events.as_slice() {
                        let result = match &envelope.event {
                            SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => {
                                streams.insert(stream.stream_id.clone(), stream.clone());
                                Ok(())
                            }
                            SurfaceEvent::Assistant(AssistantPatch::Delta {
                                stream_id,
                                offset,
                                text,
                            }) => {
                                if let Some(stream) = streams.get_mut(stream_id) {
                                    let start = offset.get() as usize;
                                    let overlap = stream.text.as_str().len().saturating_sub(start);
                                    if start > stream.text.as_str().len() {
                                        Err(())
                                    } else if let Some(suffix) = text.as_str().get(overlap..) {
                                        let result = emit_text(
                                            &sender,
                                            &id,
                                            stream,
                                            start + overlap,
                                            suffix,
                                        );
                                        stream.text = crate::surface::DisplayText::new(format!(
                                            "{}{suffix}",
                                            stream.text.as_str()
                                        ));
                                        result
                                    } else {
                                        Ok(())
                                    }
                                } else {
                                    Err(())
                                }
                            }
                            SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                                response,
                            }) => {
                                if let Some(item) = &response.message_item {
                                    let offset = streams
                                        .values()
                                        .find(|stream| stream.item_id == item.id)
                                        .map_or(0, |stream| stream.text.as_str().len());
                                    if let Some(suffix) =
                                        item.text.as_str().get(offset..).filter(|s| !s.is_empty())
                                    {
                                        let _ = sender.send(SessionNotification::new(id.clone(),
                                            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(suffix.to_string()))))
                                            .meta(metadata(json!({"version": 1, "itemId": item.id.as_str(), "offset": offset}))));
                                    }
                                }
                                Ok(())
                            }
                            SurfaceEvent::Item(ItemPatch::Added {
                                item: item @ SurfaceItem::UserMessage { input, .. },
                            }) => match replay_user_update(input) {
                                Some(update) => sender.send(
                                    SessionNotification::new(id.clone(), update)
                                        .meta(item_meta(item, 0)),
                                ),
                                None => Ok(()),
                            },
                            SurfaceEvent::Item(ItemPatch::InputResolved { item_id, fact }) => {
                                emit_user_images(&sender, &id, item_id.as_str(), fact)
                            }
                            SurfaceEvent::Assistant(AssistantPatch::StreamDiscarded {
                                stream_id,
                                ..
                            }) => {
                                streams.remove(stream_id);
                                state(&sender, &id, "reload_required", true)
                            }
                            SurfaceEvent::Operation(OperationPatch::Terminal { record }) => {
                                terminal = Some(record.operation_id.clone());
                                if matches!(sender, AcpNotificationSender::Standard(_)) {
                                    Ok(())
                                } else {
                                    let outcome =
                                        super::agent::terminal_to_stop_reason(&record.terminal);
                                    sender.send(
                                        SessionNotification::new(
                                            id.clone(),
                                            SessionUpdate::SessionInfoUpdate(
                                                SessionInfoUpdate::new(),
                                            ),
                                        )
                                        .meta(metadata(
                                            json!({
                                                "version": 1, "phase": "terminal", "active": false,
                                                "stopReason": outcome.as_ref().ok(),
                                                "error": outcome.err(),
                                            }),
                                        )),
                                    )
                                }
                            }
                            SurfaceEvent::Operation(_) => state(&sender, &id, "active", true),
                            SurfaceEvent::Plan(plan) => {
                                sender.send(SessionNotification::new(id.clone(), plan_update(plan)))
                            }
                            SurfaceEvent::Usage(next) => {
                                usage = next.thread_total.clone();
                                emit_usage(&sender, &id, &context, &usage)
                            }
                            SurfaceEvent::Context(next) => {
                                context = next.clone();
                                emit_usage(&sender, &id, &context, &usage)
                            }
                            SurfaceEvent::Settings(
                                crate::runtime_surface::SettingsPatch::Committed {
                                    snapshot, ..
                                },
                            ) => emit_settings(
                                &sender,
                                &id,
                                &snapshot.effective,
                                mode_ceiling,
                                &readiness_config,
                            ),
                            _ => {
                                emit_surface_event(&id, &sender, &envelope.event, &mut tools);
                                Ok(())
                            }
                        };
                        if result.is_err() {
                            return;
                        }
                    }
                    if let Some(terminal) = terminal {
                        // All notifications in the terminal batch have physical
                        // write acknowledgements before a prompt may return.
                        let mut flushed = stopped.flushed.lock().expect("observer delivery lock");
                        if flushed.len() == 128 {
                            flushed.pop_front();
                        }
                        flushed.push_back(terminal);
                    }
                }
            };
            run();
            stopped.stop.store(true, Ordering::Release);
            let _ = surface.detach(
                &attachment.client,
                DetachRequest {
                    request_id: SurfaceRequestId::new(),
                },
            );
        });
        ready_rx
            .await
            .map_err(Error::into_internal_error)?
            .map_err(|message| Error::internal_error().data(message))?;
        Ok(Self(stop))
    }
}

fn emit_usage(
    sender: &AcpNotificationSender,
    id: &SessionId,
    context: &crate::surface::SurfaceContextSnapshot,
    usage: &crate::surface::UsageTotals,
) -> Result<(), ()> {
    sender.send(
        SessionNotification::new(
            id.clone(),
            SessionUpdate::UsageUpdate(
                agent_client_protocol::UsageUpdate::new(context.used_tokens, context.limit_tokens)
                    .cost(agent_client_protocol::Cost::new(
                        usage.estimated_cost_usd_micros as f64 / 1_000_000.0,
                        "USD",
                    )),
            ),
        )
        .meta(metadata(json!({"version": 1, "usage": usage}))),
    )
}

pub(super) fn emit_user_images(
    sender: &AcpNotificationSender,
    id: &SessionId,
    item_id: &str,
    fact: &crate::surface::SurfaceResolvedInputFact,
) -> Result<(), ()> {
    let crate::surface::SurfaceResolvedInputFact::Replayable { input, .. } = fact else {
        return Ok(());
    };
    for (index, block) in input.blocks.as_slice().iter().enumerate() {
        if let crate::surface::SurfaceInputBlock::Image {
            source:
                crate::surface::SurfaceImageSource::Base64 {
                    media_type, data, ..
                },
            ..
        } = block
        {
            sender.send(
                SessionNotification::new(
                    id.clone(),
                    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Image(
                        agent_client_protocol::ImageContent::new(
                            data.clone(),
                            media_type.as_str().to_string(),
                        ),
                    ))),
                )
                .meta(metadata(json!({
                    "version": 1, "itemId": format!("{item_id}:image:{index}"), "offset": 0,
                }))),
            )?;
        }
    }
    Ok(())
}

fn emit_settings(
    sender: &AcpNotificationSender,
    id: &SessionId,
    settings: &crate::surface::SurfaceRuntimeSettings,
    ceiling: orca_core::approval_types::ApprovalMode,
    readiness_config: &orca_core::config::RunConfig,
) -> Result<(), ()> {
    let warnings = super::agent::settings_startup_warnings(readiness_config, settings);
    sender.send(SessionNotification::new(
        id.clone(),
        SessionUpdate::ConfigOptionUpdate(
            agent_client_protocol::ConfigOptionUpdate::new(super::settings::options(
                settings, ceiling,
            ))
            .meta(super::agent::startup_warnings_meta(&warnings)),
        ),
    ))?;
    sender.send(SessionNotification::new(
        id.clone(),
        SessionUpdate::CurrentModeUpdate(agent_client_protocol::CurrentModeUpdate::new(
            super::settings::mode_name(settings.approval_mode),
        )),
    ))
}

fn plan_update(plan: &crate::surface::SurfacePlanSnapshot) -> SessionUpdate {
    use crate::runtime_surface::{SurfacePlanPriority, SurfacePlanStatus};
    use agent_client_protocol::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus};
    SessionUpdate::Plan(Plan::new(
        plan.items
            .iter()
            .map(|item| {
                PlanEntry::new(
                    item.step.as_str(),
                    match item.priority {
                        SurfacePlanPriority::Low => PlanEntryPriority::Low,
                        SurfacePlanPriority::Medium => PlanEntryPriority::Medium,
                        SurfacePlanPriority::High => PlanEntryPriority::High,
                    },
                    match item.status {
                        SurfacePlanStatus::Pending => PlanEntryStatus::Pending,
                        SurfacePlanStatus::InProgress => PlanEntryStatus::InProgress,
                        SurfacePlanStatus::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect(),
    ))
}

fn emit_text(
    sender: &AcpNotificationSender,
    id: &SessionId,
    stream: &SurfaceAssistantStream,
    offset: usize,
    text: &str,
) -> Result<(), ()> {
    let chunk = ContentChunk::new(ContentBlock::from(text.to_string()));
    let update = match stream.channel {
        AssistantChannel::Reasoning => SessionUpdate::AgentThoughtChunk(chunk),
        AssistantChannel::Message | AssistantChannel::Plan => {
            SessionUpdate::AgentMessageChunk(chunk)
        }
    };
    sender.send(
        SessionNotification::new(id.clone(), update).meta(metadata(json!({
            "version": 1, "itemId": stream.item_id.as_str(), "offset": offset
        }))),
    )
}
