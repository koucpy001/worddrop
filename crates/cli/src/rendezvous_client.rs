//! Rendezvous client with per-scheme backend dispatch (T6).
//!
//! [`RvClient`] picks its transport backend from the base URL's scheme at
//! construction time: `http`/`https` → [`HttpBackend`] (the HTTP/1.1 client
//! below), `mqtt`/`mqtts` → [`MqttBackend`] (the rumqttc broker client:
//! Argon2id-derived mailbox topics carrying one-shot retained tickets). Any
//! other scheme is captured as
//! [`Backend::Invalid`] and only surfaces as [`RvError::BadUrl`] on the
//! first method call — so [`RvClient::new`] stays infallible and no caller
//! needs a `Result`.
//!
//! The HTTP backend is a minimal HTTP/1.1 client (one request per
//! connection, `Connection: close`) — the same pattern the T11 e2e test
//! proved against the real axum server. The rendezvous protocol is three
//! fixed one-shot JSON endpoints, so a full HTTP stack (reqwest would drag
//! in hyper/rustls/tower) is not worth a large dependency tree on a
//! RAM-constrained host for two requests per CLI run. Timeouts are built
//! in: a dead server surfaces in [`REQUEST_TIMEOUT`], never a hang.
//!
//! T4: `https` origins are supported — the plaintext socket is wrapped in
//! rustls (stock webpki roots, same trust policy as iroh: no custom CA, no
//! verification weakening) and the port defaults per scheme (80/443), so
//! `https://pair.worddrop.cloud` works as-is against a TLS-terminating
//! reverse proxy (Caddy).
//!
//! SECURITY (F1): on the HTTP path only the ticket goes to the server on
//! `allocate`; the word-code secret words never leave the client — the HTTP
//! backend ignores the `words` argument of `claim`/`publish`/`cleanup`. The
//! MQTT backend is the only place that may use `words`: the pair code is
//! folded into the mailbox topic via Argon2id (see [`MqttBackend`]) — the
//! broker exchange needs the pairing secret, at the cost documented in the
//! README「公共信箱（MQTT）模式」threat-model section.

use std::{
    fmt, io,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use argon2::Argon2;
use rand::Rng;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Outgoing, Packet, QoS, Transport};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use url::Url;

use serde::Deserialize;

/// Upper bound for one rendezvous HTTP exchange (connect + request + body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The allocated nameplate and its server-side expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub nameplate: u32,
    /// Unix epoch seconds when the server drops the pair.
    pub expires_at: u64,
}

/// Lifecycle of an allocated nameplate, as reported by `GET /status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// Not claimed yet.
    Pending,
    /// Claimed (one-shot): the ticket is gone.
    Claimed,
    /// TTL elapsed before any claim.
    Expired,
}

impl PairState {
    fn from_str(value: &str) -> Option<PairState> {
        match value {
            "pending" => Some(PairState::Pending),
            "claimed" => Some(PairState::Claimed),
            "expired" => Some(PairState::Expired),
            _ => None,
        }
    }
}

