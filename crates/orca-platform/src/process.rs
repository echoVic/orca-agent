use std::io::{self, Read};
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;

/// Owns the operating-system process-tree boundary for one spawned child.
/// Windows uses a Job Object with kill-on-close; other hosts keep this API as
/// a no-op because their process-group code remains authoritative.
#[derive(Debug)]
pub struct ProcessJob {
    platform: platform::ProcessJob,
}

impl ProcessJob {
    /// Creates an unattached Windows Job Object for use with an atomic process
    /// attribute list. The caller must include [`Self::raw_handle`] in
    /// `PROC_THREAD_ATTRIBUTE_JOB_LIST` when it creates the process.
    #[cfg(windows)]
    pub fn create_unassigned(name: Option<&str>) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::create(name)?,
        })
    }

    #[cfg(windows)]
    pub fn raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.platform.raw_handle()
    }

    /// Spawns a child inside a new operating-system process-tree boundary.
    ///
    /// On Windows the child is created suspended, assigned to a Job Object,
    /// and only then resumed, so none of its code can run outside the job.
    #[deprecated(
        note = "direct process launch bypasses the capability kernel; use ExecutionBroker"
    )]
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn(command, None)?;
        Ok((child, Self { platform }))
    }

    /// Spawns a child atomically inside a named Windows Job Object. The name
    /// allows a later Orca process to reopen and verify the ownership boundary.
    #[deprecated(
        note = "direct process launch bypasses the capability kernel; use ExecutionBroker"
    )]
    pub fn spawn_named(command: &mut Command, name: &str) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn(command, Some(name))?;
        Ok((child, Self { platform }))
    }

    /// Spawns a child in a Windows Job Object whose lifetime is managed by the
    /// child owner rather than by this handle. This is reserved for explicitly
    /// detached user-trusted workers that must outlive their launcher process.
    pub fn spawn_detached(command: &mut Command) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn_detached(command)?;
        Ok((child, Self { platform }))
    }

    /// Spawns into an existing named Windows Job when the current process is
    /// already a member; otherwise creates and assigns the named boundary.
    /// This is used by the Windows runner so descendants inherit the runtime's
    /// Job Object instead of attempting an invalid second assignment.
    pub fn spawn_named_or_inherited(
        command: &mut Command,
        name: &str,
    ) -> io::Result<(Child, Self)> {
        let (child, platform) = platform::ProcessJob::spawn_named_or_inherited(command, name)?;
        Ok((child, Self { platform }))
    }

    /// Attaches an existing process to a new boundary.
    ///
    /// This cannot contain code that ran before assignment. Prefer [`Self::spawn`]
    /// unless the process is known to still be suspended or is being recovered.
    pub fn attach(pid: u32) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::attach(pid)?,
        })
    }

    /// Attaches an existing process to a named Windows Job Object.
    /// Prefer [`Self::spawn_named`] for newly created children.
    pub fn attach_named(pid: u32, name: &str) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::attach_named(pid, name)?,
        })
    }

    pub fn open_named(name: &str) -> io::Result<Self> {
        Ok(Self {
            platform: platform::ProcessJob::open_named(name)?,
        })
    }

    pub fn contains_process(&self, pid: u32) -> io::Result<bool> {
        self.platform.contains_process(pid)
    }

    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        self.platform.terminate(exit_code)
    }
}

/// Prevents subsequently spawned background children from inheriting the
/// current process's existing standard handles. The handles remain open and
/// usable by this process.
pub fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
    platform::clear_current_process_std_handle_inheritance()
}

/// Detaches the current process stdout from a one-shot launcher handshake.
///
/// Long-lived workers use stdout only to publish their initial launch receipt.
/// Rebinding the descriptor after that receipt lets the launcher observe EOF
/// without waiting for the worker's full execution lifetime.
pub fn detach_current_process_stdout() -> io::Result<()> {
    platform::detach_current_process_stdout()
}

/// Reads a captured child pipe without making cancellation depend on a
/// blocking operating-system read returning first.
#[cfg(not(windows))]
pub fn read_child_pipe_interruptibly<R: Read>(
    reader: &mut R,
    stop: &AtomicBool,
    buffer: &mut [u8],
) -> io::Result<usize> {
    platform::read_child_pipe_interruptibly(reader, stop, buffer)
}

/// Reads a captured child pipe without making cancellation depend on a
/// blocking operating-system read returning first.
#[cfg(windows)]
pub fn read_child_pipe_interruptibly<R: Read + std::os::windows::io::AsRawHandle>(
    reader: &mut R,
    stop: &AtomicBool,
    buffer: &mut [u8],
) -> io::Result<usize> {
    platform::read_child_pipe_interruptibly(reader, stop, buffer)
}

