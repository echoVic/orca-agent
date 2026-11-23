mod cli;

fn main() {
    // Dropped by hand: `process::exit` runs no destructors, and the console
    // outlives this process.
    let console = orca_runtime::command::Utf8ConsoleOutput::enter();
    let code = cli::run();
    drop(console);
    std::process::exit(code);
}
