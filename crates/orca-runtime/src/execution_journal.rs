//! Execution journal: the single append-only source of truth for one
//! operation's terminal and checkpoint facts.
//!
//! Ordered records (`operation.started`, `budget.usage`, `turn.started`,
//! `model.response`, `tool.started`, `tool.completed`, `checkpoint.created`,
//! `operation.terminal`) are flushed atomically: either every pending record
//! is durable or none is. On reopen only committed records exist, so an
//! unflushed `tool.started` is never replayed; callers restore it as
//! `indeterminate`. Projections (JSONL stream, saved transcript, TUI) must
//! read committed records and never invent terminal facts.
//!
//! Ordering invariants enforced on append:
//! - `checkpoint.created` requires every open `tool.started` to already have a
//!   committed `tool.completed`.
//! - `operation.terminal` (any variant) requires a committed checkpoint when
//!   the terminal is `Stopped`, and may be appended only once.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use orca_core::budget::{BudgetSpec, BudgetUsage, OperationTerminal, StopReason};
use serde::{Deserialize, Serialize};

pub const JOURNAL_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalRecordKind {
    OperationStarted,
    BudgetUsage,
    TurnStarted,
    ModelResponse,
    ToolStarted,
    ToolCompleted,
    CheckpointCreated,
    OperationTerminal,
}

impl JournalRecordKind {
    pub fn as_str_for_test(&self) -> &'static str {
        match self {
            Self::OperationStarted => "operation.started",
            Self::BudgetUsage => "budget.usage",
            Self::TurnStarted => "turn.started",
            Self::ModelResponse => "model.response",
            Self::ToolStarted => "tool.started",
            Self::ToolCompleted => "tool.completed",
            Self::CheckpointCreated => "checkpoint.created",
            Self::OperationTerminal => "operation.terminal",
        }
    }
}

/// Test-only accessors for contract assertions.
impl JournalRecord {
    pub fn schema_version_for_test(&self) -> u32 {
        match self {
            Self::OperationStarted { schema_version, .. }
            | Self::BudgetUsage { schema_version, .. }
            | Self::TurnStarted { schema_version, .. }
            | Self::ModelResponse { schema_version, .. }
            | Self::ToolStarted { schema_version, .. }
            | Self::ToolCompleted { schema_version, .. }
            | Self::CheckpointCreated { schema_version, .. }
            | Self::OperationTerminal { schema_version, .. } => *schema_version,
        }
    }

    pub fn tool_call_id_for_test(&self) -> Option<&str> {
        match self {
            Self::ToolStarted { tool_call_id, .. } | Self::ToolCompleted { tool_call_id, .. } => {
                Some(tool_call_id)
            }
            _ => None,
        }
    }

    pub fn checkpoint_id_for_test(&self) -> Option<&str> {
        match self {
            Self::CheckpointCreated { checkpoint_id, .. } => Some(checkpoint_id),
            _ => None,
        }
    }
}

/// One ordered journal record. Every record carries `operation_id`, the
/// owning `turn_id` (empty for operation-level records), a monotonically
/// increasing ordinal, and the schema version.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    OperationStarted {
        operation_id: String,
        #[serde(default)]
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        started_at_ms: u64,
        budget_spec: BudgetSpec,
    },
    /// Monotonic cumulative operation usage at a durable accounting
    /// boundary. Recovery uses the newest committed snapshot; deltas are
    /// never replayed, so retrying a settlement cannot double charge.
    BudgetUsage {
        operation_id: String,
        #[serde(default)]
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        usage: BudgetUsage,
        recorded_at_ms: u64,
        #[serde(default)]
        accounting_id: Option<String>,
    },
    TurnStarted {
        operation_id: String,
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
    },
    ModelResponse {
        operation_id: String,
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        response_id: String,
        final_message: Option<String>,
    },
    ToolStarted {
        operation_id: String,
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        tool_call_id: String,
        tool_name: String,
    },
    ToolCompleted {
        operation_id: String,
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        tool_call_id: String,
        status: String,
        error: Option<String>,
    },
    CheckpointCreated {
        operation_id: String,
        #[serde(default)]
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        checkpoint_id: String,
        message_id: Option<String>,
    },
    OperationTerminal {
        operation_id: String,
        #[serde(default)]
        turn_id: String,
        ordinal: u64,
        schema_version: u32,
        terminal: OperationTerminal,
    },
}

