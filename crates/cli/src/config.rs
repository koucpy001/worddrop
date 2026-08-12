//! TOML configuration file (T12): load/save with merge over core defaults.
//!
//! File location: `<platform-config-dir>/worddrop/config.toml` — the same
//! directory core uses for the identity key (see
//! [`worddrop_core::identity::Config::config_dir`], `WORDDROP_CONFIG_DIR` honored).
//!
//! Precedence, highest first: **env var > config file > built-in default**.
//! The env vars are the ones core already defines (`WORDDROP_DATA_DIR`,
//! `WORDDROP_RENDEZVOUS_URL`, `WORDDROP_RELAY_URL`); Android's app-data hook
//! goes through the same mechanism, so file settings never fight it.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use worddrop_core::identity;
use serde::{Deserialize, Serialize};

/// File name of the TOML config inside the config dir.
pub const CONFIG_FILE: &str = "config.toml";

#[cfg(test)]
mod tests;

/// The on-disk config: only fields the user has explicitly set (`None` = "use
/// env/default"). Serializing omits `None` fields, so the file stays minimal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFile {
    pub rendezvous_url: Option<String>,
    pub relay_url: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub overwrite: Option<bool>,
}

/// Errors from config file I/O, parsing, and resolution.
#[derive(Debug)]
pub enum ConfigError {
    /// No platform config dir could be resolved (identity error carries the
    /// `WORDDROP_CONFIG_DIR` hint).
    NoConfigDir(identity::Error),
    /// Failed to read an existing config file.
    Read { path: PathBuf, source: io::Error },
    /// The config file exists but is not valid TOML.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// Failed to write or atomically rename the config file.
    Write { path: PathBuf, source: io::Error },
    /// Failed to serialize the config to TOML.
    Encode {
        path: PathBuf,
        source: toml::ser::Error,
    },
    /// `config get/set` with a key that is not one of the four known fields.
    InvalidKey(String),
    /// `config set` with a value the field cannot take.
    InvalidValue { key: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // User-facing errors are bilingual (中文 + English) per the global
        // copy rule; the English half keeps the historical wording.
        match self {
            ConfigError::NoConfigDir(source) => write!(f, "{source}"),
            ConfigError::Read { path, source } => {
                write!(
                    f,
                    "读取配置文件失败 {}: {source} / failed to read config file {}: {source}",
                    path.display(),
                    path.display()
                )
            }
            ConfigError::Parse { path, source } => {
                write!(
                    f,
                    "配置文件无效 {}: {source} / invalid config file {}: {source}",
                    path.display(),
                    path.display()
                )
            }
            ConfigError::Write { path, source } => {
                write!(
                    f,
                    "写入配置文件失败 {}: {source} / failed to write config file {}: {source}",
                    path.display(),
                    path.display()
                )
            }
            ConfigError::Encode { path, source } => {
                write!(
                    f,
                    "配置文件编码失败 {}: {source} / failed to encode config file {}: {source}",
                    path.display(),
                    path.display()
                )
            }
            ConfigError::InvalidKey(key) => write!(
                f,
                "未知配置项 {key:?}（有效项：rendezvous_url、relay_url、data_dir、overwrite） / unknown config key {key:?}; valid keys: rendezvous_url, relay_url, data_dir, overwrite"
            ),
            ConfigError::InvalidValue { key, reason } => {
                write!(
                    f,
                    "配置项 {key:?} 的值无效: {reason} / invalid value for config key {key:?}: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::NoConfigDir(source) => Some(source),
            ConfigError::Read { source, .. } | ConfigError::Write { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::Encode { source, .. } => Some(source),
            ConfigError::InvalidKey(_) | ConfigError::InvalidValue { .. } => None,
        }
    }
}

/// Serializes env-mutating test code (edition 2024 marks `set_var`/`remove_var`
/// unsafe); shared by every test module that touches env vars.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl ConfigFile {
    /// `<config-dir>/config.toml`.
    pub fn path() -> Result<PathBuf, ConfigError> {
        identity::Config::config_dir()
            .map(|dir| dir.join(CONFIG_FILE))
            .map_err(ConfigError::NoConfigDir)
    }

    /// Load from [`ConfigFile::path`]; a missing file is [`ConfigFile::default`].
    pub fn load() -> Result<ConfigFile, ConfigError> {
        ConfigFile::load_from(&ConfigFile::path()?)
    }

    /// Load from an explicit path; a missing file is [`ConfigFile::default`].
    pub fn load_from(path: &Path) -> Result<ConfigFile, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(ConfigFile::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Write atomically (temp file + rename in the same dir, like core's key
    /// file): a crash never leaves a half-written config.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string(self).map_err(|source| ConfigError::Encode {
            path: path.to_path_buf(),
            source,
        })?;
        let dir = path.parent().ok_or_else(|| ConfigError::Write {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent"),
        })?;
        fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(CONFIG_FILE);
        let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        fs::write(&tmp, text).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Set one field, validating and normalizing `value`. Returns the
    /// normalized value ("true"/"false" for `overwrite`, trimmed strings).
    pub fn set(&mut self, key: &str, value: &str) -> Result<String, ConfigError> {
        let normalized = match key {
            "rendezvous_url" => {
                let v = non_empty(key, value)?;
                self.rendezvous_url = Some(v.clone());
                v
            }
            "relay_url" => {
                let v = non_empty(key, value)?;
                self.relay_url = Some(v.clone());
                v
            }
            "data_dir" => {
                let v = non_empty(key, value)?;
                self.data_dir = Some(PathBuf::from(v.clone()));
                v
            }
            "overwrite" => {
                let v = match value.trim() {
                    "true" => "true",
                    "false" => "false",
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            key: key.to_string(),
                            reason: "expected \"true\" or \"false\"".to_string(),
                        });
                    }
                };
                self.overwrite = Some(v == "true");
                v.to_string()
            }
            _ => return Err(ConfigError::InvalidKey(key.to_string())),
        };
        Ok(normalized)
    }
}

