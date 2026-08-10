//! my-croc bridge crate: FRB facade over the my-croc core.
//!
//! T16 scaffold only: RUNTIME.block_on sync wrappers + StreamSink event
//! channel skeleton. T17 wires the real `my-croc-core` API behind these.

pub mod api;
mod frb_generated;