#[cfg(not(windows))]
mod platform {
    use std::io::{self, Read};
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    pub(super) struct ProcessJob;

    pub(super) fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
        Ok(())
    }

    pub(super) fn detach_current_process_stdout() -> io::Result<()> {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;

        let null = OpenOptions::new().write(true).open("/dev/null")?;
        // SAFETY: stdout is a process-local descriptor and dup2 atomically
        // replaces it, closing the inherited launcher pipe at fd 1.
        let result = unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn read_child_pipe_interruptibly<R: Read>(
        reader: &mut R,
        stop: &AtomicBool,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        loop {
            match reader.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    impl ProcessJob {
        pub(super) fn spawn(
            command: &mut Command,
            _name: Option<&str>,
        ) -> io::Result<(Child, Self)> {
            Ok((command.spawn()?, Self))
        }

        pub(super) fn spawn_detached(command: &mut Command) -> io::Result<(Child, Self)> {
            Self::spawn(command, None)
        }

        pub(super) fn attach(_pid: u32) -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn spawn_named_or_inherited(
            command: &mut Command,
            name: &str,
        ) -> io::Result<(Child, Self)> {
            Self::spawn(command, Some(name))
        }

        pub(super) fn attach_named(pid: u32, _name: &str) -> io::Result<Self> {
            Self::attach(pid)
        }

        pub(super) fn open_named(_name: &str) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named process jobs are only available on Windows",
            ))
        }

        pub(super) fn contains_process(&self, _pid: u32) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process job membership is only available on Windows",
            ))
        }

        pub(super) fn terminate(&self, _exit_code: u32) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, OpenJobObjectW, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, GetCurrentProcess, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread,
        THREAD_SUSPEND_RESUME,
    };

    #[derive(Debug)]
    pub(super) struct ProcessJob {
        handle: HANDLE,
    }

    // Job Object handles are kernel-owned and may be closed or used from any
    // thread. This lets process owners move the lifetime guard to reaper
    // threads without transferring any borrowed memory.
    unsafe impl Send for ProcessJob {}

    pub(super) fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(id) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }

            let mut flags = 0;
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & HANDLE_FLAG_INHERIT != 0
                && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(super) fn detach_current_process_stdout() -> io::Result<()> {
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE, SetStdHandle};

        let null_name = [b'N' as u16, b'U' as u16, b'L' as u16, 0];
        let null_handle = unsafe {
            CreateFileW(
                null_name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if null_handle.is_null() || null_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let previous = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null_handle) } == 0 {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(null_handle) };
            return Err(error);
        }
        if !previous.is_null() && previous != INVALID_HANDLE_VALUE && previous != null_handle {
            unsafe { CloseHandle(previous) };
        }
        // The new standard handle remains open until process exit and is
        // intentionally not wrapped in a Rust-owned file object.
        Ok(())
    }

    pub(super) fn read_child_pipe_interruptibly<R: io::Read + AsRawHandle>(
        reader: &mut R,
        stop: &AtomicBool,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let mut available = 0_u32;
            let peeked = unsafe {
                PeekNamedPipe(
                    reader.as_raw_handle().cast(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if peeked == 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.raw_os_error(),
                    Some(code)
                        if code == ERROR_BROKEN_PIPE as i32
                            || code == ERROR_NO_DATA as i32
                            || code == ERROR_PIPE_NOT_CONNECTED as i32
                ) {
                    return Ok(0);
                }
                return Err(error);
            }
            if available == 0 {
                if stop.load(Ordering::Acquire) {
                    return Ok(0);
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }

            let read_limit = buffer.len().min(available as usize);
            match reader.read(&mut buffer[..read_limit]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    impl ProcessJob {
        pub(super) fn spawn(
            command: &mut Command,
            name: Option<&str>,
        ) -> io::Result<(Child, Self)> {
            Self::spawn_with_lifetime(command, name, true)
        }

        pub(super) fn spawn_detached(command: &mut Command) -> io::Result<(Child, Self)> {
            Self::spawn_with_lifetime(command, None, false)
        }

        fn spawn_with_lifetime(
            command: &mut Command,
            name: Option<&str>,
            kill_on_close: bool,
        ) -> io::Result<(Child, Self)> {
            let job = Self::create_with_lifetime(name, kill_on_close)?;
            command.creation_flags(CREATE_SUSPENDED);
            let mut child = command.spawn()?;
            let process = child.as_raw_handle().cast();

            if unsafe { AssignProcessToJobObject(job.handle, process) } == 0 {
                let error = io::Error::last_os_error();
                terminate_suspended_child(&mut child, &job);
                return Err(error);
            }
            if let Err(error) = resume_process_threads(child.id()) {
                terminate_suspended_child(&mut child, &job);
                return Err(error);
            }

            Ok((child, job))
        }

        pub(super) fn spawn_named_or_inherited(
            command: &mut Command,
            name: &str,
        ) -> io::Result<(Child, Self)> {
            match Self::open_named(name) {
                Ok(job) if job.contains_current_process()? => Ok((command.spawn()?, job)),
                Ok(_) => Self::spawn(command, Some(name)),
                Err(error) if error.raw_os_error() == Some(2) => Self::spawn(command, Some(name)),
                Err(error) => Err(error),
            }
        }

        pub(super) fn attach(pid: u32) -> io::Result<Self> {
            Self::attach_with_name(pid, None)
        }

        pub(super) fn attach_named(pid: u32, name: &str) -> io::Result<Self> {
            Self::attach_with_name(pid, Some(name))
        }

        fn attach_with_name(pid: u32, name: Option<&str>) -> io::Result<Self> {
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            };
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = match Self::create(name) {
                Ok(job) => job,
                Err(error) => {
                    unsafe { CloseHandle(process) };
                    return Err(error);
                }
            };
            let assigned = unsafe { AssignProcessToJobObject(job.handle, process) } != 0;
            unsafe { CloseHandle(process) };
            if !assigned {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(super) fn create(name: Option<&str>) -> io::Result<Self> {
            Self::create_with_lifetime(name, true)
        }

        fn create_with_lifetime(name: Option<&str>, kill_on_close: bool) -> io::Result<Self> {
            if let Some(name) = name {
                validate_name(name)?;
            }
            let wide_name = name.map(to_wide_name);
            let handle = unsafe {
                CreateJobObjectW(
                    std::ptr::null(),
                    wide_name
                        .as_ref()
                        .map_or(std::ptr::null(), |name| name.as_ptr()),
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            if kill_on_close {
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = unsafe {
                    SetInformationJobObject(
                        handle,
                        JobObjectExtendedLimitInformation,
                        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )
                };
                if configured == 0 {
                    unsafe { CloseHandle(handle) };
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self { handle })
        }

        pub(super) fn raw_handle(&self) -> HANDLE {
            self.handle
        }

        pub(super) fn open_named(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let wide_name = to_wide_name(name);
            let job = unsafe {
                OpenJobObjectW(
                    JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE,
                    0,
                    wide_name.as_ptr(),
                )
            };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle: job })
        }

        pub(super) fn contains_process(&self, pid: u32) -> io::Result<bool> {
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() {
                return Ok(false);
            }
            let mut result = 0;
            let inspected = unsafe { IsProcessInJob(process, self.handle, &mut result) };
            unsafe { CloseHandle(process) };
            if inspected == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result != 0)
            }
        }

        fn contains_current_process(&self) -> io::Result<bool> {
            let mut result = 0;
            let inspected =
                unsafe { IsProcessInJob(GetCurrentProcess(), self.handle, &mut result) };
            if inspected == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(result != 0)
            }
        }

        pub(super) fn terminate(&self, exit_code: u32) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.handle, exit_code) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    fn validate_name(name: &str) -> io::Result<()> {
        if name.is_empty() || name.encode_utf16().any(|unit| unit == 0) {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process job name must be non-empty and contain no NUL characters",
            ))
        } else {
            Ok(())
        }
    }

    fn to_wide_name(name: &str) -> Vec<u16> {
        name.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    }

    fn terminate_suspended_child(child: &mut Child, job: &ProcessJob) {
        let _ = job.terminate(1);
        let _ = child.kill();
        let _ = child.wait();
    }

    fn resume_process_threads(pid: u32) -> io::Result<()> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let result = resume_process_threads_from_snapshot(snapshot, pid);
        unsafe { CloseHandle(snapshot) };
        result
    }

    fn resume_process_threads_from_snapshot(snapshot: HANDLE, pid: u32) -> io::Result<()> {
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if unsafe { Thread32First(snapshot, &mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut resumed = 0usize;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let previous_count = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous_count == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed += 1;
            }
            if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }

        if resumed == 0 {
            Err(io::Error::other(
                "spawned Windows process had no resumable threads",
            ))
        } else {
            Ok(())
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }
}
