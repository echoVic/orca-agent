use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl CommandSpec {
    /// The process this describes. A cmd.exe script goes on the command line
    /// as written: see [`cmd_script`].
    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        #[cfg(windows)]
        if let Some(script) = cmd_script(&self.program, &self.args) {
            use std::os::windows::process::CommandExt;
            command.args(&self.args[..self.args.len() - 1]);
            command.raw_arg(quoted_cmd_script(script));
            return command;
        }
        command.args(&self.args);
        command
    }
}

/// The script of a `cmd.exe /S /C <script>` command line, when `args` end in
/// one.
///
/// cmd.exe does not read its script the way other programs read an argument:
/// with `/S` it drops the first and the last quote of what follows `/C` and
/// runs the rest as typed. An argument quoted the usual way reaches it with a
/// backslash before every quote of the script, so `echo x > "a b.txt"` names
/// a file cmd.exe cannot create. Every command line built for cmd.exe puts
/// the script between one pair of quotes and leaves it otherwise untouched.
pub fn cmd_script<'a>(program: &Path, args: &'a [OsString]) -> Option<&'a OsStr> {
    let name = program.file_name()?.to_str()?;
    if !name.eq_ignore_ascii_case("cmd.exe") && !name.eq_ignore_ascii_case("cmd") {
        return None;
    }
    let is = |arg: &OsString, switch: &str| {
        arg.to_str()
            .is_some_and(|arg| arg.eq_ignore_ascii_case(switch))
    };
    let (script, switches) = args.split_last()?;
    let (last, earlier) = switches.split_last()?;
    (is(last, "/C") && earlier.iter().any(|arg| is(arg, "/S"))).then_some(script.as_os_str())
}

/// `script` as cmd.exe reads it after `/S /C`.
pub fn quoted_cmd_script(script: &OsStr) -> OsString {
    let mut quoted = OsString::from("\"");
    quoted.push(script);
    quoted.push("\"");
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_cmd_script_is_the_argument_after_s_and_c() {
        let script = r#"echo x > "a b.txt""#;
        // Named by file name: a Windows directory is only one on Windows.
        let cmd = Path::new("cmd.exe");
        assert_eq!(
            cmd_script(cmd, &args(&["/D", "/S", "/C", script])),
            Some(OsStr::new(script))
        );
        #[cfg(windows)]
        assert_eq!(
            cmd_script(
                Path::new(r"C:\Windows\System32\cmd.exe"),
                &args(&["/D", "/S", "/C", script])
            ),
            Some(OsStr::new(script))
        );
        assert_eq!(
            cmd_script(Path::new("CMD"), &args(&["/s", "/c", script])),
            Some(OsStr::new(script))
        );
        assert_eq!(
            quoted_cmd_script(OsStr::new(script)),
            OsString::from(r#""echo x > "a b.txt"""#)
        );
    }

    #[test]
    fn other_command_lines_are_not_cmd_scripts() {
        let cmd = Path::new("cmd.exe");
        // Without /S cmd.exe keeps the quotes of some scripts and drops them
        // from others; only the /S form has one reading.
        assert_eq!(cmd_script(cmd, &args(&["/D", "/C", "echo x"])), None);
        assert_eq!(cmd_script(cmd, &args(&["/S", "/C"])), None);
        assert_eq!(cmd_script(cmd, &args(&["/S", "/C", "echo", "x"])), None);
        assert_eq!(
            cmd_script(Path::new("pwsh.exe"), &args(&["/S", "/C", "echo x"])),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_cmd_script_runs_with_its_own_quotes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let spec = CommandSpec {
            program: PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC")),
            args: args(&["/D", "/S", "/C", r#"echo quoted> "a b.txt""#]),
        };
        let output = spec
            .to_command()
            .current_dir(dir.path())
            .output()
            .expect("run cmd.exe");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a b.txt"))
                .expect("the file the script names")
                .trim(),
            "quoted"
        );
    }
}
