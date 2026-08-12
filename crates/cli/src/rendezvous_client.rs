//! HTTP client for the my-croc rendezvous server (T6).
//!
//! A minimal HTTP/1.1 client (one request per connection, `Connection:
//! close`) — the same pattern the T11 e2e test proved against the real axum
//! server. The rendezvous protocol is three fixed one-shot JSON endpoints, so
//! a full HTTP stack (reqwest would drag in hyper/rustls/tower) is not worth
//! a large dependency tree on a RAM-constrained host for two requests per CLI
//! run. Timeouts are built in: a dead server surfaces in [`REQUEST_TIMEOUT`],
//! never a hang.
//!
//! T4: `https` origins are supported — the plaintext socket is wrapped in
//! rustls (stock webpki roots, same trust policy as iroh: no custom CA, no
//! verification weakening) and the port defaults per scheme (80/443), so
//! `https://pair.worddrop.cloud` works as-is against a TLS-terminating
//! reverse proxy (Caddy).
//!
//! SECURITY (F1): only the ticket goes to the server on `allocate`; the
//! word-code secret words never leave the client, so this module has no API
//! to send them.

use std::{
    fmt, io,
    sync::{Arc, OnceLock},
    time::Duration,
};

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
    /// The rendezvous URL is not a usable http/https origin.
    BadUrl { url: String, reason: String },
    /// TLS handshake or certificate verification failed (https origin).
    Tls { detail: String },
    /// No response within [`REQUEST_TIMEOUT`].
    Timeout,
    /// The response body did not parse as the expected shape.
    Parse { kind: &'static str, body: String },
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
            | RvError::Timeout
            | RvError::Parse { .. } => None,
        }
    }
}

/// Client for the three rendezvous endpoints plus `/health`.
#[derive(Debug, Clone)]
pub struct RvClient {
    base: String,
}

/// A request connection: plain TCP for http origins, TLS-wrapped for https.
trait ConnIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ConnIo for T {}

impl RvClient {
    /// `base` is the server origin, e.g. `http://127.0.0.1:8080` (LAN dev) or
    /// `https://pair.worddrop.cloud` (production, TLS-terminated by Caddy).
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
        }
    }

    /// Allocate a nameplate for `ticket` (POST /v1/pairs).
    pub async fn allocate(&self, ticket: &str) -> Result<Allocation, RvError> {
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
    /// (404 = already claimed / never existed, 410 = expired).
    pub async fn claim(&self, nameplate: u32) -> Result<String, RvError> {
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

    /// Poll the lifecycle of a nameplate (GET /v1/pairs/{n}/status).
    pub async fn status(&self, nameplate: u32) -> Result<PairState, RvError> {
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
    pub async fn health(&self) -> Result<(), RvError> {
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
        my_croc_core::transfer::engine::install_tls_provider();
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
