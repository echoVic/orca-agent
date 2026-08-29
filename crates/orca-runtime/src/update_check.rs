use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orca_core::capability::CapabilitySet;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use serde::{Deserialize, Serialize};

use orca_platform::host::{HostPlatform, OperatingSystem};

const RELEASES_URL: &str = "https://api.github.com/repos/echoVic/orca-agent/releases/latest";
const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/@blade-ai/orca/latest";
const ORCA_HOME_ENV: &str = "ORCA_HOME";
const UPDATE_CACHE_FILE: &str = "update-cache.json";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdatePreflight {
    Continue,
    Prompt(UpdateInfo),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateAction {
    NpmGlobalLatest,
    StandaloneInstaller { install_dir: Option<PathBuf> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateCommand {
    pub program: &'static str,
    pub args: Vec<String>,
    pub display: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRunOutcome {
    Updated,
    Started,
    Failed(Option<i32>),
    StartFailed(String),
}

impl UpdateAction {
    pub fn command(&self) -> UpdateCommand {
        self.command_for_os(HostPlatform::current().os)
    }

    fn command_for_os(&self, os: OperatingSystem) -> UpdateCommand {
        match self {
            Self::NpmGlobalLatest if os == OperatingSystem::Windows => windows_npm_update_command(),
            Self::NpmGlobalLatest => UpdateCommand {
                program: "npm",
                args: vec![
                    "install".to_string(),
                    "-g".to_string(),
                    "@blade-ai/orca@latest".to_string(),
                    "--registry".to_string(),
                    "https://registry.npmjs.org".to_string(),
                ],
                display:
                    "npm install -g @blade-ai/orca@latest --registry https://registry.npmjs.org"
                        .to_string(),
            },
            Self::StandaloneInstaller { install_dir } => {
                standalone_update_command_for_os(install_dir.clone(), os)
            }
        }
    }

    pub fn command_display(&self) -> String {
        self.command().display
    }
}

pub fn current_update_action() -> UpdateAction {
    let current_exe = std::env::current_exe().ok();
    update_action_from_env_and_exe(|name| std::env::var_os(name), current_exe.as_deref())
}

fn update_action_from_env_and_exe(
    get_env: impl Fn(&str) -> Option<std::ffi::OsString>,
    current_exe: Option<&Path>,
) -> UpdateAction {
    if get_env("ORCA_MANAGED_BY_NPM").is_some() {
        UpdateAction::NpmGlobalLatest
    } else {
        UpdateAction::StandaloneInstaller {
            install_dir: current_exe.and_then(|path| path.parent().map(Path::to_path_buf)),
        }
    }
}

fn standalone_update_command_for_os(
    install_dir: Option<PathBuf>,
    os: OperatingSystem,
) -> UpdateCommand {
    if os == OperatingSystem::Windows {
        return windows_standalone_update_command(install_dir);
    }

    let script = if install_dir.is_some() {
        "tmp=$(mktemp) && trap 'rm -f \"$tmp\"' EXIT INT TERM && curl -fsSL https://orcaagent.dev/install.sh -o \"$tmp\" && ORCA_NON_INTERACTIVE=1 INSTALL_DIR=\"$1\" sh \"$tmp\""
    } else {
        "tmp=$(mktemp) && trap 'rm -f \"$tmp\"' EXIT INT TERM && curl -fsSL https://orcaagent.dev/install.sh -o \"$tmp\" && ORCA_NON_INTERACTIVE=1 sh \"$tmp\""
    };
    let mut args = vec![
        "-c".to_string(),
        script.to_string(),
        "orca-update".to_string(),
    ];
    let display = if let Some(install_dir) = install_dir {
        args.push(install_dir.display().to_string());
        format!(
            "curl -fsSL https://orcaagent.dev/install.sh -o <tmp> && ORCA_NON_INTERACTIVE=1 INSTALL_DIR={} sh <tmp>",
            install_dir.display()
        )
    } else {
        "curl -fsSL https://orcaagent.dev/install.sh -o <tmp> && ORCA_NON_INTERACTIVE=1 sh <tmp>"
            .to_string()
    };

    UpdateCommand {
        program: "sh",
        args,
        display,
    }
}

fn windows_powershell_args(script: String) -> Vec<String> {
    vec![
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-Command".to_string(),
        script,
    ]
}

fn powershell_single_quoted(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

fn windows_standalone_update_command(install_dir: Option<PathBuf>) -> UpdateCommand {
    let install_dir_arg = install_dir
        .as_deref()
        .map(|path| format!(" -InstallDir {}", powershell_single_quoted(path)))
        .unwrap_or_default();
    let script = format!(
        "$ErrorActionPreference = 'Stop'; $tmp = [System.IO.Path]::GetTempFileName(); try {{ Invoke-WebRequest -UseBasicParsing -Uri 'https://orcaagent.dev/install.ps1' -OutFile $tmp; & $tmp{install_dir_arg} -WaitForPid {} -NonInteractive; if (-not $?) {{ exit 1 }} }} finally {{ Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }}",
        std::process::id()
    );
    let display = match install_dir {
        Some(path) => format!(
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File <downloaded install.ps1> -InstallDir {} -WaitForPid <orca-pid> -NonInteractive",
            path.display()
        ),
        None => "powershell.exe -NoProfile -ExecutionPolicy Bypass -File <downloaded install.ps1> -WaitForPid <orca-pid> -NonInteractive"
            .to_string(),
    };

    UpdateCommand {
        program: "powershell.exe",
        args: windows_powershell_args(script),
        display,
    }
}

fn windows_npm_update_command() -> UpdateCommand {
    let script = format!(
        "$ErrorActionPreference = 'Stop'; Wait-Process -Id {} -ErrorAction SilentlyContinue; npm install -g '@blade-ai/orca@latest' --registry 'https://registry.npmjs.org'; exit $LASTEXITCODE",
        std::process::id()
    );

    UpdateCommand {
        program: "powershell.exe",
        args: windows_powershell_args(script),
        display: "npm install -g @blade-ai/orca@latest --registry https://registry.npmjs.org"
            .to_string(),
    }
}

pub fn is_dev_build_run() -> bool {
    cfg!(debug_assertions)
        || std::env::current_exe()
            .ok()
            .is_some_and(|exe| exe_in_cargo_target(&exe))
}

fn exe_in_cargo_target(exe: &Path) -> bool {
    exe.ancestors()
        .any(|ancestor| ancestor.file_name().is_some_and(|name| name == "target"))
}

pub fn update_preflight(enabled: bool, current_version: &str) -> UpdatePreflight {
    update_preflight_with(enabled && !is_dev_build_run(), current_version, |version| {
        check_latest_for_prompt(version)
    })
}

fn update_preflight_with(
    enabled: bool,
    current_version: &str,
    check_latest: impl FnOnce(&str) -> Result<Option<UpdateInfo>, String>,
) -> UpdatePreflight {
    if !enabled {
        return UpdatePreflight::Continue;
    }
    match check_latest(current_version) {
        Ok(Some(info)) => UpdatePreflight::Prompt(info),
        Ok(None) | Err(_) => UpdatePreflight::Continue,
    }
}

pub fn run_update(action: &UpdateAction) -> UpdateRunOutcome {
    let update_command = action.command();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let broker = ExecutionBroker::with_backend(
        orca_core::capability::EnforcementState::Advisory,
        "update-user-trusted",
    );
    let launched = match broker.launch_user_trusted(
        {
            let mut command = Command::new(update_command.program);
            command.args(&update_command.args);
            command
        },
        "update-check",
        cwd,
        CapabilitySet::read_only(),
    ) {
        Ok(launched) => launched,
        Err(LaunchError::Spawn(error)) => return UpdateRunOutcome::StartFailed(error.to_string()),
        Err(error) => return UpdateRunOutcome::StartFailed(format!("{error:?}")),
    };
    let mut child = launched.child;
    if cfg!(windows) {
        return UpdateRunOutcome::Started;
    }

    match child.wait() {
        Ok(status) if status.success() => UpdateRunOutcome::Updated,
        Ok(status) => UpdateRunOutcome::Failed(status.code()),
        Err(error) => UpdateRunOutcome::StartFailed(error.to_string()),
    }
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct NpmPackage {
    version: String,
}

pub fn check_latest(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    match check_latest_npm(current_version) {
        Ok(result) => Ok(result),
        Err(_) => check_latest_github(current_version),
    }
}

fn check_latest_npm(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let response = reqwest::blocking::Client::new()
        .get(NPM_REGISTRY_URL)
        .header("User-Agent", "orca-update-check")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .map_err(|error| format!("npm registry check failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("npm registry returned HTTP {}", response.status()));
    }
    let pkg: NpmPackage = response
        .json()
        .map_err(|error| format!("invalid npm registry response: {error}"))?;
    let latest = normalize_version(&pkg.version);
    let current = normalize_version(current_version);
    if !is_newer_version(&latest, &current) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        current,
        url: format!("https://github.com/echoVic/orca-agent/releases/tag/v{latest}"),
        latest,
    }))
}

fn check_latest_github(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let response = reqwest::blocking::Client::new()
        .get(RELEASES_URL)
        .header("User-Agent", "orca-update-check")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .map_err(|error| format!("failed to check latest release: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("release check returned HTTP {}", response.status()));
    }
    let release: GitHubRelease = response
        .json()
        .map_err(|error| format!("invalid release response: {error}"))?;
    Ok(update_info_from_release(current_version, release))
}

pub fn check_latest_for_prompt(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let Some(info) = check_latest(current_version)? else {
        return Ok(None);
    };
    if should_prompt_for_update(&info, read_update_cache().skip_until_version.as_deref()) {
        Ok(Some(info))
    } else {
        Ok(None)
    }
}

pub fn dismiss_version(version: &str) -> Result<(), String> {
    write_update_cache(&UpdatePromptCache {
        skip_until_version: Some(normalize_version(version)),
    })
}

fn update_info_from_release(current_version: &str, release: GitHubRelease) -> Option<UpdateInfo> {
    let latest = normalize_version(&release.tag_name);
    let current = normalize_version(current_version);
    if !is_newer_version(&latest, &current) {
        return None;
    }
    Some(UpdateInfo {
        current,
        latest,
        url: release.html_url,
    })
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_semver_core(latest), parse_semver_core(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => latest != current,
    }
}

fn parse_semver_core(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .split_once('-')
        .map(|(core, _)| core)
        .unwrap_or(version);
    let core = core.split_once('+').map(|(core, _)| core).unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn should_prompt_for_update(info: &UpdateInfo, skip_until_version: Option<&str>) -> bool {
    match skip_until_version {
        Some(skipped) => is_newer_version(&info.latest, &normalize_version(skipped)),
        None => true,
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UpdatePromptCache {
    skip_until_version: Option<String>,
}

fn read_update_cache() -> UpdatePromptCache {
    let Some(path) = update_cache_path() else {
        return UpdatePromptCache::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return UpdatePromptCache::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn write_update_cache(cache: &UpdatePromptCache) -> Result<(), String> {
    let Some(path) = update_cache_path() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create update cache directory: {error}"))?;
    }
    let contents = serde_json::to_string_pretty(cache)
        .map_err(|error| format!("failed to serialize update cache: {error}"))?;
    fs::write(path, format!("{contents}\n"))
        .map_err(|error| format!("failed to write update cache: {error}"))
}

fn update_cache_path() -> Option<PathBuf> {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join(UPDATE_CACHE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_prompts_when_update_is_available() {
        let outcome = update_preflight_with(true, "0.1.7", |_| {
            Ok(Some(UpdateInfo {
                current: "0.1.7".to_string(),
                latest: "0.1.8".to_string(),
                url: "https://example.test/releases/tag/v0.1.8".to_string(),
            }))
        });

        assert!(matches!(
            outcome,
            UpdatePreflight::Prompt(UpdateInfo { latest, .. }) if latest == "0.1.8"
        ));
    }

    #[test]
    fn preflight_continues_when_disabled_or_check_fails() {
        assert_eq!(
            update_preflight_with(false, "0.1.7", |_| {
                panic!("disabled update check must not run")
            }),
            UpdatePreflight::Continue
        );
        assert_eq!(
            update_preflight_with(true, "0.1.7", |_| Err("offline".to_string())),
            UpdatePreflight::Continue
        );
    }

    #[test]
    fn update_action_uses_npm_when_launched_from_npm_wrapper() {
        let action = update_action_from_env_and_exe(
            |name| match name {
                "ORCA_MANAGED_BY_NPM" => Some("1".into()),
                _ => None,
            },
            Some(std::path::Path::new("/custom/bin/orca")),
        );

        assert_eq!(
            action.command().display,
            "npm install -g @blade-ai/orca@latest --registry https://registry.npmjs.org"
        );
    }

    #[test]
    fn update_action_reruns_standalone_installer_for_current_executable_dir() {
        let action = update_action_from_env_and_exe(
            |_| None,
            Some(std::path::Path::new("/custom/bin/orca")),
        );

        assert_eq!(
            action.command_for_os(OperatingSystem::Linux).display,
            "curl -fsSL https://orcaagent.dev/install.sh -o <tmp> && ORCA_NON_INTERACTIVE=1 INSTALL_DIR=/custom/bin sh <tmp>"
        );
    }

    #[test]
    fn standalone_update_command_downloads_before_running_installer() {
        let action = update_action_from_env_and_exe(
            |_| None,
            Some(std::path::Path::new("/custom/bin/orca")),
        );
        let command = action.command_for_os(OperatingSystem::Linux);

        assert_eq!(command.program, "sh");
        assert!(command.args.iter().any(|arg| arg.contains("mktemp")));
        assert!(command.args.iter().any(|arg| {
            arg.contains("curl -fsSL https://orcaagent.dev/install.sh -o \"$tmp\"")
        }));
        assert!(command.args.iter().any(|arg| {
            arg.contains("&& ORCA_NON_INTERACTIVE=1 INSTALL_DIR=\"$1\" sh \"$tmp\"")
        }));
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg.contains("| ORCA_NON_INTERACTIVE"))
        );
    }

    #[test]
    fn windows_standalone_update_uses_downloaded_powershell_installer() {
        let command = standalone_update_command_for_os(
            Some(std::path::PathBuf::from(r"C:\Program Files\O'rka\bin")),
            orca_platform::host::OperatingSystem::Windows,
        );

        assert_eq!(command.program, "powershell.exe");
        assert!(command.args.iter().any(|arg| {
            arg.contains("Invoke-WebRequest")
                && arg.contains("https://orcaagent.dev/install.ps1")
                && arg.contains("-OutFile $tmp")
        }));
        assert!(command.args.iter().any(|arg| {
            arg.contains("& $tmp -InstallDir 'C:\\Program Files\\O''rka\\bin'")
                && arg.contains("-WaitForPid")
                && arg.contains("-NonInteractive")
        }));
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg.contains("Remove-Item -LiteralPath $tmp"))
        );
        assert!(!command.args.iter().any(|arg| arg.contains("install.sh")));
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg.contains("Invoke-Expression"))
        );
    }

    #[test]
    fn windows_npm_update_waits_for_running_orca_before_replacing_package() {
        let command = UpdateAction::NpmGlobalLatest
            .command_for_os(orca_platform::host::OperatingSystem::Windows);

        assert_eq!(command.program, "powershell.exe");
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg.contains("Wait-Process -Id"))
        );
        assert!(command.args.iter().any(|arg| {
            arg.contains(
                "npm install -g '@blade-ai/orca@latest' --registry 'https://registry.npmjs.org'",
            )
        }));
    }

    #[test]
    fn development_executables_are_not_update_eligible() {
        assert!(exe_in_cargo_target(std::path::Path::new(
            "/repo/target/debug/orca"
        )));
        assert!(exe_in_cargo_target(std::path::Path::new(
            "/repo/target/aarch64-apple-darwin/release/orca"
        )));
        assert!(!exe_in_cargo_target(std::path::Path::new(
            "/Users/dev/.orca/bin/orca"
        )));
    }

    #[test]
    fn normalize_version_strips_v_prefix() {
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version(" 0.1.0 "), "0.1.0");
    }

    #[test]
    fn release_equal_to_current_version_is_not_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.6".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.6".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.6", release), None);
    }

    #[test]
    fn release_older_than_current_version_is_not_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.6".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.6".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.7", release), None);
    }

    #[test]
    fn release_newer_than_current_version_is_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.7".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.7".to_string(),
        };

        assert_eq!(
            update_info_from_release("0.1.6", release),
            Some(UpdateInfo {
                current: "0.1.6".to_string(),
                latest: "0.1.7".to_string(),
                url: "https://example.test/releases/tag/v0.1.7".to_string(),
            })
        );
    }

    #[test]
    fn prerelease_suffix_does_not_force_a_false_update_for_same_core_version() {
        let release = GitHubRelease {
            tag_name: "v0.1.7".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.7".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.7-dev", release), None);
    }

    #[test]
    fn skipped_version_suppresses_prompt_until_newer_release_exists() {
        let info = UpdateInfo {
            current: "0.1.7".to_string(),
            latest: "0.1.8".to_string(),
            url: "https://example.test/releases/tag/v0.1.8".to_string(),
        };

        assert!(!should_prompt_for_update(&info, Some("0.1.8")));
        assert!(!should_prompt_for_update(&info, Some("0.1.9")));
        assert!(should_prompt_for_update(&info, Some("0.1.7")));
        assert!(should_prompt_for_update(&info, None));
    }
}
