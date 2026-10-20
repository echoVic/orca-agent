#[path = "../support/live_harness.rs"]
mod harness;
#[cfg(target_os = "macos")]
#[path = "../live_macos.rs"]
mod macos;

fn main() {
    let cli = <harness::Cli as clap::Parser>::parse();
    #[cfg(target_os = "macos")]
    std::process::exit(macos::run(cli));
    #[cfg(not(target_os = "macos"))]
    cli.run(vec![]).exit();
}
