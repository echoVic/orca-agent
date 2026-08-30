//! Offline, read-only diagnostics shared by the CLI and first-run surfaces.
//!
//! The doctor command deliberately reports local facts only. It does not
//! contact a provider, start MCP, provision sandbox capabilities, or mutate
//! configuration/trust state.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use orca_core::capability::EnforcementState;
use orca_core::config::file::{self, ConfigOverrides, FileConfig};
use orca_core::config::folder_trust::{self, TrustLevel};
use orca_platform::host::HostPlatform;
use serde::Serialize;

pub const DOCTOR_SCHEMA_VERSION: u32 = 1;
pub const CANONICAL_PACKAGE: &str = "@blade-ai/orca";
pub const CANONICAL_WEBSITE: &str = "https://orcaagent.dev";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    pub id: String,
    pub status: DiagnosticStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCwd {
    pub requested: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub package: &'static str,
    pub website: &'static str,
    pub version: String,
    pub platform: String,
    pub cwd: DiagnosticCwd,
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    pub fn exit_code(&self) -> i32 {
        if self
            .checks
            .iter()
            .any(|check| check.status == DiagnosticStatus::Fail)
        {
            1
        } else {
            0
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_text(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "Orca doctor {}", self.version);
        let _ = writeln!(output, "Package: {}", self.package);
        let _ = writeln!(output, "Website: {}", self.website);
        let _ = writeln!(output, "Platform: {}", self.platform);
        let _ = writeln!(output, "CWD: {}", self.cwd.requested);
        if let Some(canonical) = &self.cwd.canonical {
            let _ = writeln!(output, "Canonical CWD: {canonical}");
        }
        for check in &self.checks {
            let marker = match check.status {
                DiagnosticStatus::Pass => "PASS",
                DiagnosticStatus::Warn => "WARN",
                DiagnosticStatus::Fail => "FAIL",
            };
            let _ = writeln!(output, "[{marker}] {}: {}", check.id, check.detail);
            if let Some(remediation) = &check.remediation {
                let _ = writeln!(output, "      remedy: {remediation}");
            }
        }
        output
    }
}

#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorOutputFormat {
    Json,
    Text,
}

#[derive(Clone, Debug)]
pub struct DoctorRequest {
    pub cwd: Option<PathBuf>,
    pub format: DoctorOutputFormat,
    pub app_version: String,
}

/// Render one local, read-only diagnostic report to stdout. Diagnostics are
/// intentionally never emitted to stderr because callers can parse one
/// complete text or JSON report without mixing it with progress output.
pub fn run(request: DoctorRequest) -> i32 {
    let report =
        collect_doctor_with_version(DoctorOptions { cwd: request.cwd }, request.app_version);
    let output = match request.format {
        DoctorOutputFormat::Json => match report.to_json() {
            Ok(json) => format!("{json}\n"),
            Err(error) => {
                // Serialization only contains owned scalar fields; still make
                // an unexpected failure explicit without falling into TUI.
                format!(
                    "{{\"schemaVersion\":1,\"error\":{}}}\n",
                    json_string(&error.to_string())
                )
            }
        },
        DoctorOutputFormat::Text => report.to_text(),
    };
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(output.as_bytes()).is_err() || stdout.flush().is_err() {
        return 1;
    }
    report.exit_code()
}

pub fn collect_doctor(options: DoctorOptions) -> DiagnosticReport {
    collect_doctor_with_version(options, env!("CARGO_PKG_VERSION"))
}

