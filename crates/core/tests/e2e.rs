//! End-to-end integration tests (T11) — single self-contained file, no
//! sub-modules. Seven flows that traverse the REAL relay and the REAL
//! rendezvous server:
//!
//!   1. `e2e_relay_connectivity_smoke`  — two endpoints connect via the local
//!      iroh-relay binary.
//!   2. `e2e_happy_path_full_transfer`  — full pair + offer/accept + 3-file
//!      transfer over the relay, byte-for-byte verified.
//!   3. `e2e_decline_flow`              — receiver declines, sender sees
//!      Declined, clean end.
//!   4. `e2e_cancel_flow`               — receiver cancels MID-transfer, both
//!      sides end Cancelled.
//!   5. `e2e_wrong_words_flow`          — receiver uses wrong words, key
//!      confirmation mismatches, clean FlowError, no hang (the
//!      nameplate/words-split MITM-resistance proof).
//!   6. `e2e_resume_after_interrupt`    — `receive_resumable` aborted
//!      mid-transfer, re-receive on the same data dir completes.
//!   7. `e2e_invalid_nameplate`         — rendezvous rejects a word-bearing
//!      claim path with HTTP 400.
//!
//! The iroh-relay 1.0.3 binary at `~/.cargo/bin/iroh-relay` is a HARD
//! dependency — its absence is a test failure, never an ignore.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
use iroh::{RelayMode, RelayUrl};
use iroh_blobs::ticket::BlobTicket;
use my_croc_core::pairing::handshake::HandshakeMessage;
use my_croc_core::pairing::spake::{SessionKey, SpakeError, SpakeSession};
use my_croc_core::pairing::wordcode::{WordCode, WordCodeError};
use my_croc_core::pairing::wordlist::WORDS;
use my_croc_core::protocol::wire::{WireError, WireMessage};
use my_croc_core::session::Session;
use my_croc_core::session::control::{
    ControlMessage, FileMeta, HANDSHAKE_TIMEOUT, IDLE_TIMEOUT, PROTOCOL_VERSION, SessionError,
    recv_message_timeout, send_message,
};
use my_croc_core::session::state::{SessionPhase, Transition, TransitionError};
use my_croc_core::transfer::engine::TransferEngine;
use my_croc_core::transfer::receive::{
    ReceiveError, ReceiveOptions, ReceiveProgress, TransferResult,
};
use my_croc_core::transfer::send::{ProgressEvent, SendError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{OnceCell, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

// ============================================================================
//  Constants
// ============================================================================

/// The ALPN of the pairing control stream. iroh-blobs' router handler
/// consumes EVERY incoming bidi stream on `iroh_blobs::ALPN` as a blob
/// request, so control traffic needs its own ALPN — registered on the
/// sender's endpoint (via `new_local_n0`'s extra handler) and dialed by the
/// receiver. Dialing an unregistered ALPN fails with TLS alert 120
/// ("peer doesn't support any known protocol") — the original T11 bug.
const CONTROL_ALPN: &[u8] = b"my-croc/control";

/// The relay port both endpoints use via `RelayMode::Custom`
/// (`iroh-relay --dev` binds the plain-HTTP/WebSocket relay service here).
const RELAY_PORT: u16 = 3340;
/// How long to wait for the relay's HTTP server to accept connections.
const RELAY_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for one pairing exchange round (claim, dial, handshake).
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for the peer to close the control connection after reading
/// our final message (Accept/Decline/Cancel/Result).
const ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for an entire flow (pairing + transfer).
const FLOW_TIMEOUT: Duration = Duration::from_secs(180);
/// Wire cap for handshake frames (same framing as control messages).
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

// ============================================================================
//  Temp dirs and file helpers
// ============================================================================

/// Unique temp dir per call: pid + counter, so concurrent suite runs (and
/// the parallel test threads of this binary) cannot collide.
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("my-croc-e2e-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn read_file(path: &Path) -> Vec<u8> {
    let mut file = File::open(path).expect("open file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read file");
    bytes
}

fn write_file(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path).expect("create file");
    file.write_all(bytes).expect("write file");
}

// ============================================================================
//  Relay lifecycle
// ============================================================================

/// A spawned `iroh-relay --dev` process; killed on drop. A no-op guard when
/// an already-healthy relay occupies [`RELAY_PORT`] (leftover from a crashed
/// run, or a concurrent suite) — the relay is a stateless forwarder, so any
/// healthy listener serves us equally.
struct RelayGuard {
    child: Option<Child>,
}

impl RelayGuard {
    fn reused() -> Self {
        Self { child: None }
    }
}

impl Drop for RelayGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The iroh-relay binary: `~/.cargo/bin/iroh-relay` first, then PATH.
///
/// Platform-aware (F2 CI windows fix): windows runners have no `HOME` (they
/// use `USERPROFILE`) and cargo installs binaries with a `.exe` suffix, so
/// the bare `~/.cargo/bin/iroh-relay` check always missed on windows and the
/// e2e suite failed with "relay binary not found".
fn relay_binary() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "neither HOME nor USERPROFILE is set".to_string())?;
    let exe = if cfg!(windows) {
        "iroh-relay.exe"
    } else {
        "iroh-relay"
    };
    let cargo_bin = PathBuf::from(home).join(".cargo/bin").join(exe);
    if cargo_bin.is_file() {
        return Ok(cargo_bin);
    }
    let path = PathBuf::from(exe);
    if path.is_file() {
        return Ok(path);
    }
    Err(format!(
        "iroh-relay binary not found: {} does not exist and no {} in the PATH. \
         Install with: cargo install iroh-relay --version 1.0.3 --features server",
        cargo_bin.display(),
        exe
    ))
}

