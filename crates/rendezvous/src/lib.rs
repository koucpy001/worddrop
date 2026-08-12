//! worddrop-rendezvous — axum nameplate mailbox server.
//!
//! Code <-> ticket mailbox: allocate a numeric nameplate (1-9999), one-shot
//! claim, 600s TTL, per-IP rate limits. The server stores and routes ONLY by
//! nameplate (SECURITY F1): the word-code password never reaches this server.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::mailbox::{ClaimError, MAX_TICKET_LENGTH, Mailbox, Nameplate, RateLimiter};

pub mod mailbox;
pub mod server;

/// Per-IP create limit: nameplate allocations per minute (drift parity).
pub const CREATE_LIMIT_PER_MINUTE: usize = 10;
/// Per-IP access limit: claims + status checks per minute (drift parity).
pub const ACCESS_LIMIT_PER_MINUTE: usize = 60;
/// Code lifetime before the mailbox entry expires (600s for word-code typing).
pub const TTL: Duration = Duration::from_secs(600);
/// How often the cleanup task sweeps expired entries.
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

type SharedState = Arc<AppState>;

/// Shared server state. In-memory single-node MVP: no DB.
#[derive(Debug)]
pub struct AppState {
    pairs: Mutex<Mailbox>,
    create_limiter: Mutex<RateLimiter>,
    access_limiter: Mutex<RateLimiter>,
    ttl: Duration,
    /// Total nameplate allocation requests received (all outcomes).
    allocate_total: AtomicU64,
    /// Total successful one-shot claims.
    claim_total: AtomicU64,
    /// Total requests rejected by a rate limiter.
    rate_limited_total: AtomicU64,
    /// Total HTTP requests served.
    requests_total: AtomicU64,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_ttl(TTL)
    }

    /// Construct state with a custom TTL. The production default is
    /// [`TTL`]; a shorter TTL is used by tests to exercise expiry without
    /// waiting out the full 600s.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            pairs: Mutex::new(Mailbox::default()),
            create_limiter: Mutex::new(RateLimiter::default()),
            access_limiter: Mutex::new(RateLimiter::default()),
            ttl,
            allocate_total: AtomicU64::new(0),
            claim_total: AtomicU64::new(0),
            rate_limited_total: AtomicU64::new(0),
            requests_total: AtomicU64::new(0),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the axum router with all routes wired to shared state.
pub fn app(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/pairs", post(allocate))
        .route("/v1/pairs/{nameplate}/claim", post(claim))
        .route("/v1/pairs/{nameplate}/status", get(status))
        .with_state(state)
}

/// Serve hand-rolled Prometheus text: counters from AppState atomics plus the
/// `pairs_active` gauge derived from the live mailbox size.
async fn metrics(State(state): State<SharedState>) -> Response {
    state.requests_total.fetch_add(1, Ordering::Relaxed);

    let pairs_active = match state.pairs.try_lock() {
        Ok(pairs) => pairs.len() as u64,
        Err(_) => 0, // poisoned state: /health carries the 503, gauge reads 0
    };
    let body = [
        (
            "worddrop_rendezvous_allocate_total",
            "Total nameplate allocation requests received.",
            "counter",
            state.allocate_total.load(Ordering::Relaxed),
        ),
        (
            "worddrop_rendezvous_claim_total",
            "Total successful one-shot claims.",
            "counter",
            state.claim_total.load(Ordering::Relaxed),
        ),
        (
            "worddrop_rendezvous_rate_limited_total",
            "Total requests rejected by rate limiting.",
            "counter",
            state.rate_limited_total.load(Ordering::Relaxed),
        ),
        (
            "worddrop_rendezvous_requests_total",
            "Total HTTP requests served.",
            "counter",
            state.requests_total.load(Ordering::Relaxed),
        ),
        (
            "worddrop_rendezvous_pairs_active",
            "Number of pairs currently tracked in the mailbox.",
            "gauge",
            pairs_active,
        ),
    ]
    .into_iter()
    .map(|(name, help, ty, value)| {
        format!("# HELP {name} {help}\n# TYPE {name} {ty}\n{name} {value}\n")
    })
    .collect::<String>();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (headers, body).into_response()
}

/// Real health check: verifies the mailbox mutex is lockable. A poisoned or
/// contended state degrades to 503 "degraded"; the healthy body stays the
/// literal "ok" that Caddy/CI probes match against.
async fn health(State(state): State<SharedState>) -> Response {
    state.requests_total.fetch_add(1, Ordering::Relaxed);
    match state.pairs.try_lock() {
        Ok(_) => "ok".into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "degraded").into_response(),
    }
}

// ---- request/response payloads -------------------------------------------------

#[derive(Debug, Deserialize)]
struct AllocateRequest {
    ticket: String,
}

#[derive(Debug, Serialize)]
struct AllocateResponse {
    nameplate: u32,
    expires_at: u64,
}

