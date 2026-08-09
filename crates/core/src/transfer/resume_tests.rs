//! T10 tests: persistent resume — a receive interrupted mid-download leaves
//! partial data in the FsStore plus a [`TransferRecord`]; a NEW engine
//! instance on the SAME data dir resumes from the store bitfield (only the
//! missing chunks are fetched — wire efficiency proven by the T7 spike),
//! reports progress from the record's offset, skips re-exporting files the
//! record already marks done, and deletes the record on success.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use iroh::RelayMode;
use iroh_blobs::Hash;

use crate::transfer::{
    engine::TransferEngine,
    receive::{ReceiveOptions, ReceiveProgress},
    record::{RecordStore, TransferRecord, TransferStatus},
};

/// Unique temp dir per test call: isolated from other tests and from other
/// processes (pid + counter), so concurrent suite runs cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "my-croc-resume-test-{tag}-{}-{n}",
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

async fn make_engine(dir: &Path) -> TransferEngine {
    TransferEngine::with_relay_mode(dir, RelayMode::Disabled, None)
        .await
        .expect("engine init")
}

/// A single 16 MiB + 1 byte file: crosses thousands of 64 KiB chunks, so a
/// mid-download abort lands a comfortably partial blob (the T7 spike used
/// the same size and landed between half and total with polling).
fn make_big_input(root: &Path) -> PathBuf {
    let input = root.join("big.bin");
    fs::write(&input, payload(16 * 1024 * 1024 + 1)).expect("write big file");
    input
}

/// A 4-file source tree with tiny payloads, for export-skip tests.
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

async fn prepare(engine: &TransferEngine, input: &PathBuf) -> crate::transfer::send::PreparedTransfer {
    engine
        .prepare_send(std::slice::from_ref(input), &mut |_| {})
        .await
        .expect("prepare send")
}

// ---------------------------------------------------------------------------
// resume branch (T7 spike = SUPPORTED): offset-skip resume on a new engine
// ---------------------------------------------------------------------------

/// Drive `receive_resumable` until the record's persisted `bytes_received`
/// crosses `threshold`, then drop the future mid-download — a simulated
/// crash (process kill): the FsStore keeps the partial blob, the record keeps
/// the progress. Returns the record that was durable at the drop point.
async fn abort_receive_mid_download(
    receiver: &TransferEngine,
    ticket: &iroh_blobs::ticket::BlobTicket,
    target: &Path,
    hash: Hash,
    threshold: u64,
    records: &RecordStore,
) -> TransferRecord {
    let mut noop: Box<dyn FnMut(ReceiveProgress)> = Box::new(|_| {});
    let recv = receiver.receive_resumable(
        ticket,
        ReceiveOptions { target_dir: target.to_path_buf(), overwrite: false },
        &mut noop,
    );
    tokio::pin!(recv);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        tokio::select! {
            res = &mut recv => {
                panic!("receive finished before the abort threshold: {res:?}");
            }
            _ = tokio::time::sleep(Duration::from_millis(1)) => {
                if records
                    .load(&hash, target)
                    .await
                    .filter(|record| record.bytes_received >= threshold)
                    .is_some()
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "download stalled below the abort threshold"
                );
            }
        }
    }
    // `recv` is dropped at scope exit, mid-download: the connection drops and
    // the partial data stays in the store.
    records.load(&hash, target).await.expect("record at abort point")
}

