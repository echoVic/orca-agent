use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_platform::PlatformError;
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lifecycle::RuntimeWorkflowIpc;

#[derive(Clone)]
pub(crate) struct WorkflowIpcContext {
    mailbox: Arc<WorkflowMailbox>,
    task_lists: Arc<WorkflowTaskLists>,
    sender: String,
    child_claim: Option<Arc<WorkflowChildClaim>>,
}

impl std::fmt::Debug for WorkflowIpcContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowIpcContext")
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

struct WorkflowChildClaim {
    id: String,
    call_path: String,
    task_lists: Arc<WorkflowTaskLists>,
}

impl Drop for WorkflowChildClaim {
    fn drop(&mut self) {
        if let Err(error) = self.task_lists.release_claims(Some(&self.id)) {
            tracing::warn!(%error, "could not release workflow child claims");
        }
    }
}

impl WorkflowIpcContext {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            task_lists: Arc::new(WorkflowTaskLists::default()),
            sender: "workflow-agent".to_string(),
            child_claim: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_durable_mailbox(mailbox_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::durable(mailbox_path)?),
            task_lists: Arc::new(WorkflowTaskLists::default()),
            sender: "workflow-agent".to_string(),
            child_claim: None,
        })
    }

    pub(crate) fn new_durable_task_lists(task_lists_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            task_lists: Arc::new(WorkflowTaskLists::durable(task_lists_path)?),
            sender: "workflow-agent".to_string(),
            child_claim: None,
        })
    }

    pub(crate) fn new_durable(mailbox_path: PathBuf, task_lists_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::durable(mailbox_path)?),
            task_lists: Arc::new(WorkflowTaskLists::durable(task_lists_path)?),
            sender: "workflow-agent".to_string(),
            child_claim: None,
        })
    }

    pub(crate) fn for_sender(&self, sender: impl Into<String>) -> Self {
        let sender = sender.into();
        Self {
            mailbox: Arc::clone(&self.mailbox),
            task_lists: Arc::clone(&self.task_lists),
            child_claim: Some(Arc::new(WorkflowChildClaim {
                id: uuid::Uuid::new_v4().to_string(),
                call_path: self
                    .child_claim
                    .as_ref()
                    .map_or_else(|| sender.clone(), |claim| claim.call_path.clone()),
                task_lists: Arc::clone(&self.task_lists),
            })),
            sender,
        }
    }

    pub(crate) fn for_nested_attempt(&self) -> Self {
        self.for_sender(self.sender.clone())
    }

    pub(crate) fn reconcile_verified_claims(
        path: &Path,
        open: impl FnOnce() -> io::Result<fs::File>,
    ) -> io::Result<()> {
        let read = || read_verified_task_list_state(open()?);
        match WorkflowTaskLists::durable_with(path.to_owned(), read) {
            Ok(store) => store.release_claims(None).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn cached_agent_can_be_reused(
        file: io::Result<fs::File>,
        call_path: &str,
    ) -> io::Result<bool> {
        match file.and_then(read_verified_task_list_state) {
            Ok(state) => Ok(state
                .mutating_calls
                .is_some_and(|calls| !calls.contains(call_path))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn default_sender(&self) -> &str {
        &self.sender
    }

    pub(crate) fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String> {
        self.mailbox
            .send_message(channel, from.unwrap_or(self.default_sender()), message)
    }

    pub(crate) fn read_messages(&self, channel: &str) -> Result<Value, String> {
        self.mailbox.read_messages(channel)
    }

    pub(crate) fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        self.mailbox.clear_messages(channel)
    }

    pub(crate) fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        self.task_lists
            .create_task_list(name, items, self.child_claim.as_deref())
    }

    pub(crate) fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String> {
        self.task_lists.claim_task(
            name,
            by.unwrap_or(self.default_sender()),
            self.child_claim.as_deref(),
        )
    }

    pub(crate) fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        self.task_lists.complete_task(
            name,
            task_id,
            result,
            by.unwrap_or(self.default_sender()),
            self.child_claim.as_deref(),
        )
    }

    pub(crate) fn release_child_claims(&self) -> Result<(), String> {
        if let Some(claim) = &self.child_claim {
            self.task_lists.release_claims(Some(&claim.id))?;
        }
        Ok(())
    }

    pub(crate) fn list_tasks(&self, name: &str) -> Result<Value, String> {
        self.task_lists.list_tasks(name)
    }
}

impl RuntimeWorkflowIpc for WorkflowIpcContext {
    fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String> {
        self.send_message(channel, from, message)
    }

    fn read_messages(&self, channel: &str) -> Result<Value, String> {
        self.read_messages(channel)
    }

    fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        self.clear_messages(channel)
    }

    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        self.create_task_list(name, items)
    }

    fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String> {
        self.claim_task(name, by)
    }

    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        self.complete_task(name, task_id, result, by)
    }

    fn list_tasks(&self, name: &str) -> Result<Value, String> {
        self.list_tasks(name)
    }
}

