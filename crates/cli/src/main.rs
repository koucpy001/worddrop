//! my-croc CLI entry point: parse, init tracing, dispatch, map errors to exit
//! codes (0 = ok, 1 = user error, 2 = runtime).

use std::process::ExitCode;

use clap::Parser;
use my_croc_cli::cli::Cli;
use my_croc_cli::commands;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let args = match Cli::try_parse() {
        Ok(args) => args,
        Err(err) => return print_parse_error(err),
    };
    init_tracing(args.verbose);
    match commands::run(args) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

/// Render a clap failure: `--help`/`--version` print and exit 0; usage errors
/// print and exit 1 (the CLI's user-error code, per the exit-code contract in
/// `error.rs`).
fn print_parse_error(err: clap::Error) -> ExitCode {
    let code = if err.use_stderr() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };
    let _ = err.print();
    code
}

/// Map `-v` verbosity to the tracing level: 0 = warn, 1 = info, 2+ = debug.
/// An explicit `RUST_LOG` still wins over the derived default.
fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
