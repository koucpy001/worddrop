//! Wire framing for my-croc control messages (T3): u32 LE length-prefixed
//! JSON frames with a 16 MiB cap (mirrors drift's `protocol/wire.rs`).
//!
//! Frame layout: `[u32 LE payload length][payload bytes]` where the payload
//! is a JSON-serialized message. Decoding rejects frames that declare a
//! payload beyond [`MAX_FRAME_BYTES`], are truncated, or carry trailing
//! bytes, so a stream can be decoded frame-by-frame without ambiguity.

use core::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Upper bound for a single frame's JSON payload (16 MiB, drift parity).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// A length-prefixed JSON frame. `encode` produces the wire bytes,
/// `decode` parses them back (single-use framing helper, no buffering).
#[derive(Debug)]
pub struct WireMessage<T> {
    inner: T,
}

impl<T> WireMessage<T> {
    /// Wrap a message for encoding.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the decoded message.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

/// Errors from framing a message.
#[derive(Debug)]
pub enum WireError {
    /// Frame is shorter than the 4-byte length prefix.
    MissingLengthPrefix,
    /// Declared payload length exceeds the cap.
    FrameTooLarge { declared: usize, max: usize },
    /// Frame length does not match the declared payload length.
    LengthMismatch { declared: usize, actual: usize },
    /// Failed to serialize the message body as JSON.
    Serialize(serde_json::Error),
    /// Failed to deserialize the message body as JSON.
    Deserialize(serde_json::Error),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLengthPrefix => write!(f, "frame shorter than the 4-byte length prefix"),
            Self::FrameTooLarge { declared, max } => {
                write!(f, "declared payload length {declared} exceeds cap {max}")
            }
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "frame length {actual} does not match declared payload length {declared}"
            ),
            Self::Serialize(err) => write!(f, "failed to serialize message body: {err}"),
            Self::Deserialize(err) => write!(f, "failed to deserialize message body: {err}"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(err) | Self::Deserialize(err) => Some(err),
            _ => None,
        }
    }
}

impl<T: Serialize> WireMessage<T> {
    /// Encode `self` into a `[u32 LE length][json body]` frame. Rejects
    /// payloads larger than [`MAX_FRAME_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let body = serde_json::to_vec(&self.inner).map_err(WireError::Serialize)?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(WireError::FrameTooLarge {
                declared: body.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }
}

impl<T: DeserializeOwned> WireMessage<T> {
    /// Decode one frame from `frame` (exactly one; trailing bytes are an
    /// error). A declared length beyond [`MAX_FRAME_BYTES`] is rejected
    /// before any body is read.
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        let prefix: [u8; 4] = frame
            .get(..4)
            .ok_or(WireError::MissingLengthPrefix)?
            .try_into()
            .map_err(|_| WireError::MissingLengthPrefix)?;
        let declared = u32::from_le_bytes(prefix) as usize;
        if declared > MAX_FRAME_BYTES {
            return Err(WireError::FrameTooLarge {
                declared,
                max: MAX_FRAME_BYTES,
            });
        }
        let actual = frame.len() - 4;
        if actual != declared {
            return Err(WireError::LengthMismatch { declared, actual });
        }
        let inner = serde_json::from_slice(&frame[4..]).map_err(WireError::Deserialize)?;
        Ok(Self { inner })
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::{WireError, WireMessage, MAX_FRAME_BYTES};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestMessage {
        id: u32,
        body: String,
    }

    fn sample() -> TestMessage {
        TestMessage {
            id: 7,
            body: "correct-horse-battery".to_string(),
        }
    }

    fn declared_len(frame: &[u8]) -> u32 {
        u32::from_le_bytes(frame[..4].try_into().expect("frame has a 4-byte prefix"))
    }

    #[test]
    fn encode_decode_roundtrip() {
        let frame = WireMessage::new(sample()).encode().expect("frame encodes");
        assert_eq!(declared_len(&frame) as usize, frame.len() - 4);
        let decoded = WireMessage::<TestMessage>::decode(&frame)
            .expect("frame decodes")
            .into_inner();
        assert_eq!(decoded, sample());
    }

    #[test]
    fn decode_rejects_declared_length_over_cap() {
        let frame = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
        let err = WireMessage::<TestMessage>::decode(&frame).unwrap_err();
        assert!(matches!(
            err,
            WireError::FrameTooLarge { declared, max }
                if declared == MAX_FRAME_BYTES + 1 && max == MAX_FRAME_BYTES
        ));
    }

    #[test]
    fn decode_rejects_short_frame() {
        let err = WireMessage::<TestMessage>::decode(&[0u8; 2]).unwrap_err();
        assert!(matches!(err, WireError::MissingLengthPrefix));
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        let mut frame = 10u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"abc");
        let err = WireMessage::<TestMessage>::decode(&frame).unwrap_err();
        assert!(matches!(
            err,
            WireError::LengthMismatch {
                declared: 10,
                actual: 3
            }
        ));
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut frame = WireMessage::new(sample()).encode().expect("frame encodes");
        frame.push(0);
        let err = WireMessage::<TestMessage>::decode(&frame).unwrap_err();
        assert!(matches!(
            err,
            WireError::LengthMismatch {
                declared: _,
                actual: _
            }
        ));
    }

    #[test]
    fn decode_rejects_garbage_json() {
        let mut frame = 5u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"hello");
        let err = WireMessage::<TestMessage>::decode(&frame).unwrap_err();
        assert!(matches!(err, WireError::Deserialize(_)));
    }

    #[test]
    fn encode_rejects_payload_over_cap() {
        #[derive(Serialize, Deserialize)]
        struct BigPayload {
            data: Vec<u8>,
        }
        let big = BigPayload {
            data: vec![0u8; MAX_FRAME_BYTES + 1],
        };
        let err = WireMessage::new(big).encode().unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
    }
}
