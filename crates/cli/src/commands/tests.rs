//! Command dispatch tests (T12): `config set`/`get` roundtrip, error paths,
//! and the send/receive stub output.

use std::{fs, path::PathBuf};

use my_croc_core::identity;

use crate::{
    cli::{Cli, Commands, ConfigArgs, ConfigCommands, GetArgs, ReceiveArgs, SendArgs, SetArgs},
    commands,
    config::{ConfigFile, ENV_LOCK, CONFIG_FILE},
    error::CliError,
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
        assert_eq!(set.expect("set ok"), "relay_url = http://relay.example:3340\n");

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
        file.save_to(&ConfigFile::path().expect("config path")).expect("save");

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
        let err = commands::run(cli_with(set_cmd("bogus", "x"))).expect_err("unknown key must fail");
        assert!(matches!(err, CliError::User(_)));
    });
}

#[test]
fn config_get_unknown_key_is_user_error() {
    let dir = temp_config_dir("get-unknown");
    with_config_dir(&dir, || {
        let err = commands::run(cli_with(get_cmd(Some("bogus"))))
            .expect_err("unknown key must fail");
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
fn send_stub_reports_paths() {
    let out = commands::run(cli_with(Commands::Send(SendArgs {
        paths: vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
    })));
    assert_eq!(out.expect("ok"), "send: 2 path(s): a.txt, b.txt\n");
}

#[test]
fn receive_stub_reports_args() {
    let out = commands::run(cli_with(Commands::Receive(ReceiveArgs {
        code: Some("7-correct-horse-battery".to_string()),
        output: Some(PathBuf::from("dl")),
    })));
    assert_eq!(
        out.expect("ok"),
        "receive: code = 7-correct-horse-battery, output = dl\n"
    );
}
