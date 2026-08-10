//! Config file and resolution tests (T12): save/load roundtrip, parse errors,
//! env > file > default precedence.

use std::{fs, path::PathBuf};

use my_croc_core::identity;

use super::{Config, ConfigError, ConfigFile, ENV_LOCK};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("my-croc-cli-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn full_file() -> ConfigFile {
    ConfigFile {
        rendezvous_url: Some("http://rv.example:8080".to_string()),
        relay_url: Some("http://relay.example:3340".to_string()),
        data_dir: Some(PathBuf::from("/tmp/my-croc-data")),
        overwrite: Some(true),
    }
}

/// Set/clear an env var. Callers MUST hold `ENV_LOCK`.
///
/// SAFETY: `set_var`/`remove_var` are unsafe in edition 2024; holding the
/// crate-wide `ENV_LOCK` keeps env access race-free, so no other thread reads
/// or writes env vars concurrently.
unsafe fn set_env(key: &str, value: Option<&str>) {
    match value {
        Some(value) => unsafe { std::env::set_var(key, value) },
        None => unsafe { std::env::remove_var(key) },
    }
}

#[test]
fn config_roundtrip_save_then_load() {
    let path = temp_dir("roundtrip").join("config.toml");
    full_file().save_to(&path).expect("save");
    let loaded = ConfigFile::load_from(&path).expect("load");
    assert_eq!(loaded, full_file());
}

#[test]
fn config_roundtrip_defaults() {
    let path = temp_dir("defaults").join("config.toml");
    ConfigFile::default().save_to(&path).expect("save");
    let loaded = ConfigFile::load_from(&path).expect("load");
    assert_eq!(loaded, ConfigFile::default());
}

#[test]
fn config_load_missing_file_is_default() {
    let loaded = ConfigFile::load_from(&temp_dir("missing").join("nope.toml")).expect("load");
    assert_eq!(loaded, ConfigFile::default());
}

#[test]
fn config_invalid_toml_is_parse_error() {
    let path = temp_dir("invalid").join("config.toml");
    fs::write(&path, "rendezvous_url = [unclosed").expect("write garbage");
    assert!(matches!(
        ConfigFile::load_from(&path),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn config_set_validates_and_normalizes() {
    let mut file = ConfigFile::default();
    assert_eq!(file.set("overwrite", "true").expect("ok"), "true");
    assert_eq!(file.overwrite, Some(true));
    assert!(matches!(
        file.set("overwrite", "maybe"),
        Err(ConfigError::InvalidValue { .. })
    ));
    assert!(matches!(
        file.set("relay_url", "   "),
        Err(ConfigError::InvalidValue { .. })
    ));
    assert!(matches!(
        file.set("bogus", "x"),
        Err(ConfigError::InvalidKey(_))
    ));
    assert_eq!(file.set("data_dir", " /tmp/d ").expect("ok"), "/tmp/d");
    assert_eq!(file.data_dir, Some(PathBuf::from("/tmp/d")));
}

#[test]
fn config_resolve_file_over_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        set_env(identity::ENV_RENDEZVOUS_URL, None);
        set_env(identity::ENV_RELAY_URL, None);
        set_env(identity::ENV_DATA_DIR, None);
    }
    let cfg = Config::resolve(&full_file()).expect("resolve");
    assert_eq!(cfg.rendezvous_url, "http://rv.example:8080");
    assert_eq!(cfg.relay_url, "http://relay.example:3340");
    assert_eq!(cfg.data_dir, PathBuf::from("/tmp/my-croc-data"));
    assert!(cfg.overwrite);
}

#[test]
fn config_resolve_env_wins_over_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { set_env(identity::ENV_RELAY_URL, Some("http://env-relay:9999")) };
    let cfg = Config::resolve(&full_file()).expect("resolve");
    assert_eq!(cfg.relay_url, "http://env-relay:9999");
    assert_eq!(cfg.rendezvous_url, "http://rv.example:8080");
}

#[test]
fn config_resolve_defaults() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        set_env(identity::ENV_RENDEZVOUS_URL, None);
        set_env(identity::ENV_RELAY_URL, None);
        set_env(identity::ENV_DATA_DIR, None);
    }
    let cfg = Config::resolve(&ConfigFile::default()).expect("resolve");
    assert_eq!(cfg.rendezvous_url, identity::DEFAULT_RENDEZVOUS_URL);
    assert_eq!(cfg.relay_url, identity::DEFAULT_RELAY_URL);
    assert!(!cfg.overwrite);
    assert_eq!(
        cfg.data_dir,
        identity::Config::config_dir().expect("platform config dir")
    );
}

#[test]
fn config_field_lookup() {
    let cfg = Config::resolve(&full_file()).expect("resolve");
    assert_eq!(cfg.field("overwrite").expect("ok"), "true");
    assert_eq!(
        cfg.field("relay_url").expect("ok"),
        "http://relay.example:3340"
    );
    assert!(matches!(
        cfg.field("bogus"),
        Err(ConfigError::InvalidKey(_))
    ));
}
