use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use orca_core::agent_event::{
    AgentEvent, AgentEventEnvelope, AgentRegistrySnapshot, AgentStatus, AgentSummary,
};
use orca_platform::fs::ExclusiveFileLock;

const AGENT_JOURNAL_FILE: &str = "agent-events.jsonl";
const AGENT_DEAD_LETTER_FILE: &str = "agent-events.dead-letter.jsonl";
const AGENT_JOURNAL_LOCK_FILE: &str = "agent-events.lock";

#[derive(Default)]
struct AgentRegistryState {
    revision: u64,
    agents: HashMap<(String, String), AgentSummary>,
    attempts: HashMap<(String, String, String), u64>,
    event_digests: HashMap<String, String>,
}

pub struct AgentRegistry {
    state: Mutex<AgentRegistryState>,
    journal: Option<Mutex<File>>,
    journal_lock_path: Option<PathBuf>,
    dead_letter_path: Option<PathBuf>,
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRegistry")
            .finish_non_exhaustive()
    }
}

impl AgentRegistry {
    pub fn open_default() -> io::Result<Arc<Self>> {
        #[cfg(test)]
        {
            return Ok(Arc::new(Self::in_memory()));
        }
        #[cfg(not(test))]
        let Some(root) = orca_core::config::file::config_dir() else {
            return Ok(Arc::new(Self::in_memory()));
        };
        #[cfg(not(test))]
        Self::open(root)
    }

    pub fn in_memory() -> Self {
        Self {
            state: Mutex::new(AgentRegistryState::default()),
            journal: None,
            journal_lock_path: None,
            dead_letter_path: None,
        }
    }

    pub fn open(root: impl AsRef<Path>) -> io::Result<Arc<Self>> {
        fs::create_dir_all(root.as_ref())?;
        let journal_path = root.as_ref().join(AGENT_JOURNAL_FILE);
        let journal_lock_path = root.as_ref().join(AGENT_JOURNAL_LOCK_FILE);
        let dead_letter_path = root.as_ref().join(AGENT_DEAD_LETTER_FILE);
        let mut state = AgentRegistryState::default();
        if journal_path.exists() {
            // Hold the cross-process lock during recovery too, so a concurrent
            // writer cannot append a torn line while this process reads.
            let _recovery_lock = acquire_journal_lock(&journal_lock_path)?;
            let reader = BufReader::new(File::open(&journal_path)?);
            for (index, line) in reader.lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<AgentEventEnvelope>(&line) {
                    Ok(event) => {
                        let result = event.validate().map_err(str::to_string).and_then(|()| {
                            apply_event(&mut state, &event, SequenceMode::Recover)
                                .map_err(|error| error.to_string())
                        });
                        if let Err(reason) = result {
                            append_dead_letter(
                                &dead_letter_path,
                                index as u64 + 1,
                                &reason,
                                &line,
                            )?;
                            consume_dead_letter(&mut state, &event, &reason);
                        }
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        append_dead_letter(&dead_letter_path, index as u64 + 1, &reason, &line)?;
                    }
                }
            }
        }
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)?;
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            journal: Some(Mutex::new(journal)),
            journal_lock_path: Some(journal_lock_path),
            dead_letter_path: Some(dead_letter_path),
        }))
    }

    pub fn append(&self, event: AgentEventEnvelope) -> io::Result<()> {
        event
            .validate()
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = state.event_digests.get(&event.event_id) {
            return if existing == &event.payload_digest {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "agent event id was reused with a conflicting digest",
                ))
            };
        }
        validate_next_sequence(&state, &event, SequenceMode::Strict)?;
        if let Some(journal) = self.journal.as_ref() {
            let mut encoded = serde_json::to_vec(&event)?;
            encoded.push(b'\n');
            let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
            // Serialize the append across processes: a single record can exceed
            // the atomic pipe/write size, so without an exclusive lock two Orca
            // processes sharing this journal could interleave partial lines.
            let _journal_lock = match self.journal_lock_path.as_deref() {
                Some(path) => Some(acquire_journal_lock(path)?),
                None => None,
            };
            journal.write_all(&encoded)?;
            journal.sync_data()?;
        }
        apply_event(&mut state, &event, SequenceMode::Strict)?;
        Ok(())
    }

    pub fn snapshot(&self, root_thread_id: &str) -> AgentRegistrySnapshot {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut agents = state
            .agents
            .values()
            .filter(|agent| agent.root_thread_id == root_thread_id)
            .cloned()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.agent_id.cmp(&right.agent_id))
        });
        AgentRegistrySnapshot {
            revision: state.revision,
            agents,
        }
    }

    pub fn roots(&self) -> HashSet<String> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .agents
            .values()
            .map(|agent| agent.root_thread_id.clone())
            .collect()
    }

    pub fn dead_letter_path(&self) -> Option<&Path> {
        self.dead_letter_path.as_deref()
    }
}

