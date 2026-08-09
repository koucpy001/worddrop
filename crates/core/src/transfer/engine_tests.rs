//! T7 tests: engine init, direct local connect with [`RelayMode::Disabled`],
//! and the RESUME SPIKE — does iroh-blobs 0.103 `get()` resume a
//! partially-downloaded blob from the persistent FsStore bitfield? The spike
//! verdict decides T10's design branch.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use iroh::RelayMode;
use iroh_blobs::{protocol::GetRequest, BlobFormat, HashAndFormat};

use crate::{
    identity::Config,
    transfer::engine::{Error, TransferEngine, BLOBS_DIR},
};

/// Unique temp dir per test call: isolated from other tests and from other
/// processes (pid + counter), so concurrent suite runs cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "my-croc-transfer-test-{tag}-{}-{n}",
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
fn payload(size: u64) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn init_creates_endpoint_and_store_in_temp_dir() {
    let dir = temp_dir("init");
    let engine = TransferEngine::with_relay_mode(&dir, RelayMode::Disabled, None)
        .await
        .expect("engine init");

    // The FsStore database must exist under <data_dir>/blobs.
    let db = dir.join(BLOBS_DIR).join("blobs.db");
    assert!(db.exists(), "FsStore db file created at {}", db.display());

    // The endpoint is bound with a node id (its public key).
    let node_id = engine.endpoint().id();
    assert!(!node_id.as_bytes().is_empty(), "endpoint has a node id");

    // The store handle is usable: local add + read round trip.
    let data = b"hello engine".to_vec();
    let tag = engine.store().add_slice(&data).await.expect("add slice");
    let got = engine.store().get_bytes(tag.hash).await.expect("read slice");
    assert_eq!(got.as_ref(), data.as_slice(), "store round-trips bytes");

    engine.shutdown().await.expect("clean shutdown");
    cleanup(&dir);
}

#[tokio::test]
async fn new_rejects_invalid_relay_url() {
    let dir = temp_dir("bad-relay");
    let mut config = Config::with_dir(&dir);
    config.relay_url = "not a url".to_string();

    let result = TransferEngine::new(&config).await;
    let err = match result {
        Err(err) => err,
        Ok(_) => panic!("invalid relay url must be rejected before binding"),
    };
    assert!(matches!(err, Error::RelayUrl { .. }), "got {err:?}");
    cleanup(&dir);
}

#[tokio::test]
async fn init_with_file_data_dir_is_error() {
    let dir = temp_dir("file-data-dir");
    let file = dir.join("not-a-dir");
    fs::write(&file, b"i am a file, not a directory").expect("write file");

    let result = TransferEngine::with_relay_mode(&file, RelayMode::Disabled, None).await;
    let err = match result {
        Err(err) => err,
        Ok(_) => panic!("store load over a file path must fail"),
    };
    assert!(matches!(err, Error::DataDirNotDirectory { .. }), "got {err:?}");
    cleanup(&dir);
}

#[tokio::test]
async fn two_engines_exchange_blob_over_direct_connection() {
    let sender_dir = temp_dir("ping-sender");
    let recv_dir = temp_dir("ping-recv");
    let sender = TransferEngine::with_relay_mode(&sender_dir, RelayMode::Disabled, None)
        .await
        .expect("sender engine");
    let receiver = TransferEngine::with_relay_mode(&recv_dir, RelayMode::Disabled, None)
        .await
        .expect("receiver engine");

    let data = payload(64 * 1024);
    let tag = sender.store().add_slice(&data).await.expect("sender adds blob");

    // Direct local connection (no relay anywhere): connect by the sender's
    // bound address and run a real blobs-protocol round trip.
    let conn = receiver
        .endpoint()
        .connect(sender.endpoint().addr(), iroh_blobs::ALPN)
        .await
        .expect("direct connect over loopback");
    let stats = receiver
        .store()
        .remote()
        .execute_get(conn, GetRequest::blob(tag.hash))
        .complete()
        .await
        .expect("get completes");
    assert!(
        stats.total_bytes_read() >= data.len() as u64,
        "blob data travelled the wire"
    );

    let got = receiver
        .store()
        .get_bytes(tag.hash)
        .await
        .expect("read received blob");
    assert_eq!(got.as_ref(), data.as_slice(), "payload round-trips byte-for-byte");

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&sender_dir);
    cleanup(&recv_dir);
}

