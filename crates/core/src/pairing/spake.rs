//! SPAKE2 symmetric pairing session (T3).
//!
//! Both parties run the identical `Spake2::<Ed25519Group>::start_symmetric`
//! with the secret word portion of the pairing code as the password and a
//! fixed application identity (`my-croc/v1`, magic-wormhole appid-style).
//! The numeric nameplate is deliberately NOT part of the PAKE input: it is
//! server-visible and must never influence the derived key (Oracle F1).
//!
//! Protocol (2 RTTs, mirrors magic-wormhole's SPAKE2_Symmetric + HKDF phase
//! keys):
//! 1. each side sends its 33-byte SPAKE2 message (`crate::pairing::handshake::HandshakeMessage::Spake`)
//! 2. each side derives the shared 32-byte session key via `finish`
//! 3. each side sends an HKDF-derived 16-byte confirmation token
//!    (`crate::pairing::handshake::HandshakeMessage::Confirm`) and verifies the peer's token — this
//!    catches a mismatched password before any data flows.
//!
//! The session key authenticates the pairing only; payload transport and
//! data encryption are handled by iroh QUIC (T7+).

use core::fmt;

use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// SPAKE2 symmetric identity shared by all my-croc peers. Binds the derived
/// key to this application so keys from unrelated services cannot be swapped
/// in (SPAKE2 identifier-string role, magic-wormhole appid equivalent).
pub const APP_IDENTIFIER: &[u8] = b"my-croc/v1";

/// HKDF info string for the key-confirmation token.
pub const CONFIRM_INFO: &[u8] = b"my-croc/confirm";

/// Length of the Ed25519Group SPAKE2 message (1 side byte + 32-byte point).
pub const SPAKE_MSG_LEN: usize = 33;

/// Length of the key-confirmation token.
pub const CONFIRM_TOKEN_LEN: usize = 16;

/// Length of the derived session key (SHA-256 transcript hash).
const SESSION_KEY_LEN: usize = 32;

/// Errors from the SPAKE2 pairing session.
#[derive(Debug)]
pub enum SpakeError {
    /// The SPAKE2 exchange rejected the peer's message (wrong length, wrong
    /// side marker, or an invalid curve point).
    Protocol(spake2::Error),
    /// The derived key had an unexpected length (Ed25519Group yields 32).
    KeyLengthMismatch { expected: usize, actual: usize },
    /// The peer's key-confirmation token did not match ours — the two sides
    /// did not agree on the same secret words.
    ConfirmationMismatch,
    /// A handshake message field had the wrong length.
    BadHandshakeLength {
        kind: &'static str,
        expected: usize,
        actual: usize,
    },
    /// A handshake message of the wrong round was supplied (spake vs confirm).
    WrongHandshakeKind,
    /// A handshake field was not valid hex.
    InvalidHex(String),
}

impl From<spake2::Error> for SpakeError {
    fn from(err: spake2::Error) -> Self {
        Self::Protocol(err)
    }
}

impl fmt::Display for SpakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(err) => write!(f, "spake2 protocol error: {err}"),
            Self::KeyLengthMismatch { expected, actual } => {
                write!(f, "derived key length {actual}, expected {expected}")
            }
            Self::ConfirmationMismatch => write!(f, "key-confirmation token mismatch"),
            Self::BadHandshakeLength {
                kind,
                expected,
                actual,
            } => write!(f, "{kind} message length {actual}, expected {expected}"),
            Self::WrongHandshakeKind => write!(
                f,
                "handshake message of the wrong round (expected spake, got confirm or vice versa)"
            ),
            Self::InvalidHex(value) => write!(f, "invalid hex string: {value:?}"),
        }
    }
}

impl std::error::Error for SpakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(err) => Some(err),
            _ => None,
        }
    }
}

/// A single-use SPAKE2 symmetric session. Created by [`SpakeSession::start`],
/// consumed by [`SpakeSession::finish`].
pub struct SpakeSession {
    inner: Spake2<Ed25519Group>,
}