/// How strictly a sequence number is enforced against the last-applied value.
///
/// Live [`AgentRegistry::append`] is `Strict`: the next event must be exactly
/// the successor of the last one. Recovery from a journal is `Recover`: a
/// *forward* gap is expected and benign, because the intervening frames were
/// malformed and dead-lettered, so a later valid frame must still be applied
/// rather than rejected (and discarded) as a gap. A duplicate or backward
/// sequence is a real corruption in both modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SequenceMode {
    Strict,
    Recover,
}

fn validate_next_sequence(
    state: &AgentRegistryState,
    event: &AgentEventEnvelope,
    mode: SequenceMode,
) -> io::Result<()> {
    let key = (
        event.root_thread_id.clone(),
        event.agent_id.clone(),
        event.attempt_id.clone(),
    );
    let expected = state
        .attempts
        .get(&key)
        .copied()
        .unwrap_or_default()
        .saturating_add(1);
    let ordered = match mode {
        SequenceMode::Strict => event.source_sequence == expected,
        SequenceMode::Recover => event.source_sequence >= expected,
    };
    if !ordered {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "agent event sequence gap: expected {expected}, observed {}",
                event.source_sequence
            ),
        ));
    }
    Ok(())
}

fn apply_event(
    state: &mut AgentRegistryState,
    event: &AgentEventEnvelope,
    mode: SequenceMode,
) -> io::Result<()> {
    if let Some(existing) = state.event_digests.get(&event.event_id) {
        return if existing == &event.payload_digest {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "agent event id was reused with a conflicting digest",
            ))
        };
    }
    validate_next_sequence(state, event, mode)?;

    match &event.event {
        AgentEvent::Spawned {
            batch_id,
            batch_size,
            parent_thread_id,
            description,
        } => {
            let agent_key = (event.root_thread_id.clone(), event.agent_id.clone());
            if state.agents.contains_key(&agent_key) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "agent identity already exists",
                ));
            }
            state.agents.insert(
                agent_key,
                AgentSummary {
                    root_thread_id: event.root_thread_id.clone(),
                    batch_id: batch_id.clone(),
                    batch_size: *batch_size,
                    agent_id: event.agent_id.clone(),
                    thread_id: event.thread_id.clone(),
                    parent_thread_id: parent_thread_id.clone(),
                    description: description.clone(),
                    status: AgentStatus::Queued,
                    activity: None,
                    turn: None,
                    usage: Default::default(),
                    result: None,
                    error: None,
                    created_at_ms: event.occurred_at_ms,
                    updated_at_ms: event.occurred_at_ms,
                },
            );
        }
        AgentEvent::Activity {
            activity,
            turn,
            usage,
        } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::Running;
            agent.activity = Some(activity.clone());
            if turn.is_some() {
                agent.turn = *turn;
            }
            if let Some(usage) = usage {
                agent.usage = usage.clone();
            }
            agent.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::OutputDelta { .. } => {
            require_agent(state, event)?.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::PermissionRequested { description } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::WaitingPermission;
            agent.activity = Some(orca_core::agent_event::AgentActivity::WaitingPermission {
                description: description.clone(),
            });
            agent.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::Completed { result, usage } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::Completed;
            agent.result.clone_from(result);
            agent.usage = usage.clone();
            agent.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::Failed { reason, usage } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::Failed;
            agent.error = Some(reason.clone());
            agent.usage = usage.clone();
            agent.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::Cancelled { reason, usage } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::Cancelled;
            agent.error = Some(reason.clone());
            agent.usage = usage.clone();
            agent.updated_at_ms = event.occurred_at_ms;
        }
        AgentEvent::Corrupt { reason } => {
            let agent = require_agent(state, event)?;
            agent.status = AgentStatus::Corrupt;
            agent.error = Some(reason.clone());
            agent.updated_at_ms = event.occurred_at_ms;
        }
    }

    state.attempts.insert(
        (
            event.root_thread_id.clone(),
            event.agent_id.clone(),
            event.attempt_id.clone(),
        ),
        event.source_sequence,
    );
    state
        .event_digests
        .insert(event.event_id.clone(), event.payload_digest.clone());
    state.revision = state.revision.saturating_add(1);
    Ok(())
}

