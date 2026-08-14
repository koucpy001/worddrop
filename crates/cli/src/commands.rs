//! Subcommand dispatch. `send` runs the full send flow, `receive` runs the
//! full receive flow, and `config` is fully functional — all three are
//! implemented end to end (pairing, transfer, progress UI, resume).

use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh::RelayMode;
use tokio::sync::mpsc;
use tokio::time::timeout;

use worddrop_core::identity::Identity;
use worddrop_core::transfer::engine::{EngineSpec, TransferEngine};

use crate::{
    cli::{Cli, Commands, ConfigArgs, ConfigCommands, ReceiveArgs, SendArgs},
    config::{Config, ConfigFile},
    error::CliError,
    relay::relay_mode_from_url,
    rendezvous_client::RvClient,
    send::{SendOutcome, run_send},
    ui::{SendUi, human_bytes},
    wire::{CONTROL_ALPN, ControlAcceptor, FLOW_TIMEOUT},
};

#[cfg(test)]
mod tests;

/// Run the parsed CLI; returns the text to print on stdout.
pub fn run(args: Cli) -> Result<String, CliError> {
    match args.command {
        Commands::Send(args) => send(args),
        Commands::Receive(args) => receive(args),
        Commands::Cleanup => cleanup(),
        Commands::Config(args) => config(args),
    }
}

/// Which role this process plays; each role owns a private engine data dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Send,
    Receive,
}

impl Role {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Receive => "receive",
        }
    }

    /// Chinese label for the bilingual output.
    fn label(self) -> &'static str {
        match self {
            Self::Send => "发送",
            Self::Receive => "接收",
        }
    }
}

/// The engine data dir for `role`: a per-role subdir of the configured base.
/// The redb blob store (`blobs.db`) is exclusive to one process, so a sender
/// and a receiver on the same machine must never open the same one — the
/// second opener blocks forever in the store open (D3: receive hung after
/// "creating or opening meta database" while the sender held the file lock).
fn role_data_dir(base: &Path, role: Role) -> PathBuf {
    base.join(role.dir_name())
}

/// `worddrop send <paths...>`: prepare, allocate a nameplate, pair with
/// SPAKE2 (words only), transfer with a progress bar, cancel on Ctrl+C.
fn send(args: SendArgs) -> Result<String, CliError> {
    let runtime = tokio::runtime::Runtime::new().map_err(|source| {
        CliError::runtime(format!(
            "无法启动异步运行时 / failed to start async runtime: {source}"
        ))
    })?;
    runtime.block_on(send_async(args))
}

async fn send_async(args: SendArgs) -> Result<String, CliError> {
    let config = Config::load()?;
    let config_dir = worddrop_core::identity::Config::config_dir()?;
    let identity = Identity::load_or_create(&config_dir)?;

    // The sender must accept pairing control streams on its own ALPN: the
    // engine's router registers the acceptor at bind time.
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let acceptor: Box<dyn iroh::protocol::DynProtocolHandler> =
        ControlAcceptor::new(control_tx).into();
    let relay_mode = relay_mode_from_url(&config.relay_url).map_err(|source| {
        CliError::runtime(format!("无效的中继地址 {:?} / {source}", config.relay_url))
    })?;
    let engine = TransferEngine::new_spec(EngineSpec {
        data_dir: &role_data_dir(&config.data_dir, Role::Send),
        relay_mode,
        secret_key: Some(identity.secret_key()),
        extra_handler: Some((CONTROL_ALPN.to_vec(), acceptor)),
        track_served_bytes: true,
    })
    .await
    .map_err(|source| {
        CliError::runtime(format!(
            "无法启动传输引擎 / failed to start transfer engine: {source}"
        ))
    })?;

    // The ticket must carry the relay URL: wait for relay contact before
    // preparing the transfer (T11 learning: `online()` after bind).
    // Restricted networks can stall this up to 15 s — announce it up front
    // and name the relay on failure.
    eprintln!("正在连接中继服务器... (relay {})", config.relay_url);
    timeout(Duration::from_secs(15), engine.endpoint().online())
        .await
        .map_err(|_| {
            CliError::runtime(format!(
                "连接中继服务器超时（15 秒） / timed out contacting relay server {} after 15s",
                config.relay_url
            ))
        })?;

    let ui = SendUi::new();
    let interrupt = Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    let outcome = timeout(
        FLOW_TIMEOUT,
        run_send(
            engine,
            control_rx,
            RvClient::new(&config.rendezvous_url),
            args.paths,
            ui.clone(),
            interrupt,
            None,
        ),
    )
    .await
    .map_err(|_| {
        CliError::runtime(format!(
            "传输流程超过 {} 秒上限 / flow exceeded the {}s limit",
            FLOW_TIMEOUT.as_secs(),
            FLOW_TIMEOUT.as_secs()
        ))
    })??;

    let summary = match outcome {
        SendOutcome::Completed {
            bytes,
            files,
            skipped_files,
            ..
        } => {
            let mut line = format!(
                "传输完成：{files} 个文件，{} / Transfer complete: {files} files, {}\n",
                human_bytes(bytes),
                human_bytes(bytes)
            );
            if skipped_files > 0 {
                line.push_str(&format!(
                    "（跳过 {skipped_files} 个已存在文件 / skipped {skipped_files} existing files）\n"
                ));
            }
            line
        }
        SendOutcome::Declined { reason } => {
            format!("接收方已拒绝：{reason} / Receiver declined: {reason}\n")
        }
        SendOutcome::Cancelled => "已取消 / Cancelled\n".to_string(),
    };
    Ok(summary)
}