/// Errors from talking to the rendezvous server.
#[derive(Debug)]
pub enum RvError {
    /// The server answered with a non-2xx status (404 pair not found, 410
    /// expired, 429 rate limited, ...). The body carries the server message.
    Http { status: u16, body: String },
    /// Transport-level failure (connect refused, reset, ...).
    Io(io::Error),
    /// The rendezvous URL is not a usable http/https/mqtt/mqtts origin.
    BadUrl { url: String, reason: String },
    /// TLS handshake or certificate verification failed (https origin).
    Tls { detail: String },
    /// MQTT protocol/connection failure from the broker backend (T5).
    Mqtt { detail: String },
    /// Client-side ticket validation failed. Mirrors the HTTP server's
    /// ticket checks (`rendezvous` `validate_ticket`), enforced locally so a
    /// garbage ticket fails before any network use.
    InvalidTicket { detail: String },
    /// No response within [`REQUEST_TIMEOUT`].
    Timeout,
    /// The response body did not parse as the expected shape.
    Parse { kind: &'static str, body: String },
    /// The selected backend is a placeholder: MQTT support lands in a later
    /// step (Todo 7) and is not callable yet.
    Unimplemented { detail: String },
}

impl fmt::Display for RvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // User-facing errors are bilingual (中文 + English) per the global
        // copy rule; the English half keeps the historical wording.
        match self {
            Self::Http { status, body } => {
                write!(
                    f,
                    "服务器返回 {status}: {body} / rendezvous HTTP {status}: {body}"
                )
            }
            Self::Io(err) => write!(f, "服务器连接错误 / rendezvous io error: {err}"),
            Self::BadUrl { url, reason } => write!(
                f,
                "服务器地址无效 {url}: {reason} / invalid rendezvous URL {url}: {reason}"
            ),
            Self::Tls { detail } => write!(
                f,
                "TLS 握手失败: {detail} / rendezvous TLS handshake failed: {detail}"
            ),
            Self::Mqtt { detail } => write!(f, "MQTT 错误: {detail} / MQTT error: {detail}"),
            Self::InvalidTicket { detail } => {
                write!(f, "配对凭证无效: {detail} / invalid ticket: {detail}")
            }
            Self::Timeout => write!(
                f,
                "服务器请求超时（{} 秒） / rendezvous request timed out after {}s",
                REQUEST_TIMEOUT.as_secs(),
                REQUEST_TIMEOUT.as_secs()
            ),
            Self::Parse { kind, body } => {
                write!(
                    f,
                    "解析 {kind} 响应失败 / failed to parse {kind} response: {body}"
                )
            }
            Self::Unimplemented { detail } => write!(
                f,
                "该后端暂未实现: {detail} / this backend is not implemented yet: {detail}"
            ),
        }
    }
}

impl std::error::Error for RvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RvError::Io(source) => Some(source),
            RvError::Http { .. }
            | RvError::BadUrl { .. }
            | RvError::Tls { .. }
            | RvError::Mqtt { .. }
            | RvError::InvalidTicket { .. }
            | RvError::Timeout
            | RvError::Parse { .. }
            | RvError::Unimplemented { .. } => None,
        }
    }
}

/// Client for the rendezvous service, dispatched to the transport backend
/// chosen from the base URL's scheme at [`RvClient::new`].
#[derive(Debug, Clone)]
pub struct RvClient {
    base: String,
    backend: Backend,
}

/// The transport backend selected from the base URL's scheme.
///
/// `Invalid` carries the deferred reason: the URL parsed but its scheme is
/// not usable (or the URL does not parse at all). It only surfaces as
/// [`RvError::BadUrl`] on the first method call, keeping [`RvClient::new`]
/// infallible.
#[derive(Debug, Clone)]
enum Backend {
    /// `http`/`https` origin: the one-shot JSON API (see [`HttpBackend`]).
    Http(HttpBackend),
    /// `mqtt`/`mqtts` broker: the public mailbox client (see [`MqttBackend`]).
    Mqtt(MqttBackend),
    /// Anything else: reason for the deferred [`RvError::BadUrl`].
    Invalid(String),
}

impl RvClient {
    /// `base` is the server origin: `http://127.0.0.1:8080` (LAN dev),
    /// `https://pair.worddrop.cloud` (production, TLS-terminated by Caddy)
    /// or an MQTT broker such as `mqtts://broker.emqx.io:8883` (public
    /// default, Todo 4).
    ///
    /// Infallible: the scheme is dispatched now, but an unusable scheme is
    /// only reported as [`RvError::BadUrl`] on the first method call.
    pub fn new(base: &str) -> Self {
        let backend = match Url::parse(base) {
            Ok(url) => match url.scheme() {
                "http" | "https" => Backend::Http(HttpBackend::new(base)),
                "mqtt" | "mqtts" => Backend::Mqtt(MqttBackend::new(base)),
                scheme => Backend::Invalid(format!(
                    "unsupported scheme {scheme:?} (http, https, mqtt or mqtts only)"
                )),
            },
            Err(source) => Backend::Invalid(format!("cannot parse as a URL: {source}")),
        };
        Self {
            base: base.to_string(),
            backend,
        }
    }

