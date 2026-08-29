use crossbeam_channel as mpsc;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use orca_file_search::{
    SearchMode, SearchPhase, SearchProgress, SearchSession, SearchSessionOptions, SearchSnapshot,
    SessionGeneration,
};
use orca_runtime::mentions::{self, MentionToken};
use orca_runtime::mentions::{MentionCandidate, MentionCatalog, MentionKind, MentionSigil};

use crate::protocol::TuiEvent;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::{AppState, AppStatus, PanelMode};

const WARM_IDLE: Duration = Duration::from_secs(30);
const CATALOG_RESULT_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenIdentity {
    start: usize,
    quoted: bool,
    sigil: MentionSigil,
}

struct CatalogDiscoveryResult {
    generation: u64,
    catalog: MentionCatalog,
}

impl From<&MentionToken> for TokenIdentity {
    fn from(token: &MentionToken) -> Self {
        Self {
            start: token.start,
            quoted: token.quoted,
            sigil: token.sigil,
        }
    }
}

pub(crate) struct MentionSearchManager {
    roots: Vec<PathBuf>,
    catalog: MentionCatalog,
    catalog_actions: Option<TuiSurfaceActions>,
    catalog_generation: u64,
    catalog_result_tx: mpsc::Sender<CatalogDiscoveryResult>,
    catalog_result_rx: mpsc::Receiver<CatalogDiscoveryResult>,
    catalog_workers: Vec<JoinHandle<()>>,
    event_tx: mpsc::Sender<TuiEvent>,
    next_generation: u64,
    session: Option<SearchSession>,
    stopping: Option<SearchSession>,
    active_token: Option<TokenIdentity>,
    active_generation: Option<SessionGeneration>,
    active_query: Option<String>,
    warm_deadline: Option<Instant>,
    refreshing: bool,
}

impl MentionSearchManager {
    pub(crate) fn is_enabled(state: &AppState) -> bool {
        state.panel_mode == PanelMode::Conversation
            && matches!(
                state.status,
                AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
            )
            && state.slash_menu.is_none()
            && state.user_input_dialog.is_none()
    }

    #[cfg(test)]
    pub(crate) fn new(root: PathBuf, event_tx: mpsc::Sender<TuiEvent>) -> Self {
        Self::new_roots_with_catalog(vec![root], event_tx, MentionCatalog::default())
    }

    pub(crate) fn new_roots(roots: Vec<PathBuf>, event_tx: mpsc::Sender<TuiEvent>) -> Self {
        let roots = normalize_roots(roots);
        let catalog = MentionCatalog::discover_skills(&roots);
        Self::new_roots_with_catalog(roots, event_tx, catalog)
    }