#[tokio::test]
async fn resume_interrupted_receive_completes_on_new_engine_same_data_dir() {
    let dir = temp_dir("resume");
    let sender = make_engine(&dir.join("sender-store")).await;
    // B1 and B2 share this data dir: the persistent store + records are the
    // only thing carried across the "restart".
    let receiver_data = dir.join("receiver-data");

    let input = make_big_input(&dir);
    let prepared = prepare(&sender, &input).await;
    let total = prepared.total_bytes;
    assert!(total > 16 * 1024 * 1024, "fixture: single file > 16 MiB");
    let target = dir.join("out");
    let hash = prepared.collection_hash;
    let records = RecordStore::new(&receiver_data);

    // --- Run 1: receive on engine B1, crash it mid-download. ---
    let receiver1 = make_engine(&receiver_data).await;
    let record = abort_receive_mid_download(
        &receiver1,
        &prepared.ticket,
        &target,
        hash,
        total / 2,
        &records,
    )
    .await;
    receiver1.shutdown().await.expect("shutdown B1"); // consumes B1, closing the store

    // The record persisted the interruption point, and the FsStore bitfield
    // holds a partial blob (survived the store reload — the resume premise).
    assert_eq!(record.status, TransferStatus::InProgress, "record marks the run as unfinished");
    assert!(
        record.bytes_received >= total / 2 && record.bytes_received < total,
        "record's bytes_received is mid-way: {} of {total}",
        record.bytes_received
    );
    assert!(record.exported_files.is_empty(), "no exports happened before the abort");

    // --- Run 2: a NEW engine on the SAME data dir resumes. ---
    let receiver2 = make_engine(&receiver_data).await;
    let partial = receiver2
        .store()
        .remote()
        .local(prepared.ticket.hash_and_format())
        .await
        .expect("store state");
    assert!(!partial.is_complete(), "resume run starts from a partial blob");
    assert!(
        partial.local_bytes() >= total / 2 && partial.local_bytes() < total,
        "store bitfield survived the restart with {} of {total} bytes",
        partial.local_bytes()
    );

    let mut events: Vec<ReceiveProgress> = Vec::new();
    let result = receiver2
        .receive_resumable(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("resumed receive completes");

    assert_eq!(result.bytes, total, "all payload bytes exported");
    assert_eq!(result.files, 1, "the single file exported");
    assert!(result.skipped.is_empty(), "nothing skipped on a fresh target");
    let got = fs::read(target.join("big.bin")).expect("exported file");
    assert_eq!(got, fs::read(&input).expect("source"), "resumed export is byte-for-byte");

    // The record provides the UI offset: progress resumes from where the
    // record stopped, never from zero, and still reaches the total.
    let resumed: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            ReceiveProgress::Downloading { received, .. } => Some(*received),
            _ => None,
        })
        .collect();
    assert!(!resumed.is_empty(), "resumed run must download the missing chunks");
    assert_eq!(resumed.last(), Some(&total), "progress reaches the total");
    assert!(
        resumed[0] >= record.bytes_received,
        "first progress ({}) continues from the record offset ({})",
        resumed[0],
        record.bytes_received
    );
    assert!(
        resumed[0] < total,
        "the resumed run started mid-way, not at the end"
    );

    // The record is deleted after success and the store is complete.
    assert!(
        records.load(&hash, &target).await.is_none(),
        "record deleted after a successful resume"
    );
    let done = receiver2
        .store()
        .remote()
        .local(prepared.ticket.hash_and_format())
        .await
        .expect("store state");
    assert!(done.is_complete(), "blob complete after the resumed run");

    sender.shutdown().await.expect("sender shutdown");
    receiver2.shutdown().await.expect("receiver2 shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// corrupt record -> no-resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_corrupt_record_is_treated_as_fresh_receive() {
    let dir = temp_dir("corrupt");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver_data = dir.join("receiver-data");

    let input = make_big_input(&dir);
    let prepared = prepare(&sender, &input).await;
    let target = dir.join("out");
    let hash = prepared.collection_hash;
    let records = RecordStore::new(&receiver_data);

    // A corrupt record file (a crashed partial write, or a tampered disk)
    // must not break a receive: it is treated as "no record", the download
    // runs to completion, and the record is replaced and finally deleted.
    tokio::fs::create_dir_all(records.dir()).await.expect("records dir");
    tokio::fs::write(records.path(&hash), b"{ this is not JSON !!!").await.expect("corrupt file");

    let receiver = make_engine(&receiver_data).await;
    let mut events: Vec<ReceiveProgress> = Vec::new();
    let result = receiver
        .receive_resumable(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("receive with a corrupt record still succeeds");
    assert_eq!(result.bytes, prepared.total_bytes, "full download + export");
    assert_eq!(result.files, 1);
    assert_eq!(fs::read(target.join("big.bin")).expect("exported"), fs::read(&input).expect("source"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ReceiveProgress::Downloading { .. })),
        "a fresh download was performed"
    );
    assert!(
        records.load(&hash, &target).await.is_none(),
        "record replaced and deleted after success"
    );

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// exported_files skip (record remembers done files)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_skips_files_the_record_marks_as_exported() {
    let dir = temp_dir("export-skip");
    let sender = make_engine(&dir.join("sender-store")).await;
    let receiver_data = dir.join("receiver-data");

    let input = make_source_tree(&dir);
    let prepared = prepare(&sender, &input).await;
    let target = dir.join("out");
    let hash = prepared.collection_hash;
    let records = RecordStore::new(&receiver_data);

    // Run 1: a full resumable receive — everything lands, the record is
    // deleted, the store is complete.
    let receiver = make_engine(&receiver_data).await;
    receiver
        .receive_resumable(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |_| {},
        )
        .await
        .expect("first receive");
    assert!(records.load(&hash, &target).await.is_none(), "record deleted after run 1");

    // Simulate a crash after one export: the record claims a.txt was already
    // exported (data complete), and the exported file is then missing (the
    // user deleted it, or the crash lost it).
    fs::remove_file(target.join("input/a.txt")).expect("remove exported a.txt");
    let mut record = TransferRecord::new(hash, &target);
    record.bytes_received = prepared.total_bytes;
    record.exported_files.push("input/a.txt".to_string());
    records.save(&record).await.expect("persist crash-state record");

    // Run 2: the record skips re-exporting a.txt (record says done, so it is
    // NOT restored even though the target is gone); the other files still
    // exist and are skipped by the conflict policy. No data is downloaded
    // again (the store bitfield is complete).
    let mut events: Vec<ReceiveProgress> = Vec::new();
    let result = receiver
        .receive_resumable(
            &prepared.ticket,
            ReceiveOptions { target_dir: target.clone(), overwrite: false },
            &mut |event| events.push(event),
        )
        .await
        .expect("second receive");
    assert_eq!(result.files, 0, "nothing newly exported");
    assert_eq!(result.bytes, 0);
    assert_eq!(
        result.skipped,
        vec!["input/m.txt", "input/nested/b.txt", "input/z.txt"],
        "remaining files skipped by the conflict policy"
    );
    assert!(
        !target.join("input/a.txt").exists(),
        "the record marks a.txt as done: it is not re-exported"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, ReceiveProgress::Connecting)
                && !matches!(event, ReceiveProgress::Downloading { .. })),
        "data already complete: the resumed run does not dial or download"
    );
    assert!(records.load(&hash, &target).await.is_none(), "record deleted after success");

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&dir);
}