    /// The deferred [`RvError::BadUrl`] for a [`Backend::Invalid`] base.
    fn bad_url(&self, reason: &str) -> RvError {
        RvError::BadUrl {
            url: self.base.clone(),
            reason: reason.to_string(),
        }
    }

    /// Allocate a nameplate for `ticket` (POST /v1/pairs on HTTP).
    pub async fn allocate(&self, ticket: &str) -> Result<Allocation, RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.allocate(ticket).await,
            Backend::Mqtt(backend) => backend.allocate(ticket).await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }

    /// Register the pairing code with the rendezvous so the receiver can
    /// claim it (Todo 8 wires this into the send flow).
    ///
    /// CONTRACT: `words` is exactly
    /// [`WordCode::password()`](worddrop_core::pairing::wordcode::WordCode::password)
    /// — the three hyphen-joined secret words (e.g. `"correct-horse-battery"`),
    /// NOT the `[String; 3]` array. The HTTP backend treats this as a no-op:
    /// the words never leave the client on the HTTP path (F1). The MQTT
    /// backend publishes them per its broker protocol ([`MqttBackend`]).
    pub async fn publish(&self, ticket: &str, nameplate: u32, words: &str) -> Result<(), RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.publish(ticket, nameplate, words).await,
            Backend::Mqtt(backend) => backend.publish(ticket, nameplate, words).await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }

    /// One-shot claim: returns the stored ticket, or the server's error
    /// (404 = already claimed / never existed, 410 = expired).
    ///
    /// CONTRACT: `words` is exactly
    /// [`WordCode::password()`](worddrop_core::pairing::wordcode::WordCode::password)
    /// — the three hyphen-joined secret words (e.g. `"correct-horse-battery"`),
    /// NOT the `[String; 3]` array. The HTTP backend ignores it (claim is
    /// nameplate-only on the wire, F1); the MQTT backend dispatches on it
    /// ([`MqttBackend`]).
    pub async fn claim(&self, nameplate: u32, words: &str) -> Result<String, RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.claim(nameplate, words).await,
            Backend::Mqtt(backend) => backend.claim(nameplate, words).await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }

    /// Release the nameplate / pairing state after the transfer (Todo 8
    /// wires this into the flows).
    ///
    /// CONTRACT: `words` is exactly
    /// [`WordCode::password()`](worddrop_core::pairing::wordcode::WordCode::password)
    /// — the three hyphen-joined secret words (e.g. `"correct-horse-battery"`),
    /// NOT the `[String; 3]` array. The HTTP backend treats this as a no-op;
    /// the MQTT backend cleans up its broker state ([`MqttBackend`]).
    pub async fn cleanup(&self, nameplate: u32, words: &str) -> Result<(), RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.cleanup(nameplate, words).await,
            Backend::Mqtt(backend) => backend.cleanup(nameplate, words).await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }

    /// Poll the lifecycle of a nameplate (GET /v1/pairs/{n}/status on HTTP).
    pub async fn status(&self, nameplate: u32) -> Result<PairState, RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.status(nameplate).await,
            Backend::Mqtt(backend) => backend.status(nameplate).await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }

    /// GET /health on HTTP; Ok when the server answers "ok".
    pub async fn health(&self) -> Result<(), RvError> {
        match &self.backend {
            Backend::Http(backend) => backend.health().await,
            Backend::Mqtt(backend) => backend.health().await,
            Backend::Invalid(reason) => Err(self.bad_url(reason)),
        }
    }
}

/// A request connection: plain TCP for http origins, TLS-wrapped for https.
trait ConnIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ConnIo for T {}

/// HTTP/1.1 backend for `http`/`https` origins: the one-shot JSON API.
///
/// Holds the original rendezvous client logic, moved verbatim.
#[derive(Debug, Clone)]
pub(crate) struct HttpBackend {
    base: String,
}