pub fn collect_doctor_with_version(
    options: DoctorOptions,
    version: impl Into<String>,
) -> DiagnosticReport {
    let requested = options
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canonical = requested.canonicalize().ok();
    let cwd = DiagnosticCwd {
        requested: requested.to_string_lossy().into_owned(),
        canonical: canonical
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    };
    let effective_cwd = canonical.as_deref().unwrap_or(&requested);

    let mut checks = Vec::new();
    let config_check = check_config(effective_cwd);
    let config_is_usable = config_check.status != DiagnosticStatus::Fail;
    checks.push(config_check);
    checks.push(check_credentials(config_is_usable));
    checks.push(check_model(effective_cwd, config_is_usable));
    checks.push(check_folder_trust(effective_cwd));
    checks.push(check_sandbox(effective_cwd));

    DiagnosticReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        package: CANONICAL_PACKAGE,
        website: CANONICAL_WEBSITE,
        version: version.into(),
        platform: HostPlatform::current().to_string(),
        cwd,
        checks,
    }
}

fn check_config(cwd: &Path) -> DiagnosticCheck {
    let path = file::user_config_path();
    let Some(path) = path else {
        return fail_check(
            "config",
            "cannot resolve the Orca configuration directory",
            Some("set ORCA_HOME to a writable configuration directory"),
        );
    };
    let source = path.to_string_lossy();
    if path.exists() {
        match fs::read_to_string(&path).and_then(|content| {
            toml::from_str::<FileConfig>(&content)
                .map(|_| ())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }) {
            Ok(()) => pass_check("config", format!("{source} (valid)")),
            Err(_) => fail_check(
                "config",
                format!("{source} has invalid TOML"),
                Some("fix the TOML syntax, then rerun `orca doctor`"),
            ),
        }
    } else {
        let detail = if file::project_config_path(cwd).exists() {
            format!(
                "{source} not found; project config exists but is loaded only for trusted folders"
            )
        } else {
            format!("{source} not found (built-in defaults are active)")
        };
        warn_check("config", detail, None)
    }
}

fn check_credentials(config_is_usable: bool) -> DiagnosticCheck {
    if std::env::var_os("ORCA_API_KEY").is_some_and(|value| !value.is_empty()) {
        return pass_check("credentials", "configured via ORCA_API_KEY");
    }
    if std::env::var_os("DEEPSEEK_API_KEY").is_some_and(|value| !value.is_empty()) {
        return pass_check("credentials", "configured via DEEPSEEK_API_KEY");
    }
    if let Some(path) = file::auth_path()
        && path.exists()
    {
        let has_key = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| {
                value
                    .get("DEEPSEEK_API_KEY")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| !value.trim().is_empty())
            })
            .unwrap_or(false);
        match has_key {
            true => {
                return pass_check("credentials", format!("configured via {}", path.display()));
            }
            false => {
                return fail_check(
                    "credentials",
                    format!(
                        "{} exists but has no usable DEEPSEEK_API_KEY",
                        path.display()
                    ),
                    Some("set ORCA_API_KEY or add DEEPSEEK_API_KEY to auth.json"),
                );
            }
        }
    }
    if config_is_usable
        && let Some(path) = file::user_config_path()
        && let Ok(content) = fs::read_to_string(&path)
        && let Ok(config) = toml::from_str::<FileConfig>(&content)
        && config.api_key.is_some_and(|value| !value.trim().is_empty())
    {
        return pass_check("credentials", format!("configured via {}", path.display()));
    }
    fail_check(
        "credentials",
        "no API key configured",
        Some("set ORCA_API_KEY or DEEPSEEK_API_KEY"),
    )
}

fn check_model(cwd: &Path, config_is_usable: bool) -> DiagnosticCheck {
    if !config_is_usable {
        return warn_check(
            "model",
            "not evaluated because the configuration is invalid",
            Some("fix the config check first, then rerun `orca doctor`"),
        );
    }
    match file::load_effective_config(cwd, ConfigOverrides::default()) {
        Ok(config) => {
            let model = config.model.as_deref().unwrap_or("auto");
            let base_url = config
                .base_url
                .as_deref()
                .unwrap_or("https://api.deepseek.com");
            pass_check("model", format!("{model}; endpoint {base_url}"))
        }
        Err(error) => fail_check(
            "model",
            format!("cannot resolve effective configuration: {error}"),
            Some("fix configuration errors, then rerun `orca doctor`"),
        ),
    }
}

