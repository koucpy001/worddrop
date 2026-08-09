//! Subcommand dispatch. `config` is fully functional (T12 scope); `send` and
//! `receive` are argument-reporting stubs until T13/T14 wire the real flows.

use crate::{
    cli::{Cli, Commands, ConfigArgs, ConfigCommands, ReceiveArgs, SendArgs},
    config::{Config, ConfigFile},
    error::CliError,
};

#[cfg(test)]
mod tests;

/// Run the parsed CLI; returns the text to print on stdout.
pub fn run(args: Cli) -> Result<String, CliError> {
    match args.command {
        Commands::Send(args) => send(args),
        Commands::Receive(args) => receive(args),
        Commands::Config(args) => config(args),
    }
}

/// Stub: report the parsed send arguments. The real flow (file walk + import,
/// rendezvous nameplate allocation, SPAKE2 pairing, iroh transfer) lands in T13.
fn send(args: SendArgs) -> Result<String, CliError> {
    let paths = args
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("send: {} path(s): {paths}\n", args.paths.len()))
}

/// Stub: report the parsed receive arguments. The real flow (code split,
/// rendezvous claim, SPAKE2 pairing, offer confirm, download + export) lands
/// in T14.
fn receive(args: ReceiveArgs) -> Result<String, CliError> {
    let code = args.code.as_deref().unwrap_or("<prompt>");
    let output = args
        .output
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<cwd>".to_string());
    Ok(format!("receive: code = {code}, output = {output}\n"))
}

fn config(args: ConfigArgs) -> Result<String, CliError> {
    let file = ConfigFile::load()?;
    match args.command {
        Some(ConfigCommands::Get(args)) => get(&file, args.key.as_deref()),
        Some(ConfigCommands::Set(set_args)) => set(&file, &set_args.key, &set_args.value),
        None => get(&file, None),
    }
}

/// `config get [KEY]`: the effective (resolved) config, not the raw file.
fn get(file: &ConfigFile, key: Option<&str>) -> Result<String, CliError> {
    let cfg = Config::resolve(file)?;
    match key {
        Some(key) => Ok(format!("{}\n", cfg.field(key)?)),
        None => Ok(format!(
            "rendezvous_url = \"{}\"\nrelay_url = \"{}\"\ndata_dir = \"{}\"\noverwrite = {}\n",
            cfg.rendezvous_url,
            cfg.relay_url,
            cfg.data_dir.display(),
            cfg.overwrite
        )),
    }
}

/// `config set KEY VALUE`: validate, persist atomically, echo the stored value.
fn set(file: &ConfigFile, key: &str, value: &str) -> Result<String, CliError> {
    let mut file = file.clone();
    let value = file.set(key, value)?;
    file.save_to(&ConfigFile::path()?)?;
    Ok(format!("{key} = {value}\n"))
}
