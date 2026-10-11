#[cfg(target_os = "macos")]
pub mod macos;

pub fn run_stdio() -> i32 {
    #[cfg(target_os = "macos")]
    {
        use crate::serve::{LineWriter, serve};
        let Some(home) = orca_core::home::orca_home() else {
            eprintln!("Could not resolve Orca home for computer-use session lock");
            return 2;
        };
        // Initialize AppKit and CoreGraphics on the actual main thread before
        // SCK callbacks construct filters on their concurrent replayd queue.
        let _workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
        if objc2_core_graphics::CGEvent::new(None).is_none() {
            eprintln!("Could not initialize the computer-use graphics connection");
            return 2;
        }
        let mut driver = macos::MacDriver::new(home.join("computer-use.lock"));
        driver.watch_stdin(libc::STDIN_FILENO);
        let stop = driver.stop_signal();
        let worker = std::thread::Builder::new()
            .name("computer-use-driver".into())
            .spawn(move || {
                match serve(
                    std::io::stdin().lock(),
                    LineWriter::new(std::io::stdout()),
                    &mut driver,
                ) {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("Computer-use driver failed: {error}");
                        2
                    }
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                eprintln!("Could not start computer-use driver: {error}");
                return 2;
            }
        };
        // NSWorkspace application membership updates on the actual main loop.
        // Its launch completion blocks run separately on a concurrent queue.
        use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
        let mut stdin_closed = false;
        while !worker.is_finished() {
            if !stdin_closed {
                let input = unsafe { std::os::fd::BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
                if !matches!(crate::driver_input::peer_closed(input), Ok(false)) {
                    stdin_closed = true;
                    stop.stop();
                }
            }
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, true);
        }
        worker.join().unwrap_or(2)
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("computer use is only available on macOS");
        2
    }
}
