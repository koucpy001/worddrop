//! On-wire framing shared by the pairing handshake (T3) and the session
//! control messages (T5): u32 LE length-prefixed JSON frames, 16 MiB cap.

pub mod wire;
