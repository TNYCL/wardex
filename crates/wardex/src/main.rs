use std::process::ExitCode;

use clap::Parser;

mod cli;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    eprintln!(
        "wardex {}: `{}` is not implemented yet (pre-alpha skeleton)",
        env!("CARGO_PKG_VERSION"),
        cli.command.name()
    );
    ExitCode::from(2)
}
