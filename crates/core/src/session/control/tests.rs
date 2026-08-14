use std::time::Duration;

use tokio::io::duplex;

use super::{
    ControlMessage, FileMeta, PROTOCOL_VERSION, SessionError, recv_message, recv_message_timeout,
    send_message,
};
use crate::protocol::wire::MAX_FRAME_BYTES;

fn file_meta() -> FileMeta {
    FileMeta {
        name: "photo.jpg".to_owned(),
        size: 4096,
        hash: "ab12cd34".to_owned(),
    }
}

fn offer() -> ControlMessage {
    ControlMessage::Offer {
        files: vec![file_meta()],
        total_bytes: 4096,
    }
}

#[test]
fn session_control_hello_serializes_with_type_tag() {
    let json = serde_json::to_string(&ControlMessage::Hello {
        version: PROTOCOL_VERSION,
    })
    .unwrap();
    assert!(json.contains("\"type\":\"hello\""));
    assert!(json.contains("\"version\":1"));
}

#[test]
fn session_control_offer_serializes_files_and_bytes() {
    let json = serde_json::to_string(&offer()).unwrap();
    assert!(json.contains("\"type\":\"offer\""));
    assert!(json.contains("\"name\":\"photo.jpg\""));
    assert!(json.contains("\"total_bytes\":4096"));
}

#[test]
fn session_control_unit_variants_serialize_plain() {
    assert_eq!(
        serde_json::to_string(&ControlMessage::Accept).unwrap(),
        "{\"type\":\"accept\"}"
    );
    assert_eq!(
        serde_json::to_string(&ControlMessage::Cancel).unwrap(),
        "{\"type\":\"cancel\"}"
    );
}

#[test]
fn session_control_roundtrip_each_variant() {
    let messages = [
        ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        offer(),
        ControlMessage::Accept,
        ControlMessage::Decline {
            reason: "busy".to_owned(),
        },
        ControlMessage::Cancel,
        ControlMessage::Result {
            bytes: 4096,
            files: 1,
            skipped_bytes: 0,
            skipped_files: 0,
        },
    ];
    for message in messages {
        let json = serde_json::to_vec(&message).unwrap();
        let decoded: ControlMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, message);
    }
}

#[test]
fn session_control_check_version_rejects_wrong_hello_only() {
    assert!(matches!(
        ControlMessage::Hello {
            version: PROTOCOL_VERSION + 1
        }
        .check_version(),
        Err(SessionError::VersionMismatch {
            got: 2,
            expected: 1
        })
    ));
    assert!(
        ControlMessage::Hello {
            version: PROTOCOL_VERSION
        }
        .check_version()
        .is_ok()
    );
    assert!(offer().check_version().is_ok());
}

#[tokio::test]
async fn session_control_send_recv_roundtrip_over_duplex() {
    let (mut tx, mut rx) = duplex(1024);
    send_message(&mut tx, &offer())
        .await
        .expect("send succeeds");
    send_message(&mut tx, &ControlMessage::Cancel)
        .await
        .expect("send succeeds");
    drop(tx);

    let first = recv_message(&mut rx).await.expect("first message decodes");
    assert_eq!(first, offer());
    let second = recv_message(&mut rx).await.expect("second message decodes");
    assert_eq!(second, ControlMessage::Cancel);
}

#[tokio::test]
async fn session_control_recv_truncated_stream_is_unexpected_eof() {
    let (mut tx, mut rx) = duplex(1024);
    tx.write_all(&10u32.to_le_bytes()).await.unwrap();
    tx.write_all(b"abc").await.unwrap();
    drop(tx);

    assert!(matches!(
        recv_message(&mut rx).await,
        Err(SessionError::UnexpectedEof)
    ));
}

#[tokio::test]
async fn session_control_recv_over_cap_length_is_frame_too_large() {
    let (mut tx, mut rx) = duplex(1024);
    tx.write_all(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes())
        .await
        .unwrap();
    drop(tx);

    assert!(matches!(
        recv_message(&mut rx).await,
        Err(SessionError::Wire(
            crate::protocol::wire::WireError::FrameTooLarge { .. }
        ))
    ));
}

#[tokio::test]
async fn session_control_recv_garbage_json_is_deserialize_error() {
    let (mut tx, mut rx) = duplex(1024);
    tx.write_all(&5u32.to_le_bytes()).await.unwrap();
    tx.write_all(b"hello").await.unwrap();
    drop(tx);

    assert!(matches!(
        recv_message(&mut rx).await,
        Err(SessionError::Wire(
            crate::protocol::wire::WireError::Deserialize(_)
        ))
    ));
}

#[tokio::test]
async fn session_control_recv_wrong_version_is_rejected() {
    let (mut tx, mut rx) = duplex(1024);
    send_message(
        &mut tx,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION + 1,
        },
    )
    .await
    .unwrap();
    drop(tx);

    assert!(matches!(
        recv_message(&mut rx).await,
        Err(SessionError::VersionMismatch { .. })
    ));
}

#[tokio::test]
async fn session_control_recv_timeout_elapses() {
    let (_tx, mut rx) = duplex(1024);
    let err = recv_message_timeout(&mut rx, Duration::from_millis(10), "test")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SessionError::Timeout { context: "test", limit } if limit == Duration::from_millis(10)
    ));
}

use tokio::io::AsyncWriteExt;
