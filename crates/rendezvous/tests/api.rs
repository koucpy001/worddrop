//! Integration tests for the my-croc rendezvous mailbox server.
//!
//! TDD via `tower::ServiceExt::oneshot`: the axum `Router` is exercised as a
//! `Service<Request>` without binding a socket. Client IP is injected through
//! request extensions (`ConnectInfo`), mirroring the drift server's test
//! harness, so per-IP rate limits are testable deterministically.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use my_croc_rendezvous::{
    ACCESS_LIMIT_PER_MINUTE, CREATE_LIMIT_PER_MINUTE, AppState, app,
};
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

fn ip_a() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 4000))
}

fn ip_b() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 2], 4000))
}

fn request(method: &Method, uri: &str, ip: SocketAddr, body: Body) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if method == Method::POST {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let mut request = builder.body(body).expect("request built");
    request.extensions_mut().insert(ConnectInfo(ip));
    request
}

fn json_body(value: &serde_json::Value) -> Body {
    Body::from(serde_json::to_vec(value).expect("serializable"))
}

async fn read_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("valid json")
}

#[derive(Deserialize)]
struct AllocateResponse {
    nameplate: u32,
    expires_at: u64,
}

/// Happy path: allocate -> status pending -> claim returns the ticket.
#[tokio::test]
async fn happy_allocate_status_claim() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "ticket-payload" })),
        ))
        .await
        .expect("allocate response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: AllocateResponse = serde_json::from_value(read_json(response).await).unwrap();
    assert!((1..=9999).contains(&created.nameplate));
    assert!(created.expires_at > 0);

    let uri = format!("/v1/pairs/{}/status", created.nameplate);
    let status = app
        .clone()
        .oneshot(request(&Method::GET, &uri, ip_a(), Body::empty()))
        .await
        .expect("status response");
    assert_eq!(status.status(), StatusCode::OK);
    let status_json = read_json(status).await;
    assert_eq!(status_json["state"], "pending");

    let claim_uri = format!("/v1/pairs/{}/claim", created.nameplate);
    let claim = app
        .clone()
        .oneshot(request(&Method::POST, &claim_uri, ip_a(), Body::empty()))
        .await
        .expect("claim response");
    assert_eq!(claim.status(), StatusCode::OK);
    let claim_json = read_json(claim).await;
    assert_eq!(claim_json["ticket"], "ticket-payload");
}

/// A claimed pair reports state "claimed" via status (magic-wormhole model).
#[tokio::test]
async fn status_reports_claimed_after_claim() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("allocate");
    let created: AllocateResponse = serde_json::from_value(read_json(response).await).unwrap();

    let claim_uri = format!("/v1/pairs/{}/claim", created.nameplate);
    let claim = app
        .clone()
        .oneshot(request(&Method::POST, &claim_uri, ip_a(), Body::empty()))
        .await
        .expect("claim");
    assert_eq!(claim.status(), StatusCode::OK);

    let status_uri = format!("/v1/pairs/{}/status", created.nameplate);
    let status = app
        .clone()
        .oneshot(request(&Method::GET, &status_uri, ip_a(), Body::empty()))
        .await
        .expect("status");
    assert_eq!(status.status(), StatusCode::OK);
    let status_json = read_json(status).await;
    assert_eq!(status_json["state"], "claimed");
}

/// One-shot claim: second claim on the same nameplate returns 404.
#[tokio::test]
async fn double_claim_returns_404() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("allocate");
    let created: AllocateResponse = serde_json::from_value(read_json(response).await).unwrap();

    let claim_uri = format!("/v1/pairs/{}/claim", created.nameplate);
    let first = app
        .clone()
        .oneshot(request(&Method::POST, &claim_uri, ip_a(), Body::empty()))
        .await
        .expect("first claim");
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .clone()
        .oneshot(request(&Method::POST, &claim_uri, ip_a(), Body::empty()))
        .await
        .expect("second claim");
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
}

/// Claiming an expired pair returns 410 Gone; status reports "expired".
#[tokio::test]
async fn expired_pair_returns_410_and_status_expired() {
    // Zero TTL: entry is already expired immediately after allocation.
    let app = app(Arc::new(AppState::with_ttl(Duration::ZERO)));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("allocate");
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: AllocateResponse = serde_json::from_value(read_json(response).await).unwrap();

    let claim_uri = format!("/v1/pairs/{}/claim", created.nameplate);
    let claim = app
        .clone()
        .oneshot(request(&Method::POST, &claim_uri, ip_a(), Body::empty()))
        .await
        .expect("claim");
    assert_eq!(claim.status(), StatusCode::GONE);

    let status_uri = format!("/v1/pairs/{}/status", created.nameplate);
    let status = app
        .clone()
        .oneshot(request(&Method::GET, &status_uri, ip_a(), Body::empty()))
        .await
        .expect("status");
    assert_eq!(status.status(), StatusCode::OK);
    let status_json = read_json(status).await;
    assert_eq!(status_json["state"], "expired");
}

