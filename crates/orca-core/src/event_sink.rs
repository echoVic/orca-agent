use std::io::{self, Write};
use std::sync::Arc;

use crate::config::OutputFormat;
use crate::event_schema::{EventDraft, EventEnvelope, EventType};

pub trait EventObserver: Send + Sync {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()>;
}

impl<F> EventObserver for F
where
    F: Fn(&EventEnvelope) -> io::Result<()> + Send + Sync,
{
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        self(event)
    }
}

pub struct EventSink<W: Write> {
    writer: W,
    format: OutputFormat,
    observer: Option<Arc<dyn EventObserver>>,
}

impl<W: Write> EventSink<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self {
            writer,
            format,
            observer: None,
        }
    }

    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_optional_observer(mut self, observer: Option<Arc<dyn EventObserver>>) -> Self {
        self.observer = observer;
        self
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn emit(&mut self, event: EventDraft) -> io::Result<()> {
        event.publish(|event| {
            if let Some(observer) = &self.observer {
                observer.observe(event)?;
            }
            match self.format {
                OutputFormat::Jsonl => {
                    serde_json::to_writer(&mut self.writer, event)?;
                    writeln!(self.writer)?;
                }
                OutputFormat::Text => self.emit_text(event)?,
            }

            self.writer.flush()
        })
    }

    fn emit_text(&mut self, event: &EventEnvelope) -> io::Result<()> {
        match event.event_type {
            EventType::SessionStarted => writeln!(self.writer, "session started"),
            EventType::TurnStarted => writeln!(self.writer, "turn started"),
            EventType::AssistantReasoningDelta => {
                writeln!(
                    self.writer,
                    "thinking: {}",
                    event.payload["text"].as_str().unwrap_or("")
                )
            }
            EventType::AssistantMessageDelta => {
                writeln!(
                    self.writer,
                    "assistant: {}",
                    event.payload["text"].as_str().unwrap_or("")
                )
            }
            EventType::ModelResponseCompleted => Ok(()),
            EventType::ProviderReplayUpdated => writeln!(self.writer, "provider replay updated"),
            EventType::ModelRouted => {
                let actual = event.payload["actual_model"].as_str().unwrap_or("unknown");
                let reason = event.payload["reason"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "model routed: {actual} ({reason})")
            }
            EventType::UsageUpdated => {
                let total = event.payload["total_tokens"].as_u64().unwrap_or(0);
                let cost = event.payload["estimated_cost_usd"].as_f64().unwrap_or(0.0);
                writeln!(self.writer, "usage: {total} tokens (${cost:.6})")
            }
            EventType::ContextUpdated => {
                let used = event.payload["used_tokens"].as_u64().unwrap_or_default();
                let limit = event.payload["limit_tokens"].as_u64().unwrap_or_default();
                writeln!(self.writer, "context: {used}/{limit} tokens")
            }
            EventType::ContextCompactionStarted => {
                writeln!(self.writer, "context compaction started")
            }
            EventType::ContextCompacted => {
                let before = event.payload["before_messages"].as_u64().unwrap_or(0);
                let after = event.payload["after_messages"].as_u64().unwrap_or(0);
                writeln!(
                    self.writer,
                    "context compacted: {before} -> {after} messages"
                )
            }
            EventType::ApprovalRequested => writeln!(self.writer, "approval requested"),
            EventType::ApprovalResolved => writeln!(self.writer, "approval resolved"),
            EventType::ToolCallProgress => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let bytes = event.payload["arguments_bytes"].as_u64().unwrap_or(0);
                writeln!(
                    self.writer,
                    "tool receiving arguments: {name} ({bytes} bytes)"
                )
            }
            EventType::ToolOutputDelta => write!(
                self.writer,
                "{}",
                event.payload["chunk"].as_str().unwrap_or_default()
            ),
            EventType::ToolCallRequested => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                writeln!(self.writer, "tool requested: {name}")
            }
            EventType::ToolCallCompleted => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let status = event.payload["status"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "tool completed: {name} ({status})")
            }
            EventType::PlanUpdated => {
                if let Some(explanation) = event.payload["explanation"].as_str() {
                    writeln!(self.writer, "{explanation}")?;
                }
                if let Some(plan) = event.payload["plan"].as_array() {
                    for item in plan {
                        let step = item["step"].as_str().unwrap_or("");
                        let icon = match item["status"].as_str().unwrap_or("") {
                            "completed" => "\u{2713}",
                            "in_progress" => "\u{2192}",
                            "pending" => "\u{2022}",
                            _ => "\u{00b7}",
                        };
                        writeln!(self.writer, "  {icon} {step}")?;
                    }
                }
                Ok(())
            }
            EventType::GoalCreated => writeln!(self.writer, "goal created"),
            EventType::GoalRunStarted => writeln!(self.writer, "goal run started"),
            EventType::GoalTurnStarted => writeln!(self.writer, "goal turn started"),
            EventType::GoalIntentRequested => writeln!(self.writer, "goal intent requested"),
            EventType::GoalIntentAcknowledged => {
                writeln!(self.writer, "goal intent acknowledged")
            }
            EventType::GoalTurnFinished => writeln!(self.writer, "goal turn finished"),
            EventType::GoalVerificationCompleted => {
                writeln!(self.writer, "goal verification completed")
            }
            EventType::GoalTransitioned => writeln!(self.writer, "goal transitioned"),
            EventType::GoalContinuationAdmitted => {
                writeln!(self.writer, "goal continuation admitted")
            }
            EventType::GoalContinuationRejected => {
                writeln!(self.writer, "goal continuation rejected")
            }
            EventType::GoalPaused => writeln!(self.writer, "goal paused"),
            EventType::GoalRecovered => writeln!(self.writer, "goal recovered"),
            EventType::GoalCompleted => writeln!(self.writer, "goal completed"),
            EventType::SubagentStarted => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                writeln!(self.writer, "subagent started: {description}")
            }
            EventType::SubagentProgress => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                let activity = event.payload["activity"].as_str().unwrap_or("running");
                writeln!(self.writer, "subagent progress: {description} ({activity})")
            }
            EventType::SubagentCompleted => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                let status = event.payload["status"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "subagent completed: {description} ({status})")
            }
            EventType::AgentContinuationCheckpointed => {
                writeln!(self.writer, "agent continuation checkpointed")
            }
            EventType::AgentContinuationSuspended => {
                writeln!(self.writer, "agent continuation suspended")
            }
            EventType::AgentContinuationResumed => {
                writeln!(self.writer, "agent continuation resumed")
            }
            EventType::AgentContinuationOrphanReconciled => {
                writeln!(self.writer, "agent continuation orphan reconciled")
            }
            EventType::AgentContinuationIndeterminate => {
                writeln!(self.writer, "agent continuation indeterminate")
            }
            EventType::WorkflowStarted => {
                let workflow_name = event.payload["workflowName"].as_str().unwrap_or("workflow");
                writeln!(self.writer, "workflow started: {workflow_name}")
            }
            EventType::WorkflowResumed => writeln!(self.writer, "workflow resumed"),
            EventType::WorkflowPhaseStarted => writeln!(self.writer, "workflow phase started"),
            EventType::WorkflowPhaseCompleted => {
                writeln!(self.writer, "workflow phase completed")
            }
            EventType::WorkflowAgentStarted => writeln!(self.writer, "workflow agent started"),
            EventType::WorkflowAgentCached => writeln!(self.writer, "workflow agent cached"),
            EventType::WorkflowAgentCompleted => {
                writeln!(self.writer, "workflow agent completed")
            }
            EventType::WorkflowAgentFailed => writeln!(self.writer, "workflow agent failed"),
            EventType::WorkflowPaused => writeln!(self.writer, "workflow paused"),
            EventType::WorkflowStopped => writeln!(self.writer, "workflow stopped"),
            EventType::WorkflowCompleted => writeln!(self.writer, "workflow completed"),
            EventType::WorkflowFailed => writeln!(self.writer, "workflow failed"),
            EventType::WorkflowResultAvailable => {
                writeln!(self.writer, "workflow result available")
            }
            EventType::WorkflowTasksUpdated => writeln!(self.writer, "workflow tasks updated"),
            EventType::TaskStatusUpdated => writeln!(self.writer, "task status updated"),
            EventType::VerificationStarted => writeln!(self.writer, "verification started"),
            EventType::VerificationCompleted => writeln!(self.writer, "verification completed"),
            EventType::Error => writeln!(
                self.writer,
                "error: {}",
                event.payload["message"].as_str().unwrap_or("unknown")
            ),
            EventType::SessionCompleted => {
                writeln!(
                    self.writer,
                    "status: {}",
                    event.payload["status"].as_str().unwrap_or("unknown")
                )
            }
        }
    }
}

