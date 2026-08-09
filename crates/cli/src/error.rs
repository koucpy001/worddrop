//! Unified CLI error type with a process exit-code contract.
//!
//! Exit codes (croc convention): `0` = success, `1` = user error (bad
//! arguments, invalid config), `2` = runtime error (I/O, network). clap usage
//! errors also map to `1` — see `main`'s `print_parse_error`.

use std::{fmt, process::ExitCode};

use my_croc_core::identity;

use crate::config::ConfigError;

/// CLI error classified for exit-code mapping.
#[derive(Debug)]
pub enum CliError {
    /// User error: bad arguments or configuration. Exit code 1.
    User(String),
    /// Runtime error: I/O, network, unexpected failure. Exit code 2.
    Runtime(String),
}

impl CliError {
    /// A user-facing error (exit 1): invalid input, bad config, wrong usage.
    pub fn user(msg: impl Into<String>) -> CliError {
        CliError::User(msg.into())
    }

    /// A runtime error (exit 2): I/O, network, engine failure.
    pub fn runtime(msg: impl Into<String>) -> CliError {
        CliError::Runtime(msg.into())
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            CliError::User(_) => ExitCode::from(1),
            CliError::Runtime(_) => ExitCode::from(2),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::User(msg) | CliError::Runtime(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for CliError {}

impl From<ConfigError> for CliError {
    fn from(err: ConfigError) -> CliError {
        CliError::User(err.to_string())
    }
}

impl From<identity::Error> for CliError {
    fn from(err: identity::Error) -> CliError {
        CliError::User(err.to_string())
    }
}
