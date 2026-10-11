//! Native stdin handles let the writer stop even when the driver does not read.

use std::io::{self, Write};

#[cfg(unix)]
use std::os::fd::AsFd as InputHandle;
#[cfg(windows)]
use std::os::windows::io::AsHandle as InputHandle;

/// An unbuffered driver stdin pipe, such as `PipeWriter` or `ChildStdin`.
/// Native handle access is needed to make delivery interruptible during Drop.
#[cfg(any(unix, windows))]
pub trait DriverInput: Write + Send + InputHandle {}
#[cfg(any(unix, windows))]
impl<T: Write + Send + InputHandle> DriverInput for T {}

#[cfg(not(any(unix, windows)))]
pub trait DriverInput: Write + Send {}
#[cfg(not(any(unix, windows)))]
impl<T: Write + Send> DriverInput for T {}

#[cfg(unix)]
pub(crate) fn set_nonblocking(input: &dyn DriverInput) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = input.as_fd().as_raw_fd();
    // SAFETY: the borrowed pipe descriptor stays valid throughout these calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL changes only status flags on our owned stdin pipe.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_nonblocking(input: &dyn DriverInput) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::{PIPE_NOWAIT, SetNamedPipeHandleState};
    let mode = PIPE_NOWAIT;
    // SAFETY: the borrowed pipe handle and mode pointer remain valid for the call;
    // the unused collection/timeout arguments are null. Windows anonymous pipes
    // are byte-type named pipes, so their wait mode can be changed this way.
    if unsafe {
        SetNamedPipeHandleState(
            input.as_handle().as_raw_handle(),
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn set_nonblocking(_: &dyn DriverInput) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Computer-use stdin pipes are unsupported on this platform",
    ))
}

#[cfg(all(unix, any(target_os = "macos", test)))]
pub(crate) fn peer_closed(input: std::os::fd::BorrowedFd<'_>) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut descriptor = libc::pollfd {
        fd: input.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // HUP is visible even when unread bytes are still buffered in the pipe.
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::other("Driver stdin descriptor is invalid"));
    }
    Ok(descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0)
}
#[cfg(unix)]
#[test]
fn pipe_hangup_is_detected_even_with_unread_request_bytes() {
    use std::os::fd::AsFd;
    let (input, mut output) = std::io::pipe().unwrap();
    output.write_all(b"queued request\n").unwrap();
    assert!(!peer_closed(input.as_fd()).unwrap());
    drop(output);
    assert!(
        peer_closed(input.as_fd()).unwrap(),
        "EOF must abort current work before queued bytes are drained"
    );
}
