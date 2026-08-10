//! Tests for the pairing wire helpers over `tokio::io::duplex` streams —
//! the "mocked streams" unit layer of T13: the sender-side and receiver-side
//! helpers are wired back to back with no iroh endpoint involved.

use tokio::io::{duplex, AsyncWriteExt};

use my_croc_core::pairing::spake::SpakeError;
use my_croc_core::protocol::wire::WireMessage;
use my_croc_core::session::control::ControlMessage;

use super::{PairError, recv_hello, spake_receiver_side, spake_sender_side};

/// Two cross-wired duplex pairs: `a.send` delivers into `b.recv` and
/// `b.send` into `a.recv` — a full-duplex channel between the two roles.
fn channel() -> (Duplex, Duplex) {
    let (a_send, b_recv) = duplex(4096);
    let (b_send, a_recv) = duplex(4096);
    (
        Duplex { send: a_send, recv: a_recv },
        Duplex { send: b_send, recv: b_recv },
    )
}

struct Duplex {
    send: tokio::io::DuplexStream,
    recv: tokio::io::DuplexStream,
}

#[tokio::test]
async fn spake_roundtrip_with_same_words_derives_same_key() {
    let (mut sender, mut receiver) = channel();
    let words = b"correct-horse-battery";

    let sender_side = spake_sender_side(&mut sender.send, &mut sender.recv, words);
    let receiver_side = spake_receiver_side(&mut receiver.send, &mut receiver.recv, words);

    let (sender_key, receiver_key) = tokio::join!(sender_side, receiver_side);
    let sender_key = sender_key.expect("sender spake succeeds");
    let receiver_key = receiver_key.expect("receiver spake succeeds");
    assert_eq!(sender_key.as_bytes(), receiver_key.as_bytes());
}

#[tokio::test]
async fn spake_roundtrip_with_different_words_fails_confirmation() {
    let (mut sender, mut receiver) = channel();

    let sender_side = spake_sender_side(&mut sender.send, &mut sender.recv, b"correct-horse-battery");
    let receiver_side =
        spake_receiver_side(&mut receiver.send, &mut receiver.recv, b"wrong-horse-battery");

    let (sender_result, receiver_result) = tokio::join!(sender_side, receiver_side);
    // Both sides surface ConfirmationMismatch symmetrically (T11 finding:
    // robust to either side losing the race).
    assert!(
        sender_result.is_err() && receiver_result.is_err(),
        "both sides must reject mismatched words, got {sender_result:?} / {receiver_result:?}"
    );
    for result in [sender_result, receiver_result] {
        assert!(
            matches!(result, Err(PairError::Spake(SpakeError::ConfirmationMismatch))),
            "mismatch must surface as ConfirmationMismatch"
        );
    }
}

#[tokio::test]
async fn recv_hello_rejects_non_hello_message() {
    let (mut b_send, mut a_recv) = duplex(4096);

    // The peer sends a Cancel instead of a Hello.
    let frame = WireMessage::new(&ControlMessage::Cancel).encode().expect("encode");
    b_send.write_all(&frame).await.expect("write");

    let err = recv_hello(&mut a_recv).await.expect_err("non-hello rejected");
    assert!(matches!(err, PairError::Not("hello")));
}

#[tokio::test]
async fn recv_hello_echoes_hello_for_version_gate() {
    let (mut b_send, mut a_recv) = duplex(4096);

    let frame = WireMessage::new(&ControlMessage::Hello { version: 1 })
        .encode()
        .expect("encode");
    b_send.write_all(&frame).await.expect("write");

    let echoed = recv_hello(&mut a_recv).await.expect("hello accepted");
    assert_eq!(echoed, ControlMessage::Hello { version: 1 });
}

#[tokio::test]
async fn await_peer_close_returns_on_clean_eof() {
    let (b_send, mut a_recv) = duplex(4096);
    drop(b_send); // peer hangs up

    super::await_peer_close(&mut a_recv, "peer close").await.expect("EOF is a clean close");
}

#[tokio::test]
async fn await_peer_close_times_out_on_silent_peer() {
    let (b_send, mut a_recv) = duplex(4096);
    let _b_send = b_send; // keep the peer open and silent

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        super::await_peer_close(&mut a_recv, "silent peer"),
    )
    .await
    .expect_err("silent peer must hang until the ack timeout");
    // The inner ACK_TIMEOUT is 60s; the outer 50ms probe wins first.
}
