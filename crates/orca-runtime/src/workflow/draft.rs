use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_platform::fs::VerifiedDirectory;

use orca_core::workflow_types::{WorkflowDraft, WorkflowSourceMutationRisk};

use super::script::{
    parse_workflow_meta, single_workflow_component, validate_workflow_runtime_contract,
};
use super::state::WorkflowStateStore;

#[derive(Clone)]
pub struct WorkflowDraftStore {
    root: PathBuf,
    session: Option<Arc<VerifiedDirectory>>,
}

impl WorkflowDraftStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            session: None,
        }
    }

    pub(crate) fn in_session(root: PathBuf, session: Arc<VerifiedDirectory>) -> Self {
        Self {
            root,
            session: Some(session),
        }
    }

    pub fn draft_dir(&self, draft_id: &str) -> PathBuf {
        self.root.join(draft_id)
    }

    pub fn draft_path(&self, draft_id: &str) -> PathBuf {
        self.draft_dir(draft_id).join("draft.json")
    }

    pub fn script_path(&self, draft_id: &str) -> PathBuf {
        self.draft_dir(draft_id).join("script.js")
    }

    pub fn create_from_script(
        &self,
        session_id: &str,
        cwd: &Path,
        script: &str,
        max_configured_concurrent_agents: usize,
    ) -> io::Result<WorkflowDraft> {
        let meta = parse_workflow_meta(script)?;
        validate_workflow_runtime_contract(script, &meta)?;
        let draft_id = format!("workflow-draft-{}", uuid::Uuid::new_v4());
        let script_path = self.script_path(&draft_id);
        if let Some(parent) = script_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&script_path, script)?;

        let draft = WorkflowDraft {
            draft_id: draft_id.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.display().to_string(),
            name: meta.name,
            description: meta.description,
            phases: meta.phases,
            script: script.to_string(),
            script_path: script_path.display().to_string(),
            args: None,
            estimated_agent_count: estimate_agent_count(script),
            max_configured_concurrent_agents: max_configured_concurrent_agents as u32,
            source_mutation_risk: classify_source_mutation_risk(script),
            created_at_ms: now_ms(),
        };
        self.write(&draft)?;
        Ok(draft)
    }

    pub fn write(&self, draft: &WorkflowDraft) -> io::Result<()> {
        let path = self.draft_path(&draft.draft_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let value = serde_json::to_vec_pretty(draft)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(path, value)
    }

    pub fn load(&self, draft_id: &str) -> io::Result<WorkflowDraft> {
        validate_draft_id(draft_id)?;
        let session = self
            .root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = self.draft_path(draft_id);
        let relative = path.strip_prefix(session).unwrap_or(&path);
        let file = match &self.session {
            Some(root) => orca_tools::open_relative_file(root, relative)?,
            None => {
                let root = session.canonicalize()?;
                orca_tools::open_workspace_file(&root, &root.join(relative))?
            }
        };
        let draft: WorkflowDraft = serde_json::from_reader(BufReader::new(file))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if draft.draft_id != draft_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow draft ID does not match its directory",
            ));
        }
        Ok(draft)
    }

    pub fn edit_script(
        &self,
        draft_id: &str,
        script: &str,
        max_configured_concurrent_agents: usize,
    ) -> io::Result<WorkflowDraft> {
        let existing = self.load(draft_id)?;
        let meta = parse_workflow_meta(script)?;
        validate_workflow_runtime_contract(script, &meta)?;
        let script_path = self.script_path(draft_id);
        if let Some(parent) = script_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&script_path, script)?;

        let draft = WorkflowDraft {
            draft_id: existing.draft_id,
            session_id: existing.session_id,
            cwd: existing.cwd,
            name: meta.name,
            description: meta.description,
            phases: meta.phases,
            script: script.to_string(),
            script_path: script_path.display().to_string(),
            args: existing.args,
            estimated_agent_count: estimate_agent_count(script),
            max_configured_concurrent_agents: max_configured_concurrent_agents as u32,
            source_mutation_risk: classify_source_mutation_risk(script),
            created_at_ms: existing.created_at_ms,
        };
        self.write(&draft)?;
        Ok(draft)
    }

    pub fn cancel(&self, draft_id: &str) -> io::Result<()> {
        let dir = self.draft_dir(draft_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn save_reusable(
        &self,
        draft_id: &str,
        workflow_dir: &Path,
        save_as: Option<&str>,
    ) -> io::Result<PathBuf> {
        let draft = self.load(draft_id)?;
        let name = sanitize_workflow_name(save_as.unwrap_or(&draft.name))?;
        fs::create_dir_all(workflow_dir)?;
        let path = workflow_dir.join(format!("{name}.js"));
        fs::write(&path, draft.script)?;
        Ok(path)
    }

    pub fn clone_from_run(
        &self,
        state_store: &WorkflowStateStore,
        run_id: &str,
        max_configured_concurrent_agents: usize,
    ) -> io::Result<WorkflowDraft> {
        let state = state_store.load_run(run_id)?;
        let script = fs::read_to_string(state_store.run_dir(run_id).join("script.js"))?;
        let launch_input = state_store.load_launch_input(run_id)?;
        let mut draft = self.create_from_script(
            &state.session_id,
            Path::new(&state.cwd),
            &script,
            max_configured_concurrent_agents,
        )?;
        draft.args = launch_input.args;
        self.write(&draft)?;
        Ok(draft)
    }
}

pub(crate) fn validate_draft_id(id: &str) -> io::Result<()> {
    if !single_workflow_component(id) || id.contains([':', '\0']) || id.ends_with(['.', ' ']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "draftId must be a workflow draft ID",
        ));
    }
    Ok(())
}

