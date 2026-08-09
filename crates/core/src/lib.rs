//! my-croc-core — shared core library.
//!
//! Placeholder crate; pairing / session / transfer / identity modules land in
//! later todos (T2-T10).

pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
