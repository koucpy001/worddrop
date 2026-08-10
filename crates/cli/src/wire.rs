//! Pairing control-stream wire helpers (T13), ported from the T11 e2e driver
//! (`crates/core/tests/e2e.rs`), which proves this exact framing interoperates
//! with the T5 control protocol and the T3 handshake format.
//!
//! Both roles live here: the sender-side helpers drive the CLI send command,
//! and the receiver-side halves exist for the mock-pair tests (and are the
//! natural home for T14's receive command to reuse). SECURITY (F1): the
//! SPAKE2 helpers take the secret words directly and never touch the
//! rendezvous — the nameplate is the only thing that ever leaves the client.

use std::{fmt, io, time::Duration};

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use my_croc_core::pairing::handshake::HandshakeMessage;
use my_croc_core::pairing::spake::{SessionKey, SpakeError, SpakeSession};
use my_croc_core::protocol::wire::{WireError, WireMessage, MAX_FRAME_BYTES};
use my_croc_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, IDLE_TIMEOUT, PROTOCOL_VERSION, SessionError,
    recv_message_timeout,
};

/// ALPN of the pairing control stream (mirrors the e2e constant). The
/// iroh-blobs router handler consumes every incoming bidi stream on its own
/// ALPN as a blob request, so control traffic needs its own ALPN.
pub const CONTROL_ALPN: &[u8] = b"my-croc/control";

/// Upper bound for one pairing exchange round (claim, dial, handshake).
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound for the peer to close the control connection after reading
/// our final message (Accept/Decline/Cancel/Result).
pub const ACK_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound for an entire flow (pairing + transfer).
pub const FLOW_TIMEOUT: Duration = Duration::from_secs(180);

/// Hands every accepted control connection to the send flow driver.
#[derive(Debug, Clone)]
pub struct ControlAcceptor {
    tx: mpsc::UnboundedSender<Connection>,
}

impl ControlAcceptor {
    pub fn new(tx: mpsc::UnboundedSender<Connection>) -> Self {
        Self { tx }
    }
}

impl ProtocolHandler for ControlAcceptor {
    fn accept(&self, conn: Connection) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(conn);
            Ok(())
        }
    }
}

/// Errors from the pairing control exchange.
#[derive(Debug)]
pub enum PairError {
    /// Raw stream I/O.
    Io(io::Error),
    /// Control-message framing/exchange failure.
    Control(SessionError),
    /// Handshake framing failure (the SPAKE2/confirm frames).
    Wire(WireError),
    /// SPAKE2 protocol or key-confirmation failure.
    Spake(SpakeError),
    /// Timed out waiting for `what` (no hang).
    Hung(&'static str),
    /// A message of the wrong kind arrived.
    Not(&'static str),
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // User-facing errors are bilingual (中文 + English) per the global
        // copy rule; the English half keeps the historical wording.
        match self {
            Self::Io(err) => write!(f, "IO 错误 / io error: {err}"),
            Self::Control(err) => write!(f, "控制错误 / control error: {err}"),
            Self::Wire(err) => write!(f, "协议错误 / wire error: {err}"),
            Self::Spake(SpakeError::ConfirmationMismatch) => write!(
                f,
                "配对码不匹配（请确认双方输入的配对码一致） / pairing code mismatch (check both sides entered the same code)"
            ),
            Self::Spake(err) => write!(f, "配对失败 / pairing error: {err}"),
            Self::Hung(what) => write!(f, "等待 {what} 超时 / timed out waiting for {what}"),
            Self::Not(kind) => write!(
                f,
                "预期 {kind}，收到其他消息 / expected {kind}, got a different message"
            ),
        }
    }
}

impl std::error::Error for PairError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Control(err) => Some(err),
            Self::Wire(err) => Some(err),
            Self::Spake(err) => Some(err),
            Self::Hung(_) | Self::Not(_) => None,
        }
    }
}