#[derive(Debug, Serialize)]
struct ClaimResponse {
    ticket: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

// ---- handlers ------------------------------------------------------------------

async fn allocate(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(request): Json<AllocateRequest>,
) -> Result<(StatusCode, Json<AllocateResponse>), ApiError> {
    state.requests_total.fetch_add(1, Ordering::Relaxed);
    state.allocate_total.fetch_add(1, Ordering::Relaxed);
    rate_limit(
        &state,
        addr.ip(),
        &state.create_limiter,
        CREATE_LIMIT_PER_MINUTE,
    )?;
    validate_ticket(&request.ticket)?;

    let expires_at_epoch = now_epoch()
        .checked_add(state.ttl.as_secs())
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "expiry overflow"))?;
    let nameplate = state
        .pairs
        .lock()
        .map_err(lock_error)?
        .allocate(request.ticket, state.ttl);

    info!(client_ip = %addr.ip(), nameplate = %nameplate, "nameplate allocated");

    Ok((
        StatusCode::CREATED,
        Json(AllocateResponse {
            nameplate: nameplate.value(),
            expires_at: expires_at_epoch,
        }),
    ))
}

async fn claim(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(nameplate): Path<String>,
) -> Result<Json<ClaimResponse>, ApiError> {
    state.requests_total.fetch_add(1, Ordering::Relaxed);
    rate_limit(
        &state,
        addr.ip(),
        &state.access_limiter,
        ACCESS_LIMIT_PER_MINUTE,
    )?;
    let nameplate = Nameplate::parse(&nameplate).map_err(invalid_nameplate)?;

    match state
        .pairs
        .lock()
        .map_err(lock_error)?
        .claim(nameplate, Instant::now())
    {
        Ok(ticket) => {
            state.claim_total.fetch_add(1, Ordering::Relaxed);
            info!(client_ip = %addr.ip(), %nameplate, "nameplate claimed");
            Ok(Json(ClaimResponse { ticket }))
        }
        Err(ClaimError::NotFound) => Err(ApiError::new(StatusCode::NOT_FOUND, "pair not found")),
        Err(ClaimError::Expired) => Err(ApiError::new(StatusCode::GONE, "pair has expired")),
    }
}

async fn status(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(nameplate): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    state.requests_total.fetch_add(1, Ordering::Relaxed);
    rate_limit(
        &state,
        addr.ip(),
        &state.access_limiter,
        ACCESS_LIMIT_PER_MINUTE,
    )?;
    let nameplate = Nameplate::parse(&nameplate).map_err(invalid_nameplate)?;

    let pair_state = state
        .pairs
        .lock()
        .map_err(lock_error)?
        .status(nameplate, Instant::now())
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "pair not found"))?;

    let state_str = match pair_state {
        mailbox::PairState::Pending => "pending",
        mailbox::PairState::Claimed => "claimed",
        mailbox::PairState::Expired => "expired",
    };

    Ok(Json(StatusResponse { state: state_str }))
}

// ---- helpers -------------------------------------------------------------------

fn rate_limit(
    state: &AppState,
    ip: IpAddr,
    limiter: &Mutex<RateLimiter>,
    limit: usize,
) -> Result<(), ApiError> {
    if !limiter.lock().map_err(lock_error)?.check(ip, limit) {
        state.rate_limited_total.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        ));
    }
    Ok(())
}

fn validate_ticket(ticket: &str) -> Result<(), ApiError> {
    if ticket.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "ticket must not be empty",
        ));
    }
    if ticket.len() > MAX_TICKET_LENGTH {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "ticket is too large",
        ));
    }
    Ok(())
}

fn invalid_nameplate(error: mailbox::NameplateError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server state is unavailable",
    )
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

// ---- error type -----------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorBody {
            error: self.message,
        });
        (self.status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    async fn oneshot_get(app: &Router, uri: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request built"),
            )
            .await
            .expect("response")
    }

    /// A poisoned pairs mutex degrades /health to 503 "degraded" without
    /// panicking, and the server keeps serving other routes.
    #[tokio::test]
    async fn health_returns_503_degraded_when_pairs_poisoned() {
        let state = Arc::new(AppState::new());

        let poisoned = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let _guard = state.pairs.lock().expect("lock before panic");
                panic!("poison the pairs mutex on purpose");
            })
            .join()
        };
        assert!(poisoned.is_err(), "test setup must poison the mutex");
        assert!(state.pairs.is_poisoned());

        let app = app(state);
        let response = oneshot_get(&app, "/health").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(bytes.as_ref(), b"degraded");

        // Still serving: /metrics renders (pairs_active unknown -> 0).
        let response = oneshot_get(&app, "/metrics").await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8(bytes.to_vec())
            .expect("utf-8")
            .replace("\r\n", "\n");
        assert!(text.contains("worddrop_rendezvous_pairs_active 0"));
    }

    /// The healthy path keeps the literal "ok" body (Caddy/CI probes).
    #[tokio::test]
    async fn health_returns_ok_with_200_on_healthy_state() {
        let app = app(Arc::new(AppState::new()));
        let response = oneshot_get(&app, "/health").await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(bytes.as_ref(), b"ok");
    }
}
