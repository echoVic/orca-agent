mod command;
mod resolve;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use command::CommandSpec;
pub use resolve::{ShellResolver, resolve_program, resolve_program_in};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PowerShellEdition {
    Core,
    Windows,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShellKind {
    Posix,
    PowerShell(PowerShellEdition),
    Cmd,
    GitBash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSpec {
    executable: PathBuf,
    kind: ShellKind,
}

impl ShellSpec {
    pub(crate) fn new(executable: PathBuf, kind: ShellKind) -> Self {
        Self { executable, kind }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn kind(&self) -> ShellKind {
        self.kind
    }

    pub fn command(&self, script: &str) -> CommandSpec {
        CommandSpec {
            program: self.executable.clone(),
            args: self.command_args(script),
        }
    }

    pub fn tool_name(&self) -> &'static str {
        match self.kind {
            ShellKind::PowerShell(_) => "powershell",
            ShellKind::Cmd => "cmd",
            ShellKind::Posix | ShellKind::GitBash => "bash",
        }
    }

    pub fn prompt_dialect(&self) -> &'static str {
        match self.kind {
            ShellKind::PowerShell(PowerShellEdition::Core) => {
                "PowerShell 7 syntax. Use Windows paths and PowerShell cmdlets."
            }
            ShellKind::PowerShell(PowerShellEdition::Windows) => {
                "Windows PowerShell 5.1 syntax. Do not use PowerShell 7-only operators such as && or ||."
            }
            ShellKind::Cmd => {
                "cmd.exe syntax. Use Windows paths, built-in cmd commands, and cmd quoting rules."
            }
            ShellKind::Posix => "POSIX shell syntax.",
            ShellKind::GitBash => "Git Bash POSIX shell syntax running explicitly on Windows.",
        }
    }

    fn command_args(&self, script: &str) -> Vec<OsString> {
        match self.kind {
            ShellKind::Posix | ShellKind::GitBash => {
                vec![OsString::from("-c"), OsString::from(script)]
            }
            ShellKind::PowerShell(PowerShellEdition::Core) => vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(format!(
                    "if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') {{ \
                         [Console]::InputEncoding = [Text.UTF8Encoding]::new($false); \
                         [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
                         $OutputEncoding = [Console]::OutputEncoding \
                     }}; {script}"
                )),
            ],
            ShellKind::PowerShell(PowerShellEdition::Windows) => vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(format!(
                    "if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') {{ \
                         $orcaUtf8 = New-Object System.Text.UTF8Encoding $false; \
                         [Console]::InputEncoding = $orcaUtf8; \
                         [Console]::OutputEncoding = $orcaUtf8; \
                         $OutputEncoding = $orcaUtf8 \
                     }}; {script}"
                )),
            ],
            ShellKind::Cmd => vec![
                OsString::from("/D"),
                OsString::from("/S"),
                OsString::from("/C"),
                OsString::from(script),
            ],
        }
    }
}
