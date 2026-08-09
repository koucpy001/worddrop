//! iroh transfer engine (T7): endpoint + persistent blob store + blobs
//! protocol handler. Send-side preparation (T8) walks + imports + builds the
//! collection ticket; receive flow lives in T9; resume records in T10.

pub mod engine;
pub mod receive;
pub mod record;
pub mod send;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod receive_tests;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod send_tests;