impl HttpBackend {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
        }
    }

    /// HTTP `publish` is a no-op: the word-code secret words never leave the
    /// client on the HTTP path (F1); the rendezvous only ever stores the
    /// ticket under the numeric nameplate.
    async fn publish(&self, _ticket: &str, _nameplate: u32, _words: &str) -> Result<(), RvError> {
        Ok(())
    }

    /// Allocate a nameplate for `ticket` (POST /v1/pairs).
    async fn allocate(&self, ticket: &str) -> Result<Allocation, RvError> {
        let body = serde_json::json!({ "ticket": ticket }).to_string();
        let (status, response) = self.request("POST", "/v1/pairs", Some(&body)).await?;
        if status != 201 {
            return Err(RvError::Http {
                status,
                body: response,
            });
        }
        let allocation: AllocateResponse =
            serde_json::from_str(&response).map_err(|_| RvError::Parse {
                kind: "allocate",
                body: response,
            })?;
        Ok(Allocation {
            nameplate: allocation.nameplate,
            expires_at: allocation.expires_at,
        })
    }

    /// One-shot claim: returns the stored ticket, or the server's error
    /// (404 = already claimed / never existed, 410 = expired). `words` is
    /// ignored: the claim is nameplate-only on the wire (F1).
    async fn claim(&self, nameplate: u32, _words: &str) -> Result<String, RvError> {
        let (status, response) = self
            .request("POST", &format!("/v1/pairs/{nameplate}/claim"), None)
            .await?;
        if status != 200 {
            return Err(RvError::Http {
                status,
                body: response,
            });
        }
        let value: ClaimResponse = serde_json::from_str(&response).map_err(|_| RvError::Parse {
            kind: "claim",
            body: response,
        })?;
        Ok(value.ticket)
    }

    /// HTTP `cleanup` is a no-op: there is no server-side state to release
    /// beyond the claim's one-shot semantics.
    async fn cleanup(&self, _nameplate: u32, _words: &str) -> Result<(), RvError> {
        Ok(())
    }

    /// Poll the lifecycle of a nameplate (GET /v1/pairs/{n}/status).
    async fn status(&self, nameplate: u32) -> Result<PairState, RvError> {
        let (status, response) = self
            .request("GET", &format!("/v1/pairs/{nameplate}/status"), None)
            .await?;
        if status != 200 {
            return Err(RvError::Http {
                status,
                body: response,
            });
        }
        let value: StatusResponse =
            serde_json::from_str(&response).map_err(|_| RvError::Parse {
                kind: "status",
                body: response.clone(),
            })?;
        PairState::from_str(&value.state).ok_or(RvError::Parse {
            kind: "status state",
            body: response,
        })
    }

    /// GET /health; Ok when the server answers "ok".
    async fn health(&self) -> Result<(), RvError> {
        let (status, response) = self.request("GET", "/health", None).await?;
        if status == 200 && response.trim() == "ok" {
            Ok(())
        } else {
            Err(RvError::Http {
                status,
                body: response,
            })
        }
    }

    /// Resolve the base origin into `(hostname, port, Host header, use_tls)`.
    /// The port defaults per scheme (80 for http, 443 for https) so a bare
    /// `https://pair.worddrop.cloud` works without an explicit `:443`.
    fn endpoint(&self) -> Result<(String, u16, String, bool), RvError> {
        let url = Url::parse(&self.base).map_err(|source| RvError::BadUrl {
            url: self.base.clone(),
            reason: source.to_string(),
        })?;
        let use_tls = match url.scheme() {
            "http" => false,
            "https" => true,
            scheme => {
                return Err(RvError::BadUrl {
                    url: self.base.clone(),
                    reason: format!("unsupported scheme {scheme:?} (http or https only)"),
                });
            }
        };
        let hostname = url.host_str().ok_or_else(|| RvError::BadUrl {
            url: self.base.clone(),
            reason: "missing host".to_string(),
        })?;
        let port = url.port_or_known_default().ok_or_else(|| RvError::BadUrl {
            url: self.base.clone(),
            reason: "missing port".to_string(),
        })?;
        let host = match url.port() {
            Some(p) => format!("{hostname}:{p}"),
            None => hostname.to_string(),
        };
        Ok((hostname.to_string(), port, host, use_tls))
    }

    /// One TLS connector for all https connections: stock Mozilla webpki roots
    /// (same trust policy as iroh — no custom CA, no verification weakening).
    fn tls_connector() -> &'static TlsConnector {
        static CONNECTOR: OnceLock<TlsConnector> = OnceLock::new();
        CONNECTOR.get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            TlsConnector::from(Arc::new(config))
        })
    }

    async fn tls_connect(tcp: TcpStream, hostname: &str) -> Result<TlsStream<TcpStream>, RvError> {
        // Idempotent: some flows (receive's claim) reach TLS before the
        // engine exists, so the process default must not be assumed here.
        worddrop_core::transfer::engine::install_tls_provider();
        let server_name = ServerName::try_from(hostname.to_string()).map_err(|e| RvError::Tls {
            detail: format!("invalid server name {hostname:?}: {e}"),
        })?;
        Self::tls_connector()
            .connect(server_name, tcp)
            .await
            .map_err(|e| RvError::Tls {
                detail: e.to_string(),
            })
    }

    /// One HTTP/1.1 exchange on a fresh connection. The server honors
    /// `Connection: close`, so the body read runs until EOF.
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), RvError> {
        timeout(REQUEST_TIMEOUT, self.request_inner(method, path, body))
            .await
            .map_err(|_| RvError::Timeout)?
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), RvError> {
        let (hostname, port, host, use_tls) = self.endpoint()?;
        let tcp = TcpStream::connect((hostname.clone(), port))
            .await
            .map_err(RvError::Io)?;
        let mut stream: Box<dyn ConnIo> = if use_tls {
            Box::new(Self::tls_connect(tcp, &hostname).await?)
        } else {
            Box::new(tcp)
        };
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        if let Some(body) = body {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(RvError::Io)?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(RvError::Io)?;
        let text = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| RvError::Parse {
            kind: "response head",
            body: text.clone(),
        })?;
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| RvError::Parse {
                kind: "status line",
                body: head.to_string(),
            })?;
        Ok((status, body.to_string()))
    }
}

