//! Persistent identity and config resolution.
//!
//! The iroh [`SecretKey`] is a 32-byte ed25519 seed that defines the node's
//! identity. It is generated once, persisted as raw bytes at
//! `<config-dir>/key.bin` (0600 on unix) with an atomic write (temp file +
//! rename), and loaded on every subsequent run. The public half —
//! [`SecretKey::public`] — is the node's [`PublicKey`] (a.k.a. [`EndpointId`]),
//! the addressable `NodeId` used to dial the peer.
//!
//! Only the identity key and server URLs live in the config dir. Pairing
//! word-codes and transfer payloads are never persisted here.
//!
//! Corruption contract: the key file is raw 32 bytes with no checksum —
//! ed25519 accepts every 32-byte seed as valid, so same-length corruption is
//! undetectable by design. Only wrong-length or unreadable files trigger
//! regeneration (with a warning). Atomic writes make partial files impossible.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use iroh::{PublicKey, SecretKey};

/// Default rendezvous server URL: the EMQX public MQTT broker, used as a
/// public pairing mailbox (`mqtts://broker.emqx.io:8883`). Override for
/// self-hosted/local deployments.
pub const DEFAULT_RENDEZVOUS_URL: &str = "mqtts://broker.emqx.io:8883";
/// Default relay URL: the special value `"public"`, resolved by
/// `relay_mode_from_url` into `RelayMode::Default` (iroh's public relay).
/// Override with an explicit URL for self-hosted/local relays.
pub const DEFAULT_RELAY_URL: &str = "public";
/// File name of the persisted identity key inside the config dir.
pub const KEY_FILE: &str = "key.bin";

/// Overrides the config dir (required on Android, where the app data dir is
/// chosen by the OS).
pub const ENV_CONFIG_DIR: &str = "WORDDROP_CONFIG_DIR";
/// Overrides the data dir (blobs, transfer records).
pub const ENV_DATA_DIR: &str = "WORDDROP_DATA_DIR";
/// Overrides the rendezvous URL.
pub const ENV_RENDEZVOUS_URL: &str = "WORDDROP_RENDEZVOUS_URL";
/// Overrides the relay URL.
pub const ENV_RELAY_URL: &str = "WORDDROP_RELAY_URL";

/// Errors from identity loading and config resolution.
#[derive(Debug)]
pub enum Error {
    /// No platform config dir could be resolved and no `WORDDROP_CONFIG_DIR`
    /// override is set.
    ConfigDirNotFound,
    /// Failed to read the persisted key file.
    ReadKey { path: PathBuf, source: io::Error },
    /// Failed to create, write, or atomically rename the key file.
    WriteKey { dir: PathBuf, source: io::Error },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::ConfigDirNotFound => write!(
                f,
                "no platform config dir found; set {} to an explicit directory",
                ENV_CONFIG_DIR
            ),
            Error::ReadKey { path, source } => {
                write!(
                    f,
                    "failed to read identity key file {}: {source}",
                    path.display()
                )
            }
            Error::WriteKey { dir, source } => {
                write!(
                    f,
                    "failed to write identity key in {}: {source}",
                    dir.display()
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ReadKey { source, .. } | Error::WriteKey { source, .. } => Some(source),
            Error::ConfigDirNotFound => None,
        }
    }
}

/// Runtime configuration with platform defaults and env overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directory for blobs, transfer records, and (by default) the identity key.
    pub data_dir: PathBuf,
    /// Rendezvous server base URL.
    pub rendezvous_url: String,
    /// Relay server URL.
    pub relay_url: String,
}

impl Config {
    /// The worddrop config dir: `WORDDROP_CONFIG_DIR` override, else the
    /// platform config dir (Linux `$XDG_CONFIG_HOME` / `~/.config`, Windows
    /// `%APPDATA%`, macOS `~/Library/Application Support`) joined with
    /// `worddrop`.
    pub fn config_dir() -> Result<PathBuf, Error> {
        if let Some(dir) = std::env::var(ENV_CONFIG_DIR)
            .ok()
            .filter(|dir| !dir.is_empty())
        {
            return Ok(PathBuf::from(dir));
        }
        dirs::config_dir()
            .map(|dir| dir.join("worddrop"))
            .ok_or(Error::ConfigDirNotFound)
    }