pub fn observe_event(observer: Option<&dyn EventObserver>, event: EventDraft) -> io::Result<()> {
    if observer.is_none() && !event.requires_publication_without_observer() {
        return Ok(());
    }
    event.publish(|event| match observer {
        Some(observer) => observer.observe(event),
        None => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_schema::{
        EVENT_SEQUENCE_RESERVATION_SIZE, EventFactory, EventPublicationStore,
    };
    use crate::thread_identity::TurnId;
    use crate::thread_item_projection::ModelResponseIdentity;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct RecordingPublicationStore {
        fail_reservation: AtomicBool,
        fail_journal: AtomicBool,
        reservations: Mutex<Vec<u64>>,
        semantic_events: Mutex<Vec<EventEnvelope>>,
    }

    impl EventPublicationStore for RecordingPublicationStore {
        fn reserve_through(&self, next_seq_exclusive: u64) -> io::Result<()> {
            if self.fail_reservation.load(Ordering::SeqCst) {
                return Err(io::Error::other("sequence reservation failed"));
            }
            self.reservations.lock().unwrap().push(next_seq_exclusive);
            Ok(())
        }

        fn append_semantic_event(&self, event: &EventEnvelope) -> io::Result<()> {
            if self.fail_journal.load(Ordering::SeqCst) {
                return Err(io::Error::other("semantic journal failed"));
            }
            self.semantic_events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    fn model_response_identity() -> ModelResponseIdentity {
        ModelResponseIdentity::new(TurnId::new())
    }

    #[test]
    fn jsonl_format_writes_one_line_per_event() {
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Jsonl);
        let mut f = EventFactory::new("run-1".to_string());
        let identity = model_response_identity();

        sink.emit(f.error("test error")).unwrap();
        sink.emit(f.assistant_message_delta(&identity, "hello"))
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["payload"]["message"], "test error");

        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["type"], "assistant.message.delta");
        assert_eq!(parsed["payload"]["text"], "hello");
    }

    #[test]
    fn text_format_writes_human_readable() {
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Text);
        let mut f = EventFactory::new("run-1".to_string());
        let identity = model_response_identity();

        sink.emit(f.error("something broke")).unwrap();
        sink.emit(f.assistant_message_delta(&identity, "hi"))
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("error: something broke"));
        assert!(output.contains("assistant: hi"));
    }

    #[test]
    fn observer_receives_the_same_typed_event_that_is_serialized() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_callback = Arc::clone(&observed);
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Jsonl).with_observer(Arc::new(
            move |event: &EventEnvelope| {
                observed_for_callback.lock().unwrap().push(event.clone());
                Ok(())
            },
        ));
        let mut events = EventFactory::new("typed-observer".to_string());
        let identity = model_response_identity();
        let event = events.assistant_message_delta(&identity, "hello");
        let expected_run_id = event.run_id.clone();
        let expected_event_type = event.event_type;
        let expected_payload = event.payload.clone();

        sink.emit(event).unwrap();

        let observed = observed.lock().unwrap();
        let observed = observed.first().expect("one observed event");
        assert_eq!(observed.run_id, expected_run_id);
        assert_eq!(observed.seq, 0);
        assert!(observed.timestamp_ms > 0);
        assert_eq!(observed.event_type, expected_event_type);
        assert_eq!(observed.payload, expected_payload);
        let serialized: serde_json::Value =
            serde_json::from_str(String::from_utf8(buf).unwrap().trim()).unwrap();
        assert_eq!(serialized, serde_json::to_value(observed).unwrap());
    }

    #[test]
    fn semantic_event_is_journaled_before_observer_with_exact_envelope() {
        let store = Arc::new(RecordingPublicationStore::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let store = Arc::clone(&store);
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                let journal = store.semantic_events.lock().unwrap();
                assert_eq!(journal.as_slice(), std::slice::from_ref(event));
                drop(journal);
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "journal-before-observer".to_string(),
            0,
            publication_store,
        );

        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(events.error("durable failure"))
            .unwrap();

        let journal = store.semantic_events.lock().unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(journal.as_slice(), observed.as_slice());
        let event = journal.first().expect("journaled semantic event");
        assert_eq!(event.version, crate::event_schema::EVENT_SCHEMA_VERSION);
        assert_eq!(event.run_id, "journal-before-observer");
        assert_eq!(event.seq, 0);
        assert!(event.timestamp_ms > 0);
        assert_eq!(event.event_type, EventType::Error);
        assert_eq!(event.payload["message"], "durable failure");
    }

    #[test]
    fn transient_events_are_explicitly_excluded_from_the_semantic_journal() {
        let store = Arc::new(RecordingPublicationStore::default());
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "transient-exclusion".to_string(),
            0,
            publication_store,
        );
        let mut sink = EventSink::new(io::sink(), OutputFormat::Jsonl);
        let identity = model_response_identity();

        sink.emit(events.assistant_reasoning_delta(&identity, "reasoning"))
            .unwrap();
        sink.emit(events.assistant_message_delta(&identity, "message"))
            .unwrap();
        sink.emit(events.tool_output_delta("tool-1", "chunk"))
            .unwrap();
        sink.emit(events.error("terminal")).unwrap();

        let journal = store.semantic_events.lock().unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].event_type, EventType::Error);
        assert_eq!(journal[0].seq, 3);
    }

    #[test]
    fn semantic_event_without_observer_is_still_journaled_but_transient_is_unpublished() {
        let store = Arc::new(RecordingPublicationStore::default());
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "journal-only-publication".to_string(),
            0,
            publication_store,
        );
        let identity = model_response_identity();

        observe_event(
            None,
            events.assistant_message_delta(&identity, "not published"),
        )
        .unwrap();
        observe_event(None, events.error("journal only")).unwrap();

        let journal = store.semantic_events.lock().unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].seq, 0);
        assert_eq!(journal[0].event_type, EventType::Error);
        assert_eq!(journal[0].payload["message"], "journal only");
        assert_eq!(
            store.reservations.lock().unwrap().as_slice(),
            [EVENT_SEQUENCE_RESERVATION_SIZE]
        );
    }

    #[test]
    fn event_types_have_a_closed_semantic_journal_policy() {
        let transient = [
            EventType::AssistantReasoningDelta,
            EventType::AssistantMessageDelta,
            EventType::ProviderReplayUpdated,
            EventType::UsageUpdated,
            EventType::ContextUpdated,
            EventType::ToolCallProgress,
            EventType::ToolOutputDelta,
            EventType::SubagentProgress,
            EventType::WorkflowTasksUpdated,
            EventType::TaskStatusUpdated,
        ];
        assert!(transient.iter().all(|event_type| !event_type.is_semantic()));

        let semantic = [
            EventType::SessionStarted,
            EventType::TurnStarted,
            EventType::ContextCompactionStarted,
            EventType::ContextCompacted,
            EventType::ModelRouted,
            EventType::ApprovalRequested,
            EventType::ApprovalResolved,
            EventType::ToolCallRequested,
            EventType::ToolCallCompleted,
            EventType::ModelResponseCompleted,
            EventType::PlanUpdated,
            EventType::SubagentStarted,
            EventType::SubagentCompleted,
            EventType::WorkflowStarted,
            EventType::WorkflowResumed,
            EventType::WorkflowPhaseStarted,
            EventType::WorkflowPhaseCompleted,
            EventType::WorkflowAgentStarted,
            EventType::WorkflowAgentCached,
            EventType::WorkflowAgentCompleted,
            EventType::WorkflowAgentFailed,
            EventType::WorkflowPaused,
            EventType::WorkflowStopped,
            EventType::WorkflowCompleted,
            EventType::WorkflowFailed,
            EventType::WorkflowResultAvailable,
            EventType::VerificationStarted,
            EventType::VerificationCompleted,
            EventType::Error,
            EventType::SessionCompleted,
        ];
        assert!(semantic.iter().all(|event_type| event_type.is_semantic()));
    }

    #[test]
    fn observer_failure_is_returned_to_the_operation() {
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(Arc::new(
            |_event: &EventEnvelope| Err(io::Error::other("observer rejected event")),
        ));
        let mut events = EventFactory::new("typed-observer-error".to_string());

        let error = sink.emit(events.error("boom")).unwrap_err();

        assert!(error.to_string().contains("observer rejected event"));
    }

    #[test]
    fn observer_failure_consumes_its_sequence_before_cleanup_event() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let calls = Arc::clone(&calls);
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(io::Error::other("observer rejected event"));
                }
                observed.lock().unwrap().push(event.clone());
                Ok(())
            })
        };
        let mut events = EventFactory::new("observer-cleanup".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(observer);

        assert!(sink.emit(events.error("failed delivery")).is_err());
        sink.emit(events.error("cleanup delivery")).unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seq, 1);
        assert_eq!(observed[0].payload["message"], "cleanup delivery");
    }

    #[test]
    fn forked_factories_number_events_in_publication_order() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let mut first_factory = EventFactory::new("ordered-publication".to_string());
        let mut second_factory = first_factory.fork();
        let first = first_factory.error("constructed first");
        let second = second_factory.error("published first");

        let second_observer = Arc::clone(&observer);
        std::thread::spawn(move || {
            let mut sink =
                EventSink::new(io::sink(), OutputFormat::Jsonl).with_observer(second_observer);
            sink.emit(second).unwrap();
        })
        .join()
        .unwrap();
        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(first)
            .unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(
            observed.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(observed[0].payload["message"], "published first");
        assert_eq!(observed[1].payload["message"], "constructed first");
    }

    #[test]
    fn unpublished_draft_does_not_create_a_sequence_gap() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let mut events = EventFactory::new("draft-gap".to_string());
        drop(events.error("not published"));

        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(events.error("published"))
            .unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seq, 0);
        assert_eq!(observed[0].payload["message"], "published");
    }

    #[test]
    fn sequence_block_is_reserved_before_delivery_and_shared_by_events() {
        let store = Arc::new(RecordingPublicationStore::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let store = Arc::clone(&store);
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                assert_eq!(
                    store.reservations.lock().unwrap().as_slice(),
                    [EVENT_SEQUENCE_RESERVATION_SIZE]
                );
                observed.lock().unwrap().push(event.seq);
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events =
            EventFactory::with_publication_store("reserved".to_string(), 0, publication_store);
        let mut sink = EventSink::new(io::sink(), OutputFormat::Jsonl).with_observer(observer);

        sink.emit(events.error("first")).unwrap();
        sink.emit(events.error("second")).unwrap();

        assert_eq!(observed.lock().unwrap().as_slice(), [0, 1]);
        assert_eq!(
            store.reservations.lock().unwrap().as_slice(),
            [EVENT_SEQUENCE_RESERVATION_SIZE]
        );
    }

    #[test]
    fn crossing_sequence_block_reserves_before_first_event_in_next_block() {
        let store = Arc::new(RecordingPublicationStore::default());
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "block-crossing".to_string(),
            0,
            publication_store,
        );
        let mut sink = EventSink::new(io::sink(), OutputFormat::Jsonl);

        for index in 0..=EVENT_SEQUENCE_RESERVATION_SIZE {
            sink.emit(events.error(&format!("event {index}"))).unwrap();
        }

        assert_eq!(
            store.reservations.lock().unwrap().as_slice(),
            [
                EVENT_SEQUENCE_RESERVATION_SIZE,
                EVENT_SEQUENCE_RESERVATION_SIZE * 2,
            ]
        );
    }

    #[test]
    fn resumed_factory_starts_after_prior_exclusive_reservation() {
        let store = Arc::new(RecordingPublicationStore::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.seq);
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "resumed".to_string(),
            EVENT_SEQUENCE_RESERVATION_SIZE,
            publication_store,
        );

        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(events.error("after resume"))
            .unwrap();

        assert_eq!(
            observed.lock().unwrap().as_slice(),
            [EVENT_SEQUENCE_RESERVATION_SIZE]
        );
        assert_eq!(
            store.reservations.lock().unwrap().as_slice(),
            [EVENT_SEQUENCE_RESERVATION_SIZE * 2]
        );
    }

    #[test]
    fn reservation_failure_prevents_delivery_without_consuming_sequence() {
        let store = Arc::new(RecordingPublicationStore::default());
        store.fail_reservation.store(true, Ordering::SeqCst);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.seq);
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events = EventFactory::with_publication_store(
            "reservation-error".to_string(),
            0,
            publication_store,
        );
        let mut sink =
            EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(Arc::clone(&observer));

        let error = sink.emit(events.error("not delivered")).unwrap_err();
        assert!(error.to_string().contains("sequence reservation failed"));
        assert!(observed.lock().unwrap().is_empty());
        assert!(sink.writer_mut().is_empty());

        store.fail_reservation.store(false, Ordering::SeqCst);
        sink.emit(events.error("delivered after retry")).unwrap();
        assert_eq!(observed.lock().unwrap().as_slice(), [0]);
    }

    #[test]
    fn journal_failure_prevents_delivery_and_consumes_the_ambiguous_sequence() {
        let store = Arc::new(RecordingPublicationStore::default());
        store.fail_journal.store(true, Ordering::SeqCst);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let publication_store: Arc<dyn EventPublicationStore> = store.clone();
        let mut events =
            EventFactory::with_publication_store("journal-error".to_string(), 0, publication_store);
        let mut sink =
            EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(Arc::clone(&observer));

        let error = sink.emit(events.error("ambiguous append")).unwrap_err();
        assert!(error.to_string().contains("semantic journal failed"));
        assert!(observed.lock().unwrap().is_empty());
        assert!(sink.writer_mut().is_empty());

        store.fail_journal.store(false, Ordering::SeqCst);
        sink.emit(events.error("cleanup delivery")).unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seq, 1);
        assert_eq!(observed[0].payload["message"], "cleanup delivery");
        let journal = store.semantic_events.lock().unwrap();
        assert_eq!(journal.as_slice(), observed.as_slice());
    }
}