impl From<io::Error> for PairError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<SessionError> for PairError {
    fn from(value: SessionError) -> Self {
        Self::Control(value)
    }
}
impl From<WireError> for PairError {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}
impl From<SpakeError> for PairError {
    fn from(value: SpakeError) -> Self {
        Self::Spake(value)
    }
}

/// Send one handshake frame (u32 LE length + JSON, the T3 wire format).
pub async fn send_handshake<W>(writer: &mut W, message: &HandshakeMessage) -> Result<(), PairError>
where
    W: AsyncWrite + Unpin,
{
    let frame = WireMessage::new(message).encode().map_err(PairError::Wire)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Receive one handshake frame, validating the length prefix before slicing.
pub async fn recv_handshake<R>(reader: &mut R) -> Result<HandshakeMessage, PairError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(PairError::Wire(WireError::FrameTooLarge {
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
        .map_err(PairError::Wire)?
        .into_inner())
}

/// Receive one handshake frame bounded by `limit` (no-hang guarantee).
pub async fn recv_handshake_timeout<R>(
    reader: &mut R,
    limit: Duration,
    what: &'static str,
) -> Result<HandshakeMessage, PairError>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(limit, recv_handshake(reader))
        .await
        .map_err(|_| PairError::Hung(what))?
}

/// Sender role of the SPAKE2 + confirmation round: receive the peer's
/// message, send ours, derive the key, then exchange and verify the
/// confirmation tokens. A wrong-words peer fails with
/// [`SpakeError::ConfirmationMismatch`] — the nameplate/words split
/// MITM-resistance proof.
pub async fn spake_sender_side<W, R>(
    send: &mut W,
    recv: &mut R,
    words: &[u8],
) -> Result<SessionKey, PairError>
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
    let inbound_token =
        recv_handshake_timeout(recv, PAIR_TIMEOUT, "sender confirm token").await?.into_confirm()?;
    send_handshake(send, &HandshakeMessage::confirm(key.confirm_token())).await?;
    key.verify_confirm(&inbound_token)?;
    Ok(key)
}

/// Receiver role of the SPAKE2 + confirmation round: send ours first, then
/// receive the peer's, derive the key, exchange and verify tokens.
pub async fn spake_receiver_side<W, R>(
    send: &mut W,
    recv: &mut R,
    words: &[u8],
) -> Result<SessionKey, PairError>
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
    let inbound_token =
        recv_handshake_timeout(recv, PAIR_TIMEOUT, "receiver confirm token").await?.into_confirm()?;
    key.verify_confirm(&inbound_token)?;
    Ok(key)
}

/// Receive the peer's Hello, returning a Hello of our own (both sides
/// version-gate via `recv_message`'s `check_version`).
pub async fn recv_hello<R>(recv: &mut R) -> Result<ControlMessage, PairError>
where
    R: AsyncRead + Unpin,
{
    let message = recv_message_timeout(recv, HANDSHAKE_TIMEOUT, "sender hello").await?;
    match message {
        ControlMessage::Hello { .. } => {
            Ok(ControlMessage::Hello { version: PROTOCOL_VERSION })
        }
        _other => Err(PairError::Not("hello")),
    }
}

/// Receive one control message within the idle timeout (120 s).
pub async fn recv_message_idle<R>(
    recv: &mut R,
    what: &'static str,
) -> Result<ControlMessage, PairError>
where
    R: AsyncRead + Unpin,
{
    recv_message_timeout(recv, IDLE_TIMEOUT, what).await.map_err(PairError::Control)
}

/// Wait for the peer to close the control connection after it has read our
/// final message (Accept/Decline/Cancel/Result). The sender owns the close —
/// its close frame is the acknowledgement that it consumed our message.
pub async fn await_peer_close<R>(recv: &mut R, what: &'static str) -> Result<(), PairError>
where
    R: AsyncRead + Unpin,
{
    let mut sink = [0u8; 256];
    loop {
        match tokio::time::timeout(ACK_TIMEOUT, recv.read(&mut sink)).await {
            Err(_) => return Err(PairError::Hung(what)),
            Ok(Err(_)) => return Ok(()),
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests;
