//! T8 tests: send-side preparation — walking the input paths (stable sort,
//! symlink skip policy), importing every file into the blob store via
//! `ImportMode::TryReference`, building the [`Collection`], and constructing
//! the [`BlobTicket`]. Local preparation only: no network connections.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use iroh::RelayMode;
use iroh_blobs::{BlobFormat, format::collection::Collection};

use crate::transfer::{
    engine::TransferEngine,
    send::{ProgressEvent, SendError, walk_files},
};

/// Unique temp dir per test call: isolated from other tests and from other
/// processes (pid + counter), so concurrent suite runs cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "my-croc-send-test-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

/// A source tree with 3 files + 1 nested subdir, written in non-sorted
/// creation order so the walk must sort them itself.
fn make_source_tree(root: &Path) -> PathBuf {
    let input = root.join("input");
    let nested = input.join("nested");
    fs::create_dir_all(&nested).expect("create nested dir");
    fs::write(nested.join("b.txt"), b"bbbb").expect("write b");
    fs::write(input.join("z.txt"), b"zzzzzz").expect("write z");
    fs::write(input.join("a.txt"), b"aa").expect("write a");
    fs::write(input.join("m.txt"), b"mmm").expect("write m");
    input
}

async fn make_engine(dir: &Path) -> TransferEngine {
    TransferEngine::with_relay_mode(dir, RelayMode::Disabled, None)
        .await
        .expect("engine init")
}

// ---------------------------------------------------------------------------
// walk_files
// ---------------------------------------------------------------------------

#[test]
fn send_walk_sorts_directory_entries_stably_with_root_prefix() {
    let dir = temp_dir("walk-sort");
    let input = make_source_tree(&dir);
    let mut seen = Vec::new();

    let files = walk_files(&input).expect("walk succeeds");
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "input/a.txt",
            "input/m.txt",
            "input/nested/b.txt",
            "input/z.txt"
        ],
        "files sorted by collection name with the input root as prefix"
    );
    for (name, path) in &files {
        assert!(
            path.is_file(),
            "{name} maps to a real file at {}",
            path.display()
        );
        seen.push(path.clone());
    }
    // Local paths all live under the input dir.
    for path in seen {
        assert!(path.starts_with(&input), "{} under input", path.display());
    }
    cleanup(&dir);
}

#[test]
fn send_walk_single_file_returns_itself() {
    let dir = temp_dir("walk-file");
    let file = dir.join("hello.txt");
    fs::write(&file, b"hello").expect("write file");

    let files = walk_files(&file).expect("walk succeeds");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "hello.txt");
    assert_eq!(files[0].1, file);
    cleanup(&dir);
}

#[test]
fn send_walk_missing_path_errors() {
    let dir = temp_dir("walk-missing");
    let missing = dir.join("does-not-exist");
    let err = walk_files(&missing).expect_err("missing path must error");
    assert!(
        matches!(err, SendError::MissingSource { .. }),
        "got {err:?}"
    );
    cleanup(&dir);
}

#[test]
fn send_walk_path_without_file_name_errors() {
    // "." and "/" have no file_name, so no root name can be derived.
    let err = walk_files(Path::new(".")).expect_err("'.' has no root name");
    assert!(
        matches!(err, SendError::InvalidRootName { .. }),
        "got {err:?}"
    );
}

#[cfg(unix)]
#[test]
fn send_walk_skips_nested_symlink() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("walk-link");
    let input = dir.join("input");
    fs::create_dir_all(&input).expect("create dir");
    fs::write(input.join("real.txt"), b"real").expect("write real");
    symlink("real.txt", input.join("link.txt")).expect("create symlink");

    let files = walk_files(&input).expect("walk succeeds");
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["input/real.txt"],
        "symlink skipped, real file kept"
    );
    cleanup(&dir);
}