    fn new_roots_with_catalog(
        roots: Vec<PathBuf>,
        event_tx: mpsc::Sender<TuiEvent>,
        catalog: MentionCatalog,
    ) -> Self {
        let (catalog_result_tx, catalog_result_rx) = mpsc::bounded(CATALOG_RESULT_CAPACITY);
        Self {
            roots: normalize_roots(roots),
            catalog,
            catalog_actions: None,
            catalog_generation: 0,
            catalog_result_tx,
            catalog_result_rx,
            catalog_workers: Vec::new(),
            event_tx,
            next_generation: 0,
            session: None,
            stopping: None,
            active_token: None,
            active_generation: None,
            active_query: None,
            warm_deadline: None,
            refreshing: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_root(&mut self, root: PathBuf, state: &mut AppState) {
        self.set_roots(vec![root], state);
    }

    pub(crate) fn set_roots(&mut self, roots: Vec<PathBuf>, state: &mut AppState) {
        let roots = normalize_roots(roots);
        if self.roots == roots {
            return;
        }
        self.catalog = MentionCatalog::discover_skills(&roots);
        self.roots = roots;
        self.refresh_catalog_async();
        self.warm_deadline = None;
        self.active_token = None;
        self.active_generation = None;
        self.active_query = None;
        self.refreshing = false;
        state.mention.dismissed_query = None;
        state.mention.clear_projection();
        self.begin_stop();
    }

    pub(crate) fn install_runtime_actions(&mut self, actions: TuiSurfaceActions) {
        self.catalog_actions = Some(actions);
        self.refresh_catalog_async();
    }

    pub(crate) fn consume_catalog_dirty(&mut self, event_generation: u64, state: &mut AppState) {
        self.reap_catalog_workers();
        if event_generation != self.catalog_generation {
            return;
        }
        let mut latest = None;
        while let Ok(result) = self.catalog_result_rx.try_recv() {
            if result.generation == self.catalog_generation {
                latest = Some(result.catalog);
            }
        }
        let Some(catalog) = latest else {
            return;
        };
        self.catalog = catalog;

        if self
            .active_token
            .is_some_and(|token| token.sigil == MentionSigil::Dollar)
        {
            if let Some(query) = self.active_query.as_deref() {
                self.project_skill_candidates(query, state);
            }
        } else if self.active_token.is_some() {
            let generation = self.advance_generation();
            self.active_generation = Some(generation);
            if let Some(session) = &self.session {
                session.set_generation(generation);
                if let Some(query) = &self.active_query {
                    session.update(search_mode(query));
                }
            }
            state.mention.phase = Some(SearchPhase::Searching);
        }
    }

    #[cfg(test)]
    pub(crate) fn sync(&mut self, text: &str, enabled: bool, state: &mut AppState, now: Instant) {
        self.sync_at_cursor(text, text.len(), enabled, state, now);
    }

    pub(crate) fn sync_at_cursor(
        &mut self,
        text: &str,
        cursor: usize,
        enabled: bool,
        state: &mut AppState,
        now: Instant,
    ) {
        self.poll(now);
        let token = enabled
            .then(|| mentions::mention_token_at_cursor(text, cursor))
            .flatten();
        let Some(token) = token else {
            self.deactivate(state, now);
            return;
        };

        let identity = TokenIdentity::from(&token);
        if state
            .mention
            .sigil
            .is_some_and(|sigil| sigil != token.sigil)
            || state
                .mention
                .dismissed_query
                .as_deref()
                .is_some_and(|dismissed| dismissed != token.query)
        {
            state.mention.dismissed_query = None;
        }
        if self.active_token != Some(identity) {
            let generation = self.advance_generation();
            self.active_generation = Some(generation);
            if let Some(session) = &self.session {
                session.set_generation(generation);
            }
        }
        self.active_token = Some(identity);
        self.active_query = Some(token.query.clone());
        state.mention.pending_query = Some(token.query.clone());
        state.mention.sigil = Some(token.sigil);

        if token.sigil == MentionSigil::Dollar {
            self.warm_deadline = None;
            self.active_generation = None;
            self.refreshing = false;
            self.begin_stop();
            if state.mention.dismissed_query.as_deref() == Some(token.query.as_str()) {
                state.mention.phase = None;
                state.mention.candidates.clear();
            } else {
                self.project_skill_candidates(&token.query, state);
            }
            return;
        }

        if self.warm_deadline.take().is_some()
            && self
                .session
                .as_ref()
                .is_some_and(SearchSession::tracked_state_changed)
        {
            self.refreshing = true;
            self.begin_stop();
            state.mention.phase = Some(SearchPhase::Refreshing);
            return;
        }

        if self.session.is_none() {
            if self.stopping.is_some() {
                state.mention.phase = Some(if self.refreshing {
                    SearchPhase::Refreshing
                } else {
                    SearchPhase::Searching
                });
                return;
            }
            if let Err(error) = self.start_session() {
                state.mention.phase = Some(SearchPhase::Incomplete { message: error });
                return;
            }
        }

        let mode = if token.query.is_empty() || token.query.ends_with('/') {
            SearchMode::browse(token.query.clone())
        } else {
            SearchMode::fuzzy(token.query.clone())
        };
        if let Some(session) = &self.session {
            session.update(mode);
        }
        if state.mention.dismissed_query.as_deref() == Some(token.query.as_str()) {
            state.mention.phase = None;
            state.mention.candidates.clear();
        } else if state.mention.phase.is_none() {
            state.mention.phase = Some(SearchPhase::Searching);
        }
    }

    #[cfg(test)]
    pub(crate) fn consume_dirty(
        &mut self,
        generation: SessionGeneration,
        text: &str,
        state: &mut AppState,
    ) {
        self.consume_dirty_at_cursor(generation, text, text.len(), state);
    }

    pub(crate) fn consume_dirty_at_cursor(
        &mut self,
        event_generation: SessionGeneration,
        text: &str,
        cursor: usize,
        state: &mut AppState,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        if session.generation() != event_generation
            || self.active_generation != Some(event_generation)
        {
            return;
        }
        let Some(snapshot) = session.take_latest_snapshot() else {
            return;
        };
        self.apply_snapshot_at_cursor(snapshot, text, cursor, state);
    }

    pub(crate) fn poll(&mut self, now: Instant) {
        self.reap_catalog_workers();
        if self.warm_deadline.is_some_and(|deadline| now >= deadline) {
            self.warm_deadline = None;
            self.begin_stop();
        }
        if self
            .stopping
            .as_ref()
            .is_some_and(SearchSession::is_finished)
            && let Some(mut session) = self.stopping.take()
        {
            session.join();
        }
    }

    fn start_session(&mut self) -> Result<(), String> {
        let generation = self.active_generation.unwrap_or_else(|| {
            let generation = self.advance_generation();
            self.active_generation = Some(generation);
            generation
        });
        let event_tx = self.event_tx.clone();
        let options = SearchSessionOptions::new(generation, move |generation| {
            let _ = event_tx.send(TuiEvent::MentionSearchDirty { generation });
        });
        self.session = Some(SearchSession::start_roots(&self.roots, options)?);
        self.refreshing = false;
        Ok(())
    }

    fn refresh_catalog_async(&mut self) {
        let Some(actions) = self.catalog_actions.clone() else {
            return;
        };
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        let generation = self.catalog_generation;
        let roots = self.roots.clone();
        let result_tx = self.catalog_result_tx.clone();
        let event_tx = self.event_tx.clone();
        let worker = std::thread::Builder::new()
            .name("orca-mention-catalog".to_string())
            .spawn(move || {
                let catalog = actions.discover_mention_catalog(&roots);
                if result_tx
                    .send(CatalogDiscoveryResult {
                        generation,
                        catalog,
                    })
                    .is_ok()
                {
                    let _ = event_tx.send(TuiEvent::MentionCatalogDirty { generation });
                }
            })
            .expect("mention catalog worker should start");
        self.catalog_workers.push(worker);
    }

    fn reap_catalog_workers(&mut self) {
        let mut pending = Vec::with_capacity(self.catalog_workers.len());
        for worker in self.catalog_workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                pending.push(worker);
            }
        }
        self.catalog_workers = pending;
    }

