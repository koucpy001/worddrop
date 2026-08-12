//! T13 integration: the send command flow (`run_send`) against the REAL T6
//! rendezvous server on an ephemeral port, with two in-process iroh endpoints
//! and a fake receiving peer that speaks the T5 control protocol over the
//! CONTROL_ALPN stream. `RelayMode::Disabled` keeps everything on loopback —
//! no relay binary needed (T7's two-endpoint pattern).
//!
//! Flows: decline, accept + byte-for-byte download, and local interrupt
//! (Ctrl+C) cancelling cleanly.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::RelayMode;
use iroh_blobs::ticket::BlobTicket;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use worddrop_core::pairing::wordcode::WordCode;
use worddrop_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, PROTOCOL_VERSION, recv_message_timeout, send_message,
};
use worddrop_core::transfer::engine::{EngineSpec, TransferEngine};
use worddrop_core::transfer::receive::ReceiveOptions;

use worddrop_cli::rendezvous_client::RvClient;
use worddrop_cli::send::{SendOutcome, run_send};
use worddrop_cli::ui::SendUi;
use worddrop_cli::wire::{self, CONTROL_ALPN, ControlAcceptor, PAIR_TIMEOUT};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("worddrop-cli-send-{tag}-{}-{n}", std::process::id()));
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

/// Spawn the real axum rendezvous server on an ephemeral port and wait until
/// `/health` answers (same pattern as the T11 e2e).
async fn spawn_rendezvous() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let _ = worddrop_rendezvous::server::serve_on(listener).await;
    });
    let client = RvClient::new(&url);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client.health().await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rendezvous not healthy"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (url, handle)
}

/// A sender engine with the CONTROL_ALPN acceptor and served-bytes tracking,
/// on loopback (no relay).
async fn sender_engine(
    data_dir: &PathBuf,
) -> (
    TransferEngine,
    mpsc::UnboundedReceiver<iroh::endpoint::Connection>,
) {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let acceptor: Box<dyn iroh::protocol::DynProtocolHandler> =
        ControlAcceptor::new(control_tx).into();
    let engine = TransferEngine::new_spec(EngineSpec {
        data_dir,
        relay_mode: RelayMode::Disabled,
        secret_key: None,
        extra_handler: Some((CONTROL_ALPN.to_vec(), acceptor)),
        track_served_bytes: true,
    })
    .await
    .expect("sender engine binds");
    (engine, control_rx)
}

enum PeerAction {
    Decline { reason: String },
    Accept { output: PathBuf },
}

