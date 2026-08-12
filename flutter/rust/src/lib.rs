//! worddrop bridge crate: FRB facade over the worddrop core.
//!
//! T16 scaffold only: RUNTIME.block_on sync wrappers + StreamSink event
//! channel skeleton. T17 wires the real `worddrop-core` API behind these.

pub mod api;
mod frb_generated;