impl JournalRecord {
    pub fn kind(&self) -> JournalRecordKind {
        match self {
            Self::OperationStarted { .. } => JournalRecordKind::OperationStarted,
            Self::BudgetUsage { .. } => JournalRecordKind::BudgetUsage,
            Self::TurnStarted { .. } => JournalRecordKind::TurnStarted,
            Self::ModelResponse { .. } => JournalRecordKind::ModelResponse,
            Self::ToolStarted { .. } => JournalRecordKind::ToolStarted,
            Self::ToolCompleted { .. } => JournalRecordKind::ToolCompleted,
            Self::CheckpointCreated { .. } => JournalRecordKind::CheckpointCreated,
            Self::OperationTerminal { .. } => JournalRecordKind::OperationTerminal,
        }
    }

    pub fn ordinal(&self) -> u64 {
        match self {
            Self::OperationStarted { ordinal, .. }
            | Self::BudgetUsage { ordinal, .. }
            | Self::TurnStarted { ordinal, .. }
            | Self::ModelResponse { ordinal, .. }
            | Self::ToolStarted { ordinal, .. }
            | Self::ToolCompleted { ordinal, .. }
            | Self::CheckpointCreated { ordinal, .. }
            | Self::OperationTerminal { ordinal, .. } => *ordinal,
        }
    }

    pub fn operation_id(&self) -> &str {
        match self {
            Self::OperationStarted { operation_id, .. }
            | Self::BudgetUsage { operation_id, .. }
            | Self::TurnStarted { operation_id, .. }
            | Self::ModelResponse { operation_id, .. }
            | Self::ToolStarted { operation_id, .. }
            | Self::ToolCompleted { operation_id, .. }
            | Self::CheckpointCreated { operation_id, .. }
            | Self::OperationTerminal { operation_id, .. } => operation_id,
        }
    }

    pub fn turn_id(&self) -> &str {
        match self {
            Self::OperationStarted { turn_id, .. }
            | Self::BudgetUsage { turn_id, .. }
            | Self::TurnStarted { turn_id, .. }
            | Self::ModelResponse { turn_id, .. }
            | Self::ToolStarted { turn_id, .. }
            | Self::ToolCompleted { turn_id, .. }
            | Self::CheckpointCreated { turn_id, .. }
            | Self::OperationTerminal { turn_id, .. } => turn_id,
        }
    }
}

/// Test-only fault injection for proving durability ordering.
#[derive(Clone, Debug, Default)]
pub struct JournalFaults {
    /// When set, the next `flush` fails after writing no bytes. Armed faults
    /// are consumed by one flush attempt.
    pub fail_next_flush: bool,
    /// When set, `append` of `operation.terminal` is rejected even when a
    /// checkpoint exists (simulates an adapter publishing before flush).
    pub reject_terminal_without_checkpoint: bool,
}

/// The append-only journal for one operation.
pub struct ExecutionJournal {
    path: PathBuf,
    operation_id: String,
    committed: Vec<JournalRecord>,
    pending: Vec<JournalRecord>,
    next_ordinal: u64,
    faults: JournalFaults,
    terminal_appended: bool,
}

impl ExecutionJournal {
    /// Opens (or creates) the journal file and loads committed records.
    ///
    /// The file is repaired first: a crash mid-write may have left a partial
    /// final line, which must be truncated before parsing so reopening never
    /// fails on a torn record.
    pub fn open(path: PathBuf, operation_id: impl Into<String>) -> io::Result<Self> {
        let operation_id = operation_id.into();
        let mut journal = Self {
            path,
            operation_id,
            committed: Vec::new(),
            pending: Vec::new(),
            next_ordinal: 1,
            faults: JournalFaults::default(),
            terminal_appended: false,
        };
        journal.repair_and_reload()?;
        Ok(journal)
    }

    pub fn with_faults(mut self, faults: JournalFaults) -> Self {
        self.faults = faults;
        self
    }

