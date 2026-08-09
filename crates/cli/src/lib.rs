//! my-croc-cli — Linux CLI binary crate.
//!
//! Placeholder crate; send/receive commands land in T13/T14.

pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
