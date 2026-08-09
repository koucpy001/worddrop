//! T9 tests: receive-side flow — download a prepared collection via its
//! ticket over a direct (`RelayMode::Disabled`) connection, stream progress
//! events, export into the target dir with the conflict policy, and surface
//! errors for dead peers.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use iroh::RelayMode;
use iroh_blobs::{ticket::BlobTicket, BlobFormat};

use crate::transfer::{
    engine::TransferEngine,
    receive::{export_target, ReceiveError, ReceiveOptions, ReceiveProgress},
};

/// Unique temp dir per test call: isolated from other tests and from other
/// processes (pid + counter), so concurrent suite runs cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "my-croc-receive-test-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

/// Deterministic pseudo-random payload: every byte distinct pattern.
fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// A source tree with 3 files + 1 nested subdir and small payloads, written
/// in non-sorted creation order so the walk must sort them itself.
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

/// The same tree but with 1 MiB payloads per file: guaranteed to cross many
/// 64 KiB chunks on the wire, so download progress events actually fire.
fn make_large_tree(root: &Path) -> PathBuf {
    let input = root.join("input");
    let nested = input.join("nested");
    fs::create_dir_all(&nested).expect("create nested dir");
    fs::write(nested.join("b.txt"), payload(1024 * 1024)).expect("write b");
    fs::write(input.join("a.txt"), payload(1024 * 1024)).expect("write a");
    fs::write(input.join("m.txt"), payload(1024 * 1024)).expect("write m");
    fs::write(input.join("z.txt"), payload(1024 * 1024)).expect("write z");
    input
}

async fn make_engine(dir: &Path) -> TransferEngine {
    TransferEngine::with_relay_mode(dir, RelayMode::Disabled, None)
        .await
        .expect("engine init")
}

async fn prepare(engine: &TransferEngine, input: &PathBuf) -> crate::transfer::send::PreparedTransfer {
    engine
        .prepare_send(std::slice::from_ref(input), &mut |_| {})
        .await
        .expect("prepare send")
}

// ---------------------------------------------------------------------------
// happy path: T7 engine + T8 prepare + T9 receive in one in-process pair
// ---------------------------------------------------------------------------

