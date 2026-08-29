use std::env;
use std::path::{Path, PathBuf};

use crate::PlatformError;
use crate::host::{HostPlatform, OperatingSystem};

use super::{PowerShellEdition, ShellKind, ShellSpec};

pub struct ShellResolver<P> {
    host: HostPlatform,
    probe: P,
}

impl<P> ShellResolver<P>
where
    P: Fn(&str) -> Option<PathBuf>,
{
    pub fn new(host: HostPlatform, probe: P) -> Self {
        Self { host, probe }
    }

    pub fn resolve(&self, explicit_override: Option<&str>) -> Result<ShellSpec, PlatformError> {
        if let Some(value) = explicit_override {
            return self.resolve_override(value);
        }
        match self.host.os {
            OperatingSystem::Windows => self.resolve_windows_default(),
            OperatingSystem::MacOs | OperatingSystem::Linux => self.resolve_unix_default(),
            _ => Err(PlatformError::UnsupportedHost {
                platform: self.host.clone(),
            }),
        }
    }

    pub fn resolve_from_environment(&self) -> Result<ShellSpec, PlatformError> {
        let override_value = env::var_os("ORCA_SHELL")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| PlatformError::InvalidShellOverride {
                        value: "<non-utf8>".to_string(),
                        reason: "ORCA_SHELL must be valid UTF-8".to_string(),
                    })
            })
            .transpose()?;
        self.resolve(override_value.as_deref())
    }

    fn resolve_override(&self, value: &str) -> Result<ShellSpec, PlatformError> {
        if value.trim().is_empty() {
            return Err(PlatformError::InvalidShellOverride {
                value: value.to_string(),
                reason: "the value is empty".to_string(),
            });
        }
        let executable =
            (self.probe)(value).ok_or_else(|| PlatformError::InvalidShellOverride {
                value: value.to_string(),
                reason: "the executable does not exist or is not available".to_string(),
            })?;
        let kind = match self.host.os {
            OperatingSystem::Windows => windows_override_kind(value)?,
            OperatingSystem::MacOs | OperatingSystem::Linux => ShellKind::Posix,
            _ => {
                return Err(PlatformError::UnsupportedHost {
                    platform: self.host.clone(),
                });
            }
        };
        Ok(ShellSpec::new(executable, kind))
    }

    fn resolve_windows_default(&self) -> Result<ShellSpec, PlatformError> {
        // Windows PowerShell 5.1 enters ConstrainedLanguage inside Orca's
        // AppContainer sandbox, which cannot satisfy the general shell
        // contract. Prefer cmd.exe as the built-in restricted-mode fallback;
        // callers can still select powershell.exe explicitly for full access.
        for (candidate, kind) in [
            ("pwsh.exe", ShellKind::PowerShell(PowerShellEdition::Core)),
            ("cmd.exe", ShellKind::Cmd),
            (
                "powershell.exe",
                ShellKind::PowerShell(PowerShellEdition::Windows),
            ),
        ] {
            if let Some(executable) = (self.probe)(candidate) {
                return Ok(ShellSpec::new(executable, kind));
            }
        }
        Err(PlatformError::ExecutableNotFound {
            executable: "pwsh.exe, cmd.exe, or powershell.exe".to_string(),
        })
    }

    fn resolve_unix_default(&self) -> Result<ShellSpec, PlatformError> {
        (self.probe)("sh")
            .map(|executable| ShellSpec::new(executable, ShellKind::Posix))
            .ok_or_else(|| PlatformError::ExecutableNotFound {
                executable: "sh".to_string(),
            })
    }
}

impl ShellResolver<fn(&str) -> Option<PathBuf>> {
    pub fn for_current_host() -> Self {
        Self::new(HostPlatform::current(), find_executable)
    }
}

fn windows_override_kind(value: &str) -> Result<ShellKind, PlatformError> {
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    match name.to_ascii_lowercase().as_str() {
        "pwsh" | "pwsh.exe" => Ok(ShellKind::PowerShell(PowerShellEdition::Core)),
        "powershell" | "powershell.exe" => Ok(ShellKind::PowerShell(PowerShellEdition::Windows)),
        "cmd" | "cmd.exe" => Ok(ShellKind::Cmd),
        "bash" | "bash.exe" => Ok(ShellKind::GitBash),
        _ => Err(PlatformError::InvalidShellOverride {
            value: value.to_string(),
            reason: "expected pwsh.exe, powershell.exe, cmd.exe, or explicitly selected bash.exe"
                .to_string(),
        }),
    }
}

fn find_executable(candidate: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(candidate);
    if candidate_path.components().count() > 1 {
        return absolute_existing_path(candidate_path);
    }
    let current_directory = env::current_dir().ok();
    let from_path = env::var_os("PATH")
        .into_iter()
        .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .find_map(|directory| {
            let candidate_path = directory.join(candidate);
            if current_directory
                .as_deref()
                .is_some_and(|cwd| is_current_directory_executable(&candidate_path, cwd))
            {
                return None;
            }
            absolute_existing_path(&candidate_path)
        });
    from_path.or_else(|| find_standard_windows_executable(candidate, current_directory.as_deref()))
}