#[derive(Debug)]
struct WorkflowMailbox {
    state: Mutex<WorkflowMailboxState>,
    durable_path: Option<PathBuf>,
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowMailboxState {
    next_seq: u64,
    channels: HashMap<String, Vec<Value>>,
}

impl Default for WorkflowMailbox {
    fn default() -> Self {
        Self {
            state: Mutex::new(WorkflowMailboxState::default()),
            durable_path: None,
        }
    }
}

impl WorkflowMailbox {
    fn durable(path: PathBuf) -> io::Result<Self> {
        let state = if path.exists() {
            read_mailbox_state(&path)?
        } else {
            WorkflowMailboxState::default()
        };
        Ok(Self {
            state: Mutex::new(state),
            durable_path: Some(path),
        })
    }

    fn send_message(&self, channel: &str, from: &str, message: Value) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let from = normalize_sender(from);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        state.next_seq += 1;
        let entry = json!({
            "seq": state.next_seq,
            "channel": channel,
            "from": from,
            "message": message,
        });
        state
            .channels
            .entry(channel)
            .or_default()
            .push(entry.clone());
        self.persist_locked_state(&state)?;
        Ok(entry)
    }

    fn read_messages(&self, channel: &str) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        Ok(Value::Array(
            state.channels.get(&channel).cloned().unwrap_or_default(),
        ))
    }

    fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        let cleared = state
            .channels
            .remove(&channel)
            .map(|items| items.len())
            .unwrap_or(0);
        self.persist_locked_state(&state)?;
        Ok(json!({ "cleared": cleared }))
    }

    fn persist_locked_state(&self, state: &WorkflowMailboxState) -> Result<(), String> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        write_mailbox_state(path, state).map_err(|error| {
            format!(
                "failed to persist workflow mailbox {}: {error}",
                path.display()
            )
        })
    }
}

fn read_mailbox_state(path: &Path) -> io::Result<WorkflowMailboxState> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_mailbox_state(path: &Path, state: &WorkflowMailboxState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write(path, content.as_bytes(), AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

fn normalize_channel(channel: &str) -> Result<String, String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return Err("workflow mailbox channel must be a non-empty string".to_string());
    }
    Ok(channel.to_string())
}

fn normalize_sender(sender: &str) -> String {
    let sender = sender.trim();
    if sender.is_empty() {
        "workflow-agent".to_string()
    } else {
        sender.to_string()
    }
}