fn require_agent<'a>(
    state: &'a mut AgentRegistryState,
    event: &AgentEventEnvelope,
) -> io::Result<&'a mut AgentSummary> {
    let agent = state
        .agents
        .get_mut(&(event.root_thread_id.clone(), event.agent_id.clone()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent does not exist"))?;
    if agent.root_thread_id != event.root_thread_id || agent.thread_id != event.thread_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent event thread identity changed",
        ));
    }
    Ok(agent)
}

fn consume_dead_letter(state: &mut AgentRegistryState, event: &AgentEventEnvelope, reason: &str) {
    let key = (
        event.root_thread_id.clone(),
        event.agent_id.clone(),
        event.attempt_id.clone(),
    );
    let expected = state
        .attempts
        .get(&key)
        .copied()
        .unwrap_or_default()
        .saturating_add(1);
    // A dead letter still consumes its sequence so a later valid frame is not
    // mistaken for a gap. A *forward* gap is acceptable during recovery (the
    // skipped frames were themselves dead-lettered); only a backward/duplicate
    // sequence is refused, since it would rewind the applied position.
    if event.source_sequence < expected {
        return;
    }
    state.attempts.insert(key, event.source_sequence);
    state
        .event_digests
        .insert(event.event_id.clone(), event.payload_digest.clone());
    if let Some(agent) = state
        .agents
        .get_mut(&(event.root_thread_id.clone(), event.agent_id.clone()))
    {
        agent.status = AgentStatus::Corrupt;
        agent.error = Some(reason.to_string());
        agent.updated_at_ms = event.occurred_at_ms;
    }
    state.revision = state.revision.saturating_add(1);
}

/// Acquire the cross-process advisory lock guarding journal appends. The lock
/// file is separate from the journal so the durable content is never truncated
/// by the lock's open mode, and the lock is released when the guard is dropped.
fn acquire_journal_lock(path: &Path) -> io::Result<ExclusiveFileLock> {
    ExclusiveFileLock::acquire(path).map_err(io::Error::other)
}