#[cfg(windows)]
fn find_standard_windows_executable(candidate: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let program_files = env::var_os("ProgramFiles").map(PathBuf::from);
    let system_root = env::var_os("SystemRoot").map(PathBuf::from);
    let comspec = env::var_os("COMSPEC").map(PathBuf::from);
    standard_windows_executable_candidates(
        candidate,
        program_files.as_deref(),
        system_root.as_deref(),
        comspec.as_deref(),
    )
    .into_iter()
    .find_map(|candidate_path| {
        if cwd.is_some_and(|cwd| is_current_directory_executable(&candidate_path, cwd)) {
            return None;
        }
        absolute_existing_path(&candidate_path)
    })
}

#[cfg(not(windows))]
fn find_standard_windows_executable(_candidate: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn standard_windows_executable_candidates(
    candidate: &str,
    program_files: Option<&Path>,
    system_root: Option<&Path>,
    comspec: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    match candidate.to_ascii_lowercase().as_str() {
        "pwsh" | "pwsh.exe" => {
            if let Some(program_files) = program_files {
                candidates.push(program_files.join("PowerShell").join("7").join("pwsh.exe"));
            }
            candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
        }
        "powershell" | "powershell.exe" => {
            if let Some(system_root) = system_root {
                candidates.push(
                    system_root
                        .join("System32")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join("powershell.exe"),
                );
            }
            candidates.push(PathBuf::from(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            ));
        }
        "cmd" | "cmd.exe" => {
            if let Some(comspec) = comspec {
                candidates.push(comspec.to_path_buf());
            }
            if let Some(system_root) = system_root {
                candidates.push(system_root.join("System32").join("cmd.exe"));
            }
            candidates.push(PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        }
        _ => {}
    }
    candidates
}

/// Resolve a bare child-process name to an absolute executable path before a
/// broker clears the child environment. On Windows this also handles
/// `.cmd`/`.bat` launchers exposed through `PATHEXT` (for example npm's
/// `npx.cmd`).
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return None;
    }
    let resolved = find_executable(program);
    #[cfg(windows)]
    {
        resolved.or_else(|| {
            env::var_os("PATHEXT")
                .into_iter()
                .flat_map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .map(|extension| format!("{program}{extension}"))
                .find_map(|candidate| find_executable(&candidate))
        })
    }
    #[cfg(not(windows))]
    {
        resolved
    }
}

#[cfg(all(test, windows))]
fn plan_program(
    program: &str,
    is_windows: bool,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if !is_windows || program.contains('/') || program.contains('\\') {
        return None;
    }
    resolve(program)
}

fn is_current_directory_executable(path: &Path, cwd: &Path) -> bool {
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let current = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    #[cfg(windows)]
    {
        let candidate = candidate
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let current = current
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        return candidate == current || candidate.starts_with(&format!("{current}\\"));
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(&current)
    }
}

fn absolute_existing_path(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    std::fs::canonicalize(path).ok().or_else(|| {
        if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            env::current_dir().ok().map(|cwd| cwd.join(path))
        }
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{
        is_current_directory_executable, plan_program, standard_windows_executable_candidates,
    };
    use std::path::Path;

    #[test]
    fn shell_lookup_rejects_current_directory_and_descendants() {
        let cwd = Path::new(r"C:\Work\repo");
        assert!(is_current_directory_executable(
            Path::new(r"C:\Work\repo\pwsh.exe"),
            cwd,
        ));
        assert!(is_current_directory_executable(
            Path::new(r"C:\Work\repo\tools\cmd.exe"),
            cwd,
        ));
        assert!(!is_current_directory_executable(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            cwd,
        ));
    }

    #[test]
    fn bare_windows_launcher_can_be_resolved_without_touching_absolute_paths() {
        let resolved = plan_program("npx", true, |program| {
            assert_eq!(program, "npx");
            Some(Path::new(r"C:\Node\npx.cmd").to_path_buf())
        });
        assert_eq!(resolved, Some(Path::new(r"C:\Node\npx.cmd").to_path_buf()));
        assert_eq!(
            plan_program(r"C:\Node\npx.cmd", true, |_| {
                panic!("absolute launcher paths must not be resolved")
            }),
            None,
        );
    }

    #[test]
    fn native_shell_fallbacks_use_standard_windows_install_roots() {
        let program_files = Path::new(r"D:\Program Files");
        let system_root = Path::new(r"D:\Windows");
        let comspec = Path::new(r"D:\Windows\System32\cmd.exe");

        assert_eq!(
            standard_windows_executable_candidates(
                "pwsh.exe",
                Some(program_files),
                Some(system_root),
                Some(comspec),
            )[0],
            Path::new(r"D:\Program Files\PowerShell\7\pwsh.exe")
        );
        assert_eq!(
            standard_windows_executable_candidates(
                "powershell.exe",
                Some(program_files),
                Some(system_root),
                Some(comspec),
            )[0],
            Path::new(r"D:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        );
        assert_eq!(
            standard_windows_executable_candidates(
                "cmd.exe",
                Some(program_files),
                Some(system_root),
                Some(comspec),
            )[0],
            comspec
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::resolve_program;

    #[test]
    fn bare_program_resolves_to_an_absolute_path_for_cleared_child_environments() {
        let resolved = resolve_program("sh").expect("the POSIX shell must be discoverable");
        assert!(resolved.is_absolute());
        assert!(resolved.is_file());
    }
}
