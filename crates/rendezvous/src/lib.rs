//! my-croc-rendezvous — axum nameplate mailbox server.
//!
//! Code <-> ticket mailbox: allocate a numeric nameplate (1-9999), one-shot
//! claim, 600s TTL, per-IP rate limits. The server stores and routes ONLY by
//! nameplate (SECURITY F1): the word-code password never reaches this server.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
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
        .route("/v1/pairs", post(allocate))
        .route("/v1/pairs/{nameplate}/claim", post(claim))
        .route("/v1/pairs/{nameplate}/status", get(status))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
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
    rate_limit(addr.ip(), &state.create_limiter, CREATE_LIMIT_PER_MINUTE)?;
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
    rate_limit(addr.ip(), &state.access_limiter, ACCESS_LIMIT_PER_MINUTE)?;
    let nameplate = Nameplate::parse(&nameplate).map_err(invalid_nameplate)?;

    match state
        .pairs
        .lock()
        .map_err(lock_error)?
        .claim(nameplate, Instant::now())
    {
        Ok(ticket) => {
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
    rate_limit(addr.ip(), &state.access_limiter, ACCESS_LIMIT_PER_MINUTE)?;
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

fn rate_limit(ip: IpAddr, limiter: &Mutex<RateLimiter>, limit: usize) -> Result<(), ApiError> {
    if !limiter.lock().map_err(lock_error)?.check(ip, limit) {
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