struct WorkflowTaskLists {
    state: Mutex<WorkflowTaskListState>,
    durable_path: Option<PathBuf>,
    _owner: Option<ExclusiveFileLock>,
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTaskListState {
    next_task_seq: u64,
    lists: HashMap<String, Vec<WorkflowTaskItem>>,
    // Older task lists cannot identify which calls wrote them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mutating_calls: Option<HashSet<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTaskItem {
    id: String,
    status: WorkflowTaskStatus,
    value: Value,
    claimed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_claimant: Option<String>,
    completed_by: Option<String>,
    result: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkflowTaskStatus {
    Pending,
    Running,
    Completed,
}

impl Default for WorkflowTaskLists {
    fn default() -> Self {
        Self {
            state: Mutex::new(WorkflowTaskListState {
                mutating_calls: Some(HashSet::new()),
                ..Default::default()
            }),
            durable_path: None,
            _owner: None,
        }
    }
}

impl WorkflowTaskLists {
    fn durable(path: PathBuf) -> io::Result<Self> {
        let read_path = path.clone();
        Self::durable_with(path, || read_task_list_state(&read_path))
    }

    fn durable_with(
        path: PathBuf,
        read: impl FnOnce() -> io::Result<WorkflowTaskListState>,
    ) -> io::Result<Self> {
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let owner = ExclusiveFileLock::try_acquire(Path::new(&lock_path)).map_err(|error| {
            let kind = match error {
                PlatformError::LockContended { .. } => io::ErrorKind::WouldBlock,
                PlatformError::Io { kind, .. } => kind,
                _ => io::ErrorKind::Other,
            };
            io::Error::new(kind, error)
        })?;
        let mut store = Self::default();
        match read() {
            Ok(state) => store.state = Mutex::new(state),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        store.durable_path = Some(path);
        store._owner = Some(owner);
        Ok(store)
    }

    fn create_task_list(
        &self,
        name: &str,
        items: Vec<Value>,
        child: Option<&WorkflowChildClaim>,
    ) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        self.update(child, |state| {
            let tasks = items
                .into_iter()
                .map(|value| {
                    state.next_task_seq += 1;
                    WorkflowTaskItem {
                        id: format!("workflow-task-{}", state.next_task_seq),
                        status: WorkflowTaskStatus::Pending,
                        value,
                        claimed_by: None,
                        child_claimant: None,
                        completed_by: None,
                        result: Value::Null,
                    }
                })
                .collect::<Vec<_>>();
            let value = tasks_to_value(&tasks);
            state.lists.insert(name, tasks);
            Ok(Some(value))
        })
    }

    fn claim_task(
        &self,
        name: &str,
        by: &str,
        child: Option<&WorkflowChildClaim>,
    ) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let by = normalize_task_worker(by);
        self.update(child, |state| {
            let Some(task) = state.lists.get_mut(&name).and_then(|tasks| {
                tasks
                    .iter_mut()
                    .find(|task| task.status == WorkflowTaskStatus::Pending)
            }) else {
                return Ok(None);
            };
            task.status = WorkflowTaskStatus::Running;
            task.claimed_by = Some(by);
            task.child_claimant = child.map(|claim| claim.id.clone());
            Ok(Some(task_to_value(task)))
        })
    }

    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: &str,
        child: Option<&WorkflowChildClaim>,
    ) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let task_id = normalize_task_id(task_id)?;
        let by = normalize_task_worker(by);
        self.update(child, |state| {
            let Some(task) = state
                .lists
                .get_mut(&name)
                .and_then(|tasks| tasks.iter_mut().find(|task| task.id == task_id))
            else {
                return Err(format!("workflow task not found: {task_id}"));
            };
            if task.status != WorkflowTaskStatus::Running {
                return Err(format!(
                    "workflow task {task_id} must be running to complete it"
                ));
            }
            let is_claimant = match (task.child_claimant.as_deref(), child) {
                (None, None) => task.claimed_by.as_deref() == Some(by.as_str()),
                (Some(claimant), Some(child)) => claimant == child.id,
                _ => false,
            };
            if !is_claimant {
                return Err(format!(
                    "only the claimant can complete workflow task {task_id}"
                ));
            }
            task.status = WorkflowTaskStatus::Completed;
            task.child_claimant = None;
            task.completed_by = Some(by);
            task.result = result;
            Ok(Some(task_to_value(task)))
        })
    }

    fn release_claims(&self, child: Option<&str>) -> Result<(), String> {
        self.update(None, |state| {
            let mut released = false;
            for task in state.lists.values_mut().flatten() {
                if task.status == WorkflowTaskStatus::Running
                    && child.is_none_or(|child| task.child_claimant.as_deref() == Some(child))
                {
                    task.status = WorkflowTaskStatus::Pending;
                    task.claimed_by = None;
                    task.child_claimant = None;
                    released = true;
                }
            }
            Ok(released.then_some(()))
        })
    }

    fn list_tasks(&self, name: &str) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        Ok(tasks_to_value(
            state.lists.get(&name).map(Vec::as_slice).unwrap_or(&[]),
        ))
    }

    fn update<T: Default>(
        &self,
        child: Option<&WorkflowChildClaim>,
        edit: impl FnOnce(&mut WorkflowTaskListState) -> Result<Option<T>, String>,
    ) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        let mut updated = state.clone();
        let Some(result) = edit(&mut updated)? else {
            return Ok(T::default());
        };
        if let Some(child) = child
            && let Some(calls) = &mut updated.mutating_calls
        {
            calls.insert(child.call_path.clone());
        }
        self.persist_locked_state(&updated)?;
        *state = updated;
        Ok(result)
    }

    fn persist_locked_state(&self, state: &WorkflowTaskListState) -> Result<(), String> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        write_task_list_state(path, state).map_err(|error| {
            format!(
                "failed to persist workflow task lists {}: {error}",
                path.display()
            )
        })
    }
}

fn read_verified_task_list_state(file: fs::File) -> io::Result<WorkflowTaskListState> {
    serde_json::from_reader(BufReader::new(file))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_task_list_state(path: &Path) -> io::Result<WorkflowTaskListState> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_task_list_state(path: &Path, state: &WorkflowTaskListState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write(path, content.as_bytes(), AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

fn tasks_to_value(tasks: &[WorkflowTaskItem]) -> Value {
    Value::Array(tasks.iter().map(task_to_value).collect())
}

fn task_to_value(task: &WorkflowTaskItem) -> Value {
    json!({
        "id": task.id,
        "status": task.status.as_str(),
        "value": task.value,
        "claimedBy": task.claimed_by,
        "completedBy": task.completed_by,
        "result": task.result,
    })
}

impl WorkflowTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
        }
    }
}

fn normalize_task_list_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workflow task list name must be a non-empty string".to_string());
    }
    Ok(name.to_string())
}

fn normalize_task_id(task_id: &str) -> Result<String, String> {
    let task_id = task_id.trim();
    if task_id.is_empty() {
        return Err("workflow task id must be a non-empty string".to_string());
    }
    Ok(task_id.to_string())
}

