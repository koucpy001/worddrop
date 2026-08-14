//! Session control messages and wire helpers (T5): Hello / Offer / Accept /
//! Decline / Cancel / Result, framed as u32 LE length-prefixed JSON via
//! [`crate::protocol::wire::WireMessage`] (T3, not reimplemented). Send and
//! receive helpers with drift-parity timeouts: 30 s handshake, 120 s idle.

use core::fmt;
use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::wire::{MAX_FRAME_BYTES, WireError, WireMessage};

/// Protocol version carried in [`ControlMessage::Hello`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound for completing the pairing handshake (drift parity: 30 s).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for idle time between control messages (drift parity: 120 s).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Metadata for one file in an [`ControlMessage::Offer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
    /// Content hash (blake3), hex-encoded (T3: hex for fixed-size payloads).
    pub hash: String,
}

/// Control messages exchanged over the session's control stream. Framed with
/// [`WireMessage`] (u32 LE length-prefixed JSON, 16 MiB cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// First message of the handshake; version gates protocol compat.
    Hello { version: u32 },
    /// Sender advertises the transfer it wants to start.
    Offer {
        files: Vec<FileMeta>,
        total_bytes: u64,
    },
    /// Receiver agrees to the offer.
    Accept,
    /// Receiver refuses the offer.
    Decline { reason: String },
    /// Either side aborts.
    Cancel,
    /// Final outcome of a completed transfer. `skipped_bytes` /
    /// `skipped_files` count files the receiver did not re-export because
    /// the target already existed (or an earlier resume already exported
    /// them); the sender reconciles them as delivered so a retransmit of an
    /// already-received collection does not mismatch.
    Result {
        bytes: u64,
        files: u32,
        skipped_bytes: u64,
        skipped_files: u32,
    },
}

impl ControlMessage {
    /// Reject a [`ControlMessage::Hello`] whose version differs from
    /// [`PROTOCOL_VERSION`]; other messages always pass.
    pub fn check_version(&self) -> Result<(), SessionError> {
        if let Self::Hello { version } = self
            && *version != PROTOCOL_VERSION
        {
            return Err(SessionError::VersionMismatch {
                got: *version,
                expected: PROTOCOL_VERSION,
            });
        }
        Ok(())
    }
}

/// Errors from session control message exchange.
#[derive(Debug)]
pub enum SessionError {
    /// Framing/JSON failure (reuses [`WireError`] from T3).
    Wire(WireError),
    /// Underlying stream I/O failure.
    Io(io::Error),
    /// Peer closed the connection mid-message (treated as remote cancel in T11).
    UnexpectedEof,
    /// No message arrived within `limit`.
    Timeout {
        context: &'static str,
        limit: Duration,
    },
    /// Peer's Hello carries a different protocol version.
    VersionMismatch { got: u32, expected: u32 },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(err) => write!(f, "wire error: {err}"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::UnexpectedEof => write!(f, "connection closed by peer mid-message"),
            Self::Timeout { context, limit } => {
                write!(
                    f,
                    "timed out after {}s waiting for {context}",
                    limit.as_secs()
                )
            }
            Self::VersionMismatch { got, expected } => {
                write!(
                    f,
                    "protocol version mismatch: got {got}, expected {expected}"
                )
            }
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wire(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// Send one control message as a framed JSON payload on `writer`.
pub async fn send_message<W>(writer: &mut W, message: &ControlMessage) -> Result<(), SessionError>
where
    W: AsyncWrite + Unpin,
{
    let frame = WireMessage::new(message)
        .encode()
        .map_err(SessionError::Wire)?;
    writer.write_all(&frame).await.map_err(SessionError::Io)?;
    writer.flush().await.map_err(SessionError::Io)?;
    Ok(())
}

/// Receive one control message from `reader`, validating framing, JSON shape
/// and the Hello version. An empty/truncated stream surfaces
/// [`SessionError::UnexpectedEof`].
pub async fn recv_message<R>(reader: &mut R) -> Result<ControlMessage, SessionError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    read_exact_or_eof(reader, &mut prefix).await?;
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(SessionError::Wire(WireError::FrameTooLarge {
            declared,
            max: MAX_FRAME_BYTES,
        }));
    }
    let mut body = vec![0u8; declared];
    read_exact_or_eof(reader, &mut body).await?;
    let mut frame = Vec::with_capacity(4 + declared);
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(&body);
    let message = WireMessage::<ControlMessage>::decode(&frame)
        .map_err(SessionError::Wire)?
        .into_inner();
    message.check_version()?;
    Ok(message)
}

/// Receive one control message bounded by `timeout` (call with
/// [`HANDSHAKE_TIMEOUT`] / [`IDLE_TIMEOUT`] in the session driver).
pub async fn recv_message_timeout<R>(
    reader: &mut R,
    timeout: Duration,
    context: &'static str,
) -> Result<ControlMessage, SessionError>
where
    R: AsyncRead + Unpin,
{
    match tokio::time::timeout(timeout, recv_message(reader)).await {
        Ok(result) => result,
        Err(_) => Err(SessionError::Timeout {
            context,
            limit: timeout,
        }),
    }
}

async fn read_exact_or_eof<R>(reader: &mut R, buffer: &mut [u8]) -> Result<(), SessionError>
where
    R: AsyncRead + Unpin,
{
    reader.read_exact(buffer).await.map(|_| ()).map_err(|err| {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            SessionError::UnexpectedEof
        } else {
            SessionError::Io(err)
        }
    })
}

#[cfg(test)]
mod tests;