/// Spawn `iroh-relay --dev` (plain HTTP/WebSocket on port [`RELAY_PORT`], no
/// TLS — verified against `iroh-relay --help`) and wait until its HTTP
/// server accepts connections. Fails loudly with the relay's log when the
/// binary is absent or exits during startup.
async fn spawn_relay() -> Result<RelayGuard, String> {
    // Reuse an already-running relay instead of colliding on the port.
    if TcpStream::connect(("127.0.0.1", RELAY_PORT)).await.is_ok() {
        return Ok(RelayGuard::reused());
    }
    let path = relay_binary()?;
    let log = temp_dir("relay").join("relay.log");
    let log_file = File::create(&log).map_err(|e| format!("create relay log: {e}"))?;
    let child = Command::new(&path)
        .arg("--dev")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("spawn iroh-relay {path:?}: {e}"))?;
    let mut guard = RelayGuard { child: Some(child) };
    let deadline = std::time::Instant::now() + RELAY_START_TIMEOUT;
    loop {
        if let Some(child) = guard.child.as_mut()
            && let Some(status) = child.try_wait().map_err(|e| format!("wait relay: {e}"))?
        {
            let log_text = fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "iroh-relay exited during startup with {status}; log:\n{log_text}"
            ));
        }
        if TcpStream::connect(("127.0.0.1", RELAY_PORT)).await.is_ok() {
            return Ok(guard);
        }
        if std::time::Instant::now() > deadline {
            let log_text = fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "iroh-relay did not become ready within {}s; log:\n{log_text}",
                RELAY_START_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// One relay per test binary, shared by all tests. Held until the binary
/// exits, when the guard's Drop kills the child.
static RELAY: OnceCell<RelayGuard> = OnceCell::const_new();

async fn ensure_relay() -> &'static RelayGuard {
    RELAY
        .get_or_init(|| async {
            spawn_relay().await.expect(
                "iroh-relay must be installed: \
                 cargo install iroh-relay --version 1.0.3 --features server",
            )
        })
        .await
}

fn relay_mode() -> RelayMode {
    let url =
        RelayUrl::from_str(&format!("http://127.0.0.1:{RELAY_PORT}")).expect("valid relay URL");
    RelayMode::Custom(url.into())
}

/// A transfer engine bound with the real relay. `new_local_n0` registers
/// `iroh_blobs::ALPN` (and `CONTROL_ALPN` with an extra handler) on BOTH the
/// endpoint and the router. We wait for the endpoint to go online before
/// returning: tickets built earlier would lack the relay URL and could only
/// be dialed over direct LAN IPs.
async fn receiver_engine(data_dir: &Path) -> TransferEngine {
    let eng = TransferEngine::new_local_n0(data_dir, relay_mode(), None)
        .await
        .expect("receiver engine must bind");
    wait_online(&eng).await;
    eng
}

async fn sender_engine(
    data_dir: &Path,
    control_tx: mpsc::UnboundedSender<Connection>,
) -> TransferEngine {
    let handler: Box<dyn DynProtocolHandler> = ControlAcceptor::new(control_tx).into();
    let eng = TransferEngine::new_local_n0(
        data_dir,
        relay_mode(),
        Some((CONTROL_ALPN.to_vec(), handler)),
    )
    .await
    .expect("sender engine must bind");
    wait_online(&eng).await;
    eng
}

/// Wait until the endpoint has contacted the relay; fail loudly after a
/// generous bound (a relay-less hang is worse than a clear failure).
async fn wait_online(eng: &TransferEngine) {
    timeout(Duration::from_secs(15), eng.endpoint().online())
        .await
        .expect("endpoint must reach the local relay within 15s");
}

// ============================================================================
//  Rendezvous server (in-process axum, T6) + minimal HTTP client
// ============================================================================

/// Spawn the T6 axum rendezvous server in-process on an ephemeral port and
/// return its base URL. The task is cancelled when the test runtime drops.
async fn spawn_rendezvous() -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral rendezvous port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let _ = my_croc_rendezvous::server::serve(addr).await;
    });
    // The serve task binds asynchronously; poll /health until it answers.
    let client = RvClient::new(&url);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client.health().await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rendezvous server did not become healthy at {url}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (url, handle)
}

/// Errors from talking to the rendezvous server.
#[derive(Debug)]
enum RvError {
    /// The server answered with a non-2xx status.
    Http { status: u16, body: String },
    /// Transport-level failure.
    Io(std::io::Error),
    /// The response body did not parse as expected.
    Parse { kind: &'static str, body: String },
}

impl fmt::Display for RvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Parse { kind, body } => write!(f, "failed to parse {kind} response: {body}"),
        }
    }
}

impl std::error::Error for RvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Http { .. } | Self::Parse { .. } => None,
        }
    }
}

/// Minimal HTTP/1.1 client (one request per connection, `Connection: close`)
/// for the rendezvous endpoints. Kept dependency-free on purpose.
struct RvClient {
    base: String,
}