/// RESUME SPIKE (T7 gate for T10).
///
/// Given: a blob half-downloaded into the receiver's FsStore, then the
/// connection dropped. When: a fresh `get()` is issued from the store's
/// `missing()` state (sendme's resume path). Then: the blob completes and the
/// re-get reads only the missing bytes — NOT the whole blob.
#[tokio::test]
async fn resume_spike_partial_download_then_reget_fetches_only_missing() {
    let sender_dir = temp_dir("spike-sender");
    let recv_dir = temp_dir("spike-recv");
    let sender = TransferEngine::with_relay_mode(&sender_dir, RelayMode::Disabled, None)
        .await
        .expect("sender engine");
    let receiver = TransferEngine::with_relay_mode(&recv_dir, RelayMode::Disabled, None)
        .await
        .expect("receiver engine");

    let size = 16 * 1024 * 1024; // 16 MiB: needs tens of ms on loopback
    let data = payload(size);
    let tag = sender.store().add_slice(&data).await.expect("sender adds blob");
    let hash_and_format = HashAndFormat { hash: tag.hash, format: BlobFormat::Raw };

    // Step 1: start a full download, abort once half is in the store, then
    // drop the connection (simulated interruption).
    let conn1 = receiver
        .endpoint()
        .connect(sender.endpoint().addr(), iroh_blobs::ALPN)
        .await
        .expect("first connect");
    let get1 = receiver
        .store()
        .remote()
        .execute_get(conn1, GetRequest::blob(tag.hash));
    let task = tokio::spawn(async move { get1.complete().await });
    let half = size / 2;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let local = receiver.store().remote().local(hash_and_format).await.expect("store state");
        if local.local_bytes() >= half {
            break;
        }
        assert!(Instant::now() < deadline, "partial download stalled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort(); // drop the connection mid-transfer
    let _ = task.await;

    // Step 2: what actually landed in the store?
    let partial = receiver.store().remote().local(hash_and_format).await.expect("store state");
    let landed = partial.local_bytes();
    assert!(
        (half..size).contains(&landed),
        "partial data landed before the abort (got {landed} of {size} bytes)"
    );

    // Step 3: re-get from the fresh store state via missing() — the exact
    // resume path sendme's receive uses.
    let conn2 = receiver
        .endpoint()
        .connect(sender.endpoint().addr(), iroh_blobs::ALPN)
        .await
        .expect("second connect");
    let stats2 = receiver
        .store()
        .remote()
        .execute_get(conn2, partial.missing())
        .complete()
        .await
        .expect("re-get completes");
    let re_fetched = stats2.total_bytes_read();

    // The re-get must complete the blob...
    let done = receiver.store().remote().local(hash_and_format).await.expect("store state");
    assert!(done.is_complete(), "blob complete after re-get");

    // ...with the payload intact...
    let got = receiver.store().get_bytes(tag.hash).await.expect("read blob");
    assert_eq!(got.as_ref(), data.as_slice(), "payload intact after resume");

    // ...and without a full re-fetch: a re-fetch of everything would read the
    // whole blob (> size). A resumed fetch reads ~ the missing bytes only.
    assert!(
        re_fetched < size,
        "resume: re-get read {re_fetched} bytes — a full re-fetch would read ~{size}"
    );

    eprintln!(
        "RESUME SPIKE: blob={size} landed={landed} missing={} re_fetched={re_fetched} -> SUPPORTED",
        size - landed
    );

    sender.shutdown().await.expect("sender shutdown");
    receiver.shutdown().await.expect("receiver shutdown");
    cleanup(&sender_dir);
    cleanup(&recv_dir);
}