/// MQTT broker backend (`mqtt`/`mqtts`): the default public mailbox
/// (`mqtts://broker.emqx.io:8883`).
///
/// Topic model (README「公共信箱（MQTT）模式」): the mailbox topic is derived
/// from the pair code by folding BOTH the nameplate and the secret words
/// into an Argon2id password hash — [`derive_topic`] — so a broker-side
/// offline brute force must search the full `9999 × 256×255×254 ≈ 1.66e11`
/// space instead of the words-only `1.66e7` (hours on a GPU farm). The
/// 32-byte Argon2id output is hex-encoded in full (64 chars), giving the
/// topic `worddrop/v1/{hash}`.
///
/// Lifecycle: [`publish`](Self::publish) stores the sender ticket as a
/// **retained** message (the broker re-delivers it to every later
/// subscriber); [`claim`](Self::claim) subscribes, reads the ticket and
/// immediately clears the retained message (approximate one-shot);
/// [`cleanup`](Self::cleanup) deletes the retained message without reading
/// it and is idempotent. A public broker has no server-side TTL for
/// retained messages — the `expires_at` from [`allocate`](Self::allocate)
/// is a DISPLAY-ONLY value mirroring the HTTP server's 600 s so the CLI
/// countdown UX matches.
///
/// CONTRACT: `words` is exactly
/// [`WordCode::password()`](worddrop_core::pairing::wordcode::WordCode::password)
/// — the three hyphen-joined secret words (e.g. `"correct-horse-battery"`),
/// never the `[String; 3]` array. Topic derivation normalizes it (trim +
/// lowercase) internally; the SPAKE2 handshake always uses the raw words
/// (wire.rs).
#[derive(Debug, Clone)]
pub(crate) struct MqttBackend {
    /// Broker origin, e.g. `mqtts://broker.emqx.io:8883`.
    base: String,
}

