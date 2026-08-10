//! T14 integration: the receive command flow (`run_receive`) against the REAL
//! T6 rendezvous server on an ephemeral port, with a fake sender peer that
//! drives the control protocol. `RelayMode::Disabled` keeps everything on
//! loopback — no relay binary needed.
//!
//! Flows: accept (download + byte-for-byte verification), decline (sender
//! sees Declined), wrong-words (SPAKE2 mismatch → clean failure, exit code 1).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::RelayMode;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use my_croc_core::pairing::wordcode::WordCode;
use my_croc_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, PROTOCOL_VERSION, recv_message_timeout, send_message,
};
use my_croc_core::transfer::engine::{EngineSpec, TransferEngine};
use my_croc_core::transfer::send::ProgressEvent;

use my_croc_cli::receive::{ReceiveOpts, ReceiveOutcome, RecvError, run_receive};
use my_croc_cli::rendezvous_client::RvClient;
use my_croc_cli::wire::{self, CONTROL_ALPN, ControlAcceptor, PAIR_TIMEOUT};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("my-croc-cli-recv-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let a = dir.join("a.txt");
    let b = dir.join("b.bin");
    let c = dir.join("c.dat");
    fs::write(&a, b"hello world from a\n").expect("write a");
    fs::write(&b, b"binary\x00\xff\xee data").expect("write b");
    fs::write(&c, b"third! with more bytes for testing").expect("write c");
    (a, b, c)
}

fn verify_exported(output: &Path) {
    assert_eq!(
        fs::read(output.join("a.txt")).expect("read a"),
        b"hello world from a\n"
    );
    assert_eq!(
        fs::read(output.join("b.bin")).expect("read b"),
        b"binary\x00\xff\xee data"
    );
    assert_eq!(
        fs::read(output.join("c.dat")).expect("read c"),
        b"third! with more bytes for testing"
    );
}

/// Spawn the real axum rendezvous server on an ephemeral port.
async fn spawn_rendezvous() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let _ = my_croc_rendezvous::server::serve(addr).await;
    });
    let client = RvClient::new(&url);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client.health().await.is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "rendezvous not healthy");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (url, handle)
}

/// A sender engine with the CONTROL_ALPN acceptor and served-bytes tracking,
/// on loopback.
async fn sender_engine(
    data_dir: &PathBuf,
) -> (TransferEngine, mpsc::UnboundedReceiver<iroh::endpoint::Connection>) {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let acceptor: Box<dyn iroh::protocol::DynProtocolHandler> =
        ControlAcceptor::new(control_tx).into();
    let engine = TransferEngine::new_spec(EngineSpec {
        data_dir,
        relay_mode: RelayMode::Disabled,
        secret_key: None,
        extra_handler: Some((CONTROL_ALPN.to_vec(), acceptor)),
        track_served_bytes: false,
    })
    .await
    .expect("sender engine binds");
    (engine, control_rx)
}

/// What the sender peer tells the test once the code is ready.
struct PairInfo {
    code: String,
    _nameplate: u32,
}

/// Start the sender side: prepare, allocate a nameplate, generate the code,
/// return the code to the test. The caller drives the returned future to
/// completion.
async fn start_sender(
    engine: TransferEngine,
    control_rx: mpsc::UnboundedReceiver<iroh::endpoint::Connection>,
    rv_url: &str,
    paths: Vec<PathBuf>,
) -> (
    std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SenderDone, String>>>,
    >,
    PairInfo,
) {
    let (code_tx, mut code_rx) = mpsc::channel(1);
    let rv = RvClient::new(rv_url);
    let mut sender_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SenderDone, String>>>,
    > = Box::pin(run_fake_sender(engine, control_rx, rv, paths, code_tx));
    // Poll the sender while waiting for its code (e2e's await_sender_code
    // pattern): a sender that fails before generating a code must surface as
    // a panic, not a silent 60s hang.
    let pair = timeout(Duration::from_secs(60), async {
        tokio::select! {
            result = &mut sender_fut => panic!("sender failed before code: {result:?}"),
            code = code_rx.recv() => code.expect("code channel stays open"),
        }
    })
    .await
    .expect("code arrives within 60s");
    (sender_fut, pair)
}

#[derive(Debug)]
enum SenderResult {
    Completed,
    Declined,
    Cancelled,
}

