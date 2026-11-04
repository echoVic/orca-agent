use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn cargo_metadata() -> Value {
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse cargo metadata JSON")
}

fn package<'a>(metadata: &'a Value, name: &str) -> &'a Value {
    metadata["packages"]
        .as_array()
        .expect("metadata packages")
        .iter()
        .find(|package| package["name"] == name)
        .unwrap_or_else(|| panic!("metadata is missing package {name}"))
}

fn normal_dependencies(package: &Value) -> BTreeSet<&str> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies")
        .iter()
        .filter(|dependency| dependency["kind"].is_null() && dependency["target"].is_null())
        .map(|dependency| dependency["name"].as_str().expect("dependency name"))
        .collect()
}

fn target_source<'a>(package: &'a Value, kind: &str, name: &str) -> &'a str {
    package["targets"]
        .as_array()
        .expect("package targets")
        .iter()
        .find(|target| {
            target["name"] == name
                && target["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|candidate| candidate == kind))
        })
        .and_then(|target| target["src_path"].as_str())
        .unwrap_or_else(|| panic!("package target {name} ({kind}) is missing"))
}

#[test]
fn cargo_metadata_enforces_the_thin_root_dependency_graph() {
    let metadata = cargo_metadata();
    let root = package(&metadata, "orca-agent");
    assert_eq!(
        normal_dependencies(root),
        ["clap", "orca-core", "orca-runtime", "orca-tui"]
            .into_iter()
            .collect()
    );

    let runtime = package(&metadata, "orca-runtime");
    let tui = package(&metadata, "orca-tui");
    assert!(!normal_dependencies(runtime).contains("orca-tui"));
    assert!(normal_dependencies(tui).contains("orca-runtime"));
}

#[test]
fn provider_does_not_depend_on_tools() {
    let metadata = cargo_metadata();
    let provider = package(&metadata, "orca-provider");

    assert!(
        !normal_dependencies(provider).contains("orca-tools"),
        "provider adapters must consume provider-neutral definitions supplied by the runtime"
    );
}

#[test]
fn tui_does_not_depend_on_provider() {
    let metadata = cargo_metadata();
    let tui = package(&metadata, "orca-tui");

    assert!(
        !normal_dependencies(tui).contains("orca-provider"),
        "TUI must depend on runtime facades, not provider adapters"
    );
}

#[test]
fn cargo_metadata_enforces_binary_and_library_target_ownership() {
    let metadata = cargo_metadata();
    let root = package(&metadata, "orca-agent");
    let runtime = package(&metadata, "orca-runtime");
    let tui = package(&metadata, "orca-tui");

    assert!(Path::new(target_source(root, "bin", "orca")).ends_with("src/main.rs"));
    assert!(
        Path::new(target_source(runtime, "lib", "orca_runtime"))
            .ends_with("crates/orca-runtime/src/lib.rs")
    );
    assert!(
        Path::new(target_source(tui, "lib", "orca_tui")).ends_with("crates/orca-tui/src/lib.rs")
    );
}

#[test]
fn public_runtime_boundaries_compile_through_library_facades() {
    fn assert_public_type<T>() {}

    assert_public_type::<orca_runtime::surface::SurfaceCursor>();
    assert_public_type::<orca_runtime::surface::RuntimeSurfaceClientHandle>();
    assert_public_type::<orca_runtime::workflow::command::WorkflowCommandRequest>();
    assert_public_type::<orca_runtime::update_check::UpdateAction>();
}
