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

    for command in ["exec", "workflow", "trust", "mcp"] {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim_start().starts_with(command)),
            "root help is missing {command}"
        );
    }
    let tokens = stdout.split_whitespace().collect::<Vec<_>>();
    for option in ["--resume", "--fork", "--continue", "--model", "--mode"] {
        assert!(tokens.contains(&option), "root help is missing {option}");
    }
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