fn append_dead_letter(path: &Path, line: u64, reason: &str, raw: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "line": line,
            "reason": reason,
            "raw": raw,
        }),
    )?;
    file.write_all(b"\n")?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::agent_event::{AgentEvent, AgentEventEnvelope};

    fn event(sequence: u64, event_id: &str, event: AgentEvent) -> AgentEventEnvelope {
        AgentEventEnvelope::new(
            event_id,
            "root",
            "agent",
            "thread",
            "attempt",
            sequence,
            sequence as i64,
            event,
        )
    }

    #[test]
    fn registry_reduces_one_ordered_event_stream() {
        let registry = AgentRegistry::in_memory();
        registry
            .append(event(
                1,
                "spawn",
                AgentEvent::Spawned {
                    batch_id: "batch".to_string(),
                    batch_size: 1,
                    parent_thread_id: "root".to_string(),
                    description: "inspect".to_string(),
                },
            ))
            .unwrap();
        registry
            .append(event(
                2,
                "done",
                AgentEvent::Completed {
                    result: Some("ok".to_string()),
                    usage: Default::default(),
                },
            ))
            .unwrap();

        let snapshot = registry.snapshot("root");
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.agents[0].status, AgentStatus::Completed);
        assert_eq!(snapshot.agents[0].result.as_deref(), Some("ok"));
    }

    #[test]
    fn poison_record_is_dead_lettered_and_does_not_block_later_frames() {
        let root = tempfile::tempdir().unwrap();
        let journal = root.path().join(AGENT_JOURNAL_FILE);
        let first = event(
            1,
            "spawn",
            AgentEvent::Spawned {
                batch_id: "batch".to_string(),
                batch_size: 1,
                parent_thread_id: "root".to_string(),
                description: "inspect".to_string(),
            },
        );
        let mut poison = event(
            2,
            "poison",
            AgentEvent::Activity {
                activity: orca_core::agent_event::AgentActivity::Thinking,
                turn: Some(1),
                usage: None,
            },
        );
        poison.payload_digest = "invalid".to_string();
        let third = event(
            3,
            "done",
            AgentEvent::Completed {
                result: Some("ok".to_string()),
                usage: Default::default(),
            },
        );
        fs::write(
            &journal,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&poison).unwrap(),
                serde_json::to_string(&third).unwrap()
            ),
        )
        .unwrap();

        let registry = AgentRegistry::open(root.path()).unwrap();

        assert_eq!(
            registry.snapshot("root").agents[0].status,
            AgentStatus::Completed
        );
        assert!(registry.dead_letter_path().unwrap().exists());
    }

    #[test]
    fn unparseable_line_does_not_strand_later_valid_frames() {
        // A line that fails JSON parsing entirely cannot consume a sequence
        // number. Recovery must still apply the following valid frame across
        // that forward gap instead of rejecting it as a sequence gap.
        let root = tempfile::tempdir().unwrap();
        let journal = root.path().join(AGENT_JOURNAL_FILE);
        let first = event(
            1,
            "spawn",
            AgentEvent::Spawned {
                batch_id: "batch".to_string(),
                batch_size: 1,
                parent_thread_id: "root".to_string(),
                description: "inspect".to_string(),
            },
        );
        let third = event(
            3,
            "done",
            AgentEvent::Completed {
                result: Some("ok".to_string()),
                usage: Default::default(),
            },
        );
        fs::write(
            &journal,
            format!(
                "{}\n{{ this is not valid json\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&third).unwrap()
            ),
        )
        .unwrap();

        let registry = AgentRegistry::open(root.path()).unwrap();

        let snapshot = registry.snapshot("root");
        assert_eq!(snapshot.agents[0].status, AgentStatus::Completed);
        assert_eq!(snapshot.agents[0].result.as_deref(), Some("ok"));
        assert!(registry.dead_letter_path().unwrap().exists());
    }

    #[test]
    fn live_append_still_rejects_a_forward_sequence_gap() {
        // The forward-gap tolerance is a recovery-only affordance; a live
        // append that skips a sequence is still a programming error.
        let registry = AgentRegistry::in_memory();
        registry
            .append(event(
                1,
                "spawn",
                AgentEvent::Spawned {
                    batch_id: "batch".to_string(),
                    batch_size: 1,
                    parent_thread_id: "root".to_string(),
                    description: "inspect".to_string(),
                },
            ))
            .unwrap();
        let error = registry
            .append(event(
                3,
                "skips-two",
                AgentEvent::Completed {
                    result: Some("ok".to_string()),
                    usage: Default::default(),
                },
            ))
            .expect_err("live append must reject a forward gap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn concurrent_processes_do_not_interleave_journal_lines() {
        use std::thread;

        // Model two Orca processes by opening two independent registries over
        // the same directory. Each appends large records (well past a single
        // atomic write) for its own agent. The cross-process lock must keep
        // every line intact, so reopening parses cleanly with no dead letters.
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();

        let large = "x".repeat(128 * 1024);
        let writer = |agent: &'static str, blob: String| {
            let root_path = root_path.clone();
            move || {
                let registry = AgentRegistry::open(&root_path).unwrap();
                for sequence in 1..=8u64 {
                    let payload = if sequence == 1 {
                        AgentEvent::Spawned {
                            batch_id: "batch".to_string(),
                            batch_size: 2,
                            parent_thread_id: "root".to_string(),
                            description: blob.clone(),
                        }
                    } else {
                        AgentEvent::OutputDelta { text: blob.clone() }
                    };
                    let envelope = AgentEventEnvelope::new(
                        format!("{agent}-{sequence}"),
                        "root",
                        agent,
                        format!("thread-{agent}"),
                        "attempt",
                        sequence,
                        sequence as i64,
                        payload,
                    );
                    registry.append(envelope).unwrap();
                }
            }
        };

        let first = thread::spawn(writer("agent-a", large.clone()));
        let second = thread::spawn(writer("agent-b", large));
        first.join().unwrap();
        second.join().unwrap();

        // Reopening reduces the journal; any torn line would fail JSON parsing
        // and land in the dead-letter file.
        let reopened = AgentRegistry::open(&root_path).unwrap();
        let snapshot = reopened.snapshot("root");
        assert_eq!(snapshot.agents.len(), 2);
        let dead_letter = reopened.dead_letter_path().unwrap();
        assert!(
            !dead_letter.exists() || fs::read_to_string(dead_letter).unwrap().trim().is_empty(),
            "no journal line should have been torn by interleaved writes"
        );
    }
}
