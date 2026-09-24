#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::process::Command;

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, INVALID_HANDLE_VALUE, STILL_ACTIVE,
        SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Console::{COORD, HPCON};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    use crate::process::ProcessJob;

    const INFINITE: u32 = 0xffff_ffff;
    const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002_000d;
    const KERNEL32_DLL: &[u16] = &[
        b'k' as u16,
        b'e' as u16,
        b'r' as u16,
        b'n' as u16,
        b'e' as u16,
        b'l' as u16,
        b'3' as u16,
        b'2' as u16,
        b'.' as u16,
        b'd' as u16,
        b'l' as u16,
        b'l' as u16,
        0,
    ];

    pub fn windows_pty_supported() -> bool {
        conpty_api().is_ok()
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PtyExitStatus {
        code: u32,
    }

    impl PtyExitStatus {
        pub fn code(self) -> Option<i32> {
            i32::try_from(self.code).ok()
        }

        pub fn success(self) -> bool {
            self.code == 0
        }
    }

    pub struct WindowsPtyChild {
        process: OwnedHandle,
        pid: u32,
    }

    impl WindowsPtyChild {
        pub fn id(&self) -> io::Result<u32> {
            Ok(self.pid)
        }

        pub fn try_wait(&mut self) -> io::Result<Option<PtyExitStatus>> {
            let mut code = 0u32;
            if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if code == STILL_ACTIVE as u32 {
                Ok(None)
            } else {
                Ok(Some(PtyExitStatus { code }))
            }
        }

        pub fn wait(&mut self) -> io::Result<PtyExitStatus> {
            if unsafe { WaitForSingleObject(self.process.raw(), INFINITE) } != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            self.try_wait()?
                .ok_or_else(|| io::Error::other("ConPTY child remained active after wait"))
        }

        pub fn kill(&mut self) -> io::Result<()> {
            if unsafe { TerminateProcess(self.process.raw(), 137) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    pub struct WindowsPtyInput {
        console: Option<PseudoConsole>,
        writer: Option<std::fs::File>,
        closed: bool,
    }

    impl WindowsPtyInput {
        pub fn write_all(&mut self, input: &[u8]) -> io::Result<()> {
            if self.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "ConPTY input is closed",
                ));
            }
            let writer = self.writer.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "ConPTY input is closed")
            })?;
            writer.write_all(input)?;
            writer.flush()
        }

        pub fn close(&mut self) {
            // Closing the input pipe while the child is active can turn a
            // normal ConPTY exit into an interrupt status. Send the Windows
            // console EOF sequence, then retain the pipe and pseudoconsole so
            // resize remains available while the child drains and exits.
            if !self.closed
                && let Some(writer) = self.writer.as_mut()
            {
                let _ = writer.write_all(b"\x1a\r\n");
                let _ = writer.flush();
            }
            self.closed = true;
        }

        pub fn close_terminal(&mut self) {
            self.closed = true;
            self.writer.take();
            self.console.take();
        }

        pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            self.console
                .as_ref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ConPTY is closed"))?
                .resize(cols, rows)
        }
    }

    pub struct SpawnedWindowsPty {
        pub child: WindowsPtyChild,
        pub process_job: ProcessJob,
        pub input: WindowsPtyInput,
        pub reader: Box<dyn Read + Send>,
    }

    pub fn spawn_windows_pty(
        command: &Command,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> io::Result<SpawnedWindowsPty> {
        spawn_windows_pty_with_job(command, cols, rows, None, false)
    }

    pub fn spawn_windows_pty_detached(
        command: &Command,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> io::Result<SpawnedWindowsPty> {
        spawn_windows_pty_with_job(command, cols, rows, None, true)
    }

    pub fn spawn_windows_pty_named(
        command: &Command,
        cols: Option<u16>,
        rows: Option<u16>,
        name: &str,
    ) -> io::Result<SpawnedWindowsPty> {
        spawn_windows_pty_with_job(command, cols, rows, Some(name), false)
    }

    fn spawn_windows_pty_with_job(
        command: &Command,
        cols: Option<u16>,
        rows: Option<u16>,
        job_name: Option<&str>,
        detached: bool,
    ) -> io::Result<SpawnedWindowsPty> {
        let pty = PtyPipeSet::new(cols, rows)?;
        let process_job = if detached {
            ProcessJob::create_unassigned_detached(job_name)?
        } else {
            ProcessJob::create_unassigned(job_name)?
        };
        let mut attributes = ProcessAttributeList::new(2)?;
        attributes.set_pseudo_console(pty.console.raw())?;
        attributes.set_job(process_job.raw_handle())?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: INVALID_HANDLE_VALUE,
                hStdOutput: INVALID_HANDLE_VALUE,
                hStdError: INVALID_HANDLE_VALUE,
                ..unsafe { std::mem::zeroed() }
            },
            lpAttributeList: attributes.as_mut_ptr(),
        };
        let mut command_line = command_line(command);
        let mut environment = environment_block(command);
        let cwd = command.get_current_dir().map(wide_path);
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        if unsafe {
            CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                environment.as_mut_ptr().cast(),
                cwd.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let process = OwnedHandle::new(info.hProcess);
        let thread = OwnedHandle::new(info.hThread);
        drop(thread);
        let (input, output) = pty.into_io();
        Ok(SpawnedWindowsPty {
            child: WindowsPtyChild {
                process,
                pid: info.dwProcessId,
            },
            process_job,
            input,
            reader: Box::new(output),
        })
    }

    struct PtyPipeSet {
        console: PseudoConsole,
        input: std::fs::File,
        output: std::fs::File,
    }

    impl PtyPipeSet {
        fn new(cols: Option<u16>, rows: Option<u16>) -> io::Result<Self> {
            let (console_input, mut parent_input) = create_pipe_pair()?;
            let (mut parent_output, console_output) = create_pipe_pair()?;
            for handle in [parent_input.raw(), parent_output.raw()] {
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT as HANDLE_FLAGS, 0) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            let console = PseudoConsole::new(console_input, console_output, cols, rows)?;
            Ok(Self {
                console,
                input: unsafe { std::fs::File::from_raw_handle(parent_input.take()) },
                output: unsafe { std::fs::File::from_raw_handle(parent_output.take()) },
            })
        }

        fn into_io(self) -> (WindowsPtyInput, std::fs::File) {
            (
                WindowsPtyInput {
                    console: Some(self.console),
                    writer: Some(self.input),
                    closed: false,
                },
                self.output,
            )
        }
    }

    fn create_pipe_pair() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((OwnedHandle::new(read), OwnedHandle::new(write)))
        }
    }

    struct PseudoConsole {
        raw: HPCON,
        api: ConPtyApi,
        _input: OwnedHandle,
        _output: OwnedHandle,
    }

    impl PseudoConsole {
        fn new(
            input: OwnedHandle,
            output: OwnedHandle,
            cols: Option<u16>,
            rows: Option<u16>,
        ) -> io::Result<Self> {
            let api = conpty_api()?;
            let mut raw = 0;
            let result = unsafe {
                (api.create)(pty_size(cols, rows), input.raw(), output.raw(), 0, &mut raw)
            };
            if result < 0 {
                Err(io::Error::other(format!(
                    "CreatePseudoConsole failed with HRESULT {result:#x}"
                )))
            } else {
                Ok(Self {
                    raw,
                    api,
                    _input: input,
                    _output: output,
                })
            }
        }

        fn raw(&self) -> HPCON {
            self.raw
        }

        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            let result = unsafe { (self.api.resize)(self.raw, pty_size(Some(cols), Some(rows))) };
            if result < 0 {
                Err(io::Error::other(format!(
                    "ResizePseudoConsole failed with HRESULT {result:#x}"
                )))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            unsafe { (self.api.close)(self.raw) };
        }
    }

    #[derive(Clone, Copy)]
    struct ConPtyApi {
        create: CreatePseudoConsoleFn,
        resize: ResizePseudoConsoleFn,
        close: ClosePseudoConsoleFn,
    }

    type CreatePseudoConsoleFn =
        unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
    type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
    type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

    fn conpty_api() -> io::Result<ConPtyApi> {
        let module = unsafe { GetModuleHandleW(KERNEL32_DLL.as_ptr()) };
        if module.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel32.dll is unavailable",
            ));
        }
        let create = conpty_proc(module, b"CreatePseudoConsole\0")?;
        let resize = conpty_proc(module, b"ResizePseudoConsole\0")?;
        let close = conpty_proc(module, b"ClosePseudoConsole\0")?;
        Ok(ConPtyApi {
            create: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, CreatePseudoConsoleFn>(
                    create,
                )
            },
            resize: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizePseudoConsoleFn>(
                    resize,
                )
            },
            close: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ClosePseudoConsoleFn>(
                    close,
                )
            },
        })
    }

    fn conpty_proc(
        module: windows_sys::Win32::Foundation::HMODULE,
        name: &'static [u8],
    ) -> io::Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(module, name.as_ptr()) }.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "ConPTY requires Windows 10 version 1809 or newer",
            )
        })
    }

    fn pty_size(cols: Option<u16>, rows: Option<u16>) -> COORD {
        COORD {
            X: cols.unwrap_or(80).clamp(1, i16::MAX as u16) as i16,
            Y: rows.unwrap_or(24).clamp(1, i16::MAX as u16) as i16,
        }
    }

    struct ProcessAttributeList {
        buffer: Vec<u8>,
        jobs: Vec<HANDLE>,
    }

    impl ProcessAttributeList {
        fn new(attribute_count: u32) -> io::Result<Self> {
            let mut size = 0usize;
            unsafe {
                InitializeProcThreadAttributeList(
                    std::ptr::null_mut(),
                    attribute_count,
                    0,
                    &mut size,
                );
            }
            if size == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0u8; size];
            let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if unsafe { InitializeProcThreadAttributeList(list, attribute_count, 0, &mut size) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                buffer,
                jobs: Vec::new(),
            })
        }

        fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
        }

        fn set_pseudo_console(&mut self, pseudo_console: HPCON) -> io::Result<()> {
            if unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    pseudo_console as *const std::ffi::c_void,
                    std::mem::size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn set_job(&mut self, job: HANDLE) -> io::Result<()> {
            self.jobs = vec![job];
            let value = self.jobs.as_ptr().cast();
            let size = std::mem::size_of_val(self.jobs.as_slice());
            if unsafe {
                UpdateProcThreadAttribute(
                    self.as_mut_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST,
                    value,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for ProcessAttributeList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
        }
    }

    fn command_line(command: &Command) -> Vec<u16> {
        let mut line = quote_windows_arg(&command.get_program().to_string_lossy());
        for argument in command.get_args() {
            line.push(' ');
            line.push_str(&quote_windows_arg(&argument.to_string_lossy()));
        }
        line.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn environment_block(command: &Command) -> Vec<u16> {
        let mut values = std::env::vars_os().collect::<Vec<_>>();
        for (key, value) in command.get_envs() {
            values.retain(|(existing, _)| !env_keys_equal(existing, key));
            if let Some(value) = value {
                values.push((OsString::from(key), OsString::from(value)));
            }
        }
        values.sort_by(|left, right| {
            left.0
                .to_string_lossy()
                .to_ascii_lowercase()
                .cmp(&right.0.to_string_lossy().to_ascii_lowercase())
        });
        let mut block = Vec::new();
        for (key, value) in values {
            block.extend(key.encode_wide());
            block.push('=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    fn env_keys_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }

    fn quote_windows_arg(value: &str) -> String {
        if !value.is_empty()
            && !value
                .chars()
                .any(|character| character.is_whitespace() || character == '"')
        {
            return value.to_string();
        }
        let mut result = String::from("\"");
        let mut slashes = 0;
        for character in value.chars() {
            if character == '\\' {
                slashes += 1;
            } else if character == '"' {
                result.push_str(&"\\".repeat(slashes * 2 + 1));
                result.push(character);
                slashes = 0;
            } else {
                result.push_str(&"\\".repeat(slashes));
                result.push(character);
                slashes = 0;
            }
        }
        result.push_str(&"\\".repeat(slashes * 2));
        result.push('"');
        result
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    struct OwnedHandle {
        raw: HANDLE,
    }

    unsafe impl Send for OwnedHandle {}

    impl OwnedHandle {
        fn new(raw: HANDLE) -> Self {
            Self { raw }
        }

        fn raw(&self) -> HANDLE {
            self.raw
        }

        fn take(&mut self) -> HANDLE {
            std::mem::replace(&mut self.raw, std::ptr::null_mut())
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.raw.is_null() {
                unsafe { CloseHandle(self.raw) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn windows_argument_quoting_preserves_spaces_quotes_and_backslashes() {
            assert_eq!(quote_windows_arg("plain"), "plain");
            assert_eq!(quote_windows_arg("two words"), "\"two words\"");
            assert_eq!(
                quote_windows_arg("C:\\Program Files\\Orca\\"),
                "\"C:\\Program Files\\Orca\\\\\""
            );
            assert_eq!(
                quote_windows_arg("say \\\"hi\\\""),
                "\"say \\\\\\\"hi\\\\\\\"\""
            );
        }

        #[test]
        fn command_line_keeps_unicode_program_and_arguments() {
            let mut command = Command::new(r"C:\Program Files\Orca\orca.exe");
            command.args(["--label", "Windows 终端"]);
            let encoded = command_line(&command);
            let decoded = String::from_utf16(&encoded[..encoded.len() - 1]).expect("UTF-16");
            assert_eq!(
                decoded,
                r#""C:\Program Files\Orca\orca.exe" --label "Windows 终端""#
            );
        }
    }
}

#[cfg(windows)]
pub use windows::{
    PtyExitStatus, SpawnedWindowsPty, WindowsPtyChild, WindowsPtyInput, spawn_windows_pty,
    spawn_windows_pty_detached, spawn_windows_pty_named, windows_pty_supported,
};

pub fn native_pty_supported() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(windows)]
    {
        windows_pty_supported()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Marks every descriptor above stdio that refers to a terminal
/// close-on-exec, and returns how many it had to change.
///
/// A terminal library may keep its own duplicates of the controlling terminal
/// (qwertty `dup`s stdin for its reader and resize stream). Left inheritable,
/// every child the process spawns -- a detached subagent worker, an MCP
/// server, a backgrounded shell command -- holds the terminal open, so it
/// outlives the session and whatever reads that terminal to its end hangs.
/// Stdio itself is left alone: children are given their streams explicitly.
#[cfg(unix)]
pub fn keep_terminal_descriptors_out_of_children() -> std::io::Result<usize> {
    let mut changed = 0;
    for entry in std::fs::read_dir("/dev/fd")? {
        let Some(fd) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if fd <= 2 || unsafe { libc::isatty(fd) } != 1 {
            continue;
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || flags & libc::FD_CLOEXEC != 0 {
            continue;
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        changed += 1;
    }
    Ok(changed)
}

/// Windows handles are only inherited when a spawn asks for it, so there is
/// nothing to mark.
#[cfg(not(unix))]
pub fn keep_terminal_descriptors_out_of_children() -> std::io::Result<usize> {
    Ok(0)
}

#[cfg(all(test, unix))]
mod descriptor_tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::keep_terminal_descriptors_out_of_children;

    fn close_on_exec(fd: &OwnedFd) -> bool {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        flags & libc::FD_CLOEXEC != 0
    }

    #[test]
    fn inheritable_terminal_descriptors_become_close_on_exec() {
        let (mut master, mut slave) = (-1, -1);
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "{}", std::io::Error::last_os_error());
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // A plain dup, as qwertty makes of stdin: inheritable.
        let dup = unsafe { OwnedFd::from_raw_fd(libc::dup(slave.as_raw_fd())) };
        assert!(!close_on_exec(&dup));

        assert!(keep_terminal_descriptors_out_of_children().unwrap() >= 1);
        assert!(close_on_exec(&dup));
        assert!(close_on_exec(&slave));
        drop(master);
    }
}
