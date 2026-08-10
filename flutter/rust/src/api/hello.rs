//! Placeholder bridge functions (T16 skeleton).
//!
//! `hello` proves the whole FRB chain end to end: Dart -> codegen -> cdylib
//! -> sync wrapper -> tokio RUNTIME.block_on. T17 replaces these with the
//! real session/transfer API surface.

use crate::api::RUNTIME;

/// Placeholder sync wrapper: demonstrates the drift `RUNTIME.block_on`
/// pattern. Returns a greeting so a Dart smoke test can assert on it.
pub fn hello(name: String) -> String {
    RUNTIME.block_on(async move { format!("Hello, {name}! This is my-croc's Rust bridge.") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_returns_greeting() {
        let greeting = hello("world".to_owned());
        assert!(greeting.starts_with("Hello, world!"), "got: {greeting}");
        assert!(greeting.contains("my-croc"));
    }
}
