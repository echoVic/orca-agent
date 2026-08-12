use std::sync::mpsc;
use std::time::Duration;

use orca_core::provider_types::ProviderStep;
use orca_core::thread_item_projection::ModelResponseIdentity;
use orca_provider::{ProviderStreamEvent, ProviderStreamingCall};

use crate::model_response::RuntimeModelResponse;

pub trait RuntimeProviderSuspensionControl: std::fmt::Debug + Send + Sync {
    fn take_suspension_request(&self) -> bool;
}

pub enum RuntimeProviderSuspensionEvent {
    Step(ProviderStep),
    Completed(RuntimeModelResponse),
}

/// A resumable handle to the suspended operation's execution journal and
/// budget spec. The background completion path reopens the journal (appending
/// continues exactly where the loop stopped) and settles cost, wall time, and
/// the operation terminal against the authoritative journal.
#[derive(Clone, Debug)]
pub struct SuspendedOperationHandle {
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) operation_id: String,
    pub(crate) spec: orca_core::budget::BudgetSpec,
    /// The controller's usage at suspension, so the completion path resumes
    /// accounting from the exact point the loop stopped (turns, tools, cost,
    /// and wall time) instead of a fresh zero-usage controller.
    pub(crate) usage: orca_core::budget::BudgetUsage,
}

pub struct RuntimeProviderSuspension {
    stream: ProviderStreamingCall,
    model: Option<String>,
    identity: ModelResponseIdentity,
    operation: Option<SuspendedOperationHandle>,
}

impl RuntimeProviderSuspension {
    pub(crate) fn new(
        stream: ProviderStreamingCall,
        model: Option<String>,
        identity: ModelResponseIdentity,
    ) -> Self {
        Self {
            stream,
            model,
            identity,
            operation: None,
        }
    }

    pub(crate) fn with_operation(mut self, operation: SuspendedOperationHandle) -> Self {
        self.operation = Some(operation);
        self
    }

    pub(crate) fn operation(&self) -> Option<&SuspendedOperationHandle> {
        self.operation.as_ref()
    }

    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<RuntimeProviderSuspensionEvent, mpsc::RecvTimeoutError> {
        match self.stream.recv_timeout(timeout)? {
            ProviderStreamEvent::Step(delivery) => Ok(RuntimeProviderSuspensionEvent::Step(
                delivery.step().clone(),
            )),
            ProviderStreamEvent::Completed(response) => {
                Ok(RuntimeProviderSuspensionEvent::Completed(
                    RuntimeModelResponse::from_parts(response, self.identity.clone()),
                ))
            }
        }
    }

    pub fn cancel(&self) {
        self.stream.cancel();
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn identity(&self) -> &ModelResponseIdentity {
        &self.identity
    }
}