impl RvClient {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
        }
    }

    /// Allocate a nameplate for `ticket`; returns the allocated nameplate.
    async fn allocate(&self, ticket: &str) -> Result<u32, RvError> {
        let body = serde_json::json!({ "ticket": ticket }).to_string();
        let (status, response) = self.request("POST", "/v1/pairs", Some(&body)).await?;
        if status != 201 {
            return Err(RvError::Http {
                status,
                body: response,
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(&response).map_err(|_| RvError::Parse {
                kind: "allocate",
                body: response.clone(),
            })?;
        value["nameplate"]
            .as_u64()
            .map(|n| n as u32)
            .ok_or(RvError::Parse {
                kind: "allocate",
                body: response,
            })
    }

    /// One-shot claim: returns the stored ticket, or the server's error.
    async fn claim(&self, nameplate: u32) -> Result<String, RvError> {
        self.claim_str(&nameplate.to_string()).await
    }

    /// Claim with a raw path segment (exercises the server's 400 path for
    /// malformed nameplates, which never reach `u32` parsing).
    async fn claim_str(&self, nameplate: &str) -> Result<String, RvError> {
        let (status, response) = self
            .request("POST", &format!("/v1/pairs/{nameplate}/claim"), None)
            .await?;
        if status != 200 {
            return Err(RvError::Http {
                status,
                body: response,
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(&response).map_err(|_| RvError::Parse {
                kind: "claim",
                body: response.clone(),
            })?;
        value["ticket"]
            .as_str()
            .map(str::to_string)
            .ok_or(RvError::Parse {
                kind: "claim",
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

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), RvError> {
        let mut stream = TcpStream::connect(self.base.trim_start_matches("http://"))
            .await
            .map_err(RvError::Io)?;
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.base
        );
        if body.is_some() {
            request.push_str("Content-Type: application/json\r\n");
        }
        if let Some(body) = body {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
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

// ============================================================================
//  FlowError — every driver failure flows through this so tests can match
//  the specific failure mode (e.g. a wrong-words pairing must surface as
//  ConfirmationMismatch, never a hang).
// ============================================================================

#[derive(Debug)]
enum FlowError {
    /// Raw stream I/O.
    Io(std::io::Error),
    /// Control-message framing/exchange failure.
    Control(SessionError),
    /// Handshake framing failure (the SPAKE2/confirm frames).
    Wire(WireError),
    /// SPAKE2 protocol or key-confirmation failure.
    Spake(SpakeError),
    /// Rendezvous HTTP failure.
    Rv(RvError),
    /// Word-code split/generation failure.
    Word(WordCodeError),
    /// Illegal session transition.
    Transition(TransitionError),
    /// Control dial to the sender failed.
    Connect(iroh::endpoint::ConnectError),
    /// Stream open/accept on the control connection failed.
    Connection(iroh::endpoint::ConnectionError),
    /// The transfer receive failed.
    Receive(ReceiveError),
    /// Transfer preparation failed.
    Send(SendError),
    /// The ticket from the rendezvous did not parse.
    Ticket(String),
    /// A message of the wrong kind arrived.
    Not(&'static str),
    /// Waiting for something timed out (no hang).
    Hung(&'static str),
    /// Anything not covered above.
    Unexpected(String),
    /// The other side's task panicked or was cancelled.
    TaskJoin(tokio::task::JoinError),
}

impl From<std::io::Error> for FlowError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<SessionError> for FlowError {
    fn from(value: SessionError) -> Self {
        Self::Control(value)
    }
}
impl From<WireError> for FlowError {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}
impl From<SpakeError> for FlowError {
    fn from(value: SpakeError) -> Self {
        Self::Spake(value)
    }
}
impl From<RvError> for FlowError {
    fn from(value: RvError) -> Self {
        Self::Rv(value)
    }
}
impl From<WordCodeError> for FlowError {
    fn from(value: WordCodeError) -> Self {
        Self::Word(value)
    }
}
impl From<TransitionError> for FlowError {
    fn from(value: TransitionError) -> Self {
        Self::Transition(value)
    }
}
impl From<iroh::endpoint::ConnectError> for FlowError {
    fn from(value: iroh::endpoint::ConnectError) -> Self {
        Self::Connect(value)
    }
}
impl From<iroh::endpoint::ConnectionError> for FlowError {
    fn from(value: iroh::endpoint::ConnectionError) -> Self {
        Self::Connection(value)
    }
}
impl From<ReceiveError> for FlowError {
    fn from(value: ReceiveError) -> Self {
        Self::Receive(value)
    }
}
impl From<SendError> for FlowError {
    fn from(value: SendError) -> Self {
        Self::Send(value)
    }
}
impl From<tokio::task::JoinError> for FlowError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::TaskJoin(value)
    }
}

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Control(err) => write!(f, "control error: {err}"),
            Self::Wire(err) => write!(f, "wire error: {err}"),
            Self::Spake(err) => write!(f, "spake error: {err}"),
            Self::Rv(err) => write!(f, "rendezvous error: {err}"),
            Self::Word(err) => write!(f, "word-code error: {err}"),
            Self::Transition(err) => write!(f, "session transition error: {err}"),
            Self::Connect(err) => write!(f, "control dial failed: {err}"),
            Self::Connection(err) => write!(f, "control stream failed: {err}"),
            Self::Receive(err) => write!(f, "receive failed: {err}"),
            Self::Send(err) => write!(f, "send preparation failed: {err}"),
            Self::Ticket(err) => write!(f, "invalid ticket: {err}"),
            Self::Not(kind) => write!(f, "expected {kind}, got a different message"),
            Self::Hung(what) => write!(f, "timed out waiting for {what}"),
            Self::Unexpected(msg) => write!(f, "unexpected: {msg}"),
            Self::TaskJoin(err) => write!(f, "peer task failed: {err}"),
        }
    }
}

impl std::error::Error for FlowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Control(err) => Some(err),
            Self::Wire(err) => Some(err),
            Self::Spake(err) => Some(err),
            Self::Rv(err) => Some(err),
            Self::Word(err) => Some(err),
            Self::Transition(err) => Some(err),
            Self::Connect(err) => Some(err),
            Self::Connection(err) => Some(err),
            Self::Receive(err) => Some(err),
            Self::Send(err) => Some(err),
            Self::TaskJoin(err) => Some(err),
            Self::Not(_) | Self::Hung(_) | Self::Unexpected(_) | Self::Ticket(_) => None,
        }
    }
}

// ============================================================================
//  Pairing control protocol: CONTROL_ALPN acceptor + SPAKE2 over wire frames
// ============================================================================

/// Hands every accepted control connection to the sender flow driver.
#[derive(Debug)]
struct ControlAcceptor {
    tx: mpsc::UnboundedSender<Connection>,
}

impl ControlAcceptor {
    fn new(tx: mpsc::UnboundedSender<Connection>) -> Self {
        Self { tx }
    }
}

impl ProtocolHandler for ControlAcceptor {
    fn accept(&self, conn: Connection) -> impl Future<Output = Result<(), AcceptError>> + Send {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(conn);
            Ok(())
        }
    }
}

/// Send one handshake frame (u32 LE length + JSON, the T3 wire format).
async fn send_handshake<W>(writer: &mut W, message: &HandshakeMessage) -> Result<(), FlowError>
where
    W: AsyncWrite + Unpin,
{
    let frame = WireMessage::new(message)
        .encode()
        .map_err(FlowError::Wire)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Receive one handshake frame, validating the length prefix.
async fn recv_handshake<R>(reader: &mut R) -> Result<HandshakeMessage, FlowError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(FlowError::Wire(WireError::FrameTooLarge {
            declared,
            max: MAX_FRAME_BYTES,
        }));
    }
    let mut body = vec![0u8; declared];
    reader.read_exact(&mut body).await?;
    let mut frame = Vec::with_capacity(4 + declared);
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(&body);
    Ok(WireMessage::<HandshakeMessage>::decode(&frame)
        .map_err(FlowError::Wire)?
        .into_inner())
}

/// Receive one handshake frame bounded by `timeout` (no-hang guarantee).
async fn recv_handshake_timeout<R>(
    reader: &mut R,
    limit: Duration,
    what: &'static str,
) -> Result<HandshakeMessage, FlowError>
where
    R: AsyncRead + Unpin,
{
    timeout(limit, recv_handshake(reader))
        .await
        .map_err(|_| FlowError::Hung(what))?
}

/// Wait for the sender to close the control connection after it has read
/// our final message (Accept/Decline/Cancel/Result).
///
/// The receiver must never close the connection right after writing: noq's
/// `flush` is a no-op, so the message can still sit in the connection's send
/// buffer and the close frame would discard it (the peer then sees
/// `ConnectionLost` instead of the message). Instead the sender owns the
/// close — it only closes after consuming our message, so its close frame
/// is the acknowledgement. A clean FIN and a connection-close error both
/// count; only a timeout is a failure.
async fn await_peer_close<R>(recv: &mut R, what: &'static str) -> Result<(), FlowError>
where
    R: AsyncRead + Unpin,
{
    let mut sink = [0u8; 256];
    loop {
        match timeout(ACK_TIMEOUT, recv.read(&mut sink)).await {
            Err(_) => return Err(FlowError::Hung(what)),
            Ok(Err(_)) => return Ok(()),
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(_)) => {}
        }
    }
}

/// Sender role of the SPAKE2 + confirmation round over the control stream:
/// receive the peer's message, send ours, derive the key, then exchange and
/// verify the confirmation tokens. A wrong-words peer fails here with
/// `ConfirmationMismatch` — the nameplate/words split MITM-resistance proof.
async fn spake_sender_side<W, R>(
    send: &mut W,
    recv: &mut R,
    words: &[u8],
) -> Result<SessionKey, FlowError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let inbound = recv_handshake_timeout(recv, PAIR_TIMEOUT, "sender spake message")
        .await?
        .into_pake()?;
    let (session, message) = SpakeSession::start(words);
    send_handshake(send, &HandshakeMessage::spake(&message)?).await?;
    let key = session.finish(&inbound)?;
    let inbound_token = recv_handshake_timeout(recv, PAIR_TIMEOUT, "sender confirm token")
        .await?
        .into_confirm()?;
    send_handshake(send, &HandshakeMessage::confirm(key.confirm_token())).await?;
    key.verify_confirm(&inbound_token)?;
    Ok(key)
}