fn estimate_agent_count(script: &str) -> Option<u32> {
    let count = script.match_indices("agent(").count() as u32;
    if count == 0 { None } else { Some(count) }
}

fn classify_source_mutation_risk(script: &str) -> WorkflowSourceMutationRisk {
    let lower = script.to_ascii_lowercase();
    let source_mutation_markers = [
        "edit(",
        "write_file",
        "writefile",
        "apply_patch",
        "modify",
        "implement",
        "fix ",
        "refactor",
        "commit",
        "isolation: \"worktree\"",
        "isolation: 'worktree'",
    ];
    if source_mutation_markers
        .iter()
        .any(|marker| lower.contains(marker))
    {
        WorkflowSourceMutationRisk::SourceMutationPossible
    } else {
        WorkflowSourceMutationRisk::ReadOnlyLikely
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn sanitize_workflow_name(raw: &str) -> io::Result<String> {
    let name = raw.trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name.contains("..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workflow save name must be a simple file stem",
        ));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::WorkflowDraftStore;

    #[test]
    fn draft_rejects_string_phase_workflow_without_executable_body() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowDraftStore::new(temp.path().join("drafts"));
        let error = store
            .create_from_script(
                "session-1",
                temp.path(),
                r#"export const meta = { name: "noop", description: "No-op", phases: ["scan"] };"#,
                4,
            )
            .expect_err("no-op string phase workflow should be rejected");

        assert!(
            error.to_string().contains("hand-written workflow"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn draft_rejects_string_phase_workflow_when_markers_only_appear_in_comments() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowDraftStore::new(temp.path().join("drafts"));
        let error = store
            .create_from_script(
                "session-1",
                temp.path(),
                r#"
export const meta = { name: "noop", description: "No-op", phases: ["scan"] };
// TODO: call phase("scan", async () => agent("inspect")) later.
"#,
                4,
            )
            .expect_err("comment-only workflow markers should not make the draft executable");

        assert!(
            error.to_string().contains("hand-written workflow"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn draft_rejects_invented_workflow_phase_apis() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkflowDraftStore::new(temp.path().join("drafts"));
        let error = store
            .create_from_script(
                "session-1",
                temp.path(),
                r#"
export const meta = { name: "bad", description: "Bad API", phases: ["scan"] };
async function run({ phases }) {
  const [scan] = phases;
  await scan.runParallel({ tasks: ["inspect"] });
  await phase.agent("inspect", { prompt: "inspect repo" });
}
"#,
                4,
            )
            .expect_err("invented phase API should be rejected");

        assert!(
            error.to_string().contains("unsupported workflow API"),
            "unexpected error: {error}"
        );
    }
}
