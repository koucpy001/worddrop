//! iroh transfer engine (T7): endpoint + persistent blob store + blobs
//! protocol handler. Send/receive flows live in T8/T9; resume records in T10.

pub mod engine;

#[cfg(test)]
mod engine_tests;
