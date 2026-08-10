//! Config get/set for the GUI: mirror of the CLI's `my-croc config` command,
//! reusing `my_croc_cli::config` (env > file > default precedence).

use my_croc_cli::config::{Config, ConfigFile};

/// The effective runtime config (env > file > built-in default).
#[derive(Debug, Clone)]
pub struct ConfigDto {
    pub rendezvous_url: String,
    pub relay_url: String,
    pub data_dir: String,
    pub overwrite: bool,
}

/// Read the effective config.
pub fn get_config() -> Result<ConfigDto, String> {
    let cfg = Config::load().map_err(|err| err.to_string())?;
    Ok(ConfigDto {
        rendezvous_url: cfg.rendezvous_url,
        relay_url: cfg.relay_url,
        data_dir: cfg.data_dir.display().to_string(),
        overwrite: cfg.overwrite,
    })
}

/// Set one config key in the config file and persist it. Valid keys:
/// rendezvous_url, relay_url, data_dir, overwrite. Returns the normalized
/// stored value ("true"/"false" for overwrite).
pub fn set_config(key: String, value: String) -> Result<String, String> {
    let mut file = ConfigFile::load().map_err(|err| err.to_string())?;
    let normalized = file.set(&key, &value).map_err(|err| err.to_string())?;
    let path = ConfigFile::path().map_err(|err| err.to_string())?;
    file.save_to(&path).map_err(|err| err.to_string())?;
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use my_croc_core::identity;

    use super::*;
    use crate::api::ENV_LOCK;

    /// Point the config dir at a fresh temp dir for the duration of `f`.
    fn with_temp_config<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "my-croc-bridge-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized by ENV_LOCK; restored on drop.
        unsafe {
            std::env::set_var(identity::ENV_CONFIG_DIR, &dir);
        }
        let result = f();
        // SAFETY: serialized by ENV_LOCK; we restore the previous value.
        unsafe {
            std::env::remove_var(identity::ENV_CONFIG_DIR);
        }
        result
    }

    #[test]
    fn set_then_get_round_trips_each_key() {
        with_temp_config(|| {
            set_config("rendezvous_url".into(), "http://127.0.0.1:18081".into()).unwrap();
            set_config("relay_url".into(), "disabled".into()).unwrap();
            set_config("data_dir".into(), "/tmp/my-croc-gui".into()).unwrap();
            set_config("overwrite".into(), "true".into()).unwrap();

            let cfg = get_config().unwrap();
            assert_eq!(cfg.rendezvous_url, "http://127.0.0.1:18081");
            assert_eq!(cfg.relay_url, "disabled");
            assert_eq!(cfg.data_dir, "/tmp/my-croc-gui");
            assert!(cfg.overwrite);
        });
    }

    #[test]
    fn set_overwrite_normalizes_bool_strings() {
        with_temp_config(|| {
            assert_eq!(set_config("overwrite".into(), "false".into()).unwrap(), "false");
            assert!(!get_config().unwrap().overwrite);
        });
    }

    #[test]
    fn unknown_key_or_bad_value_is_rejected() {
        with_temp_config(|| {
            assert!(set_config("bogus".into(), "x".into()).is_err());
            assert!(set_config("overwrite".into(), "maybe".into()).is_err());
            assert!(set_config("relay_url".into(), "".into()).is_err());
        });
    }

    #[test]
    fn unset_keys_fall_back_to_defaults() {
        with_temp_config(|| {
            let cfg = get_config().unwrap();
            assert_eq!(cfg.rendezvous_url, identity::DEFAULT_RENDEZVOUS_URL);
            assert_eq!(cfg.relay_url, identity::DEFAULT_RELAY_URL);
            assert!(!cfg.overwrite);
        });
    }
}
