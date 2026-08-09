//! Parse tests for the clap command surface (T12).

use std::path::PathBuf;

use clap::{error::ErrorKind, CommandFactory, Parser};

use super::{Cli, Commands, ReceiveArgs};

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("my-croc").chain(args.iter().copied()))
}

#[test]
fn parse_send_multiple_paths() {
    let cli = parse(&["send", "a.txt", "b.txt"]).expect("parses");
    assert_eq!(cli.verbose, 0);
    match cli.command {
        Commands::Send(args) => {
            assert_eq!(args.paths, [PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn parse_send_requires_at_least_one_path() {
    let err = parse(&["send"]).expect_err("missing paths must fail");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn parse_receive_code_and_output() {
    let cli = parse(&["receive", "--code", "7-correct-horse-battery", "--output", "dl"])
        .expect("parses");
    match cli.command {
        Commands::Receive(args) => {
            assert_eq!(args.code.as_deref(), Some("7-correct-horse-battery"));
            assert_eq!(args.output, Some(PathBuf::from("dl")));
        }
        other => panic!("expected Receive, got {other:?}"),
    }
}

#[test]
fn parse_receive_is_fully_optional() {
    let cli = parse(&["receive"]).expect("parses");
    match cli.command {
        Commands::Receive(ReceiveArgs { code, output }) => {
            assert_eq!(code, None);
            assert_eq!(output, None);
        }
        other => panic!("expected Receive, got {other:?}"),
    }
}

#[test]
fn parse_verbose_counts_at_one_position() {
    // clap merges subcommand-level counts OVER top-level ones (same-level
    // occurrences add, mixed-level ones do not) — assert the documented
    // contract: -v -v in a single position counts 2.
    let before = parse(&["-v", "-v", "send", "a.txt"]).expect("parses");
    assert_eq!(before.verbose, 2);
    let after = parse(&["send", "-v", "-v", "a.txt"]).expect("parses");
    assert_eq!(after.verbose, 2);
    let single = parse(&["-v", "send", "a.txt"]).expect("parses");
    assert_eq!(single.verbose, 1);
}

#[test]
fn parse_unknown_subcommand_rejected() {
    let err = parse(&["frobnicate"]).expect_err("unknown subcommand must fail");
    assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
}

#[test]
fn help_lists_send_receive_config() {
    let command = Cli::command();
    let names = command
        .get_subcommands()
        .map(|cmd| cmd.get_name())
        .collect::<Vec<_>>();
    assert_eq!(names, ["send", "receive", "config"]);
}