#[cfg(unix)]
#[test]
fn send_walk_skips_top_level_symlink() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("walk-top-link");
    let target = dir.join("target.txt");
    fs::write(&target, b"data").expect("write target");
    let link = dir.join("link.txt");
    symlink(&target, &link).expect("create symlink");

    // The explicit input is a symlink: skipped (empty walk), not an error.
    let files = walk_files(&link).expect("walk succeeds");
    assert!(files.is_empty(), "top-level symlink skipped: {files:?}");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// prepare_send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_prepare_collects_files_with_correct_sizes() {
    let dir = temp_dir("prepare-size");
    let input = make_source_tree(&dir);
    let engine = make_engine(&dir.join("store")).await;

    let prepared = engine
        .prepare_send(&[input], &mut |_| {})
        .await
        .expect("prepare succeeds");

    let names: Vec<&str> = prepared.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "input/a.txt",
            "input/m.txt",
            "input/nested/b.txt",
            "input/z.txt"
        ]
    );
    let sizes: Vec<u64> = prepared.files.iter().map(|f| f.size).collect();
    assert_eq!(sizes, vec![2, 3, 4, 6], "per-file sizes match the payloads");
    assert_eq!(prepared.total_bytes, 15);

    // The collection stored under the ticket hash is the same set of files.
    let collection = Collection::load(prepared.collection_hash, engine.store())
        .await
        .expect("collection loads from store");
    let collection_names: Vec<&str> = collection.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        collection_names, names,
        "collection names match prepared files"
    );
    for file in &prepared.files {
        let got = engine
            .store()
            .get_bytes(file.hash)
            .await
            .expect("blob readable from store");
        assert_eq!(
            got.len() as u64,
            file.size,
            "blob {} ({} {}) has the expected size",
            file.hash,
            file.name,
            file.path.display()
        );
    }

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_hash_is_deterministic_for_same_input() {
    let dir = temp_dir("prepare-deterministic");
    let input = make_source_tree(&dir);
    let engine = make_engine(&dir.join("store")).await;
    let paths = vec![input.clone()];

    let first = engine
        .prepare_send(&paths, &mut |_| {})
        .await
        .expect("first prepare");
    let second = engine
        .prepare_send(&paths, &mut |_| {})
        .await
        .expect("second prepare");

    assert_eq!(
        first.collection_hash, second.collection_hash,
        "same input must produce the same collection hash"
    );
    assert_eq!(
        first.ticket.hash(),
        second.ticket.hash(),
        "tickets hash the same collection"
    );

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_empty_dir_yields_empty_collection() {
    let dir = temp_dir("prepare-empty");
    let input = dir.join("empty");
    fs::create_dir_all(&input).expect("create empty dir");
    let engine = make_engine(&dir.join("store")).await;

    let prepared = engine
        .prepare_send(&[input], &mut |_| {})
        .await
        .expect("empty dir is not an error");
    assert!(prepared.files.is_empty());
    assert_eq!(prepared.total_bytes, 0);

    let collection = Collection::load(prepared.collection_hash, engine.store())
        .await
        .expect("empty collection loads");
    assert!(collection.is_empty(), "empty collection has no blobs");

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn send_prepare_skips_symlink_with_warning() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("prepare-link");
    let input = dir.join("input");
    fs::create_dir_all(&input).expect("create dir");
    fs::write(input.join("real.txt"), b"real").expect("write real");
    symlink("real.txt", input.join("link.txt")).expect("create symlink");
    let engine = make_engine(&dir.join("store")).await;

    let prepared = engine
        .prepare_send(&[input], &mut |_| {})
        .await
        .expect("prepare succeeds");
    let names: Vec<&str> = prepared.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["input/real.txt"],
        "symlink skipped, real file kept"
    );
    assert_eq!(prepared.total_bytes, 4);

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_missing_source_path_errors() {
    let dir = temp_dir("prepare-missing");
    let engine = make_engine(&dir.join("store")).await;
    let missing = dir.join("nope");

    let err = engine
        .prepare_send(std::slice::from_ref(&missing), &mut |_| {})
        .await
        .expect_err("missing source must be rejected");
    match err {
        SendError::MissingSource { path } => assert_eq!(path, missing),
        other => panic!("unexpected error: {other:?}"),
    }

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_duplicate_names_across_inputs_error() {
    let dir = temp_dir("prepare-dup");
    let input = dir.join("input");
    fs::create_dir_all(&input).expect("create dir");
    fs::write(input.join("same.txt"), b"same").expect("write file");
    let engine = make_engine(&dir.join("store")).await;

    // The same directory passed twice yields the same collection names twice.
    let err = engine
        .prepare_send(&[input.clone(), input], &mut |_| {})
        .await
        .expect_err("duplicate collection names must error");
    match err {
        SendError::DuplicateName { name } => assert_eq!(name, "input/same.txt"),
        other => panic!("unexpected error: {other:?}"),
    }

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_emits_progress_events() {
    let dir = temp_dir("prepare-progress");
    let input = make_source_tree(&dir);
    let engine = make_engine(&dir.join("store")).await;

    let mut events: Vec<ProgressEvent> = Vec::new();
    engine
        .prepare_send(&[input], &mut |event| events.push(event))
        .await
        .expect("prepare succeeds");

    // One FileFound (with size) and one FileImported per file, in name order.
    let found: Vec<(&str, u64)> = events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::FileFound { name, size } => Some((name.as_str(), *size)),
            ProgressEvent::FileImported { .. } => None,
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("input/a.txt", 2),
            ("input/m.txt", 3),
            ("input/nested/b.txt", 4),
            ("input/z.txt", 6),
        ]
    );
    let imported: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::FileImported { name } => Some(name.as_str()),
            ProgressEvent::FileFound { .. } => None,
        })
        .collect();
    assert_eq!(
        imported,
        vec![
            "input/a.txt",
            "input/m.txt",
            "input/nested/b.txt",
            "input/z.txt"
        ]
    );

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_single_file_input() {
    let dir = temp_dir("prepare-one");
    let file = dir.join("solo.txt");
    fs::write(&file, b"solo").expect("write file");
    let engine = make_engine(&dir.join("store")).await;

    let prepared = engine
        .prepare_send(std::slice::from_ref(&file), &mut |_| {})
        .await
        .expect("prepare succeeds");
    assert_eq!(prepared.files.len(), 1);
    assert_eq!(prepared.files[0].name, "solo.txt");
    assert_eq!(prepared.files[0].path, file);
    assert_eq!(prepared.files[0].size, 4);
    assert_eq!(prepared.total_bytes, 4);

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn send_prepare_ticket_roundtrips_and_points_at_collection() {
    let dir = temp_dir("prepare-ticket");
    let input = make_source_tree(&dir);
    let engine = make_engine(&dir.join("store")).await;

    let prepared = engine
        .prepare_send(&[input], &mut |_| {})
        .await
        .expect("prepare succeeds");
    let ticket = prepared.ticket;

    // Ticket string round-trips and identifies the prepared collection.
    let parsed: iroh_blobs::ticket::BlobTicket =
        ticket.to_string().parse().expect("ticket parses back");
    assert_eq!(parsed.hash(), ticket.hash());
    assert_eq!(parsed.hash(), prepared.collection_hash);
    assert_eq!(
        parsed.format(),
        BlobFormat::HashSeq,
        "collection ticket is a hash seq"
    );
    assert_eq!(
        parsed.addr().id,
        engine.endpoint().id(),
        "ticket carries the sending endpoint id"
    );

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}
