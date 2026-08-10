//! FRB-exposed API surface (rust_input: crate::api).
//!
//! Mirror of drift's flutter/rust/src/api/mod.rs: a lazily-initialized
//! multi-thread tokio runtime shared by every sync wrapper via
//! `RUNTIME.block_on`. FRB functions are plain synchronous `pub fn`s.

use std::sync::LazyLock;

use tokio::runtime::Runtime;

pub mod events;
pub mod hello;

pub(crate) static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
});