    /// Arms test-only faults on an already-open journal.
    pub fn set_faults(&mut self, faults: JournalFaults) {
        self.faults = faults;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Committed records in ordinal order (durable facts only).
    pub fn committed(&self) -> &[JournalRecord] {
        &self.committed
    }

    /// Pending records not yet flushed.
    pub fn pending(&self) -> &[JournalRecord] {
        &self.pending
    }

    /// True when an `operation.terminal` record is committed.
    pub fn has_terminal(&self) -> bool {
        self.terminal_appended
    }

    /// The committed terminal, when present.
    pub fn terminal(&self) -> Option<&OperationTerminal> {
        self.committed.iter().rev().find_map(|record| match record {
            JournalRecord::OperationTerminal { terminal, .. } => Some(terminal),
            _ => None,
        })
    }

    /// Newest committed cumulative budget usage, if any.
    pub fn latest_budget_usage(&self) -> Option<BudgetUsage> {
        self.committed.iter().rev().find_map(|record| match record {
            JournalRecord::BudgetUsage { usage, .. } => Some(*usage),
            _ => None,
        })
    }

    /// Original wall-clock start timestamp. Wall time is an operation
    /// deadline, so time spent suspended or with the process down counts.
    pub fn operation_started_at_ms(&self) -> Option<u64> {
        self.committed.iter().find_map(|record| match record {
            JournalRecord::OperationStarted { started_at_ms, .. } => Some(*started_at_ms),
            _ => None,
        })
    }

    pub fn operation_budget_spec(&self) -> Option<BudgetSpec> {
        self.committed.iter().find_map(|record| match record {
            JournalRecord::OperationStarted { budget_spec, .. } => Some(*budget_spec),
            _ => None,
        })
    }

    pub fn has_budget_accounting_id(&self, accounting_id: &str) -> bool {
        self.committed.iter().any(|record| {
            matches!(
                record,
                JournalRecord::BudgetUsage {
                    accounting_id: Some(existing),
                    ..
                } if existing == accounting_id
            )
        })
    }

    /// Committed `tool.started` records without a matching committed
    /// `tool.completed`. These must be restored as `indeterminate` and never
    /// replayed as completed (their external effects are unknown).
    pub fn unmatched_tool_starts(&self) -> Vec<&JournalRecord> {
        let mut started: Vec<&JournalRecord> = Vec::new();
        for record in &self.committed {
            match record {
                JournalRecord::ToolStarted { .. } => started.push(record),
                JournalRecord::ToolCompleted { tool_call_id, .. } => {
                    started.retain(|record| match record {
                        JournalRecord::ToolStarted {
                            tool_call_id: id, ..
                        } => id != tool_call_id,
                        _ => true,
                    });
                }
                _ => {}
            }
        }
        started
    }

    /// Last committed checkpoint record, newest first.
    pub fn last_checkpoint(&self) -> Option<&JournalRecord> {
        self.committed.iter().rev().find_map(|record| {
            matches!(record, JournalRecord::CheckpointCreated { .. }).then_some(record)
        })
    }

    pub fn append(&mut self, record: JournalRecord) -> Result<(), String> {
        if record.schema_version_for_test() != JOURNAL_SCHEMA_VERSION {
            return Err(format!(
                "journal record schema {} is unsupported; expected {}",
                record.schema_version_for_test(),
                JOURNAL_SCHEMA_VERSION
            ));
        }
        if record.operation_id() != self.operation_id {
            return Err(format!(
                "journal record operation {} does not match journal operation {}",
                record.operation_id(),
                self.operation_id
            ));
        }
        self.validate_ordering(&record)?;
        // The journal owns ordinal assignment: builders may pre-create a batch
        // of records, and each append stamps the next ordinal so a pre-built
        // batch never produces duplicate ordinals.
        let ordinal = self.next_ordinal;
        self.next_ordinal = ordinal.saturating_add(1);
        self.pending.push(stamp_ordinal(record, ordinal));
        Ok(())
    }

    /// Appends and immediately flushes (durable before returning).
    pub fn append_durable(&mut self, record: JournalRecord) -> io::Result<()> {
        self.append(record).map_err(io::Error::other)?;
        self.flush()
    }

    /// Atomically flushes all pending records: either every record is durable
    /// or none is. With an armed `fail_next_flush` fault the flush fails
    /// before writing anything and the pending records stay pending.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.faults.fail_next_flush {
            self.faults.fail_next_flush = false;
            return Err(io::Error::other("injected flush failure"));
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write all pending lines with one open handle, then sync. A crash
        // before sync leaves the file without these records; a crash after
        // sync but before the method returns is indistinguishable from
        // success, which is the durability contract.
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.path)?;
        repair_incomplete_final_line(&mut file)?;
        for record in &self.pending {
            let mut line = serde_json::to_vec(record)
                .map_err(|error| io::Error::other(format!("journal serialization: {error}")))?;
            line.push(b'\n');
            file.write_all(&line)?;
        }
        file.flush()?;
        file.sync_data()?;
        let flushed = std::mem::take(&mut self.pending);
        self.committed.extend(flushed);
        self.terminal_appended = self
            .committed
            .iter()
            .any(|record| matches!(record, JournalRecord::OperationTerminal { .. }));
        Ok(())
    }

    /// Emits committed records as JSONL lines through `writer`.
    pub fn write_projection(&self, mut writer: impl Write) -> io::Result<()> {
        for record in &self.committed {
            serde_json::to_writer(&mut writer, record)
                .map_err(|error| io::Error::other(format!("journal projection: {error}")))?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }

    fn validate_ordering(&self, record: &JournalRecord) -> Result<(), String> {
        let existing = self.committed.iter().chain(self.pending.iter());
        let has_started = existing
            .clone()
            .any(|record| matches!(record, JournalRecord::OperationStarted { .. }));
        if !matches!(record, JournalRecord::OperationStarted { .. }) && !has_started {
            return Err("operation.started must precede every journal record".to_string());
        }
        if self.terminal_appended
            || existing
                .clone()
                .any(|record| matches!(record, JournalRecord::OperationTerminal { .. }))
        {
            return Err(
                "operation.terminal may be appended only once and must be the final journal record"
                    .to_string(),
            );
        }
        match record {
            JournalRecord::OperationStarted { .. } => {
                if !self.committed.is_empty() || !self.pending.is_empty() {
                    return Err("operation.started must be the first journal record".to_string());
                }
            }
            JournalRecord::BudgetUsage {
                usage,
                accounting_id,
                ..
            } => {
                if let Some(previous) = self
                    .committed
                    .iter()
                    .chain(self.pending.iter())
                    .rev()
                    .find_map(|record| match record {
                        JournalRecord::BudgetUsage { usage, .. } => Some(*usage),
                        _ => None,
                    })
                    && (usage.turns < previous.turns
                        || usage.tool_calls < previous.tool_calls
                        || usage.cost_usd_micros < previous.cost_usd_micros
                        || usage.wall_time_ms < previous.wall_time_ms)
                {
                    return Err("budget.usage must be cumulative and monotonic".to_string());
                }
                if let Some(accounting_id) = accounting_id
                    && self
                        .committed
                        .iter()
                        .chain(self.pending.iter())
                        .any(|record| {
                            matches!(
                                record,
                                JournalRecord::BudgetUsage {
                                    accounting_id: Some(existing),
                                    ..
                                } if existing == accounting_id
                            )
                        })
                {
                    return Err(format!(
                        "budget accounting id {accounting_id} may be committed only once"
                    ));
                }
            }
            JournalRecord::ToolCompleted { tool_call_id, .. } => {
                if !self
                    .unmatched_tool_starts_including_pending()
                    .iter()
                    .any(|started| {
                        matches!(
                            started,
                            JournalRecord::ToolStarted {
                                tool_call_id: started_id,
                                ..
                            } if started_id == tool_call_id
                        )
                    })
                {
                    return Err(format!(
                        "tool.completed {tool_call_id} requires an unmatched tool.started"
                    ));
                }
            }
            JournalRecord::CheckpointCreated { .. } => {
                if !self.unmatched_tool_starts_including_pending().is_empty() {
                    return Err(
                        "checkpoint.created requires every open tool.started to have a committed tool.completed"
                            .to_string(),
                    );
                }
            }
            JournalRecord::OperationTerminal { terminal, .. } => {
                if let OperationTerminal::Stopped { checkpoint_id, .. } = terminal {
                    let checkpoint_matches = self.committed.iter().any(|record| {
                        matches!(
                            record,
                            JournalRecord::CheckpointCreated {
                                checkpoint_id: committed_id,
                                ..
                            } if committed_id == checkpoint_id
                        )
                    });
                    if !checkpoint_matches {
                        return Err(
                            "stopped operation.terminal requires its checkpoint.created to be committed"
                                .to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn unmatched_tool_starts_including_pending(&self) -> Vec<&JournalRecord> {
        let mut started = Vec::new();
        for record in self.committed.iter().chain(self.pending.iter()) {
            match record {
                JournalRecord::ToolStarted { .. } => started.push(record),
                JournalRecord::ToolCompleted { tool_call_id, .. } => {
                    started.retain(|record| {
                        !matches!(
                            record,
                            JournalRecord::ToolStarted {
                                tool_call_id: started_id,
                                ..
                            } if started_id == tool_call_id
                        )
                    });
                }
                _ => {}
            }
        }
        started
    }

    fn repair_and_reload(&mut self) -> io::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        // A crash mid-write can leave a torn final line; truncate it to the
        // last complete newline before parsing committed records.
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.path)?;
        repair_incomplete_final_line(&mut file)?;
        drop(file);
        self.reload_committed()
    }

    fn reload_committed(&mut self) -> io::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut expected_ordinal = 1_u64;
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("journal record {index} invalid: {error}"),
                )
            })?;
            let schema_version = value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("journal record {index} has no integer schema_version"),
                    )
                })?;
            if schema_version != u64::from(JOURNAL_SCHEMA_VERSION) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "journal record {index} uses unsupported schema {schema_version}; expected {JOURNAL_SCHEMA_VERSION}"
                    ),
                ));
            }
            let record: JournalRecord = serde_json::from_value(value).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("journal record {index} invalid: {error}"),
                )
            })?;
            if record.ordinal() != expected_ordinal {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "journal record {index} has ordinal {}; expected {expected_ordinal}",
                        record.ordinal()
                    ),
                ));
            }
            if record.operation_id() != self.operation_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "journal record {index} belongs to operation {} not {}",
                        record.operation_id(),
                        self.operation_id
                    ),
                ));
            }
            self.validate_ordering(&record).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("journal record {index} violates ordering: {error}"),
                )
            })?;
            let terminal = matches!(record, JournalRecord::OperationTerminal { .. });
            self.committed.push(record);
            self.terminal_appended = terminal;
            expected_ordinal = expected_ordinal.saturating_add(1);
        }
        self.next_ordinal = expected_ordinal;
        Ok(())
    }
}

