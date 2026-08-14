//! clap command surface (T12): `worddrop send|receive|config`, global `-v`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[cfg(test)]
mod tests;

/// worddrop — secure cross-platform file transfer with word-code pairing.
///
/// Pair with a short code phrase (`nameplate-word-word-word`), then transfer
/// files end-to-end encrypted. The rendezvous server only ever sees the
/// numeric nameplate; the secret words never leave either client.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Increase logging verbosity (-v = info, -vv = debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Send files or directories with word-code pairing.
    Send(SendArgs),
    /// Receive files by entering the pairing word code.
    Receive(ReceiveArgs),
    /// Clean up the blob cache (sent/received data in the data dir).
    ///
    /// Sweeps unreferenced blobs from both role stores (`<data_dir>/send` and
    /// `<data_dir>/receive`). Resume records and received files are kept.
    Cleanup,
    /// Show or modify the configuration file.
    Config(ConfigArgs),
}

#[derive(Parser, Debug)]
pub struct SendArgs {
    /// Files or directories to send.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct ReceiveArgs {
    /// Word code to receive with, e.g. `7-correct-horse-battery`. Prompts
    /// interactively when omitted.
    #[arg(short, long)]
    pub code: Option<String>,
    /// Directory to save received files into.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommands>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Print the effective configuration (one key with `get <KEY>`).
    Get(GetArgs),
    /// Set a configuration value and persist it to config.toml.
    Set(SetArgs),
}

#[derive(Parser, Debug)]
pub struct GetArgs {
    /// Key to print: rendezvous_url, relay_url, data_dir, or overwrite.
    /// Prints all keys when omitted.
    pub key: Option<String>,
}

#[derive(Parser, Debug)]
pub struct SetArgs {
    /// Key: rendezvous_url, relay_url, data_dir, or overwrite.
    pub key: String,
    /// Value to store.
    pub value: String,
}
