use std::process::Command;

use orca_core::capability::CapabilitySet;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};

pub fn notify(title: &str, message: &str) -> Result<(), String> {
    match std::env::consts::OS {
        "macos" => notify_macos(title, message),
        "linux" => notify_linux(title, message),
        _ => Err("desktop notifications are not supported on this platform".to_string()),
    }
}

fn notify_macos(title: &str, message: &str) -> Result<(), String> {
    let script = format!(
        "display notification {} with title {}",
        applescript_string(message),
        applescript_string(title)
    );
    let mut command = Command::new("osascript");
    command.arg("-e").arg(script);
    run(command)
}

fn notify_linux(title: &str, message: &str) -> Result<(), String> {
    let mut command = Command::new("notify-send");
    command.arg(title).arg(message);
    run(command)
}

fn run(command: Command) -> Result<(), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let broker = ExecutionBroker::with_backend(
        orca_core::capability::EnforcementState::Advisory,
        "desktop-notification",
    );
    let mut launched = broker
        .launch_user_trusted(command, "notification", cwd, CapabilitySet::read_only())
        .map_err(|error| match error {
            LaunchError::Spawn(error) => format!("failed to send notification: {error}"),
            other => format!("notification broker rejected launch: {other:?}"),
        })?;
    let status = launched
        .child
        .wait()
        .map_err(|error| format!("failed to wait for notification: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("notification command exited with {status}"))
    }
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_string_escapes_quotes() {
        assert_eq!(applescript_string("a \"quote\""), "\"a \\\"quote\\\"\"");
    }
}