/// Upper bound for one MQTT exchange (connect + publish/subscribe + write
/// confirmation), the same 15 s as the HTTP backend's [`REQUEST_TIMEOUT`].
const MQTT_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound for the best-effort DISCONNECT flush in [`MqttBackend::close`].
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Fixed Argon2id salt for mailbox topic derivation. Never change: a new
/// salt would orphan every topic published before the change.
const TOPIC_SALT: &[u8] = b"worddrop-mailbox-v1";

/// Topic prefix for all mailbox topics.
const TOPIC_PREFIX: &str = "worddrop/v1/";

/// Largest accepted ticket, mirroring `rendezvous::mailbox::MAX_TICKET_LENGTH`.
const MAX_TICKET_LENGTH: usize = 4096;

/// Display-only "TTL" reported by [`MqttBackend::allocate`], mirroring the
/// HTTP server's 600 s TTL (`rendezvous::TTL`).
const RENDEZVOUS_TTL_SECS: u64 = 600;

/// Derive the mailbox topic for a pair code: Argon2id (OWASP defaults,
/// `m = 19456 KiB, t = 2, p = 1`) over `"{nameplate}-{words}"` with the
/// fixed [`TOPIC_SALT`], hex-encoded in full (32 bytes → 64 hex chars).
///
/// The nameplate is folded INTO the password (not used as salt): a
/// words-only hash would collapse the brute-force space to the 24-bit word
/// space, while folding the nameplate in gives
/// `9999 × 256×255×254 ≈ 1.66e11`. `words` is normalized first (trim +
/// lowercase) so `"007-CORRECT-Horse-battery"` and
/// `"correct-horse-battery"` land on the same topic; the SPAKE2 handshake
/// still uses the raw words.
pub(crate) fn derive_topic(nameplate: u32, words: &str) -> String {
    let password = format!("{nameplate}-{}", words.trim().to_lowercase());
    let mut digest = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), TOPIC_SALT, &mut digest)
        .expect("Argon2id over a short password cannot fail");
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{TOPIC_PREFIX}{hash}")
}

/// Map a rumqttc protocol/connection error onto [`RvError::Mqtt`].
fn rv_mqtt(err: impl fmt::Display) -> RvError {
    RvError::Mqtt {
        detail: err.to_string(),
    }
}

/// Poll the event loop until `pred` matches, surfacing connection errors.
async fn await_event(
    eventloop: &mut EventLoop,
    pred: impl Fn(&Event) -> bool,
) -> Result<(), RvError> {
    loop {
        match eventloop.poll().await {
            Ok(event) if pred(&event) => return Ok(()),
            Ok(_) => continue,
            Err(err) => return Err(rv_mqtt(err)),
        }
    }
}