    /// Load config from env with platform defaults.
    ///
    /// The data dir defaults to the config dir (identity key, blobs, and
    /// transfer records all live under one tree, per the plan's T4 decision);
    /// `WORDDROP_DATA_DIR` moves the data tree elsewhere.
    pub fn load() -> Result<Config, Error> {
        let data_dir = match std::env::var(ENV_DATA_DIR) {
            Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => Config::config_dir()?,
        };
        Ok(Config {
            data_dir,
            rendezvous_url: env_or(ENV_RENDEZVOUS_URL, DEFAULT_RENDEZVOUS_URL),
            relay_url: env_or(ENV_RELAY_URL, DEFAULT_RELAY_URL),
        })
    }

    /// Config rooted at an explicit data dir with default URLs.
    pub fn with_dir(data_dir: impl Into<PathBuf>) -> Config {
        Config {
            data_dir: data_dir.into(),
            rendezvous_url: DEFAULT_RENDEZVOUS_URL.to_string(),
            relay_url: DEFAULT_RELAY_URL.to_string(),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => value,
        _ => default.to_string(),
    }
}

/// The persistent node identity: a [`SecretKey`] with its derived [`PublicKey`].
#[derive(Debug, Clone)]
pub struct Identity {
    key: SecretKey,
}

impl Identity {
    /// Path of the persisted key file inside `config_dir`.
    pub fn key_path(config_dir: &Path) -> PathBuf {
        config_dir.join(KEY_FILE)
    }

    /// Load the persisted key or create a fresh one.
    ///
    /// - Key missing: generate + persist atomically.
    /// - Key present and exactly 32 bytes: load.
    /// - Key present but corrupt (wrong length): regenerate + persist, logging
    ///   a warning.
    /// - Other read failures (e.g. permissions): propagate as [`Error::ReadKey`].
    pub fn load_or_create(config_dir: &Path) -> Result<Identity, Error> {
        let key = load_or_create_key(config_dir)?;
        Ok(Identity { key })
    }

    /// The node's addressable public key (a.k.a. [`iroh::EndpointId`]).
    pub fn node_id(&self) -> PublicKey {
        self.key.public()
    }

    /// The secret key, for iroh endpoint construction.
    pub fn secret_key(&self) -> &SecretKey {
        &self.key
    }

    /// Raw 32-byte secret seed.
    pub fn key_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }
}

fn load_or_create_key(dir: &Path) -> Result<SecretKey, Error> {
    let path = dir.join(KEY_FILE);
    match fs::read(&path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            Ok(SecretKey::from_bytes(&seed))
        }
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                "identity key file is corrupt (expected 32 bytes); regenerating"
            );
            let key = SecretKey::generate();
            write_key_atomic(dir, &key.to_bytes())?;
            Ok(key)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            write_key_atomic(dir, &key.to_bytes())?;
            Ok(key)
        }
        Err(source) => Err(Error::ReadKey { path, source }),
    }
}

/// Write the key atomically: temp file in the same dir (0600 on unix, set
/// before any bytes land), `sync_all`, then rename over the target. A reader
/// never observes a partial file. Stale temp files from a crashed run are
/// removed and retried once.
fn write_key_atomic(dir: &Path, bytes: &[u8; 32]) -> Result<(), Error> {
    fs::create_dir_all(dir).map_err(|source| Error::WriteKey {
        dir: dir.to_path_buf(),
        source,
    })?;
    let tmp = dir.join(format!(".{KEY_FILE}.{}.tmp", std::process::id()));
    for attempt in 0..2 {
        match write_tmp(&tmp, bytes) {
            Ok(()) => {
                fs::rename(&tmp, dir.join(KEY_FILE)).map_err(|source| Error::WriteKey {
                    dir: dir.to_path_buf(),
                    source,
                })?;
                return Ok(());
            }
            // A stale temp from a crashed previous run: remove and retry once.
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                let _ = fs::remove_file(&tmp);
            }
            Err(source) => {
                return Err(Error::WriteKey {
                    dir: dir.to_path_buf(),
                    source,
                });
            }
        }
    }
    unreachable!("write_tmp retried exactly once")
}

fn write_tmp(tmp: &Path, bytes: &[u8; 32]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0600 before writing: the key never exists world-readable, even
        // briefly.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}
