use std::sync::{Arc, RwLock};

use orca_core::config::RunConfig;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeExecutionPolicyHandle {
    state: Arc<RwLock<RuntimeExecutionPolicyState>>,
}

#[derive(Clone, Debug)]
struct RuntimeExecutionPolicyState {
    revision: u64,
    config: Arc<RunConfig>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeExecutionPolicySnapshot {
    revision: u64,
    config: Arc<RunConfig>,
}

impl RuntimeExecutionPolicyHandle {
    pub(crate) fn new(config: RunConfig, revision: u64) -> Self {
        Self {
            state: Arc::new(RwLock::new(RuntimeExecutionPolicyState {
                revision,
                config: Arc::new(config),
            })),
        }
    }

    pub(crate) fn snapshot(&self) -> RuntimeExecutionPolicySnapshot {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RuntimeExecutionPolicySnapshot {
            revision: state.revision,
            config: Arc::clone(&state.config),
        }
    }

    pub(crate) fn publish(&self, config: RunConfig, revision: u64) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.revision = revision;
        state.config = Arc::new(config);
    }
}

impl RuntimeExecutionPolicySnapshot {
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn config(&self) -> &RunConfig {
        &self.config
    }
}
