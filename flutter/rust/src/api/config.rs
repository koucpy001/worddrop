//! Config get/set for the GUI: mirror of the CLI's `worddrop config` command,
//! reusing `worddrop_cli::config` (env > file > default precedence).

// Android-only use on the host lib target (cfg-gated functions below).
#[cfg_attr(not(target_os = "android"), allow(unused_imports))]
use std::path::PathBuf;
#[cfg_attr(not(target_os = "android"), allow(unused_imports))]
use std::sync::Once;

use worddrop_cli::config::{Config, ConfigFile};
#[cfg_attr(not(target_os = "android"), allow(unused_imports))]
use worddrop_core::identity;

/// Android app-scoped external files dir, e.g. for `com.worddrop.app`:
/// `/storage/emulated/0/Android/data/com.worddrop.app/files`.
///
/// This is the Rust-side equivalent of `Context.getExternalFilesDir(null)`
/// (scoped storage, writable without any runtime permission). It is used as
/// the Android config/data dir because `dirs::config_dir()` returns `None`
/// in the Android app sandbox (no `HOME`/`XDG_CONFIG_HOME`), which would
/// otherwise fail every config-dependent call with "no platform config dir
/// found" (real-device finding, T8).
#[cfg(target_os = "android")]
fn android_app_files_dir() -> Option<PathBuf> {
    // The first NUL-terminated token of /proc/self/cmdline is the process
    // name == applicationId (e.g. `com.worddrop.app`).
    let pkg = std::fs::read_to_string("/proc/self/cmdline")
        .ok()?
        .split('\0')
        .next()
        .filter(|s| !s.is_empty())?
        .to_string();
    let external = std::env::var_os("EXTERNAL_STORAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/storage/emulated/0"));
    Some(external.join("Android/data").join(pkg).join("files"))
}

/// Make the config dir resolvable on Android: the platform default
/// (`dirs::config_dir()`) is `None` in the app sandbox, so we point
/// `WORDDROP_CONFIG_DIR` at the app-scoped external files dir unless the user
/// already set it. No-op on other platforms. Call before any config load.
pub fn ensure_android_config_dir() {
    #[cfg(target_os = "android")]
    {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            if std::env::var_os(identity::ENV_CONFIG_DIR).is_some() {
                return; // explicit user override wins
            }
            if let Some(dir) = android_app_files_dir() {
                // SAFETY: runs once at bridge startup before any other
                // thread reads the environment; mirrors the test helpers.
                unsafe { std::env::set_var(identity::ENV_CONFIG_DIR, &dir) };
            }
        });
    }
}

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
    ensure_android_config_dir();
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
    ensure_android_config_dir();
    let mut file = ConfigFile::load().map_err(|err| err.to_string())?;
    let normalized = file.set(&key, &value).map_err(|err| err.to_string())?;
    let path = ConfigFile::path().map_err(|err| err.to_string())?;
    file.save_to(&path).map_err(|err| err.to_string())?;
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use worddrop_core::identity;

    use super::*;
    use crate::api::ENV_LOCK;

    /// The app-scoped external dir derivation, factored out for host tests.
    fn app_files_dir(external: &str, pkg: &str) -> PathBuf {
        PathBuf::from(external)
            .join("Android/data")
            .join(pkg)
            .join("files")
    }

    #[test]
    fn android_app_files_dir_shape_matches_get_external_files_dir() {
        assert_eq!(
            app_files_dir("/storage/emulated/0", "com.worddrop.app"),
            PathBuf::from("/storage/emulated/0/Android/data/com.worddrop.app/files")
        );
    }

    #[test]
    fn ensure_android_config_dir_is_noop_off_android() {
        let _guard = ENV_LOCK.lock().unwrap();
        let had = std::env::var_os(identity::ENV_CONFIG_DIR);
        // SAFETY: serialized by ENV_LOCK; restored on drop.
        unsafe {
            std::env::remove_var(identity::ENV_CONFIG_DIR);
        }
        ensure_android_config_dir();
        assert!(std::env::var_os(identity::ENV_CONFIG_DIR).is_none());
        if let Some(v) = had {
            // SAFETY: serialized by ENV_LOCK.
            unsafe { std::env::set_var(identity::ENV_CONFIG_DIR, v) };
        }
    }

    /// Point the config dir at a fresh temp dir for the duration of `f`.
    fn with_temp_config<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("worddrop-bridge-config-test-{}", std::process::id()));
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
            set_config("data_dir".into(), "/tmp/worddrop-gui".into()).unwrap();
            set_config("overwrite".into(), "true".into()).unwrap();

            let cfg = get_config().unwrap();
            assert_eq!(cfg.rendezvous_url, "http://127.0.0.1:18081");
            assert_eq!(cfg.relay_url, "disabled");
            assert_eq!(cfg.data_dir, "/tmp/worddrop-gui");
            assert!(cfg.overwrite);
        });
    }

    #[test]
    fn set_overwrite_normalizes_bool_strings() {
        with_temp_config(|| {
            assert_eq!(
                set_config("overwrite".into(), "false".into()).unwrap(),
                "false"
            );
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