    fn deactivate(&mut self, state: &mut AppState, now: Instant) {
        if self.active_token.take().is_some() && self.session.is_some() {
            self.warm_deadline = Some(now + WARM_IDLE);
            let generation = self.advance_generation();
            if let Some(session) = &self.session {
                session.set_generation(generation);
                session.clear_query();
            }
        }
        self.active_generation = None;
        self.active_query = None;
        self.refreshing = false;
        state.mention.clear_projection();
    }

    fn begin_stop(&mut self) {
        if self.stopping.is_none()
            && let Some(session) = self.session.take()
        {
            session.cancel();
            self.stopping = Some(session);
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.begin_stop();
        if let Some(session) = self.stopping.take() {
            session.cancel();
            let _ = std::thread::Builder::new()
                .name("orca-file-search-reaper".to_string())
                .spawn(move || {
                    let mut session = session;
                    session.join();
                });
        }
        if !self.catalog_workers.is_empty() {
            let workers = std::mem::take(&mut self.catalog_workers);
            let _ = std::thread::Builder::new()
                .name("orca-mention-catalog-reaper".to_string())
                .spawn(move || {
                    for worker in workers {
                        let _ = worker.join();
                    }
                });
        }
    }

    #[cfg(test)]
    fn apply_snapshot(&self, snapshot: SearchSnapshot, text: &str, state: &mut AppState) {
        self.apply_snapshot_at_cursor(snapshot, text, text.len(), state);
    }

    fn apply_snapshot_at_cursor(
        &self,
        snapshot: SearchSnapshot,
        text: &str,
        cursor: usize,
        state: &mut AppState,
    ) {
        if self.session.as_ref().map(SearchSession::generation) != Some(snapshot.generation) {
            return;
        }
        if self.active_generation != Some(snapshot.generation) {
            return;
        }
        let Some(token) = mentions::mention_token_at_cursor(text, cursor) else {
            return;
        };
        if token.sigil != MentionSigil::At {
            return;
        }
        if self.active_token != Some(TokenIdentity::from(&token))
            || self.active_query.as_deref() != Some(token.query.as_str())
            || state.mention.pending_query.as_deref() != Some(token.query.as_str())
            || snapshot.mode.query() != token.query
            || state.mention.dismissed_query.as_deref() == Some(token.query.as_str())
        {
            return;
        }

        let files = snapshot
            .matches
            .iter()
            .map(MentionCandidate::from_file_match)
            .collect::<Vec<_>>();
        let static_candidates = self.catalog.search(&token.query, 12);
        let candidates = mentions::merge_candidates(&token.query, static_candidates, files, 12);
        Self::replace_candidates(state, candidates);
        state.mention.sigil = Some(MentionSigil::At);
        state.mention.phase = Some(snapshot.phase);
        state.mention.progress = snapshot.progress;
    }

    fn project_skill_candidates(&self, query: &str, state: &mut AppState) {
        let candidates = self.catalog.search_all_kind(query, MentionKind::Skill);
        Self::replace_candidates(state, candidates);
        state.mention.sigil = Some(MentionSigil::Dollar);
        state.mention.phase = Some(SearchPhase::Complete);
        state.mention.progress = SearchProgress::default();
    }

    fn replace_candidates(state: &mut AppState, candidates: Vec<MentionCandidate>) {
        let previous_index = state.mention.selected;
        let anchored_candidate = state
            .mention
            .manual_selection
            .then(|| state.mention.selected_identity.clone())
            .flatten();
        state.mention.candidates = candidates;
        if let Some(id) = anchored_candidate {
            state.mention.selected = state
                .mention
                .candidates
                .iter()
                .position(|candidate| candidate.id == id)
                .unwrap_or_else(|| {
                    previous_index.min(state.mention.candidates.len().saturating_sub(1))
                });
        } else {
            state.mention.selected = 0;
        }
        state.mention.selected_identity = state
            .mention
            .candidates
            .get(state.mention.selected)
            .map(|candidate| candidate.id.clone());
    }

    fn advance_generation(&mut self) -> SessionGeneration {
        self.next_generation = self.next_generation.wrapping_add(1);
        SessionGeneration(self.next_generation)
    }
}

fn search_mode(query: &str) -> SearchMode {
    if query.is_empty() || query.ends_with('/') {
        SearchMode::browse(query)
    } else {
        SearchMode::fuzzy(query)
    }
}

fn normalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut normalized = Vec::new();
    for root in roots {
        let root = root.canonicalize().unwrap_or(root);
        if !normalized.contains(&root) {
            normalized.push(root);
        }
    }
    if normalized.is_empty() {
        normalized.push(std::env::current_dir().unwrap_or_default());
    }
    normalized
}

