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
        let mut driver = macos::MacDriver::new(home.join("computer-use.lock"));
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
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("computer use is only available on macOS");
        2
    }
}