/// 10 creates per IP per minute allowed; the 11th returns 429.
#[tokio::test]
async fn create_rate_limit_returns_429() {
    let app = app(Arc::new(AppState::new()));

    for _ in 0..CREATE_LIMIT_PER_MINUTE {
        let response = app
            .clone()
            .oneshot(request(
                &Method::POST,
                "/v1/pairs",
                ip_a(),
                json_body(&serde_json::json!({ "ticket": "t" })),
            ))
            .await
            .expect("create");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let blocked = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("blocked create");
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);

    // A different IP is not affected by A's quota.
    let other = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_b(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("other ip create");
    assert_eq!(other.status(), StatusCode::CREATED);
}

/// 60 accesses (status/claim) per IP per minute allowed; the 61st returns 429.
#[tokio::test]
async fn access_rate_limit_returns_429() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "t" })),
        ))
        .await
        .expect("allocate");
    let created: AllocateResponse = serde_json::from_value(read_json(response).await).unwrap();
    let status_uri = format!("/v1/pairs/{}/status", created.nameplate);

    for _ in 0..ACCESS_LIMIT_PER_MINUTE {
        let response = app
            .clone()
            .oneshot(request(&Method::GET, &status_uri, ip_a(), Body::empty()))
            .await
            .expect("status access");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let blocked = app
        .clone()
        .oneshot(request(&Method::GET, &status_uri, ip_a(), Body::empty()))
        .await
        .expect("blocked status");
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// SECURITY F1: a word-bearing claim path (the pairing words) is rejected 400.
/// The words must never reach the mailbox.
#[tokio::test]
async fn word_bearing_claim_path_rejected_with_400() {
    let app = app(Arc::new(AppState::new()));

    // "7-correct-horse-battery" — nameplate with the word password appended.
    let claim = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs/7-correct-horse-battery/claim",
            ip_a(),
            Body::empty(),
        ))
        .await
        .expect("word claim");
    assert_eq!(claim.status(), StatusCode::BAD_REQUEST);

    let status = app
        .clone()
        .oneshot(request(
            &Method::GET,
            "/v1/pairs/7-correct-horse-battery/status",
            ip_a(),
            Body::empty(),
        ))
        .await
        .expect("word status");
    assert_eq!(status.status(), StatusCode::BAD_REQUEST);
}

/// Claim paths that are not canonical numeric nameplates are rejected 400:
/// out of range, leading zeros, empty.
#[tokio::test]
async fn non_canonical_nameplate_paths_rejected() {
    let app = app(Arc::new(AppState::new()));

    for bad_path in [
        "/v1/pairs/0/claim",
        "/v1/pairs/10000/claim",
        "/v1/pairs/007/claim",
        "/v1/pairs/1_000/claim",
        "/v1/pairs/+5/claim",
        "/v1/pairs/1a/claim",
    ] {
        let response = app
            .clone()
            .oneshot(request(&Method::POST, bad_path, ip_a(), Body::empty()))
            .await
            .expect("bad claim");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad_path}"
        );
    }

    for bad_path in [
        "/v1/pairs/0/status",
        "/v1/pairs/10000/status",
        "/v1/pairs/007/status",
        "/v1/pairs/abc/status",
    ] {
        let response = app
            .clone()
            .oneshot(request(&Method::GET, bad_path, ip_a(), Body::empty()))
            .await
            .expect("bad status");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad_path}"
        );
    }
}

/// Status on an unknown nameplate returns 404.
#[tokio::test]
async fn status_unknown_nameplate_returns_404() {
    let app = app(Arc::new(AppState::new()));

    let status = app
        .clone()
        .oneshot(request(&Method::GET, "/v1/pairs/9999/status", ip_a(), Body::empty()))
        .await
        .expect("status");
    assert_eq!(status.status(), StatusCode::NOT_FOUND);
}

/// Allocate rejects an empty ticket (400).
#[tokio::test]
async fn allocate_rejects_empty_ticket() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(
            &Method::POST,
            "/v1/pairs",
            ip_a(),
            json_body(&serde_json::json!({ "ticket": "" })),
        ))
        .await
        .expect("empty ticket allocate");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Health endpoint answers ok.
#[tokio::test]
async fn health_returns_ok() {
    let app = app(Arc::new(AppState::new()));

    let response = app
        .clone()
        .oneshot(request(&Method::GET, "/health", ip_a(), Body::empty()))
        .await
        .expect("health response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(bytes.as_ref(), b"ok");
}
