//! Write text to the system clipboard from inside the TUI.
//!
//! Primary channel is OSC 52 written straight to stdout: the terminal
//! emulator (VS Code, iTerm2, kitty, WezTerm, ...) performs the clipboard
//! write, so it also works over SSH. Inside tmux the sequence is wrapped in a
//! DCS passthrough envelope. Very large selections skip OSC 52 (terminals cap
//! the sequence length, commonly around 100 KB) and rely on the local
//! fallback: `pbcopy` on macOS, `wl-copy`/`xclip` elsewhere on Unix, and
//! PowerShell's native clipboard API on Windows.

use std::io::{self, Write as _};
use std::time::{Duration, Instant};

use crate::selection::{osc52_copy_sequence, tmux_passthrough};

/// Above this size the OSC 52 write is skipped: common terminals silently
/// truncate or drop oversized sequences, which would make the "copied" notice
/// a lie. The local fallback still receives the full text.
pub(crate) const OSC52_MAX_TEXT_BYTES: usize = 100_000;

const LOCAL_CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn copy_to_clipboard(text: &str) {
    // The OSC 52 write is an in-memory buffer flush on the UI thread; the
    // terminal does the actual clipboard work.
    let osc52_succeeded = if text.len() <= OSC52_MAX_TEXT_BYTES {
        let sequence = osc52_copy_sequence(&text);
        let sequence = if std::env::var_os("TMUX").is_some() {
            tmux_passthrough(&sequence)
        } else {
            sequence
        };
        let mut stdout = io::stdout();
        stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok()
    } else {
        false
    };

    let owned_text = text.to_owned();
    std::thread::spawn(move || {
        let _ = osc52_succeeded || local_clipboard_best_effort(&owned_text);
    });
}

#[cfg(target_os = "macos")]
fn local_clipboard_best_effort(text: &str) -> bool {
    pipe_through(&["pbcopy"], text, LOCAL_CLIPBOARD_TIMEOUT)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn local_clipboard_best_effort(text: &str) -> bool {
    pipe_through(&["wl-copy"], text, LOCAL_CLIPBOARD_TIMEOUT)
        || pipe_through(
            &["xclip", "-selection", "clipboard"],
            text,
            LOCAL_CLIPBOARD_TIMEOUT,
        )
}

#[cfg(windows)]
fn local_clipboard_best_effort(text: &str) -> bool {
    set_windows_clipboard_text(text, LOCAL_CLIPBOARD_TIMEOUT).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn local_clipboard_best_effort(_text: &str) -> bool {
    false
}

#[cfg(unix)]
fn pipe_through(command: &[&str], text: &str, timeout: Duration) -> bool {
    use std::process::{Command, Stdio};

    let mut child_command = Command::new(command[0]);
    child_command
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let cwd = std::env::current_dir().ok();
    let Some(cwd) = cwd else {
        return false;
    };
    let Ok((mut child, process_job, _receipt)) =
        orca_tools::process::spawn_user_trusted(child_command, "tui:clipboard", &cwd)
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let text = text.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&text));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = process_job.terminate(1);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let wrote = writer.join().is_ok_and(|result| result.is_ok());
    wrote && status.is_some_and(|status| status.success())
}

#[cfg(windows)]
fn set_windows_clipboard_text(text: &str, timeout: Duration) -> io::Result<()> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    const CF_UNICODETEXT: u32 = 13;

    if text.encode_utf16().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard text contains an embedded NUL",
        ));
    }
    let wide = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let byte_len = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "clipboard text is too large")
        })?;

    let clipboard = ClipboardGuard::open_until(Instant::now() + timeout)?;
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if memory.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut memory = GlobalMemory::new(memory);
    let target = unsafe { GlobalLock(memory.raw()) };
    if target.is_null() {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target.cast::<u16>(), wide.len());
        GlobalUnlock(memory.raw());
    }
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory.raw()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    memory.transfer_to_clipboard();
    let close_result = clipboard.close();

    struct ClipboardGuard {
        open: bool,
    }

    impl ClipboardGuard {
        fn open_until(deadline: Instant) -> io::Result<Self> {
            loop {
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Ok(Self { open: true });
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "Windows clipboard remained busy until the copy deadline",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn close(mut self) -> io::Result<()> {
            self.open = false;
            if unsafe { CloseClipboard() } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            if self.open {
                unsafe { CloseClipboard() };
            }
        }
    }

    struct GlobalMemory {
        handle: windows_sys::Win32::Foundation::HGLOBAL,
    }

    impl GlobalMemory {
        fn new(handle: windows_sys::Win32::Foundation::HGLOBAL) -> Self {
            Self { handle }
        }

        fn raw(&self) -> windows_sys::Win32::Foundation::HGLOBAL {
            self.handle
        }

        fn transfer_to_clipboard(&mut self) {
            self.handle = std::ptr::null_mut();
        }
    }

    impl Drop for GlobalMemory {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe { GlobalFree(self.handle) };
            }
        }
    }

    close_result
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    #[test]
    fn native_clipboard_round_trips_unicode_text() {
        let expected = "Orca Windows clipboard: 终端";
        set_windows_clipboard_text(expected, LOCAL_CLIPBOARD_TIMEOUT)
            .expect("write Unicode clipboard text");

        let deadline = Instant::now() + LOCAL_CLIPBOARD_TIMEOUT;
        loop {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "clipboard stayed busy after write"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        assert!(!handle.is_null(), "clipboard did not retain CF_UNICODETEXT");
        let pointer = unsafe { GlobalLock(handle) }.cast::<u16>();
        assert!(!pointer.is_null(), "lock clipboard text");
        let units = unsafe { std::slice::from_raw_parts(pointer, GlobalSize(handle) / 2) };
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .expect("NUL terminator");
        let actual = String::from_utf16(&units[..end]).expect("clipboard UTF-16");
        unsafe {
            GlobalUnlock(handle);
            CloseClipboard();
        }
        assert_eq!(actual, expected);
    }
}