/// Receiver role of the SPAKE2 + confirmation round: send ours first, then
/// receive the peer's, derive the key, exchange and verify tokens.
async fn spake_receiver_side<W, R>(
    send: &mut W,
    recv: &mut R,
    words: &[u8],
) -> Result<SessionKey, FlowError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let (session, message) = SpakeSession::start(words);
    send_handshake(send, &HandshakeMessage::spake(&message)?).await?;
    let inbound = recv_handshake_timeout(recv, PAIR_TIMEOUT, "receiver spake message")
        .await?
        .into_pake()?;
    let key = session.finish(&inbound)?;
    send_handshake(send, &HandshakeMessage::confirm(key.confirm_token())).await?;
    let inbound_token = recv_handshake_timeout(recv, PAIR_TIMEOUT, "receiver confirm token")
        .await?
        .into_confirm()?;
    key.verify_confirm(&inbound_token)?;
    Ok(key)
}

// ============================================================================
//  Sender driver
// ============================================================================

/// How the sender's flow ended.
#[derive(Debug)]
enum SenderOutcome {
    /// The receiver accepted and reported the transfer result.
    Completed { bytes: u64, files: u32 },
    /// The receiver refused the offer.
    Declined { reason: String },
    /// The receiver cancelled.
    Cancelled,
}

/// The sender's terminal state: outcome plus final session phase.
#[derive(Debug)]
struct SenderDone {
    outcome: SenderOutcome,
    phase: SessionPhase,
}

/// What the sender hands the test once the code is ready.
#[derive(Debug)]
struct PairInfo {
    code: String,
}

/// Run the whole sender side of a flow. The engine is consumed: its router
/// serves blob requests while this task drives the control stream.
async fn run_sender_flow(
    engine: TransferEngine,
    mut control_rx: mpsc::UnboundedReceiver<Connection>,
    rv: RvClient,
    paths: Vec<PathBuf>,
    code_tx: mpsc::Sender<PairInfo>,
) -> Result<SenderDone, FlowError> {
    let session = Session::new();
    session.transition(Transition::StartPairing).await?;

    let mut cb: Box<dyn FnMut(ProgressEvent) + Send> = Box::new(|_| {});
    let prepared = engine.prepare_send(&paths, cb.as_mut()).await?;
    let total = prepared.total_bytes;
    let files = prepared.files.len();
    let ticket = prepared.ticket.to_string();
    let nameplate = rv.allocate(&ticket).await?;
    let code = WordCode::generate(nameplate, &mut rand::rng())?;
    let words = code.password();
    let _ = code_tx
        .send(PairInfo {
            code: code.to_string(),
        })
        .await;

    let conn = timeout(PAIR_TIMEOUT, control_rx.recv())
        .await
        .map_err(|_| FlowError::Hung("sender: control connection from receiver"))?
        .ok_or_else(|| FlowError::Unexpected("sender: control channel closed".to_string()))?;
    let (mut send, mut recv) = conn.accept_bi().await?;

    let hello = recv_hello(&mut recv).await?;
    send_message(&mut send, &hello).await?;
    let _key = spake_sender_side(&mut send, &mut recv, words.as_bytes()).await?;
    session.transition(Transition::PairConfirmed).await?;

    let offer = ControlMessage::Offer {
        files: prepared
            .files
            .iter()
            .map(|f| FileMeta {
                name: f.name.clone(),
                size: f.size,
                hash: f.hash.to_hex(),
            })
            .collect(),
        total_bytes: total,
    };
    send_message(&mut send, &offer).await?;

    let response = recv_message_idle(&mut recv, "offer response").await?;
    let outcome = match response {
        ControlMessage::Accept => {
            session.transition(Transition::TransferStarted).await?;
            // The transfer runs while we wait; the receiver may still cancel
            // mid-transfer, so Result and Cancel are both expected here.
            match recv_message_idle(&mut recv, "transfer result or cancel").await? {
                ControlMessage::Result { bytes, files: got } => {
                    if bytes != total || got as usize != files {
                        return Err(FlowError::Unexpected(format!(
                            "sender: result mismatch: expected {total} bytes / {files} \
                             files, got {bytes} / {got}"
                        )));
                    }
                    session.transition(Transition::Completed).await?;
                    SenderOutcome::Completed { bytes, files: got }
                }
                ControlMessage::Cancel => {
                    session.cancel().await?;
                    SenderOutcome::Cancelled
                }
                other => {
                    return Err(FlowError::Unexpected(format!(
                        "sender: unexpected message after accept: {other:?}"
                    )));
                }
            }
        }
        ControlMessage::Decline { reason } => SenderOutcome::Declined { reason },
        ControlMessage::Cancel => {
            session.cancel().await?;
            SenderOutcome::Cancelled
        }
        other => {
            return Err(FlowError::Unexpected(format!(
                "sender: unexpected response {other:?}"
            )));
        }
    };
    Ok(SenderDone {
        outcome,
        phase: session.phase().await,
    })
}