/// The fake receiver: claim the nameplate, dial the sender on CONTROL_ALPN,
/// hello + SPAKE2 with the words from the code, then respond to the offer.
async fn run_fake_peer(
    engine: &TransferEngine,
    rv: &RvClient,
    code: &str,
    action: PeerAction,
) -> Result<(), String> {
    let (nameplate, words) = WordCode::split(code).map_err(|err| err.to_string())?;
    let ticket_str = rv.claim(nameplate).await.map_err(|err| err.to_string())?;
    let ticket =
        BlobTicket::from_str(&ticket_str).map_err(|_| format!("invalid ticket {ticket_str:?}"))?;

    let conn = timeout(
        PAIR_TIMEOUT,
        engine
            .endpoint()
            .connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await
    .map_err(|_| "timed out dialing the sender".to_string())?
    .map_err(|err| format!("dial failed: {err}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|err| format!("open_bi failed: {err}"))?;

    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await
    .map_err(|err| err.to_string())?;
    let _hello = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "peer hello")
        .await
        .map_err(|err| err.to_string())?;
    let _key = wire::spake_receiver_side(&mut send, &mut recv, words.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    let offer = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "peer offer")
        .await
        .map_err(|err| err.to_string())?;
    let ControlMessage::Offer { .. } = &offer else {
        return Err(format!("expected offer, got {offer:?}"));
    };

    match action {
        PeerAction::Decline { reason } => {
            send_message(&mut send, &ControlMessage::Decline { reason })
                .await
                .map_err(|err| err.to_string())?;
            wire::await_peer_close(&mut recv, "sender close after decline")
                .await
                .map_err(|err| err.to_string())?;
            Ok(())
        }
        PeerAction::Accept { output } => {
            send_message(&mut send, &ControlMessage::Accept)
                .await
                .map_err(|err| err.to_string())?;
            let result = engine
                .receive(
                    &ticket,
                    ReceiveOptions {
                        target_dir: output.clone(),
                        overwrite: false,
                    },
                    &mut |_| {},
                )
                .await
                .map_err(|err| err.to_string())?;
            assert!(
                result.files == 3,
                "peer received 3 files, got {}",
                result.files
            );
            send_message(
                &mut send,
                &ControlMessage::Result {
                    bytes: result.bytes,
                    files: result.files as u32,
                },
            )
            .await
            .map_err(|err| err.to_string())?;
            wire::await_peer_close(&mut recv, "sender close after result")
                .await
                .map_err(|err| err.to_string())?;
            Ok(())
        }
    }
}

/// Start the sender side of a flow and hand the generated code to the test.
/// The caller must drive the returned future concurrently with the fake
/// peer (e2e's await_sender_code pattern).
async fn start_sender_side(
    engine: TransferEngine,
    control_rx: mpsc::UnboundedReceiver<iroh::endpoint::Connection>,
    rv_url: &str,
    paths: Vec<PathBuf>,
) -> (
    std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SendOutcome, worddrop_cli::send::SendError>>>,
    >,
    String,
) {
    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SendOutcome, worddrop_cli::send::SendError>>>,
    > = Box::pin(run_send(
        engine,
        control_rx,
        RvClient::new(rv_url),
        paths,
        SendUi::new(),
        std::future::pending::<()>(),
        Some(code_tx),
    ));
    let mut sender_fut = sender_fut;
    // Poll the sender while waiting for the code (e2e's await_sender_code
    // pattern): a sender that fails before generating a code must surface
    // as a panic, not a silent 60s hang.
    let code = timeout(Duration::from_secs(60), async {
        tokio::select! {
            result = &mut sender_fut => panic!("sender failed before code: {result:?}"),
            code = code_rx.recv() => code.expect("code channel stays open"),
        }
    })
    .await
    .expect("code arrives within 60s");
    (sender_fut, code)
}

fn verify_exported(output: &Path) {
    assert_eq!(
        fs::read(output.join("a.txt")).expect("a"),
        b"hello world from a\n"
    );
    assert_eq!(
        fs::read(output.join("b.bin")).expect("b"),
        b"binary\x00\xff\xee data"
    );
    assert_eq!(
        fs::read(output.join("c.dat")).expect("c"),
        b"third! with more bytes for testing"
    );
}

#[tokio::test]
async fn send_flow_decline_reaches_the_sender() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let peer_dir = temp_dir("peer");

    let (sender, control_rx) = sender_engine(&sender_dir).await;
    let peer = TransferEngine::with_relay_mode(&peer_dir, RelayMode::Disabled, None)
        .await
        .expect("peer engine");

    let (sender_fut, code) = start_sender_side(sender, control_rx, &rv_url, vec![a, b, c]).await;
    let peer_rv = RvClient::new(&rv_url);
    let peer_result = run_fake_peer(
        &peer,
        &peer_rv,
        &code,
        PeerAction::Decline {
            reason: "not now".to_string(),
        },
    );
    let (outcome, peer_result) =
        tokio::join!(timeout(Duration::from_secs(180), sender_fut), peer_result);

    assert_eq!(
        outcome
            .expect("flow completes within 180s")
            .expect("sender flow succeeds"),
        SendOutcome::Declined {
            reason: "not now".to_string()
        },
        "sender sees the decline"
    );
    peer_result.expect("peer decline flow succeeds");
    let _ = peer.shutdown().await;
}

#[tokio::test]
async fn send_flow_accept_transfers_files_byte_for_byte() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let peer_dir = temp_dir("peer");
    let output = temp_dir("output");

    let (sender, control_rx) = sender_engine(&sender_dir).await;
    let peer = TransferEngine::with_relay_mode(&peer_dir, RelayMode::Disabled, None)
        .await
        .expect("peer engine");

    let (sender_fut, code) = start_sender_side(sender, control_rx, &rv_url, vec![a, b, c]).await;
    let peer_rv = RvClient::new(&rv_url);
    let peer_result = run_fake_peer(
        &peer,
        &peer_rv,
        &code,
        PeerAction::Accept {
            output: output.clone(),
        },
    );
    let (outcome, peer_result) =
        tokio::join!(timeout(Duration::from_secs(180), sender_fut), peer_result);

    match outcome
        .expect("flow completes within 180s")
        .expect("sender flow succeeds")
    {
        SendOutcome::Completed { bytes, files } => {
            assert!(bytes > 0, "sender reports positive bytes");
            assert_eq!(files, 3, "sender reports 3 files");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    peer_result.expect("peer accept flow succeeds");
    verify_exported(&output);
    let _ = peer.shutdown().await;
}

#[tokio::test]
async fn send_flow_local_interrupt_cancels_cleanly() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");

    let (sender, control_rx) = sender_engine(&sender_dir).await;

    // The "Ctrl+C" fires 50ms in, while the flow waits for a receiver that
    // never dials in.
    let (code_tx, _code_rx) = mpsc::channel(1);
    let interrupt = Box::pin(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let outcome = timeout(
        Duration::from_secs(180),
        run_send(
            sender,
            control_rx,
            RvClient::new(&rv_url),
            vec![a, b, c],
            SendUi::new(),
            interrupt,
            Some(code_tx),
        ),
    )
    .await
    .expect("flow completes within 180s")
    .expect("cancel is not an error");

    assert_eq!(
        outcome,
        SendOutcome::Cancelled,
        "interrupt cancels the flow cleanly"
    );
}
