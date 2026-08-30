pub use orca_core::execution_broker::{BrokerLaunch, ExecutionBroker, LaunchError};

#[cfg(test)]
mod tests {
    use std::process::Command;

    use orca_core::capability::{
        CapabilityProcessClass, CapabilityRequest, CapabilitySet, EffectiveCapability,
        EnforcementState,
    };

    use super::{ExecutionBroker, LaunchError};

    #[test]
    fn unavailable_backend_rejects_non_dangerous_launch() {
        let capability = EffectiveCapability::resolve(
            CapabilityRequest::new(
                "broker-1",
                CapabilityProcessClass::SandboxedTool,
                CapabilitySet::read_only(),
                std::env::current_dir().expect("current directory"),
            ),
            CapabilitySet::read_only(),
            orca_core::approval_types::ApprovalMode::Plan,
        )
        .expect("resolve capability");
        let broker = ExecutionBroker::new(EnforcementState::Unavailable);

        let error = broker
            .launch(Command::new("true"), capability)
            .expect_err("unavailable backend must reject launch");
        assert!(matches!(error, LaunchError::EnforcementUnavailable));
    }

    #[test]
    fn enforced_launch_returns_receipt() {
        let cwd = std::env::current_dir().expect("current directory");
        let capability = EffectiveCapability::resolve(
            CapabilityRequest::new(
                "broker-2",
                CapabilityProcessClass::SandboxedTool,
                CapabilitySet::read_only(),
                cwd,
            ),
            CapabilitySet::read_only(),
            orca_core::approval_types::ApprovalMode::Plan,
        )
        .expect("resolve capability");
        let broker = ExecutionBroker::new(EnforcementState::Enforced);
        let launched = broker
            .launch(Command::new("true"), capability)
            .expect("launch through broker");

        assert_eq!(launched.receipt.enforcement, EnforcementState::Enforced);
        assert_eq!(launched.receipt.request_id, "broker-2");
    }
}
