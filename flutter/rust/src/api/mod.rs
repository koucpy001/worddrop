//! FRB-exposed API surface (rust_input: crate::api).
//!
//! Mirror of drift's flutter/rust/src/api/mod.rs: a lazily-initialized
//! multi-thread tokio runtime shared by every sync wrapper via
//! `RUNTIME.block_on`. FRB functions are plain synchronous `pub fn`s.

use std::sync::LazyLock;

use tokio::runtime::Runtime;

pub mod config;
pub mod events;
pub mod hello;
pub mod session;
pub(crate) mod flows;

pub(crate) static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
});

/// Serializes env-mutating test code (edition 2024 marks `set_var`/`remove_var`
/// unsafe); shared by every bridge test module that touches env vars.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
