//! Defenses against inherited `O_NONBLOCK` on the stdio file descriptors.
//!
//! The npm wrapper launches the TUI from a Node process. Once libuv touches
//! `process.stdin`/`stdout` it flips `O_NONBLOCK` on the tty's open file
//! description, which is shared with this child process. Small incremental
//! frames still fit in the kernel tty buffer, but a resize storm forces
//! full-screen redraws faster than the terminal drains them and `write(2)`
//! starts failing with `EAGAIN` ("Resource temporarily unavailable"),
//! killing the TUI.

use std::io::{self, Write};

/// Clear `O_NONBLOCK` on stdin/stdout/stderr so reads and writes block
/// instead of failing with `EAGAIN`. Call once before any terminal I/O.
#[cfg(unix)]
pub(crate) fn clear_stdio_nonblocking() {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        clear_fd_nonblocking(fd);
    }
}

/// Strip the `O_NONBLOCK` bit from a single fd's open file description.
/// Best-effort: leaves the fd untouched if the flag is already clear or the
/// `fcntl` calls fail (e.g. the fd is closed).
#[cfg(unix)]
fn clear_fd_nonblocking(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags != -1 && flags & libc::O_NONBLOCK != 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn clear_stdio_nonblocking() {}

/// Writer that retries `WouldBlock`/`Interrupted` instead of surfacing them.
///
/// Belt-and-braces for environments that flip the fd back to non-blocking
/// after startup (anything sharing the open file description can do so at
/// any time). Waits briefly between attempts to let the terminal drain.
pub(crate) struct RetryWriter<W> {
    inner: W,
    /// Set when the TUI handles a hangup itself (`termination_signals`):
    /// once a write fails with EIO, as one does on a terminal whose other
    /// end closed, that write and every later one are dropped. The renderer
    /// then quits on the hangup, which its input reports too.
    drops_writes_after_a_hangup: bool,
    hung_up: bool,
}

impl<W> RetryWriter<W> {
    pub(crate) const fn new(inner: W) -> Self {
        Self {
            inner,
            drops_writes_after_a_hangup: false,
            hung_up: false,
        }
    }

    pub(crate) const fn dropping_writes_after_a_hangup(mut self) -> Self {
        self.drops_writes_after_a_hangup = true;
        self
    }

    /// Whether `error` says the terminal hung up, and writes from now on are
    /// to be dropped.
    fn hangs_up(&mut self, error: &io::Error) -> bool {
        if self.drops_writes_after_a_hangup && is_eio(error) {
            self.hung_up = true;
        }
        self.hung_up
    }
}

#[cfg(unix)]
fn is_eio(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
}

/// Only the unix TUI handles a hangup itself.
#[cfg(not(unix))]
fn is_eio(_error: &io::Error) -> bool {
    false
}

const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

impl<W: Write> Write for RetryWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.hung_up {
            return Ok(buf.len());
        }
        loop {
            match self.inner.write(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(e) if self.hangs_up(&e) => return Ok(buf.len()),
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.hung_up {
            return Ok(());
        }
        loop {
            match self.inner.flush() {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(e) if self.hangs_up(&e) => return Ok(()),
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FlakyWriter {
        failures_left: usize,
        written: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.failures_left > 0 {
                self.failures_left -= 1;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "EAGAIN"));
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.failures_left > 0 {
                self.failures_left -= 1;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "EAGAIN"));
            }
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn write_retries_would_block_until_success() {
        let mut writer = RetryWriter::new(FlakyWriter {
            failures_left: 3,
            written: Vec::new(),
            flushes: 0,
        });
        assert_eq!(writer.write(b"frame").unwrap(), 5);
        assert_eq!(writer.inner.written, b"frame");
    }

    #[test]
    fn flush_retries_would_block_until_success() {
        let mut writer = RetryWriter::new(FlakyWriter {
            failures_left: 2,
            written: Vec::new(),
            flushes: 0,
        });
        writer.flush().unwrap();
        assert_eq!(writer.inner.flushes, 1);
    }

    #[test]
    fn write_propagates_real_errors() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "EPIPE"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "EPIPE"))
            }
        }
        let mut writer = RetryWriter::new(BrokenWriter);
        assert_eq!(
            writer.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    /// A terminal that hung up fails every write with EIO. When the TUI
    /// handles the hangup itself, the first such write marks it gone and
    /// that write and every later one are dropped: a frame drawn before the
    /// renderer quits on the hangup, or the cursor ratatui shows again when
    /// its terminal is dropped, cannot fail the session (ratatui's drop would
    /// panic, printing that failure to the same terminal).
    #[cfg(unix)]
    #[test]
    fn writes_after_a_hangup_are_dropped_once_told_to() {
        struct HungUpTerminal {
            writes: usize,
        }
        impl Write for HungUpTerminal {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
        }

        let mut writer = RetryWriter::new(HungUpTerminal { writes: 0 });
        assert_eq!(
            writer.write(b"x").unwrap_err().raw_os_error(),
            Some(libc::EIO),
            "untold, it fails as before"
        );

        let mut writer =
            RetryWriter::new(HungUpTerminal { writes: 0 }).dropping_writes_after_a_hangup();
        assert_eq!(writer.write(b"frame").unwrap(), 5);
        assert_eq!(writer.write(b"cursor").unwrap(), 6);
        writer.flush().unwrap();
        assert_eq!(
            writer.inner.writes, 1,
            "nothing was written after the hangup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn clear_stdio_nonblocking_is_idempotent() {
        clear_stdio_nonblocking();
        clear_stdio_nonblocking();
    }

    #[cfg(unix)]
    #[test]
    fn clear_fd_nonblocking_strips_the_flag() {
        // Open a real pipe, flip O_NONBLOCK on the read end the way libuv does
        // to an inherited tty, then prove the helper clears it.
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let [read_fd, write_fd] = fds;

        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_eq!(
                libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0
            );
            assert_ne!(
                libc::fcntl(read_fd, libc::F_GETFL) & libc::O_NONBLOCK,
                0,
                "precondition: fd should be non-blocking"
            );
        }

        clear_fd_nonblocking(read_fd);

        let after = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        assert_eq!(
            after & libc::O_NONBLOCK,
            0,
            "O_NONBLOCK should be cleared after clear_fd_nonblocking"
        );

        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }
}