/// Truncates a torn final line so only complete newline-terminated records
/// survive. Scans back to the last `\n`; if the file does not end with a
/// newline, everything after the previous newline is discarded (or the whole
/// file when no complete record exists).
fn repair_incomplete_final_line(file: &mut File) -> io::Result<()> {
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(());
    }
    // Find the last newline by scanning backwards in chunks.
    const CHUNK: u64 = 4096;
    let mut offset = file_len;
    loop {
        let start = offset.saturating_sub(CHUNK);
        let len = offset - start;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0_u8; len as usize];
        file.read_exact(&mut buf)?;
        if let Some(relative) = buf.iter().rposition(|byte| *byte == b'\n') {
            let keep_through = start + relative as u64;
            let new_len = if keep_through + 1 == file_len {
                file_len
            } else {
                keep_through + 1
            };
            if new_len != file_len {
                file.set_len(new_len)?;
            }
            file.seek(SeekFrom::End(0))?;
            return Ok(());
        }
        if start == 0 {
            // No newline anywhere: the file is one torn line (or garbage).
            file.set_len(0)?;
            file.seek(SeekFrom::End(0))?;
            return Ok(());
        }
        offset = start;
    }
}

/// Replaces the ordinal on a record with the journal-assigned value.
fn stamp_ordinal(record: JournalRecord, ordinal: u64) -> JournalRecord {
    match record {
        JournalRecord::OperationStarted {
            operation_id,
            turn_id,
            schema_version,
            started_at_ms,
            budget_spec,
            ..
        } => JournalRecord::OperationStarted {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            started_at_ms,
            budget_spec,
        },
        JournalRecord::BudgetUsage {
            operation_id,
            turn_id,
            schema_version,
            usage,
            recorded_at_ms,
            accounting_id,
            ..
        } => JournalRecord::BudgetUsage {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            usage,
            recorded_at_ms,
            accounting_id,
        },
        JournalRecord::TurnStarted {
            operation_id,
            turn_id,
            schema_version,
            ..
        } => JournalRecord::TurnStarted {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
        },
        JournalRecord::ModelResponse {
            operation_id,
            turn_id,
            schema_version,
            response_id,
            final_message,
            ..
        } => JournalRecord::ModelResponse {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            response_id,
            final_message,
        },
        JournalRecord::ToolStarted {
            operation_id,
            turn_id,
            schema_version,
            tool_call_id,
            tool_name,
            ..
        } => JournalRecord::ToolStarted {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            tool_call_id,
            tool_name,
        },
        JournalRecord::ToolCompleted {
            operation_id,
            turn_id,
            schema_version,
            tool_call_id,
            status,
            error,
            ..
        } => JournalRecord::ToolCompleted {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            tool_call_id,
            status,
            error,
        },
        JournalRecord::CheckpointCreated {
            operation_id,
            turn_id,
            schema_version,
            checkpoint_id,
            message_id,
            ..
        } => JournalRecord::CheckpointCreated {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            checkpoint_id,
            message_id,
        },
        JournalRecord::OperationTerminal {
            operation_id,
            turn_id,
            schema_version,
            terminal,
            ..
        } => JournalRecord::OperationTerminal {
            operation_id,
            turn_id,
            ordinal,
            schema_version,
            terminal,
        },
    }
}