#[derive(Debug)]
struct SenderDone {
    result: SenderResult,
}

/// Minimal fake sender: prepare files, allocate nameplate, generate code,
/// then handle the control exchange.
async fn run_fake_sender(
    engine: TransferEngine,
    mut control_rx: mpsc::UnboundedReceiver<iroh::endpoint::Connection>,
    rv: RvClient,
    paths: Vec<PathBuf>,
    code_tx: mpsc::Sender<PairInfo>,
) -> Result<SenderDone, String> {
    let mut cb: Box<dyn FnMut(ProgressEvent) + Send> = Box::new(|_| {});
    let prepared = engine
        .prepare_send(&paths, cb.as_mut())
        .await
        .map_err(|err| err.to_string())?;
    let total = prepared.total_bytes;
    let file_count = prepared.files.len() as u32;
    let ticket_str = prepared.ticket.to_string();

    let allocation = rv
        .allocate(&ticket_str)
        .await
        .map_err(|err| err.to_string())?;

    let code = WordCode::generate(allocation.nameplate, &mut rand::rng())
        .map_err(|err| err.to_string())?;
    let words = code.password().to_owned();
    let _ = code_tx
        .send(PairInfo {
            code: code.to_string(),
            _nameplate: allocation.nameplate,
        })
        .await;

    let conn = timeout(PAIR_TIMEOUT, control_rx.recv())
        .await
        .map_err(|_| "timed out waiting for receiver control connection".to_string())?
        .ok_or_else(|| "control channel closed".to_string())?;
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|err| format!("accept_bi failed: {err}"))?;

    let hello =
        recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "sender hello")
            .await
            .map_err(|err| err.to_string())?;
    let ControlMessage::Hello { .. } = &hello else {
        return Err("expected hello".to_string());
    };
    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await
    .map_err(|err| err.to_string())?;

    let _key = wire::spake_sender_side(&mut send, &mut recv, words.as_bytes())
        .await
        .map_err(|err| err.to_string())?;

    let offer = ControlMessage::Offer {
        files: prepared
            .files
            .iter()
            .map(|f| my_croc_core::session::control::FileMeta {
                name: f.name.clone(),
                size: f.size,
                hash: f.hash.to_hex(),
            })
            .collect(),
        total_bytes: total,
    };
    send_message(&mut send, &offer)
        .await
        .map_err(|err| err.to_string())?;

    let response =
        recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver response")
            .await
            .map_err(|err| err.to_string())?;

    match response {
        ControlMessage::Accept => {
            // The receiver now downloads. Wait for its Result.
            let final_msg = recv_message_timeout(
                &mut recv,
                HANDSHAKE_TIMEOUT,
                "receiver result or cancel",
            )
            .await
            .map_err(|err| err.to_string())?;
            match final_msg {
                ControlMessage::Result { bytes, files } => {
                    if bytes != total || files != file_count {
                        return Err(format!(
                            "result mismatch: expected {total}/{file_count}, got {bytes}/{files}"
                        ));
                    }
                    Ok(SenderDone {
                        result: SenderResult::Completed,
                    })
                }
                ControlMessage::Cancel => Ok(SenderDone {
                    result: SenderResult::Cancelled,
                }),
                other => Err(format!("unexpected final message: {other:?}")),
            }
        }
        ControlMessage::Decline { .. } => Ok(SenderDone {
            result: SenderResult::Declined,
        }),
        ControlMessage::Cancel => Ok(SenderDone {
            result: SenderResult::Cancelled,
        }),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

// ============================================================================
//  Flow 1 — accept + byte-for-byte download
// ============================================================================

#[tokio::test]
async fn receive_flow_accept_downloads_all_files() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (sender, control_rx) = sender_engine(&sender_dir).await;

    let (sender_fut, pair) =
        start_sender(sender, control_rx, &rv_url, vec![a, b, c]).await;

    let receiver_fut = run_receive(
        Some(pair.code),
        ReceiveOpts {
            output: Some(output.clone()),
            data_dir: receiver_dir,
            rendezvous_url: rv_url,
            relay_mode: RelayMode::Disabled,
            overwrite: false,
            auto_accept: Some(true),
        },
        std::future::pending::<()>(),
    );
    tokio::pin!(receiver_fut);

    let (sender_result, receiver_result) = tokio::join!(sender_fut, receiver_fut);
    let sender_result = sender_result.expect("sender must succeed");

    assert!(
        matches!(sender_result.result, SenderResult::Completed),
        "sender sees Completed, got {:?}",
        sender_result.result
    );

    match receiver_result.expect("receiver flow must succeed") {
        ReceiveOutcome::Completed { bytes, files } => {
            assert!(bytes > 0, "receiver reports positive bytes");
            assert_eq!(files, 3, "receiver reports 3 files");
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    verify_exported(&output);
}

// ============================================================================
//  Flow 2 — decline
// ============================================================================

#[tokio::test]
async fn receive_flow_decline_reached_sender() {
    // NOTE: decline requires interactive stdin, so this test uses the
    // --code flag but the interactive prompt is N/A for this integration
    // test since we can't feed stdin here. Instead, we test that the
    // flow parses the code, claims the nameplate, and reaches the offer
    // stage successfully — the decline prompt is tested manually.
    //
    // For automated decline testing, the code below drives the full
    // protocol: the fake sender waits for the accept/decline, and the
    // real receive flow (with interactive stdin) is exercised in the
    // manual QA phase.

    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (sender, control_rx) = sender_engine(&sender_dir).await;

    let (sender_fut, pair) =
        start_sender(sender, control_rx, &rv_url, vec![a, b, c]).await;

    // Auto-decline to test the decline path.
    let receiver_fut = run_receive(
        Some(pair.code),
        ReceiveOpts {
            output: Some(output.clone()),
            data_dir: receiver_dir,
            rendezvous_url: rv_url,
            relay_mode: RelayMode::Disabled,
            overwrite: false,
            auto_accept: Some(false),
        },
        std::future::pending::<()>(),
    );
    tokio::pin!(receiver_fut);

    let (sender_result, _receiver_result) = tokio::join!(sender_fut, receiver_fut);
    let sender_result = sender_result.expect("sender must succeed");

    assert!(
        matches!(sender_result.result, SenderResult::Declined),
        "sender sees Declined, got {:?}",
        sender_result.result
    );
}

// ============================================================================
//  Flow 3 — wrong-words (SPAKE2 mismatch confirms)
// ============================================================================

#[tokio::test]
async fn receive_flow_wrong_words_fails_cleanly() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (sender, control_rx) = sender_engine(&sender_dir).await;

    let (sender_fut, pair) =
        start_sender(sender, control_rx, &rv_url, vec![a, b, c]).await;

    // Build a wrong-words code: same nameplate, different words.
    let (nameplate, _words) = WordCode::split(&pair.code).expect("split code");
    let wrong_code = WordCode::generate(nameplate, &mut rand::rng())
        .expect("generate wrong code")
        .to_string();

    let receiver_fut = run_receive(
        Some(wrong_code),
        ReceiveOpts {
            output: Some(output.clone()),
            data_dir: receiver_dir,
            rendezvous_url: rv_url,
            relay_mode: RelayMode::Disabled,
            overwrite: false,
            auto_accept: Some(true),
        },
        std::future::pending::<()>(),
    );
    tokio::pin!(receiver_fut);

    let (sender_result, receiver_result) = tokio::join!(sender_fut, receiver_fut);

    // Both sides verify each other's confirmation token with their own key.
    // With mismatched words, the mismatch surfaces on one side first; the
    // other side then sees a stream error when the mismatching peer closes
    // the connection. Assert: the receiver fails (never reaches Done), and
    // at least one side surfaces ConfirmationMismatch.
    assert!(
        receiver_result.is_err(),
        "wrong-words receiver must fail, got {receiver_result:?}"
    );

    let receiver_is_mismatch = matches!(
        &receiver_result,
        Err(RecvError::Pair(wire::PairError::Spake(
            my_croc_core::pairing::spake::SpakeError::ConfirmationMismatch,
        )))
    );
    let sender_is_mismatch = matches!(
        &sender_result,
        Err(e) if e.contains("配对码不匹配") || e.contains("pairing code mismatch")
    );

    assert!(
        receiver_is_mismatch || sender_is_mismatch,
        "expected ConfirmationMismatch on at least one side; \
         receiver: {receiver_result:?}, sender: {sender_result:?}"
    );
}
