//! #178: the shells an agent runs through a shell session never inherit the
//! credentials Orca itself runs with. This binary holds one test because it
//! sets process environment variables that no other test may observe.

use orca_runtime::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
};
use orca_runtime::tasks::TaskRegistry;
use std::collections::BTreeMap;
use std::time::Duration;

const CREDENTIALS: [&str; 3] = ["ORCA_API_KEY", "DEEPSEEK_API_KEY", "BRAVE_SEARCH_API_KEY"];

#[test]
fn shell_sessions_do_not_inherit_orca_credentials() {
    for name in CREDENTIALS {
        // SAFETY: this is the only test in the binary, and it sets the
        // variables before it starts any thread.
        unsafe { std::env::set_var(name, "sk-test-credential-178") };
    }
    if cfg!(windows) {
        // Without pwsh the default shell is cmd.exe; the probe is PowerShell.
        // SAFETY: as above.
        unsafe { std::env::set_var("ORCA_SHELL", "powershell.exe") };
    }
    let cwd = tempfile::tempdir().expect("tempdir");
    let mut manager =
        RuntimeShellSessionManager::new(TaskRegistry::new("shell-credentials".to_string()));
    let command = if cfg!(windows) {
        "[Console]::Out.Write(((@('ORCA_API_KEY','DEEPSEEK_API_KEY','BRAVE_SEARCH_API_KEY') | ForEach-Object { if ([Environment]::GetEnvironmentVariable($_)) { 'set' } else { 'unset' } }) -join '|'))"
    } else {
        "printf '%s|%s|%s' \"${ORCA_API_KEY-unset}\" \"${DEEPSEEK_API_KEY-unset}\" \"${BRAVE_SEARCH_API_KEY-unset}\""
    };

    // Full access is the agent shell with no sandbox, launched as a
    // user-trusted process: the case a launch-class rule would miss.
    let handle = manager
        .spawn(ShellSessionCommand {
            command: command.to_string(),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            metadata_read_only_paths: Vec::new(),
            env: BTreeMap::new(),
            description: "credential check".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");
    let output = manager
        .wait(&handle.id, Duration::from_secs(30))
        .expect("wait for shell");

    assert_eq!(output.stdout, "unset|unset|unset");
}