impl Drop for MentionSearchManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use orca_file_search::{
        MatchKind, SearchMatch, SearchMode, SearchPhase, SearchProgress, SearchSnapshot,
        SessionGeneration,
    };
    use tempfile::tempdir;

    use super::{MentionCandidate, MentionSearchManager, MentionSigil, TokenIdentity};
    use crate::types::{AppState, AppStatus, PanelMode};

    fn state() -> AppState {
        let (action_tx, _action_rx) = mpsc::unbounded();
        AppState::new(
            action_tx,
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        )
    }

    #[test]
    fn mention_search_enablement_covers_only_editable_conversation_states() {
        let mut state = state();
        for (status, enabled) in [
            (AppStatus::Idle, true),
            (AppStatus::Running, true),
            (AppStatus::WaitingUserInput, true),
            (AppStatus::Compacting, false),
            (AppStatus::WaitingApproval, false),
            (AppStatus::Setup, false),
            (AppStatus::SessionPicker, false),
        ] {
            state.status = status;
            state.panel_mode = PanelMode::Conversation;
            state.slash_menu = None;
            assert_eq!(
                MentionSearchManager::is_enabled(&state),
                enabled,
                "{status:?}"
            );
        }
        state.status = AppStatus::WaitingUserInput;
        state.user_input_dialog = Some(crate::user_input_dialog::UserInputDialog::new(
            "Choose?",
            vec!["A - First".to_string(), "B - Second".to_string()],
        ));
        assert!(!MentionSearchManager::is_enabled(&state));
        state.user_input_dialog = None;
        state.status = AppStatus::Running;
        for panel in [PanelMode::Workflows, PanelMode::Agents] {
            state.panel_mode = panel;
            assert!(!MentionSearchManager::is_enabled(&state), "{panel:?}");
        }
    }

    fn snapshot(generation: SessionGeneration, query: &str, paths: &[&str]) -> SearchSnapshot {
        SearchSnapshot {
            generation,
            mode: SearchMode::fuzzy(query),
            matches: paths
                .iter()
                .map(|path| SearchMatch {
                    root: PathBuf::from("/workspace"),
                    path: (*path).to_string(),
                    kind: MatchKind::File,
                    score: 1,
                    indices: Vec::new(),
                })
                .collect(),
            phase: SearchPhase::Scanning,
            progress: SearchProgress {
                scanned_paths: paths.len(),
                walk_complete: false,
            },
        }
    }

    fn file_candidates(paths: &[&str]) -> Vec<MentionCandidate> {
        snapshot(SessionGeneration(0), "", paths)
            .matches
            .iter()
            .map(MentionCandidate::from_file_match)
            .collect()
    }

    #[test]
    fn warm_session_expires_after_thirty_seconds() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        let now = Instant::now();

        manager.sync("@main", true, &mut state, now);
        manager.sync("", true, &mut state, now);
        manager.poll(now + Duration::from_secs(29));
        assert!(manager.session.is_some());

        manager.poll(now + Duration::from_secs(30));
        assert!(manager.session.is_none());
    }

    #[test]
    fn dismissed_query_stays_hidden_until_query_changes() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        state.mention.dismissed_query = Some("main".to_string());

        manager.sync("@main", true, &mut state, Instant::now());
        assert_eq!(state.mention.phase, None);

        manager.sync("@mainr", true, &mut state, Instant::now());
        assert_ne!(state.mention.phase, Some(SearchPhase::Complete));
        assert_eq!(state.mention.dismissed_query, None);
    }

    #[test]
    fn stale_mention_generation_is_ignored_before_snapshot_consumption() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        state.mention.candidates = file_candidates(&["sentinel.rs"]);

        let current = manager.session.as_ref().unwrap().generation();
        manager.consume_dirty(SessionGeneration(current.0 + 1), "@main", &mut state);

        assert_eq!(state.mention.candidates[0].display, "sentinel.rs");
    }

    #[test]
    fn stale_mention_token_identity_is_ignored() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        state.mention.candidates = file_candidates(&["sentinel.rs"]);

        manager.apply_snapshot(
            snapshot(generation, "main", &["new.rs"]),
            "prefix @main",
            &mut state,
        );

        assert_eq!(state.mention.candidates[0].display, "sentinel.rs");
    }

    #[test]
    fn stale_mention_pending_query_is_ignored() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        state.mention.pending_query = Some("older".to_string());
        state.mention.candidates = file_candidates(&["sentinel.rs"]);

        manager.apply_snapshot(
            snapshot(generation, "main", &["new.rs"]),
            "@main",
            &mut state,
        );

        assert_eq!(state.mention.candidates[0].display, "sentinel.rs");
    }

    #[test]
    fn mention_selection_stays_anchored_by_path_across_snapshots() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@m", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        manager.active_token = Some(TokenIdentity {
            start: 0,
            quoted: false,
            sigil: MentionSigil::At,
        });
        manager.active_query = Some("m".to_string());
        state.mention.pending_query = Some("m".to_string());
        state.mention.candidates = file_candidates(&["a.rs", "b.rs", "c.rs"]);
        let a_identity = state.mention.candidates[0].id.clone();
        let b_identity = state.mention.candidates[1].id.clone();
        state.mention.selected = 1;
        state.mention.selected_identity = Some(state.mention.candidates[1].id.clone());
        state.mention.manual_selection = true;

        manager.apply_snapshot(
            snapshot(generation, "m", &["c.rs", "a.rs", "b.rs"]),
            "@m",
            &mut state,
        );
        assert_eq!(state.mention.selected, 2);
        assert_eq!(state.mention.selected_identity, Some(b_identity));

        manager.apply_snapshot(
            snapshot(generation, "m", &["c.rs", "a.rs"]),
            "@m",
            &mut state,
        );
        assert_eq!(state.mention.selected, 1);
        assert_eq!(state.mention.selected_identity, Some(a_identity));
    }

    #[test]
    fn moving_between_same_query_tokens_advances_generation_without_a_second_catalog() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        let text = "@same and @same";

        manager.sync_at_cursor(text, 5, true, &mut state, Instant::now());
        let first = manager.session.as_ref().unwrap().generation();
        manager.sync_at_cursor(text, text.len(), true, &mut state, Instant::now());
        let second = manager.session.as_ref().unwrap().generation();

        assert_ne!(first, second);
        assert!(manager.stopping.is_none());
    }

    #[test]
    fn dollar_skill_search_uses_catalog_without_starting_file_scan() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("code-review.rs"), "not a skill").unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();

        manager.sync("$code-review", true, &mut state, Instant::now());

        assert!(manager.session.is_none());
        assert_eq!(state.mention.sigil, Some(MentionSigil::Dollar));
        assert_eq!(state.mention.phase, Some(SearchPhase::Complete));
        assert!(state.mention.candidates.is_empty());
    }

    #[test]
    fn shutdown_cancels_without_joining_on_the_tui_thread() {
        let root = tempdir().unwrap();
        for index in 0..2_000 {
            fs::write(root.path().join(format!("file-{index}.rs")), "file").unwrap();
        }
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@file", true, &mut state, Instant::now());

        let started = Instant::now();
        manager.shutdown();

        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(manager.session.is_none());
        assert!(manager.stopping.is_none());
    }

    #[test]
    fn changing_root_stops_old_generation_before_starting_new_one() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut manager = MentionSearchManager::new(first.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let first_generation = manager.session.as_ref().unwrap().generation();

        manager.set_root(second.path().to_path_buf(), &mut state);

        assert!(manager.session.is_none());
        let mut stopping = manager.stopping.take().unwrap();
        stopping.join();
        manager.sync("@main", true, &mut state, Instant::now());
        let second_generation = manager.session.as_ref().unwrap().generation();
        assert_ne!(first_generation, second_generation);
        assert_eq!(manager.roots, vec![second.path().canonicalize().unwrap()]);
    }

    #[test]
    fn manager_preserves_all_runtime_workspace_roots() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded();

        let manager = MentionSearchManager::new_roots(
            vec![first.path().to_path_buf(), second.path().to_path_buf()],
            event_tx,
        );

        assert_eq!(
            manager.roots,
            vec![
                first.path().canonicalize().unwrap(),
                second.path().canonicalize().unwrap(),
            ]
        );
    }
}
