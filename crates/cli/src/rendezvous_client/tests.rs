//! Tests for the rendezvous client against a tiny in-process mock HTTP
//! server (canned responses, request capture). The real T6 server interop is
//! covered by the send-pair integration test (tests/send_pair.rs).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{Allocation, PairState, RvClient};

/// A canned HTTP response: status line + optional JSON body.
struct Response {
    status: &'static str,
    body: &'static str,
}

/// Spawn a mock server that answers every connection with the next scripted
/// response and records the raw request text. Returns the base URL and a
/// handle (cancelled when the test runtime drops).
async fn mock_server(script: Vec<Response>) -> (String, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock port");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("http://{addr}");
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let handle = tokio::spawn(async move {
        let mut script = script.into_iter();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let Some(response) = script.next() else {
                return;
            };
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                captured
                    .lock()
                    .expect("capture lock")
                    .push(String::from_utf8_lossy(&raw).into_owned());
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (url, handle, requests)
}

fn json_response(status: &'static str, json: &'static str) -> Response {
    Response { status, body: json }
}

#[tokio::test]
async fn allocate_parses_nameplate_and_expiry() {
    let (url, _task, _captured) = mock_server(vec![json_response(
        "201 Created",
        r#"{"nameplate":42,"expires_at":1780000000}"#,
    )])
    .await;
    let client = RvClient::new(&url);

    let allocation = client
        .allocate("ticket-abc")
        .await
        .expect("allocate succeeds");
    assert_eq!(
        allocation,
        Allocation {
            nameplate: 42,
            expires_at: 1780000000
        }
    );
}

#[tokio::test]
async fn allocate_places_ticket_in_request_body() {
    let (url, _task, captured) = mock_server(vec![json_response(
        "201 Created",
        r#"{"nameplate":1,"expires_at":1}"#,
    )])
    .await;
    let client = RvClient::new(&url);

    client
        .allocate("ticket-abc")
        .await
        .expect("allocate succeeds");
    let raw = captured
        .lock()
        .expect("lock")
        .first()
        .expect("one request")
        .clone();
    assert!(
        raw.starts_with("POST /v1/pairs HTTP/1.1"),
        "method + path: {raw}"
    );
    assert!(
        raw.contains(r#""ticket":"ticket-abc""#),
        "ticket in body: {raw}"
    );
}

#[tokio::test]
async fn claim_parses_ticket() {
    let (url, _task, _captured) = mock_server(vec![json_response(
        "200 OK",
        r#"{"ticket":"blob-ticket-value"}"#,
    )])
    .await;
    let client = RvClient::new(&url);

    let ticket = client.claim(7).await.expect("claim succeeds");
    assert_eq!(ticket, "blob-ticket-value");
}

#[tokio::test]
async fn claim_second_claim_maps_to_http_404() {
    let (url, _task, _captured) = mock_server(vec![json_response(
        "404 Not Found",
        r#"{"error":"pair not found"}"#,
    )])
    .await;
    let client = RvClient::new(&url);

    let err = client.claim(7).await.expect_err("claimed pair is 404");
    let message = err.to_string();
    assert!(message.contains("HTTP 404"), "status surfaced: {message}");
    assert!(
        message.contains("pair not found"),
        "server body surfaced: {message}"
    );
}

#[tokio::test]
async fn claim_expired_maps_to_http_410() {
    let (url, _task, _captured) = mock_server(vec![json_response(
        "410 Gone",
        r#"{"error":"pair has expired"}"#,
    )])
    .await;
    let client = RvClient::new(&url);

    let err = client.claim(7).await.expect_err("expired pair is 410");
    assert!(err.to_string().contains("HTTP 410"));
}

#[tokio::test]
async fn status_parses_each_state() {
    let (url, _task, _captured) = mock_server(vec![
        json_response("200 OK", r#"{"state":"pending"}"#),
        json_response("200 OK", r#"{"state":"claimed"}"#),
        json_response("200 OK", r#"{"state":"expired"}"#),
    ])
    .await;
    let client = RvClient::new(&url);

    assert_eq!(client.status(7).await.expect("pending"), PairState::Pending);
    assert_eq!(client.status(7).await.expect("claimed"), PairState::Claimed);
    assert_eq!(client.status(7).await.expect("expired"), PairState::Expired);
}

#[tokio::test]
async fn status_unknown_state_is_parse_error() {
    let (url, _task, _captured) =
        mock_server(vec![json_response("200 OK", r#"{"state":"limbo"}"#)]).await;
    let client = RvClient::new(&url);

    let err = client.status(7).await.expect_err("unknown state rejected");
    assert!(matches!(
        err,
        super::RvError::Parse {
            kind: "status state",
            ..
        }
    ));
}

#[tokio::test]
async fn health_ok_when_server_answers_ok() {
    let (url, _task, _captured) = mock_server(vec![Response {
        status: "200 OK",
        body: "ok",
    }])
    .await;
    let client = RvClient::new(&url);
    client.health().await.expect("health ok");
}

#[tokio::test]
async fn connection_refused_is_io_error() {
    // Bind then drop: the port is free, connect gets ECONNREFUSED.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let client = RvClient::new(&format!("http://{addr}"));

    let err = client
        .health()
        .await
        .expect_err("dead server surfaces io error");
    assert!(matches!(err, super::RvError::Io(_)));
}

#[tokio::test]
async fn malformed_response_is_parse_error() {
    let (url, _task, _captured) = mock_server(vec![Response {
        status: "201 Created",
        body: "not-json",
    }])
    .await;
    let client = RvClient::new(&url);

    let err = client
        .allocate("x")
        .await
        .expect_err("garbage body rejected");
    assert!(matches!(
        err,
        super::RvError::Parse {
            kind: "allocate",
            ..
        }
    ));
}