impl SpakeSession {
    /// Start a symmetric SPAKE2 session and produce the outbound 33-byte
    /// message. `words` is the secret word portion of the pairing code ONLY —
    /// the nameplate must never be fed into the PAKE (it is server-visible).
    pub fn start(words: &[u8]) -> (Self, Vec<u8>) {
        let (inner, message) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(words),
            &Identity::new(APP_IDENTIFIER),
        );
        (Self { inner }, message)
    }

    /// Consume the peer's message and derive the shared session key.
    ///
    /// A corrupt, truncated, or wrong-role message yields
    /// [`SpakeError::Protocol`]. Note that a valid message from a peer with
    /// different words still completes with a *different* key — the caller
    /// must run the key-confirmation round to detect that case.
    pub fn finish(self, inbound: &[u8]) -> Result<SessionKey, SpakeError> {
        let key = self.inner.finish(inbound)?;
        SessionKey::from_raw(key)
    }
}

/// The 32-byte shared session key, plus the key-confirmation round.
/// ZeroizeOnDrop wipes the key material when the value is dropped (same
/// hygiene as iroh's `SecretKey`).
#[derive(Clone, PartialEq, Eq, zeroize::ZeroizeOnDrop)]
pub struct SessionKey([u8; SESSION_KEY_LEN]);

impl SessionKey {
    fn from_raw(key: Vec<u8>) -> Result<Self, SpakeError> {
        let bytes: [u8; SESSION_KEY_LEN] =
            key.try_into()
                .map_err(|key: Vec<u8>| SpakeError::KeyLengthMismatch {
                    expected: SESSION_KEY_LEN,
                    actual: key.len(),
                })?;
        Ok(Self(bytes))
    }

    /// Raw session key bytes (for downstream HKDF derivation, e.g. the
    /// version/data phase keys in T5).
    pub fn as_bytes(&self) -> &[u8; SESSION_KEY_LEN] {
        &self.0
    }

    /// Derive the 16-byte key-confirmation token:
    /// `HKDF-SHA256(key, info = "my-croc/confirm")`.
    pub fn confirm_token(&self) -> [u8; CONFIRM_TOKEN_LEN] {
        let hkdf = Hkdf::<Sha256>::new(None, &self.0);
        let mut token = [0u8; CONFIRM_TOKEN_LEN];
        hkdf.expand(CONFIRM_INFO, &mut token)
            .expect("HKDF expand fails only past 255*32 output bytes; 16 is far below");
        token
    }

    /// Verify the peer's key-confirmation token in constant time. Returns
    /// [`SpakeError::ConfirmationMismatch`] on any difference (including a
    /// wrong token length).
    pub fn verify_confirm(&self, token: &[u8]) -> Result<(), SpakeError> {
        if constant_time_eq(&self.confirm_token(), token) {
            Ok(())
        } else {
            Err(SpakeError::ConfirmationMismatch)
        }
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak key material through debug formatting/logs.
        f.write_str("SessionKey([REDACTED])")
    }
}

