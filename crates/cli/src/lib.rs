//! my-croc-cli — Linux CLI binary crate.
//!
//! Command surface (clap, T12), TOML config file with env/default merge,
//! unified error-to-exit-code mapping. Send/receive command logic lands in
//! T13/T14.

pub mod cli;
pub mod commands;
pub mod config;
pub mod error;
pub mod receive;
pub mod rendezvous_client;
pub mod send;
pub mod ui;
pub mod wire;

pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
