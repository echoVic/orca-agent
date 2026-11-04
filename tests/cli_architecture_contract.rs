use std::process::Command;

fn run_orca(argument: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_orca"))
        .arg(argument)
        .output()
        .expect("run Orca binary")
}

#[test]
fn root_binary_exposes_the_supported_command_surface() {
    let output = run_orca("--help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");

    for command in ["exec", "app-server", "acp", "workflow", "trust", "mcp"] {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim_start().starts_with(command)),
            "root help is missing {command}"
        );
    }
    let tokens = stdout.split_whitespace().collect::<Vec<_>>();
    for option in [
        "--resume",
        "--fork",
        "--continue",
        "--model",
        "--mode",
        "--yolo",
        "--plan",
    ] {
        assert!(tokens.contains(&option), "root help is missing {option}");
    }
    assert!(!stdout.contains("computer-use-driver"));
}

#[test]
fn root_binary_reports_the_workspace_package_version() {
    let output = run_orca("--version");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("version is UTF-8")
            .trim(),
        format!("orca {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_protocol_and_mode_combinations_fail_before_startup() {
    for args in [
        vec!["--mode", "server", "--continue"],
        vec!["--mode", "acp", "--resume", "session"],
        vec!["--fork", "session", "app-server"],
        vec!["--plan", "acp"],
        vec!["--mode", "plan", "doctor"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn conflicting_exec_resume_options_fail_without_printing_credentials() {
    for args in [
        vec![
            "exec", "--model", "one", "resume", "--last", "--model", "two", "inspect",
        ],
        vec![
            "exec",
            "--plan",
            "resume",
            "--last",
            "--mode",
            "full-auto",
            "inspect",
        ],
        vec![
            "exec",
            "--output-format",
            "text",
            "resume",
            "--last",
            "--json",
            "inspect",
        ],
        vec![
            "--api-key",
            "first-test-key",
            "exec",
            "resume",
            "--last",
            "--api-key",
            "second-test-key",
            "inspect",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            !error.contains("first-test-key") && !error.contains("second-test-key"),
            "{error}"
        );
    }
}