/// Constant-time equality for secret comparison (no early exit on mismatch).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::super::handshake::HandshakeMessage;
    use super::super::spake::{
        CONFIRM_TOKEN_LEN, SPAKE_MSG_LEN, SessionKey, SpakeError, SpakeSession,
    };
    use crate::protocol::wire::WireMessage;

    fn pair_sessions(words_a: &[u8], words_b: &[u8]) -> (SessionKey, SessionKey) {
        let (a, msg_a) = SpakeSession::start(words_a);
        let (b, msg_b) = SpakeSession::start(words_b);
        let key_a = a.finish(&msg_b).expect("a finishes the exchange");
        let key_b = b.finish(&msg_a).expect("b finishes the exchange");
        (key_a, key_b)
    }

    #[test]
    fn outbound_message_is_33_bytes() {
        let (_, msg) = SpakeSession::start(b"correct-horse-battery");
        assert_eq!(msg.len(), SPAKE_MSG_LEN);
    }

    #[test]
    fn same_words_derive_identical_key() {
        let words = b"correct-horse-battery";
        let (key_a, key_b) = pair_sessions(words, words);
        assert_eq!(key_a, key_b);
        assert_eq!(key_a.confirm_token(), key_b.confirm_token());
        key_a
            .verify_confirm(&key_b.confirm_token())
            .expect("a accepts b's confirmation token");
        key_b
            .verify_confirm(&key_a.confirm_token())
            .expect("b accepts a's confirmation token");
    }

    #[test]
    fn different_words_derive_different_keys() {
        let (key_a, key_b) = pair_sessions(b"correct-horse-battery", b"wrong-horse-battery");
        assert_ne!(key_a, key_b);
        assert_ne!(key_a.confirm_token(), key_b.confirm_token());
        let err = key_a.verify_confirm(&key_b.confirm_token()).unwrap_err();
        assert!(matches!(err, SpakeError::ConfirmationMismatch));
        let err = key_b.verify_confirm(&key_a.confirm_token()).unwrap_err();
        assert!(matches!(err, SpakeError::ConfirmationMismatch));
    }

    #[test]
    fn tampered_message_wrong_length_is_rejected() {
        let (session, _) = SpakeSession::start(b"correct-horse-battery");
        let err = session.finish(&[0u8; 10]).unwrap_err();
        assert!(matches!(
            err,
            SpakeError::Protocol(spake2::Error::WrongLength)
        ));
    }

    #[test]
    fn tampered_message_bad_side_is_rejected() {
        let (session, msg) = SpakeSession::start(b"correct-horse-battery");
        let mut tampered = msg;
        tampered[0] = 0x41; // 'A' — symmetric mode expects the 'S' side marker
        let err = session.finish(&tampered).unwrap_err();
        assert!(matches!(err, SpakeError::Protocol(spake2::Error::BadSide)));
    }

    #[test]
    fn tampered_message_corrupt_point_is_rejected() {
        let (session, msg) = SpakeSession::start(b"correct-horse-battery");
        let mut tampered = msg;
        // Overwrite the whole point with a fixed encoding (y = 2) whose
        // x-coordinate is a non-residue mod p: rejected on every run.
        tampered[1..].fill(0);
        tampered[1] = 2;
        let err = session.finish(&tampered).unwrap_err();
        assert!(matches!(
            err,
            SpakeError::Protocol(spake2::Error::CorruptMessage)
        ));
    }

    #[test]
    fn confirmation_token_mismatch_is_rejected() {
        let (key_a, _) = pair_sessions(b"correct-horse-battery", b"correct-horse-battery");
        let wrong = [0u8; CONFIRM_TOKEN_LEN];
        let err = key_a.verify_confirm(&wrong).unwrap_err();
        assert!(matches!(err, SpakeError::ConfirmationMismatch));
    }

    #[test]
    fn handshake_completes_over_wire_frames() {
        let words = b"correct-horse-battery";
        let (a, msg_a) = SpakeSession::start(words);
        let (b, msg_b) = SpakeSession::start(words);

        // Round 1: framed SPAKE2 messages.
        let frame_a = WireMessage::new(HandshakeMessage::spake(&msg_a).expect("33-byte pake msg"))
            .encode()
            .expect("a's pake frame encodes");
        let frame_b = WireMessage::new(HandshakeMessage::spake(&msg_b).expect("33-byte pake msg"))
            .encode()
            .expect("b's pake frame encodes");
        let recv_a = WireMessage::<HandshakeMessage>::decode(&frame_b)
            .expect("a decodes b's frame")
            .into_inner()
            .into_pake()
            .expect("a recovers b's pake bytes");
        let recv_b = WireMessage::<HandshakeMessage>::decode(&frame_a)
            .expect("b decodes a's frame")
            .into_inner()
            .into_pake()
            .expect("b recovers a's pake bytes");

        let key_a = a.finish(&recv_a).expect("a derives the session key");
        let key_b = b.finish(&recv_b).expect("b derives the session key");
        assert_eq!(key_a, key_b);

        // Round 2: framed key-confirmation tokens.
        let frame_a = WireMessage::new(HandshakeMessage::confirm(key_a.confirm_token()))
            .encode()
            .expect("a's token frame encodes");
        let frame_b = WireMessage::new(HandshakeMessage::confirm(key_b.confirm_token()))
            .encode()
            .expect("b's token frame encodes");
        let token_from_b = WireMessage::<HandshakeMessage>::decode(&frame_b)
            .expect("a decodes b's token")
            .into_inner()
            .into_confirm()
            .expect("a recovers b's token");
        let token_from_a = WireMessage::<HandshakeMessage>::decode(&frame_a)
            .expect("b decodes a's token")
            .into_inner()
            .into_confirm()
            .expect("b recovers a's token");
        key_a
            .verify_confirm(&token_from_b)
            .expect("a accepts b's token");
        key_b
            .verify_confirm(&token_from_a)
            .expect("b accepts a's token");
    }
}