fn normalize_task_worker(worker: &str) -> String {
    let worker = worker.trim();
    if worker.is_empty() {
        "workflow-agent".to_string()
    } else {
        worker.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::WorkflowIpcContext;

    #[test]
    fn durable_mailbox_preserves_messages_across_context_reloads() {
        let temp = tempdir().unwrap();
        let mailbox_path = temp.path().join("mailbox.json");
        let first = WorkflowIpcContext::new_durable_mailbox(mailbox_path.clone()).unwrap();

        let sent = first
            .send_message("findings", Some("scanner"), json!({ "severity": "high" }))
            .expect("send durable message");
        assert_eq!(sent["seq"], 1);

        let second = WorkflowIpcContext::new_durable_mailbox(mailbox_path).unwrap();
        let messages = second
            .read_messages("findings")
            .expect("read durable messages");

        assert_eq!(messages.as_array().unwrap().len(), 1);
        assert_eq!(messages[0]["from"], "scanner");
        assert_eq!(messages[0]["message"]["severity"], "high");
    }

    #[test]
    fn durable_task_lists_preserve_claimed_tasks_across_context_reloads() {
        let temp = tempdir().unwrap();
        let task_lists_path = temp.path().join("task-lists.json");
        let first = WorkflowIpcContext::new_durable_task_lists(task_lists_path.clone()).unwrap();

        first
            .create_task_list("audit", vec![json!({ "area": "api" })])
            .expect("create durable task list");
        let claimed = first
            .claim_task("audit", Some("worker-a"))
            .expect("claim durable task");
        assert_eq!(claimed["id"], "workflow-task-1");
        assert_eq!(claimed["status"], "running");

        drop(first);
        let second = WorkflowIpcContext::new_durable_task_lists(task_lists_path).unwrap();
        let tasks = second.list_tasks("audit").expect("list durable tasks");

        assert_eq!(tasks.as_array().unwrap().len(), 1);
        assert_eq!(tasks[0]["id"], "workflow-task-1");
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["claimedBy"], "worker-a");
    }

    #[test]
    fn workflow_task_list_has_one_writer_per_path() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("task-lists.json");
        let first = WorkflowIpcContext::new_durable_task_lists(path.clone()).unwrap();
        assert!(WorkflowIpcContext::new_durable_task_lists(path.clone()).is_err());
        drop(first);
        assert!(WorkflowIpcContext::new_durable_task_lists(path).is_ok());
    }

    #[test]
    fn completing_an_unclaimed_task_is_refused() {
        let ipc = WorkflowIpcContext::new();
        ipc.create_task_list("audit", vec![json!("api")]).unwrap();
        let parent = ipc.for_sender("child-a");
        let owner = parent.for_nested_attempt();
        let other = parent.for_nested_attempt();
        let claimed = owner.claim_task("audit", Some("worker")).unwrap();
        let id = claimed["id"].as_str().unwrap();

        let refusal = other.complete_task("audit", id, json!("wrong"), Some("worker"));
        assert!(refusal.unwrap_err().contains("claimant"));
        assert_eq!(ipc.list_tasks("audit").unwrap(), json!([claimed]));

        let completed = owner
            .complete_task("audit", id, json!("done"), None)
            .unwrap();
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"], "done");
        assert!(
            owner
                .complete_task("audit", id, json!("again"), None)
                .is_err()
        );
        assert_eq!(ipc.list_tasks("audit").unwrap(), json!([completed]));

        ipc.create_task_list("second", vec![json!("docs")]).unwrap();
        let claimed = owner.claim_task("second", None).unwrap();
        assert_eq!(claimed["claimedBy"], "child-a");
        drop(owner);
        assert_eq!(
            other.claim_task("second", None).unwrap()["id"],
            claimed["id"]
        );
    }

    #[test]
    fn concurrent_claims_do_not_share_a_task() {
        let ipc = WorkflowIpcContext::new();
        ipc.create_task_list("audit", vec![json!("api"), json!("docs")])
            .unwrap();
        let start = std::sync::Barrier::new(2);
        let left = ipc.for_sender("left");
        let right = ipc.for_sender("right");
        let (left, right, start) = (&left, &right, &start);
        let claimed = std::thread::scope(|scope| {
            let claim = |child: &WorkflowIpcContext| {
                start.wait();
                child.claim_task("audit", None).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let left = scope.spawn(move || claim(left));
            let right = scope.spawn(move || claim(right));
            let mut ids = [left.join().unwrap(), right.join().unwrap()];
            ids.sort();
            ids
        });
        assert_eq!(claimed, ["workflow-task-1", "workflow-task-2"]);
        assert!(ipc.claim_task("audit", None).unwrap().is_null());
    }
}