/// Convenience builders used by the agent loop.
impl ExecutionJournal {
    pub fn record_operation_started(&self, started_at_ms: u64) -> JournalRecord {
        JournalRecord::OperationStarted {
            operation_id: self.operation_id.clone(),
            turn_id: String::new(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            started_at_ms,
            budget_spec: BudgetSpec::default(),
        }
    }

    pub fn record_turn_started(&self, turn_id: &str) -> JournalRecord {
        JournalRecord::TurnStarted {
            operation_id: self.operation_id.clone(),
            turn_id: turn_id.to_string(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
        }
    }

    pub fn record_model_response(
        &self,
        turn_id: &str,
        response_id: &str,
        final_message: Option<String>,
    ) -> JournalRecord {
        JournalRecord::ModelResponse {
            operation_id: self.operation_id.clone(),
            turn_id: turn_id.to_string(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            response_id: response_id.to_string(),
            final_message,
        }
    }

    pub fn record_tool_started(
        &self,
        turn_id: &str,
        tool_call_id: &str,
        tool_name: &str,
    ) -> JournalRecord {
        JournalRecord::ToolStarted {
            operation_id: self.operation_id.clone(),
            turn_id: turn_id.to_string(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
        }
    }

    pub fn record_tool_completed(
        &self,
        turn_id: &str,
        tool_call_id: &str,
        status: &str,
        error: Option<String>,
    ) -> JournalRecord {
        JournalRecord::ToolCompleted {
            operation_id: self.operation_id.clone(),
            turn_id: turn_id.to_string(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            tool_call_id: tool_call_id.to_string(),
            status: status.to_string(),
            error,
        }
    }

    pub fn record_checkpoint(
        &self,
        checkpoint_id: &str,
        message_id: Option<String>,
    ) -> JournalRecord {
        JournalRecord::CheckpointCreated {
            operation_id: self.operation_id.clone(),
            turn_id: String::new(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            checkpoint_id: checkpoint_id.to_string(),
            message_id,
        }
    }

    pub fn record_terminal(&self, terminal: OperationTerminal) -> JournalRecord {
        JournalRecord::OperationTerminal {
            operation_id: self.operation_id.clone(),
            turn_id: String::new(),
            ordinal: self.next_ordinal,
            schema_version: JOURNAL_SCHEMA_VERSION,
            terminal,
        }
    }
}

/// Free-standing helpers for building terminals from controller state.
pub fn completed_terminal(usage: BudgetUsage) -> OperationTerminal {
    OperationTerminal::Completed { usage }
}

pub fn stopped_terminal(
    reason: StopReason,
    usage: BudgetUsage,
    checkpoint_id: impl Into<String>,
    resumable: bool,
) -> OperationTerminal {
    OperationTerminal::Stopped {
        reason,
        usage,
        checkpoint_id: checkpoint_id.into(),
        resumable,
    }
}