#[tokio::test]
async fn receive_e2e_pair_downloads_and_exports_byte_for_byte() {
    let dir = temp_dir("e2e");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver = make_engine(&dir.join("receiver-store")).await;

    let input = make_source_tree(&dir);
    let prepared = prepare(&sender, &input).await;
    let target = dir.join("out");

    let result = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |_| {},
        )
        .await
        .expect("receive succeeds");
    assert_eq!(result.bytes, prepared.total_bytes, "all payload bytes exported");
    assert_eq!(result.files, prepared.files.len(), "every file exported");
    assert!(result.skipped.is_empty(), "nothing skipped on a fresh target");

    let expected: Vec<(&str, &[u8])> = vec![
        ("input/a.txt", b"aa"),
        ("input/m.txt", b"mmm"),
        ("input/nested/b.txt", b"bbbb"),
        ("input/z.txt", b"zzzzzz"),
    ];
    for (name, bytes) in expected {
        let got = fs::read(target.join(name)).expect("exported file exists");
        assert_eq!(got, bytes, "{name} matches the source byte-for-byte");
    }

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// progress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn receive_emits_monotonic_download_progress_then_done() {
    let dir = temp_dir("progress");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver = make_engine(&dir.join("receiver-store")).await;

    let input = make_large_tree(&dir);
    let prepared = prepare(&sender, &input).await;
    assert_eq!(prepared.total_bytes, 4 * 1024 * 1024, "fixture: 4 x 1 MiB");

    let mut events: Vec<ReceiveProgress> = Vec::new();
    let result = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: dir.join("out"), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("receive succeeds");

    assert_eq!(events.first(), Some(&ReceiveProgress::Connecting), "dial first");

    let mut last_received = 0u64;
    let mut downloads = 0usize;
    for event in &events {
        if let ReceiveProgress::Downloading { received, total } = event {
            downloads += 1;
            assert_eq!(*total, prepared.total_bytes, "progress total is the payload size");
            assert!(*received >= last_received, "received bytes are monotonic");
            assert!(*received <= *total, "received never exceeds total");
            last_received = *received;
        }
    }
    assert!(downloads > 0, "a multi-MiB transfer must produce download progress");
    assert_eq!(last_received, prepared.total_bytes, "progress reaches the total");

    let exporting: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ReceiveProgress::Exporting { file } => Some(file.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        exporting,
        vec!["input/a.txt", "input/m.txt", "input/nested/b.txt", "input/z.txt"]
    );
    assert_eq!(
        events.last(),
        Some(&ReceiveProgress::Done { bytes: result.bytes, files: result.files })
    );
    assert_eq!(result.bytes, prepared.total_bytes);
    assert_eq!(result.files, 4);

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// failure: dead peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn receive_from_dead_peer_emits_error_event() {
    let dir = temp_dir("dead");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver = make_engine(&dir.join("receiver-store")).await;

    let input = make_source_tree(&dir);
    let prepared = prepare(&sender, &input).await;
    let addr = prepared.ticket.addr().clone();
    let hash = prepared.ticket.hash();
    sender.shutdown().await.expect("sender shutdown"); // the port is now closed

    let dead_ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);
    let mut events: Vec<ReceiveProgress> = Vec::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        receiver.receive(
            &dead_ticket,
            ReceiveOptions { target_dir: dir.join("out"), overwrite: false },
            &mut |event| events.push(event),
        ),
    )
    .await
    .expect("dead-peer connect must fail within the 30s dial budget + margin");

    let err = match outcome {
        Err(err) => err,
        Ok(_) => panic!("receive from a dead peer must fail"),
    };
    // A dead UDP peer never answers: noq has no handshake timeout, so the
    // receive flow's own CONNECT_TIMEOUT fires (a fast local error is also
    // possible on platforms that surface ICMP port-unreachable).
    assert!(
        matches!(&err, ReceiveError::Connect { .. } | ReceiveError::ConnectTimeout),
        "got {err:?}"
    );
    assert_eq!(events.last(), Some(&ReceiveProgress::Error), "error event emitted");
    assert!(
        matches!(events.first(), Some(ReceiveProgress::Connecting)),
        "the dial was attempted before the failure"
    );

    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// conflict policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn receive_conflict_policy_skips_without_overwrite_replaces_with_overwrite() {
    let dir = temp_dir("conflict");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver = make_engine(&dir.join("receiver-store")).await;

    let input = make_source_tree(&dir);
    let prepared = prepare(&sender, &input).await;
    let target = dir.join("out");

    let first = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |_| {},
        )
        .await
        .expect("first receive");
    assert_eq!(first.skipped.len(), 0);
    assert_eq!(first.files, 4);

    // Corrupt one exported file and delete another, then re-receive WITHOUT
    // overwrite: existing files must be skipped (untouched, recorded), the
    // deleted one re-exported.
    let z_path = target.join("input/z.txt");
    fs::write(&z_path, b"EVIL").expect("corrupt the exported file");
    fs::remove_file(target.join("input/a.txt")).expect("remove one exported file");
    let mut events: Vec<ReceiveProgress> = Vec::new();
    let second = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("second receive");
    assert_eq!(
        second.skipped,
        vec!["input/m.txt", "input/nested/b.txt", "input/z.txt"],
        "existing files recorded as skipped"
    );
    assert_eq!(second.files, 1, "only the deleted file is exported");
    assert_eq!(second.bytes, 2, "only the re-exported file's bytes counted");
    assert_eq!(fs::read(&z_path).expect("read target"), b"EVIL", "skipped file untouched");
    assert_eq!(fs::read(target.join("input/a.txt")).expect("read target"), b"aa");
    assert!(
        events.iter().all(|event| !matches!(event, ReceiveProgress::Connecting)
            && !matches!(event, ReceiveProgress::Downloading { .. })),
        "data already complete: re-receive does not dial or download"
    );

    // Now WITH overwrite: the corrupted file must be replaced byte-for-byte.
    let third = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: true },
            &mut |_| {},
        )
        .await
        .expect("third receive");
    assert!(third.skipped.is_empty(), "overwrite replaces instead of skipping");
    assert_eq!(third.files, 4);
    assert_eq!(third.bytes, 15);
    assert_eq!(fs::read(&z_path).expect("read target"), b"zzzzzz", "file restored");

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn receive_empty_collection_exports_nothing() {
    let dir = temp_dir("empty");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver = make_engine(&dir.join("receiver-store")).await;

    let empty = dir.join("empty");
    fs::create_dir_all(&empty).expect("create empty dir");
    let prepared = prepare(&sender, &empty).await;

    let mut events: Vec<ReceiveProgress> = Vec::new();
    let result = receiver
        .receive(
            &prepared.ticket,
            ReceiveOptions { target_dir: dir.join("out"), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("empty collection receive succeeds");
    assert_eq!(result.bytes, 0);
    assert_eq!(result.files, 0);
    assert!(result.skipped.is_empty());
    assert_eq!(events.last(), Some(&ReceiveProgress::Done { bytes: 0, files: 0 }));

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

#[test]
fn receive_export_target_validates_collection_names() {
    let root = Path::new("/tmp/root");
    assert_eq!(export_target(root, "a/b.txt").expect("nested ok"), root.join("a/b.txt"));
    assert_eq!(export_target(root, "solo.txt").expect("flat ok"), root.join("solo.txt"));

    for bad in ["../evil", "a/../evil", "a//b", "/abs", "a/", "", "a\\b"] {
        let err = export_target(root, bad).expect_err("unsafe name must be rejected");
        assert!(
            matches!(err, ReceiveError::InvalidCollectionName { .. }),
            "{bad:?} -> {err:?}"
        );
    }
}