/// `worddrop receive [--code CODE] [--output DIR]`: split the typed code into
/// nameplate + words (F1: words never leave the client), claim the NAME-plate
/// only via rendezvous, on pending → dial sender via ticket, SPAKE2 with the
/// WORDS as password, receive Offer → prompt accept/decline (interactive,
/// default no after 60s), on accept → download + export with progress.
fn receive(args: ReceiveArgs) -> Result<String, CliError> {
    let runtime = tokio::runtime::Runtime::new().map_err(|source| {
        CliError::runtime(format!(
            "无法启动异步运行时 / failed to start async runtime: {source}"
        ))
    })?;
    runtime.block_on(receive_async(args))
}

async fn receive_async(args: ReceiveArgs) -> Result<String, CliError> {
    let config = Config::load()?;
    let interrupt = Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    let relay_mode = relay_mode_from_url(&config.relay_url).map_err(|source| {
        CliError::runtime(format!("无效的中继地址 {:?} / {source}", config.relay_url))
    })?;
    let outcome = tokio::time::timeout(
        FLOW_TIMEOUT,
        crate::receive::run_receive(
            args.code,
            crate::receive::ReceiveOpts {
                output: args.output,
                data_dir: role_data_dir(&config.data_dir, Role::Receive),
                rendezvous_url: config.rendezvous_url.clone(),
                relay_mode,
                overwrite: config.overwrite,
                auto_accept: None, // interactive prompt
            },
            interrupt,
        ),
    )
    .await
    .map_err(|_| {
        CliError::runtime(format!(
            "传输流程超过 {} 秒上限 / flow exceeded the {}s limit",
            FLOW_TIMEOUT.as_secs(),
            FLOW_TIMEOUT.as_secs()
        ))
    })??;

    let summary = match outcome {
        crate::receive::ReceiveOutcome::Completed {
            bytes,
            files,
            skipped_files,
            ..
        } => {
            let mut line = format!(
                "传输完成：{files} 个文件，{} / Transfer complete: {files} files, {}\n",
                human_bytes(bytes),
                human_bytes(bytes)
            );
            if skipped_files > 0 {
                line.push_str(&format!(
                    "（跳过 {skipped_files} 个已存在文件 / skipped {skipped_files} existing files）\n"
                ));
            }
            line
        }
        crate::receive::ReceiveOutcome::Declined => "已拒绝 / Declined\n".to_string(),
        crate::receive::ReceiveOutcome::Cancelled => "已取消 / Cancelled\n".to_string(),
    };
    Ok(summary)
}

fn config(args: ConfigArgs) -> Result<String, CliError> {
    let file = ConfigFile::load()?;
    match args.command {
        Some(ConfigCommands::Get(args)) => get(&file, args.key.as_deref()),
        Some(ConfigCommands::Set(set_args)) => set(&file, &set_args.key, &set_args.value),
        None => get(&file, None),
    }
}

/// `worddrop cleanup`: clear the blob caches of both role stores.
///
/// No pairing, no network: each role dir gets a short-lived engine with the
/// relay disabled. The cached blob count is read before the clear (the whole
/// `<data_dir>/<role>/blobs` directory is removed — no per-blob GC). Engines
/// are created and shut down one role at a time — two open FsStores on the
/// same redb would deadlock (D3).
fn cleanup() -> Result<String, CliError> {
    let runtime = tokio::runtime::Runtime::new().map_err(|source| {
        CliError::runtime(format!(
            "无法启动异步运行时 / failed to start async runtime: {source}"
        ))
    })?;
    runtime.block_on(cleanup_async())
}

async fn cleanup_async() -> Result<String, CliError> {
    let config = Config::load()?;
    let mut lines = Vec::with_capacity(2);
    for role in [Role::Send, Role::Receive] {
        let engine = TransferEngine::new_spec(EngineSpec {
            data_dir: &role_data_dir(&config.data_dir, role),
            relay_mode: RelayMode::Disabled,
            secret_key: None,
            extra_handler: None,
            track_served_bytes: false,
        })
        .await
        .map_err(|source| {
            CliError::runtime(format!(
                "无法打开 {} 角色的缓存 / failed to open {} role cache: {source}",
                role.dir_name(),
                role.dir_name()
            ))
        })?;
        let count = engine
            .store()
            .blobs()
            .list()
            .hashes()
            .await
            .map_err(|source| {
                CliError::runtime(format!(
                    "无法读取 {} 角色的缓存 / failed to read {} role cache: {source}",
                    role.dir_name(),
                    role.dir_name()
                ))
            })?
            .len();
        engine.clear_cache().await.map_err(|source| {
            CliError::runtime(format!(
                "无法清空 {} 角色的缓存，请确认没有进行中的传输后重试（Windows 下文件被占用会失败）/ failed to clear {} role cache, retry after closing any active transfer (Windows may fail while files are in use): {source}",
                role.dir_name(),
                role.dir_name()
            ))
        })?;
        engine.shutdown().await.map_err(|source| {
            CliError::runtime(format!(
                "无法关闭 {} 角色的引擎 / failed to shut down {} role engine: {source}",
                role.dir_name(),
                role.dir_name()
            ))
        })?;
        lines.push(format!(
            "已清空{}缓存 {} 个 blob / Cleared {} cache ({} blobs)",
            role.label(),
            count,
            role.dir_name(),
            count
        ));
    }
    Ok(lines.join("\n"))
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
