//! Command dispatch tests (T12 + T14): `config set`/`get` roundtrip, error
//! paths, and receive-code split + error mapping.

use std::{error::Error, fs, path::PathBuf};

use my_croc_core::identity;
use my_croc_core::pairing::spake::SpakeError;
use my_croc_core::pairing::wordcode::WordCode;

use crate::{
    cli::{Cli, Commands, ConfigArgs, ConfigCommands, GetArgs, SetArgs},
    commands,
    config::{CONFIG_FILE, ConfigFile, ENV_LOCK},
    error::CliError,
    receive::RecvError,
};

fn temp_config_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("my-croc-cli-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Run `f` with `MY_CROC_CONFIG_DIR` pointed at `dir`.
///
/// SAFETY: `set_var`/`remove_var` are unsafe in edition 2024; the crate-wide
/// `ENV_LOCK` keeps env access race-free.
fn with_config_dir<F: FnOnce()>(dir: &PathBuf, f: F) {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe { std::env::set_var(identity::ENV_CONFIG_DIR, dir) };
    f();
    unsafe { std::env::remove_var(identity::ENV_CONFIG_DIR) };
}

fn cli_with(command: Commands) -> Cli {
    Cli {
        verbose: 0,
        command,
    }
}

fn get_cmd(key: Option<&str>) -> Commands {
    Commands::Config(ConfigArgs {
        command: Some(ConfigCommands::Get(GetArgs {
            key: key.map(str::to_string),
        })),
    })
}

fn set_cmd(key: &str, value: &str) -> Commands {
    Commands::Config(ConfigArgs {
        command: Some(ConfigCommands::Set(SetArgs {
            key: key.to_string(),
            value: value.to_string(),
        })),
    })
}

#[test]
fn config_set_then_get_roundtrip() {
    let dir = temp_config_dir("set-get");
    with_config_dir(&dir, || {
        let set = commands::run(cli_with(set_cmd("relay_url", "http://relay.example:3340")));
        assert_eq!(
            set.expect("set ok"),
            "relay_url = http://relay.example:3340\n"
        );

        let file = ConfigFile::load().expect("file loads");
        assert_eq!(file.relay_url.as_deref(), Some("http://relay.example:3340"));

        let get = commands::run(cli_with(get_cmd(Some("relay_url"))));
        assert_eq!(get.expect("get ok"), "http://relay.example:3340\n");
    });
}

#[test]
fn config_get_all_prints_effective_values() {
    let dir = temp_config_dir("get-all");
    with_config_dir(&dir, || {
        let mut file = ConfigFile::default();
        file.set("overwrite", "true").expect("set");
        file.save_to(&ConfigFile::path().expect("config path"))
            .expect("save");

        let out = commands::run(cli_with(get_cmd(None))).expect("get");
        assert!(out.contains(&format!(
            "rendezvous_url = \"{}\"",
            identity::DEFAULT_RENDEZVOUS_URL
        )));
        assert!(out.contains("overwrite = true"));
    });
}

#[test]
fn config_set_invalid_value_is_user_error() {
    let dir = temp_config_dir("set-invalid");
    with_config_dir(&dir, || {
        let err = commands::run(cli_with(set_cmd("overwrite", "maybe")))
            .expect_err("invalid bool must fail");
        assert!(matches!(err, CliError::User(_)));
    });
}

#[test]
fn config_set_unknown_key_is_user_error() {
    let dir = temp_config_dir("set-unknown");
    with_config_dir(&dir, || {
        let err =
            commands::run(cli_with(set_cmd("bogus", "x"))).expect_err("unknown key must fail");
        assert!(matches!(err, CliError::User(_)));
    });
}

#[test]
fn config_get_unknown_key_is_user_error() {
    let dir = temp_config_dir("get-unknown");
    with_config_dir(&dir, || {
        let err =
            commands::run(cli_with(get_cmd(Some("bogus")))).expect_err("unknown key must fail");
        assert!(matches!(err, CliError::User(_)));
    });
}

#[test]
fn config_invalid_toml_surfaces_as_user_error() {
    let dir = temp_config_dir("broken-toml");
    with_config_dir(&dir, || {
        fs::write(dir.join(CONFIG_FILE), "relay_url = [oops").expect("write broken toml");
        let err = commands::run(cli_with(get_cmd(None))).expect_err("broken toml must fail");
        assert!(matches!(err, CliError::User(_)));
    });
}

#[test]
fn receive_code_split_valid_and_invalid() {
    // split() splits on the first '-' only — the security seam that separates
    // the nameplate from the words. It validates the nameplate but NOT the
    // word count (full validation is WordCode::validate).
    let (n, w) = WordCode::split("7-correct-horse-battery").expect("valid code splits");
    assert_eq!(n, 7);
    assert_eq!(w, "correct-horse-battery");

    // split() accepts a single word after the nameplate — it returns words
    // verbatim (the security boundary; full validation is later).
    let (n, w) = WordCode::split("7-correct").expect("split single word");
    assert_eq!(n, 7);
    assert_eq!(w, "correct");

    // Missing hyphen fails.
    assert!(WordCode::split("not-a-code").is_err());

    // Empty words portion fails.
    assert!(WordCode::split("7-").is_err());

    // Non-numeric nameplate fails.
    assert!(WordCode::split("abc-words").is_err());
}

#[test]
fn recv_error_user_vs_runtime_mapping() {
    // Word-code and wrong-words failures are user errors (exit 1).
    let user_err = RecvError::NoCode;
    let cli: CliError = user_err.into();
    assert!(
        matches!(cli, CliError::User(_)),
        "no-code should be user error, got {cli:?}"
    );

    let user_err = RecvError::Pair(crate::wire::PairError::Spake(
        SpakeError::ConfirmationMismatch,
    ));
    let cli: CliError = user_err.into();
    assert!(
        matches!(cli, CliError::User(_)),
        "wrong-words should be user error, got {cli:?}"
    );

    // Runtime errors map to exit 2.
    let runtime_err = RecvError::RelayHung;
    let cli: CliError = runtime_err.into();
    assert!(
        matches!(cli, CliError::Runtime(_)),
        "relay-hung should be runtime error, got {cli:?}"
    );

    let runtime_err = RecvError::Hung("something");
    let cli: CliError = runtime_err.into();
    assert!(
        matches!(cli, CliError::Runtime(_)),
        "hung should be runtime error, got {cli:?}"
    );
}

#[test]
fn recv_error_display_and_source() {
    let err = RecvError::Ticket("bad".to_string());
    assert!(err.to_string().contains("invalid ticket"));
    assert!(err.source().is_none());

    let err = RecvError::Word(WordCode::split("bad").unwrap_err());
    assert!(err.to_string().contains("word-code error"));
    assert!(err.source().is_some());
}

#[test]
fn role_data_dirs_are_private_per_role() {
    // D3 regression: send and receive previously shared one engine data dir,
    // so a concurrent pair on one machine deadlocked on the redb blob store
    // (blobs.db is exclusive to one process — the receiver hung right after
    // "creating or opening meta database" while the sender held the file
    // lock). Each role must own a private subdir of the configured base.
    let base = std::env::temp_dir().join("my-croc-role-base");
    let send = commands::role_data_dir(&base, commands::Role::Send);
    let recv = commands::role_data_dir(&base, commands::Role::Receive);
    assert_eq!(send, base.join("send"));
    assert_eq!(recv, base.join("receive"));
    assert_ne!(
        send, recv,
        "send and receive must not share an engine data dir"
    );
    assert!(send.starts_with(&base) && recv.starts_with(&base));
}
