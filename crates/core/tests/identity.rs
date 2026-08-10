//! Integration tests for `my_croc_core::identity` — persistent SecretKey,
//! NodeId derivation, and config dir resolution (T4 acceptance).

use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use iroh::SecretKey;
use my_croc_core::identity::{
    Config, DEFAULT_RELAY_URL, DEFAULT_RENDEZVOUS_URL, ENV_CONFIG_DIR, ENV_DATA_DIR, ENV_RELAY_URL,
    ENV_RENDEZVOUS_URL, Error, Identity,
};

/// Unique temp dir per test call: isolated from other tests and from other
/// processes (pid + counter), so concurrent suite runs cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serializes tests that mutate process-global env vars. Safety: all env
/// mutation in this suite happens under this lock; no other code in this
/// binary mutates process env concurrently.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "my-croc-identity-test-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[cfg(unix)]
fn file_mode(path: &PathBuf) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .expect("stat key file")
        .permissions()
        .mode()
}

fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn identity_first_run_creates_key_file_with_0600_perms() {
    let dir = temp_dir("first-run");
    let identity = Identity::load_or_create(&dir).expect("create identity");

    let path = Identity::key_path(&dir);
    assert!(path.exists(), "key file must exist after first run");
    let bytes = fs::read(&path).expect("read key file");
    assert_eq!(bytes.len(), 32, "key file holds the raw 32-byte seed");
    #[cfg(unix)]
    assert_eq!(
        file_mode(&path) & 0o777,
        0o600,
        "key file must be 0600 on unix"
    );
    assert_eq!(
        identity.node_id(),
        SecretKey::from_bytes(&bytes.try_into().expect("32 bytes")).public(),
        "NodeId derives from the persisted key"
    );
    cleanup(&dir);
}

#[test]
fn identity_second_run_loads_same_key() {
    let dir = temp_dir("second-run");
    let first = Identity::load_or_create(&dir).expect("first run");
    let second = Identity::load_or_create(&dir).expect("second run");

    assert_eq!(
        first.key_bytes(),
        second.key_bytes(),
        "key must be stable across runs"
    );
    assert_eq!(
        first.node_id(),
        second.node_id(),
        "NodeId must be stable across runs"
    );
    cleanup(&dir);
}

#[test]
fn identity_corrupt_key_file_is_regenerated_with_warning() {
    // Wrong length (not 32 bytes): corrupt -> regenerate + persist.
    let dir = temp_dir("corrupt-short");
    fs::write(Identity::key_path(&dir), b"garbage-not-32-bytes").expect("write garbage");
    let identity = Identity::load_or_create(&dir).expect("regenerate on corrupt file");
    let bytes = fs::read(Identity::key_path(&dir)).expect("read regenerated key");
    assert_eq!(
        bytes.len(),
        32,
        "corrupt file must be replaced with a valid key"
    );
    assert_eq!(
        identity.node_id(),
        SecretKey::from_bytes(&bytes.try_into().expect("32 bytes")).public()
    );
    cleanup(&dir);
}

#[test]
fn identity_any_32_byte_seed_is_accepted_as_the_key() {
    // Contract: a 32-byte file IS the key. ed25519 accepts every 32-byte
    // seed, so same-length corruption is undetectable by design; only wrong
    // length (or unreadable) files trigger regeneration.
    let dir = temp_dir("seed-32");
    let garbage = [0xAAu8; 32];
    fs::write(Identity::key_path(&dir), garbage).expect("write seed");
    let identity = Identity::load_or_create(&dir).expect("load 32-byte seed");
    assert_eq!(identity.key_bytes(), garbage, "32-byte seed is used as-is");
    assert_eq!(identity.node_id(), SecretKey::from_bytes(&garbage).public());
    cleanup(&dir);
}

#[test]
fn identity_config_defaults_are_correct() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    for var in [
        ENV_CONFIG_DIR,
        ENV_DATA_DIR,
        ENV_RENDEZVOUS_URL,
        ENV_RELAY_URL,
    ] {
        // Safety: env mutation is serialized under ENV_LOCK.
        unsafe { std::env::remove_var(var) };
    }
    let cfg = Config::load().expect("load config");
    assert_eq!(cfg.rendezvous_url, DEFAULT_RENDEZVOUS_URL);
    assert_eq!(cfg.relay_url, DEFAULT_RELAY_URL);
    assert_eq!(
        cfg.data_dir,
        Config::config_dir().expect("config dir"),
        "data dir defaults to the config dir (T4 decision)"
    );
}

#[test]
fn identity_my_croc_config_dir_env_override_works() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let dir = temp_dir("env-config-dir");
    let original = std::env::var(ENV_CONFIG_DIR).ok();
    // Safety: env mutation is serialized under ENV_LOCK.
    unsafe { std::env::set_var(ENV_CONFIG_DIR, &dir) };

    let result = (|| -> Result<(), Error> {
        assert_eq!(
            Config::config_dir()?,
            dir,
            "override replaces the platform config dir"
        );
        let cfg = Config::load()?;
        assert_eq!(
            cfg.data_dir, dir,
            "data dir follows the config dir override"
        );
        let identity = Identity::load_or_create(&dir)?;
        assert!(
            Identity::key_path(&dir).exists(),
            "key lands in the override dir"
        );
        assert_eq!(identity.key_bytes().len(), 32);
        Ok(())
    })();

    match original {
        Some(value) => {
            // Safety: env mutation is serialized under ENV_LOCK.
            unsafe { std::env::set_var(ENV_CONFIG_DIR, value) };
        }
        None => {
            // Safety: env mutation is serialized under ENV_LOCK.
            unsafe { std::env::remove_var(ENV_CONFIG_DIR) };
        }
    }
    result.expect("config dir override");
    cleanup(&dir);
}

#[test]
fn identity_env_url_and_data_dir_overrides_apply() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let data = temp_dir("env-data-dir");
    let original = [
        (ENV_DATA_DIR, std::env::var(ENV_DATA_DIR).ok()),
        (ENV_RENDEZVOUS_URL, std::env::var(ENV_RENDEZVOUS_URL).ok()),
        (ENV_RELAY_URL, std::env::var(ENV_RELAY_URL).ok()),
    ];
    // Safety: env mutation is serialized under ENV_LOCK.
    unsafe {
        std::env::set_var(ENV_DATA_DIR, &data);
        std::env::set_var(ENV_RENDEZVOUS_URL, "http://example.test:9999");
        std::env::set_var(ENV_RELAY_URL, "http://relay.example.test:4444");
    }

    let cfg = Config::load().expect("load config with overrides");
    assert_eq!(cfg.data_dir, data);
    assert_eq!(cfg.rendezvous_url, "http://example.test:9999");
    assert_eq!(cfg.relay_url, "http://relay.example.test:4444");

    for (var, value) in original {
        // Safety: env mutation is serialized under ENV_LOCK.
        unsafe {
            match value {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }
    cleanup(&data);
}