/// Echo the peer's Hello (both sides version-gate via `recv_message`'s
/// `check_version`), returning a Hello of our own.
async fn recv_hello<R>(recv: &mut R) -> Result<ControlMessage, FlowError>
where
    R: AsyncRead + Unpin,
{
    let message = recv_message_timeout(recv, HANDSHAKE_TIMEOUT, "sender hello").await?;
    match message {
        ControlMessage::Hello { .. } => Ok(ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        }),
        other => Err(FlowError::Unexpected(format!(
            "sender: expected hello, got {other:?}"
        ))),
    }
}

/// Receive one control message within the idle timeout (120 s).
async fn recv_message_idle<R>(recv: &mut R, what: &'static str) -> Result<ControlMessage, FlowError>
where
    R: AsyncRead + Unpin,
{
    recv_message_timeout(recv, IDLE_TIMEOUT, what)
        .await
        .map_err(FlowError::Control)
}

/// A word triple guaranteed to differ from `words` (for the wrong-words
/// flow): replace the last word with one from the wordlist that the code
/// does not use.
fn different_words(words: &str) -> String {
    let parts: Vec<&str> = words.split('-').collect();
    let fallback = WORDS
        .iter()
        .find(|word| !parts.contains(word))
        .expect("wordlist has words outside any 3-word code");
    let mut parts = parts;
    parts[2] = fallback;
    parts.join("-")
}

/// Wait for the sender to hand over the pairing code, panicking if the
/// sender fails first. The `select!` branches both exit, but neither may be
/// ready for several polls, so the loop is genuinely iterable — clippy's
/// never_loop heuristic does not apply.
#[allow(clippy::never_loop)]
async fn await_sender_code(
    sender_fut: &mut (impl std::future::Future<Output = Result<SenderDone, FlowError>> + Unpin),
    code_rx: &mut mpsc::Receiver<PairInfo>,
) -> PairInfo {
    loop {
        tokio::select! {
            result = &mut *sender_fut => panic!("sender failed before code: {result:?}"),
            code = code_rx.recv() => return code.expect("sender channel closed without code"),
        }
    }
}

// ============================================================================
//  Receiver driver
// ============================================================================

/// How the receiver should respond to the offer.
#[derive(Debug, Clone)]
enum ReceiverAction {
    /// Accept the offer and download the files.
    Accept,
    /// Decline the offer with a reason.
    Decline { reason: String },
}

/// The receiver's terminal state.
#[derive(Debug)]
struct ReceiverDone {
    /// The transfer result (zero for decline/cancel).
    result: TransferResult,
    /// Final session phase.
    phase: SessionPhase,
}

/// Run the whole receiver side of a flow.
///
/// `override_words` replaces the words extracted from `code` — used by the
/// wrong-words flow to prove the nameplate/words split blocks MITM.
async fn run_receiver_flow(
    engine: TransferEngine,
    code: &str,
    override_words: Option<&str>,
    rv: RvClient,
    output_dir: PathBuf,
    action: ReceiverAction,
) -> Result<ReceiverDone, FlowError> {
    let (nameplate, words_from_code) = WordCode::split(code)?;
    let words = override_words.unwrap_or(&words_from_code);

    // Claim the nameplate to get the sender's ticket.
    let ticket_str = rv.claim(nameplate).await?;
    let ticket =
        BlobTicket::from_str(&ticket_str).map_err(|_| FlowError::Ticket(ticket_str.to_string()))?;

    // Dial the sender on the control ALPN.
    let conn = timeout(
        PAIR_TIMEOUT,
        engine
            .endpoint()
            .connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await
    .map_err(|_| FlowError::Hung("receiver: dial sender on control ALPN"))??;
    let (mut send, mut recv) = conn.open_bi().await?;

    // Hello exchange: we send first, the sender echoes back. Both sides
    // version-gate via recv_message's `check_version` on arrival.
    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let _hello = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver hello").await?;

    // SPAKE2 + key confirmation over the control stream.
    let _key = spake_receiver_side(&mut send, &mut recv, words.as_bytes()).await?;

    // The sender now sends us an Offer.
    let offer = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver offer").await?;
    let ControlMessage::Offer { .. } = &offer else {
        return Err(FlowError::Not("offer"));
    };

    let session = Session::new();
    session.transition(Transition::StartPairing).await?;
    session.transition(Transition::PairConfirmed).await?;

    match action {
        ReceiverAction::Accept => {
            send_message(&mut send, &ControlMessage::Accept).await?;
            session.transition(Transition::TransferStarted).await?;

            let result = engine
                .receive(
                    &ticket,
                    ReceiveOptions {
                        target_dir: output_dir,
                        overwrite: false,
                    },
                    &mut |_| {},
                )
                .await?;

            // Notify the sender of the result, then wait for it to close the
            // control connection: its close is the acknowledgement that it
            // read the result (closing ourselves here would race the send
            // buffer and lose the message).
            send_message(
                &mut send,
                &ControlMessage::Result {
                    bytes: result.bytes,
                    files: result.files as u32,
                },
            )
            .await?;
            await_peer_close(&mut recv, "sender to close after result").await?;
            session.transition(Transition::Completed).await?;
            Ok(ReceiverDone {
                result,
                phase: session.phase().await,
            })
        }
        ReceiverAction::Decline { reason } => {
            send_message(&mut send, &ControlMessage::Decline { reason }).await?;
            await_peer_close(&mut recv, "sender to close after decline").await?;
            Ok(ReceiverDone {
                result: TransferResult {
                    bytes: 0,
                    files: 0,
                    skipped: Vec::new(),
                },
                phase: session.phase().await,
            })
        }
    }
}

/// Receiver side of the MID-TRANSFER cancel flow: pair up, accept the
/// offer, abort the download at its first progress event, then send Cancel
/// so the sender ends Cancelled too.
async fn run_receiver_cancel_mid_transfer(
    engine: TransferEngine,
    code: &str,
    rv: RvClient,
    output_dir: PathBuf,
) -> Result<ReceiverDone, FlowError> {
    let (nameplate, words) = WordCode::split(code)?;
    let ticket_str = rv.claim(nameplate).await?;
    let ticket =
        BlobTicket::from_str(&ticket_str).map_err(|_| FlowError::Ticket(ticket_str.to_string()))?;

    let conn = timeout(
        PAIR_TIMEOUT,
        engine
            .endpoint()
            .connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await
    .map_err(|_| FlowError::Hung("receiver: dial sender on control ALPN"))??;
    let (mut send, mut recv) = conn.open_bi().await?;

    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let _hello = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver hello").await?;
    let _key = spake_receiver_side(&mut send, &mut recv, words.as_bytes()).await?;
    let offer = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver offer").await?;
    let ControlMessage::Offer { .. } = &offer else {
        return Err(FlowError::Not("offer"));
    };

    let session = Session::new();
    session.transition(Transition::StartPairing).await?;
    session.transition(Transition::PairConfirmed).await?;

    send_message(&mut send, &ControlMessage::Accept).await?;
    session.transition(Transition::TransferStarted).await?;

    // Abort the download at the first progress event: a true mid-transfer
    // interrupt, not a refusal before bytes flowed.
    let aborted = abort_receive_mid_transfer(
        &engine,
        &ticket,
        ReceiveOptions {
            target_dir: output_dir,
            overwrite: false,
        },
        false,
    )
    .await?;
    if !aborted {
        return Err(FlowError::Unexpected(
            "transfer completed before the cancel could fire".to_string(),
        ));
    }

    // Tell the sender, wait for its acknowledgement (close), then cancel.
    send_message(&mut send, &ControlMessage::Cancel).await?;
    await_peer_close(&mut recv, "sender to close after cancel").await?;
    session.cancel().await?;
    Ok(ReceiverDone {
        result: TransferResult {
            bytes: 0,
            files: 0,
            skipped: Vec::new(),
        },
        phase: session.phase().await,
    })
}

/// Drive `receive`/`receive_resumable` up to the FIRST `Downloading`
/// progress event with `received > 0`, then abort by dropping the future — a
/// deterministic mid-transfer interrupt that cannot race the download
/// completing (the first event fires after the first chunk, the future needs
/// all 16 MiB).
///
/// Returns `true` when the interrupt actually fired mid-transfer.
async fn abort_receive_mid_transfer(
    engine: &TransferEngine,
    ticket: &BlobTicket,
    options: ReceiveOptions,
    resumable: bool,
) -> Result<bool, FlowError> {
    let (tx, mut rx) = mpsc::channel(1);
    let mut progress = |p: ReceiveProgress| {
        if let ReceiveProgress::Downloading { received, .. } = p
            && received > 0
        {
            let _ = tx.try_send(());
        }
    };
    let fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TransferResult, ReceiveError>>>,
    > = if resumable {
        Box::pin(engine.receive_resumable(ticket, options, &mut progress))
    } else {
        Box::pin(engine.receive(ticket, options, &mut progress))
    };
    tokio::pin!(fut);
    timeout(FLOW_TIMEOUT, async {
        tokio::select! {
            _ = rx.recv() => Ok(()),
            result = &mut fut => Err(FlowError::Unexpected(format!(
                "transfer ended before the interrupt fired: {result:?}"
            ))),
        }
    })
    .await
    .map_err(|_| FlowError::Hung("transfer download progress"))??;
    // Dropping `fut` here aborts the download mid-transfer.
    Ok(true)
}

// ============================================================================
//  Fixtures
// ============================================================================

fn fixture_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let a = dir.join("a.txt");
    let b = dir.join("b.bin");
    let c = dir.join("c.dat");
    write_file(&a, b"hello world from a\n");
    write_file(&b, b"binary\x00\xff\xee data");
    write_file(&c, b"third! with more bytes for testing");
    (a, b, c)
}

/// A 16 MiB deterministic-pattern file (same size class as the T10 abort
/// test): a wide interrupt window for the mid-transfer flows.
fn big_fixture(dir: &Path) -> PathBuf {
    let big = dir.join("big.bin");
    let bytes: Vec<u8> = (0..16 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    write_file(&big, &bytes);
    big
}

fn verify_exported(output: &Path) {
    assert_eq!(read_file(&output.join("a.txt")), b"hello world from a\n");
    assert_eq!(read_file(&output.join("b.bin")), b"binary\x00\xff\xee data");
    assert_eq!(
        read_file(&output.join("c.dat")),
        b"third! with more bytes for testing"
    );
}

fn verify_big(output: &Path) {
    let bytes = read_file(&output.join("big.bin"));
    assert_eq!(bytes.len(), 16 * 1024 * 1024, "big.bin has the full size");
    assert!(
        bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8),
        "big.bin content is byte-for-byte identical"
    );
}

// ============================================================================
//  Flow 0 — relay connectivity smoke test
// ============================================================================

#[tokio::test]
async fn e2e_relay_connectivity_smoke() {
    let _relay = ensure_relay().await;

    let ep1 = receiver_engine(&temp_dir("ep1")).await;
    let ep2 = receiver_engine(&temp_dir("ep2")).await;

    let addr1 = ep1.endpoint().addr();
    // The endpoint must be registered with the configured local relay.
    assert!(
        addr1.relay_urls().next().is_some(),
        "endpoint addr must carry the local relay URL, got {addr1:?}"
    );

    // Dial with a real registered ALPN (iroh-blobs): the engines only accept
    // `iroh_blobs::ALPN` (+ CONTROL_ALPN with an extra handler), so an
    // unregistered ALPN like b"test" is rejected with TLS alert 120.
    let conn = timeout(
        Duration::from_secs(10),
        ep2.endpoint().connect(addr1, iroh_blobs::ALPN),
    )
    .await
    .expect("dial via relay must complete within 10s")
    .expect("dial via relay must succeed");

    // A live connection proves the relay forwarded the QUIC handshake, and
    // the remote id proves we reached the right node.
    assert_eq!(
        conn.remote_id(),
        ep1.endpoint().id(),
        "smoke dial reached the intended node"
    );
    drop(conn);

    let _ = ep1.shutdown().await;
    let _ = ep2.shutdown().await;
}

// ============================================================================
//  Flow 1 — happy path
// ============================================================================

#[tokio::test]
async fn e2e_happy_path_full_transfer() {
    let _relay = ensure_relay().await;
    let (rv_url, _rv_task) = spawn_rendezvous().await;

    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (control_tx, control_rx) = mpsc::unbounded_channel::<Connection>();
    let sender_eng = sender_engine(&sender_dir, control_tx).await;
    let receiver_eng = receiver_engine(&receiver_dir).await;

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_rv = RvClient::new(&rv_url);
    let receiver_rv = RvClient::new(&rv_url);

    timeout(FLOW_TIMEOUT, async {
        let sender_fut = run_sender_flow(sender_eng, control_rx, sender_rv, vec![a, b, c], code_tx);
        tokio::pin!(sender_fut);

        let pair = await_sender_code(&mut sender_fut, &mut code_rx).await;

        let receiver_fut = run_receiver_flow(
            receiver_eng,
            &pair.code,
            None,
            receiver_rv,
            output.clone(),
            ReceiverAction::Accept,
        );
        tokio::pin!(receiver_fut);

        let (sender_done, receiver_done) = tokio::join!(sender_fut, receiver_fut);
        let sender_done = sender_done.expect("sender flow must succeed");
        let receiver_done = receiver_done.expect("receiver flow must succeed");

        match sender_done.outcome {
            SenderOutcome::Completed { bytes, files } => {
                assert!(bytes > 0, "sender reports positive bytes");
                assert_eq!(files as usize, 3, "sender reports 3 files");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert_eq!(sender_done.phase, SessionPhase::Done);
        assert_eq!(receiver_done.result.files, 3);
        assert_eq!(receiver_done.phase, SessionPhase::Done);
    })
    .await
    .expect("happy-path flow must finish within the flow timeout");

    verify_exported(&output);
}

// ============================================================================
//  Flow 2 — decline
// ============================================================================

#[tokio::test]
async fn e2e_decline_flow() {
    let _relay = ensure_relay().await;
    let (rv_url, _rv_task) = spawn_rendezvous().await;

    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (control_tx, control_rx) = mpsc::unbounded_channel::<Connection>();
    let sender_eng = sender_engine(&sender_dir, control_tx).await;
    let receiver_eng = receiver_engine(&receiver_dir).await;

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_rv = RvClient::new(&rv_url);
    let receiver_rv = RvClient::new(&rv_url);

    timeout(FLOW_TIMEOUT, async {
        let sender_fut = run_sender_flow(sender_eng, control_rx, sender_rv, vec![a, b, c], code_tx);
        tokio::pin!(sender_fut);

        let pair = await_sender_code(&mut sender_fut, &mut code_rx).await;

        let receiver_fut = run_receiver_flow(
            receiver_eng,
            &pair.code,
            None,
            receiver_rv,
            output.clone(),
            ReceiverAction::Decline {
                reason: "not now".to_string(),
            },
        );
        tokio::pin!(receiver_fut);

        let (sender_done, _receiver_done) = tokio::join!(sender_fut, receiver_fut);
        let sender_done = sender_done.expect("sender flow must succeed");

        match sender_done.outcome {
            SenderOutcome::Declined { reason } => {
                assert!(reason.contains("not now"), "decline reason propagated");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    })
    .await
    .expect("decline flow must finish within the flow timeout");
}

// ============================================================================
//  Flow 3 — cancel mid-transfer
// ============================================================================

#[tokio::test]
async fn e2e_cancel_flow() {
    let _relay = ensure_relay().await;
    let (rv_url, _rv_task) = spawn_rendezvous().await;

    let fixture = temp_dir("fixtures");
    let big = big_fixture(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (control_tx, control_rx) = mpsc::unbounded_channel::<Connection>();
    let sender_eng = sender_engine(&sender_dir, control_tx).await;
    let receiver_eng = receiver_engine(&receiver_dir).await;

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_rv = RvClient::new(&rv_url);
    let receiver_rv = RvClient::new(&rv_url);

    timeout(FLOW_TIMEOUT, async {
        let sender_fut = run_sender_flow(sender_eng, control_rx, sender_rv, vec![big], code_tx);
        tokio::pin!(sender_fut);

        let pair = await_sender_code(&mut sender_fut, &mut code_rx).await;

        let receiver_fut =
            run_receiver_cancel_mid_transfer(receiver_eng, &pair.code, receiver_rv, output);
        tokio::pin!(receiver_fut);

        let (sender_done, receiver_done) = tokio::join!(sender_fut, receiver_fut);
        let sender_done = sender_done.expect("sender flow must succeed");
        let receiver_done = receiver_done.expect("receiver flow must succeed");

        assert!(
            matches!(sender_done.outcome, SenderOutcome::Cancelled),
            "sender sees Cancelled, got {:?}",
            sender_done.outcome
        );
        assert_eq!(sender_done.phase, SessionPhase::Cancelled);
        assert_eq!(receiver_done.phase, SessionPhase::Cancelled);
    })
    .await
    .expect("cancel flow must finish within the flow timeout");
}

// ============================================================================
//  Flow 4 — wrong words (MITM-resistance proof)
// ============================================================================

#[tokio::test]
async fn e2e_wrong_words_flow() {
    let _relay = ensure_relay().await;
    let (rv_url, _rv_task) = spawn_rendezvous().await;

    let fixture = temp_dir("fixtures");
    let (a, b, c) = fixture_files(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver");
    let output = temp_dir("output");

    let (control_tx, control_rx) = mpsc::unbounded_channel::<Connection>();
    let sender_eng = sender_engine(&sender_dir, control_tx).await;
    let receiver_eng = receiver_engine(&receiver_dir).await;

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_rv = RvClient::new(&rv_url);
    let receiver_rv = RvClient::new(&rv_url);

    timeout(FLOW_TIMEOUT, async {
        let sender_fut = run_sender_flow(sender_eng, control_rx, sender_rv, vec![a, b, c], code_tx);
        tokio::pin!(sender_fut);

        let pair = await_sender_code(&mut sender_fut, &mut code_rx).await;

        // The receiver knows the nameplate (server-visible) but has a
        // different word triple — the MITM scenario: an attacker who can see
        // the nameplate but not the secret words.
        let (_, words) = WordCode::split(&pair.code).expect("split code");
        let wrong_words = different_words(&words);

        let receiver_fut = run_receiver_flow(
            receiver_eng,
            &pair.code,
            Some(&wrong_words),
            receiver_rv,
            output.clone(),
            ReceiverAction::Accept,
        );
        tokio::pin!(receiver_fut);

        // Both sides verify the other's confirmation token with their own
        // key, so with mismatched words the mismatch surfaces on one side
        // first; the losing side then fails with a stream error when the
        // mismatching peer closes. Assert the invariant: the receiver never
        // reaches Done and at least one side reports ConfirmationMismatch —
        // and nothing hangs.
        let (sender_result, receiver_result) = tokio::join!(sender_fut, receiver_fut);

        assert!(
            receiver_result.is_err(),
            "wrong-words receiver should NOT reach Done, got {receiver_result:?}"
        );
        match (&sender_result, &receiver_result) {
            (_, Err(FlowError::Spake(SpakeError::ConfirmationMismatch)))
            | (Err(FlowError::Spake(SpakeError::ConfirmationMismatch)), _) => {}
            _ => panic!(
                "expected ConfirmationMismatch on one side; \
                 sender: {sender_result:?}, receiver: {receiver_result:?}"
            ),
        }
    })
    .await
    .expect("wrong-words flow must finish within the flow timeout (no hang)");
}

// ============================================================================
//  Flow 5 — resume after interrupt (T10 receive_resumable, e2e)
// ============================================================================

#[tokio::test]
async fn e2e_resume_after_interrupt() {
    let _relay = ensure_relay().await;
    let (rv_url, _rv_task) = spawn_rendezvous().await;

    let fixture = temp_dir("fixtures");
    let big = big_fixture(&fixture);
    let sender_dir = temp_dir("sender");
    let receiver_dir = temp_dir("receiver-resume");
    let output = temp_dir("output");

    let (control_tx, control_rx) = mpsc::unbounded_channel::<Connection>();
    let sender_eng = sender_engine(&sender_dir, control_tx).await;

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let sender_rv = RvClient::new(&rv_url);

    // Spawn the sender flow as a runtime task: it stays alive (its engine
    // keeps serving blobs) for both the interrupted download and the resume
    // below, until we close the control exchange at the end.
    let sender_handle = tokio::task::spawn(run_sender_flow(
        sender_eng,
        control_rx,
        sender_rv,
        vec![big.clone()],
        code_tx,
    ));

    // Wait for the code from the sender.
    let pair = timeout(FLOW_TIMEOUT, code_rx.recv())
        .await
        .expect("timed out waiting for sender code")
        .expect("sender must produce code");
    let (nameplate, words) = WordCode::split(&pair.code).expect("split code");

    let receiver_rv = RvClient::new(&rv_url);
    let ticket_str = receiver_rv.claim(nameplate).await.expect("claim nameplate");
    let ticket = BlobTicket::from_str(&ticket_str).expect("parse ticket");

    let receiver_eng1 = receiver_engine(&receiver_dir).await;

    // Dial the control connection from a dedicated engine: receiver_eng1 is
    // shut down after the interrupted download, which would close any
    // connection dialed from it (including this control stream, still needed
    // for the final result exchange).
    let control_eng = receiver_engine(&temp_dir("control")).await;

    let conn = timeout(
        PAIR_TIMEOUT,
        control_eng
            .endpoint()
            .connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await
    .expect("connect to sender")
    .expect("dial sender");
    let (mut send, mut recv) = conn.open_bi().await.expect("open bidi");

    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await
    .expect("send hello");
    recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver hello")
        .await
        .expect("recv hello");

    spake_receiver_side(&mut send, &mut recv, words.as_bytes())
        .await
        .expect("SPAKE2 pairing");

    let offer = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "receiver offer")
        .await
        .expect("recv offer");
    assert!(matches!(offer, ControlMessage::Offer { .. }));
    send_message(&mut send, &ControlMessage::Accept)
        .await
        .expect("send accept");

    // Keep (send, recv, conn) alive: the sender flow blocks reading the
    // transfer result, which keeps its engine — and the blob-serving router
    // — alive for the interrupted download and the resume below. Dropping
    // the connection here EOFs the sender and kills the engine before the
    // resume can start.

    let options = ReceiveOptions {
        target_dir: output.clone(),
        overwrite: false,
    };

    // Interrupt mid-download at the first progress event.
    let aborted = abort_receive_mid_transfer(&receiver_eng1, &ticket, options.clone(), true)
        .await
        .expect("interrupted receive must end cleanly");
    assert!(
        aborted,
        "receive must be interrupted mid-transfer, not before"
    );
    let _ = receiver_eng1.shutdown().await;

    // A FRESH engine on the same data dir: the FsStore bitfield and the
    // resume record persist the partial state (T10).
    let receiver_eng2 = receiver_engine(&receiver_dir).await;
    let result = receiver_eng2
        .receive_resumable(&ticket, options, &mut |_| {})
        .await
        .expect("resume receive must succeed");
    assert_eq!(result.files, 1, "the single file arrives after resume");
    verify_big(&output);
    let _ = receiver_eng2.shutdown().await;

    // Close the control exchange: report the result, then wait for the
    // sender flow to read it and close the connection itself (its close is
    // the acknowledgement; closing first would race the send buffer).
    send_message(
        &mut send,
        &ControlMessage::Result {
            bytes: result.bytes,
            files: result.files as u32,
        },
    )
    .await
    .expect("send result");
    await_peer_close(&mut recv, "sender to close after result")
        .await
        .expect("sender must close after result");
    drop((send, recv, conn));
    let sender_done = sender_handle
        .await
        .expect("sender task must not panic")
        .expect("sender flow must succeed");
    assert!(
        matches!(sender_done.outcome, SenderOutcome::Completed { .. }),
        "sender sees the completed transfer: {:?}",
        sender_done.outcome
    );

    let _ = control_eng.shutdown().await;
}

// ============================================================================
//  Flow 6 — invalid nameplate claim (rendezvous 400)
// ============================================================================

#[tokio::test]
async fn e2e_invalid_nameplate() {
    let (rv_url, _rv_task) = spawn_rendezvous().await;
    let rv = RvClient::new(&rv_url);

    // A word-bearing claim path (the full "nameplate-word-word-word" code)
    // must be rejected by the server: it routes by the numeric nameplate
    // ONLY and never accepts words in the path.
    let result = rv.claim_str("7-correct-horse-battery").await;

    match result {
        Err(RvError::Http { status, .. }) => {
            assert_eq!(
                status, 400,
                "server must return 400 for a word-bearing claim path"
            );
        }
        other => panic!("expected HTTP 400, got {other:?}"),
    }
}