impl MqttBackend {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
        }
    }

    /// Resolve the broker origin into `(host, port, use_tls)`. The port
    /// defaults per scheme (8883 for mqtts, 1883 for mqtt) so a bare
    /// `mqtts://broker.emqx.io` works without an explicit `:8883`.
    fn endpoint(&self) -> Result<(String, u16, bool), RvError> {
        let url = Url::parse(&self.base).map_err(|source| RvError::BadUrl {
            url: self.base.clone(),
            reason: source.to_string(),
        })?;
        let use_tls = match url.scheme() {
            "mqtts" => true,
            "mqtt" => false,
            scheme => {
                return Err(RvError::BadUrl {
                    url: self.base.clone(),
                    reason: format!("unsupported scheme {scheme:?} (mqtt or mqtts only)"),
                });
            }
        };
        let hostname = url.host_str().ok_or_else(|| RvError::BadUrl {
            url: self.base.clone(),
            reason: "missing host".to_string(),
        })?;
        let port = url.port().unwrap_or(if use_tls { 8883 } else { 1883 });
        Ok((hostname.to_string(), port, use_tls))
    }

    /// Build a fresh broker connection with a random client id (the broker
    /// must never resume a previous session across operations).
    ///
    /// The rustls process default is installed once and idempotently via
    /// `worddrop_core::transfer::engine::install_tls_provider` (a second
    /// `install_default` would panic "crypto provider already set"); rumqttc
    /// is built with `use-rustls-no-provider` and relies on that default.
    async fn connect(&self) -> Result<(AsyncClient, EventLoop), RvError> {
        worddrop_core::transfer::engine::install_tls_provider();
        let (host, port, use_tls) = self.endpoint()?;
        let mut options = MqttOptions::new(
            format!("worddrop-{:016x}", rand::rng().random::<u64>()),
            host,
            port,
        );
        options.set_keep_alive(Duration::from_secs(60));
        if use_tls {
            options.set_transport(Transport::tls_with_default_config());
        }
        Ok(AsyncClient::new(options, 10))
    }

    /// Best-effort clean close: queue DISCONNECT and poll until the packet is
    /// written (or the connection errored — either way the broker session is
    /// gone) so the broker drops the session instead of waiting out keep-alive.
    /// Bounded by [`CLOSE_TIMEOUT`]: a half-open dead connection must never
    /// stall an already-successful operation. Never fails what it follows.
    async fn close(client: &AsyncClient, eventloop: &mut EventLoop) {
        if client.disconnect().await.is_ok() {
            let _ = timeout(
                CLOSE_TIMEOUT,
                await_event(eventloop, |event| {
                    matches!(event, Event::Outgoing(Outgoing::Disconnect))
                }),
            )
            .await;
        }
    }

    /// Allocate a nameplate without publishing anything: the broker has no
    /// allocation step, so the client picks a random `1..=9999` nameplate
    /// and a later [`publish`](Self::publish) pins the ticket to it.
    ///
    /// `expires_at` is a DISPLAY-ONLY value (see [`RENDEZVOUS_TTL_SECS`]):
    /// the public broker keeps retained messages with no server-side TTL.
    async fn allocate(&self, _ticket: &str) -> Result<Allocation, RvError> {
        let nameplate = rand::rng().random_range(1..=9999);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RvError::Io(io::Error::other("system clock before Unix epoch")))?
            .as_secs();
        Ok(Allocation {
            nameplate,
            expires_at: now + RENDEZVOUS_TTL_SECS,
        })
    }

    /// Store `ticket` on the broker under the pair-code topic as a retained
    /// message: the broker re-delivers it to any later subscriber.
    ///
    /// The ticket is validated first (mirror of the HTTP server's
    /// `validate_ticket`) so a garbage ticket fails before any network use.
    async fn publish(&self, ticket: &str, nameplate: u32, words: &str) -> Result<(), RvError> {
        if ticket.trim().is_empty() {
            return Err(RvError::InvalidTicket {
                detail: "ticket must not be empty".to_string(),
            });
        }
        if ticket.len() > MAX_TICKET_LENGTH {
            return Err(RvError::InvalidTicket {
                detail: "ticket is too large".to_string(),
            });
        }
        let topic = derive_topic(nameplate, words);
        let (client, mut eventloop) = self.connect().await?;
        timeout(MQTT_TIMEOUT, async {
            client
                .publish(topic, QoS::AtLeastOnce, true, ticket)
                .await
                .map_err(rv_mqtt)?;
            // Outgoing::Publish is yielded only after the packet is written
            // and flushed — the retained message is on the wire.
            await_event(&mut eventloop, |event| {
                matches!(event, Event::Outgoing(Outgoing::Publish(_)))
            })
            .await?;
            Ok::<(), RvError>(())
        })
        .await
        .map_err(|_| RvError::Timeout)??;
        Self::close(&client, &mut eventloop).await;
        Ok(())
    }

    /// One-shot claim: subscribe to the pair-code topic and return the
    /// stored ticket. `words` is used ONLY for the topic derivation — the
    /// SPAKE2 handshake (wire.rs) still uses the original raw words.
    ///
    /// A retained message with an empty payload means the mailbox holds no
    /// ticket (cleared, or never published): keep polling — the sender may
    /// publish a moment later. The poll is bounded by [`MQTT_TIMEOUT`] and
    /// maps to [`RvError::Timeout`], surfaced in the UI as「配对码不匹配或
    /// 配对超时」. On success the retained ticket is cleared immediately
    /// (approximate one-shot).
    async fn claim(&self, nameplate: u32, words: &str) -> Result<String, RvError> {
        let topic = derive_topic(nameplate, words);
        let (client, mut eventloop) = self.connect().await?;
        let ticket = timeout(MQTT_TIMEOUT, async {
            client
                .subscribe(&topic, QoS::AtLeastOnce)
                .await
                .map_err(rv_mqtt)?;
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        if publish.payload.is_empty() {
                            // Empty retained payload: no ticket yet. Keep
                            // polling until the timeout bounds the wait.
                            continue;
                        }
                        let ticket = String::from_utf8(publish.payload.to_vec()).map_err(|_| {
                            RvError::Parse {
                                kind: "ticket",
                                body: String::from_utf8_lossy(&publish.payload).into_owned(),
                            }
                        })?;
                        // One-shot: clear the retained ticket so a second
                        // claim finds nothing. Best-effort — the ticket above
                        // is the primary outcome.
                        let _ = client.publish(&topic, QoS::AtLeastOnce, true, "").await;
                        let _ = await_event(&mut eventloop, |event| {
                            matches!(event, Event::Outgoing(Outgoing::Publish(_)))
                        })
                        .await;
                        return Ok(ticket);
                    }
                    Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                        // rumqttc #250: an automatic reconnect re-runs the
                        // CONNACK handshake and rumqttc clears pending
                        // requests when `session_present` is false — the
                        // subscription is silently lost. Re-subscribe on
                        // every fresh session (harmless when duplicated).
                        if !ack.session_present {
                            client
                                .subscribe(&topic, QoS::AtLeastOnce)
                                .await
                                .map_err(rv_mqtt)?;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => return Err(rv_mqtt(err)),
                }
            }
        })
        .await
        .map_err(|_| RvError::Timeout)??;
        Self::close(&client, &mut eventloop).await;
        Ok(ticket)
    }

    /// Delete the retained ticket without reading it: publishing an empty
    /// retained payload removes the retained message (MQTT 3.1.1 §3.3.1.3).
    /// Idempotent — deleting an absent retained message is a no-op.
    async fn cleanup(&self, nameplate: u32, words: &str) -> Result<(), RvError> {
        let topic = derive_topic(nameplate, words);
        let (client, mut eventloop) = self.connect().await?;
        timeout(MQTT_TIMEOUT, async {
            client
                .publish(topic, QoS::AtLeastOnce, true, "")
                .await
                .map_err(rv_mqtt)?;
            await_event(&mut eventloop, |event| {
                matches!(event, Event::Outgoing(Outgoing::Publish(_)))
            })
            .await?;
            Ok::<(), RvError>(())
        })
        .await
        .map_err(|_| RvError::Timeout)??;
        Self::close(&client, &mut eventloop).await;
        Ok(())
    }

    /// A nameplate on the broker is "not claimed yet" until a claim lands;
    /// the one-shot clear is what makes it read as claimed. There is no
    /// production consumer of this state.
    async fn status(&self, _nameplate: u32) -> Result<PairState, RvError> {
        Ok(PairState::Pending)
    }

    /// Broker health: a successful connect + CONNACK is the check.
    async fn health(&self) -> Result<(), RvError> {
        let (client, mut eventloop) = self.connect().await?;
        timeout(MQTT_TIMEOUT, async {
            await_event(&mut eventloop, |event| {
                matches!(event, Event::Incoming(Packet::ConnAck(_)))
            })
            .await?;
            Ok::<(), RvError>(())
        })
        .await
        .map_err(|_| RvError::Timeout)??;
        Self::close(&client, &mut eventloop).await;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct AllocateResponse {
    nameplate: u32,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
struct ClaimResponse {
    ticket: String,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    state: String,
}

#[cfg(test)]
mod tests;