fn check_folder_trust(cwd: &Path) -> DiagnosticCheck {
    match folder_trust::trust_level(cwd) {
        Some(TrustLevel::Trusted) => {
            pass_check("folder-trust", format!("{} is trusted", cwd.display()))
        }
        Some(TrustLevel::Untrusted) => fail_check(
            "folder-trust",
            format!(
                "{} is explicitly untrusted; restricted runs remain fail-closed",
                cwd.display()
            ),
            Some("run `orca trust add --cwd <PATH>` only after reviewing the folder"),
        ),
        None => warn_check(
            "folder-trust",
            format!(
                "{} has no trust decision; treated as untrusted",
                cwd.display()
            ),
            Some("run `orca trust add --cwd <PATH>` only after reviewing the folder"),
        ),
    }
}

fn check_sandbox(cwd: &Path) -> DiagnosticCheck {
    #[cfg(windows)]
    {
        let Some(home) = folder_trust::config_dir() else {
            return fail_check(
                "sandbox",
                "cannot resolve the Windows sandbox capability directory",
                Some("set ORCA_HOME and run the Windows sandbox setup helper"),
            );
        };
        let store = orca_windows_sandbox::CapabilityStore::new(home.join("windows-capabilities"));
        return match store
            .verify_setup_for_workspace(cwd, orca_windows_sandbox::SETUP_HELPER_VERSION)
        {
            Ok(_) => pass_check("sandbox", "Windows AppContainer capability setup is valid"),
            Err(error) => fail_check(
                "sandbox",
                format!("Windows sandbox setup is not ready: {error}"),
                Some("run the Windows installer with -SetupSandbox from this workspace"),
            ),
        };
    }
    #[cfg(not(windows))]
    let _ = cwd;
    match orca_tools::sandbox::enforcement_state() {
        EnforcementState::Enforced => {
            pass_check("sandbox", "OS-enforced restricted backend available")
        }
        EnforcementState::Advisory => warn_check(
            "sandbox",
            "restricted backend is advisory on this host",
            Some("use a host with an OS-enforced sandbox for restricted runs"),
        ),
        EnforcementState::Unavailable => fail_check(
            "sandbox",
            "no OS-enforced restricted backend is available",
            Some("install/enable the platform sandbox backend before running restricted tools"),
        ),
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"serialization failed\"".to_string())
}

fn pass_check(id: impl Into<String>, detail: impl Into<String>) -> DiagnosticCheck {
    DiagnosticCheck {
        id: id.into(),
        status: DiagnosticStatus::Pass,
        detail: detail.into(),
        remediation: None,
    }
}

fn warn_check(
    id: impl Into<String>,
    detail: impl Into<String>,
    remediation: Option<&str>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        id: id.into(),
        status: DiagnosticStatus::Warn,
        detail: detail.into(),
        remediation: remediation.map(str::to_string),
    }
}

fn fail_check(
    id: impl Into<String>,
    detail: impl Into<String>,
    remediation: Option<&str>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        id: id.into(),
        status: DiagnosticStatus::Fail,
        detail: detail.into(),
        remediation: remediation.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_serialization_redacts_credential_values() {
        let report = collect_doctor_with_version(DoctorOptions::default(), "test-version");
        let json = report.to_json().unwrap();
        assert!(!json.contains("DEEPSEEK_API_KEY=\""));
        assert_eq!(report.schema_version, DOCTOR_SCHEMA_VERSION);
        assert_eq!(report.package, CANONICAL_PACKAGE);
    }

    #[test]
    fn warnings_do_not_change_success_exit_code() {
        let report = DiagnosticReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            package: CANONICAL_PACKAGE,
            website: CANONICAL_WEBSITE,
            version: "test".to_string(),
            platform: "test".to_string(),
            cwd: DiagnosticCwd {
                requested: "/workspace".to_string(),
                canonical: None,
            },
            checks: vec![warn_check("folder-trust", "unknown", None)],
        };
        assert_eq!(report.exit_code(), 0);
    }
}
