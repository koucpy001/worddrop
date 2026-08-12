//! Pairing handshake messages for the wire format (T3).
//!
//! Binary fields are hex-encoded (magic-wormhole's `bytes_to_hexstr` wire
//! style) so the JSON stays human-readable and debuggable. Constructors and
//! accessors enforce the fixed field lengths.

use crate::pairing::spake::{CONFIRM_TOKEN_LEN, SPAKE_MSG_LEN, SpakeError};

/// Pairing handshake messages, exchanged over the wire format
/// ([`crate::protocol::wire::WireMessage`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandshakeMessage {
    /// First round: the 33-byte SPAKE2 symmetric message, hex-encoded.
    Spake { msg: String },
    /// Second round: the 16-byte key-confirmation token, hex-encoded.
    Confirm { token: String },
}

impl HandshakeMessage {
    /// Build a first-round message from the raw 33-byte SPAKE2 bytes.
    pub fn spake(msg: &[u8]) -> Result<Self, SpakeError> {
        if msg.len() != SPAKE_MSG_LEN {
            return Err(SpakeError::BadHandshakeLength {
                kind: "spake",
                expected: SPAKE_MSG_LEN,
                actual: msg.len(),
            });
        }
        Ok(Self::Spake {
            msg: hex_encode(msg),
        })
    }

    /// Build a second-round message from the raw 16-byte token.
    pub fn confirm(token: [u8; CONFIRM_TOKEN_LEN]) -> Self {
        Self::Confirm {
            token: hex_encode(&token),
        }
    }

    /// Recover the raw 33-byte SPAKE2 bytes if this is a first-round message.
    pub fn into_pake(self) -> Result<[u8; SPAKE_MSG_LEN], SpakeError> {
        match self {
            Self::Spake { msg } => hex_decode_array(&msg, "spake"),
            Self::Confirm { .. } => Err(SpakeError::WrongHandshakeKind),
        }
    }

    /// Recover the raw 16-byte token if this is a second-round message.
    pub fn into_confirm(self) -> Result<[u8; CONFIRM_TOKEN_LEN], SpakeError> {
        match self {
            Self::Confirm { token } => hex_decode_array(&token, "confirm"),
            Self::Spake { .. } => Err(SpakeError::WrongHandshakeKind),
        }
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, SpakeError> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(SpakeError::InvalidHex(hex.to_string()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_val(pair[0]).ok_or_else(|| SpakeError::InvalidHex(hex.to_string()))?;
        let lo = hex_val(pair[1]).ok_or_else(|| SpakeError::InvalidHex(hex.to_string()))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_decode_array<const N: usize>(hex: &str, kind: &'static str) -> Result<[u8; N], SpakeError> {
    let bytes = hex_decode(hex)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| SpakeError::BadHandshakeLength {
            kind,
            expected: N,
            actual: bytes.len(),
        })
}

#[cfg(test)]
mod tests {
    use super::super::handshake::HandshakeMessage;
    use super::super::spake::{SPAKE_MSG_LEN, SpakeError};

    #[test]
    fn handshake_rejects_wrong_length_spake() {
        let err = HandshakeMessage::spake(&[0u8; 10]).unwrap_err();
        assert!(matches!(
            err,
            SpakeError::BadHandshakeLength {
                kind: "spake",
                expected: SPAKE_MSG_LEN,
                actual: 10
            }
        ));
    }

    #[test]
    fn handshake_rejects_invalid_hex() {
        let msg = HandshakeMessage::Spake {
            msg: "zz".to_string(),
        };
        let err = msg.into_pake().unwrap_err();
        assert!(matches!(err, SpakeError::InvalidHex(_)));
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = b"\x00\x01\xab\xffworddrop";
        let encoded = super::hex_encode(bytes);
        assert_eq!(encoded, "0001abff776f726464726f70");
        assert_eq!(super::hex_decode(&encoded).unwrap(), bytes);
    }
}