fn non_empty(key: &str, value: &str) -> Result<String, ConfigError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError::InvalidValue {
            key: key.to_string(),
            reason: "value must not be empty".to_string(),
        });
    }
    Ok(value.to_string())
}

/// Effective runtime config: the config file merged over core defaults, with
/// core's env overrides winning over both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub data_dir: PathBuf,
    pub rendezvous_url: String,
    pub relay_url: String,
    pub overwrite: bool,
}

impl Config {
    /// Load the config file (defaults if missing) and resolve it.
    pub fn load() -> Result<Config, ConfigError> {
        Config::resolve(&ConfigFile::load()?)
    }

    /// Merge precedence: env var > config file > built-in default.
    pub fn resolve(file: &ConfigFile) -> Result<Config, ConfigError> {
        let config_dir = identity::Config::config_dir().map_err(ConfigError::NoConfigDir)?;
        let data_dir = env_path(identity::ENV_DATA_DIR)
            .or_else(|| file.data_dir.clone())
            .unwrap_or(config_dir.clone());
        Ok(Config {
            data_dir,
            rendezvous_url: env_str(identity::ENV_RENDEZVOUS_URL)
                .or_else(|| file.rendezvous_url.clone())
                .unwrap_or_else(|| identity::DEFAULT_RENDEZVOUS_URL.to_string()),
            relay_url: env_str(identity::ENV_RELAY_URL)
                .or_else(|| file.relay_url.clone())
                .unwrap_or_else(|| identity::DEFAULT_RELAY_URL.to_string()),
            overwrite: file.overwrite.unwrap_or(false),
        })
    }

    /// Look up one field as its display string (for `config get <key>`).
    pub fn field(&self, key: &str) -> Result<String, ConfigError> {
        match key {
            "rendezvous_url" => Ok(self.rendezvous_url.clone()),
            "relay_url" => Ok(self.relay_url.clone()),
            "data_dir" => Ok(self.data_dir.display().to_string()),
            "overwrite" => Ok(self.overwrite.to_string()),
            _ => Err(ConfigError::InvalidKey(key.to_string())),
        }
    }
}

fn env_str(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    match std::env::var_os(key) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}
