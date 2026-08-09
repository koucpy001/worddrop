//! my-croc-core — shared core library.
//!
//! Pairing (SPAKE2 word-code), session state machine, iroh transfer engine,
//! persistent identity, resume records. Used by CLI, GUI (via FRB bridge) and Android.

pub mod identity;
pub mod pairing;
pub mod protocol;
pub mod session;

pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
