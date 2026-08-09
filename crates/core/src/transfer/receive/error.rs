//! Errors from the receive flow (T9). Split out of `receive.rs` to keep both
//! files under the 250 pure-LOC ceiling.

use std::{fmt, path::PathBuf};

use iroh_blobs::get::GetError;

use super::core::CONNECT_TIMEOUT;

/// Errors from the receive flow.
#[derive(Debug)]
pub enum ReceiveError {
    /// Failed to create the target dir.
    TargetDir { path: PathBuf, source: std::io::Error },
    /// Failed to resolve the target dir to an absolute path (the export API
    /// requires absolute targets).
    TargetDirResolve { path: PathBuf, source: std::io::Error },
    /// Failed to read the local store state for the ticket hash.
    LocalState {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Failed to connect to the sender endpoint from the ticket address.
    Connect { source: iroh::endpoint::ConnectError },
    /// The dial to the sender exceeded [`CONNECT_TIMEOUT`].
    ConnectTimeout,
    /// Failed to fetch the collection sizes from the peer.
    Sizes { source: GetError },
    /// The download failed after connecting.
    Download { source: GetError },
    /// The downloaded collection could not be loaded from the store.
    LoadCollection { source: Box<dyn std::error::Error + Send + Sync> },
    /// A collection name cannot be mapped safely to a path under the target
    /// dir (empty / `.` / `..` / backslash components).
    InvalidCollectionName { name: String },
    /// Export of a file failed.
    Export { file: String, source: iroh_blobs::api::Error },
    /// The export stream ended without a result.
    ExportStreamEnded { file: String },
    /// An existing target could not be removed for overwrite.
    RemoveExisting { path: PathBuf, source: std::io::Error },
    /// A transfer record could not be persisted (the resume convenience
    /// state; the transfer itself is unaffected).
    RecordSave { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiveError::TargetDir { path, source } => {
                write!(f, "failed to create target dir {}: {source}", path.display())
            }
            ReceiveError::TargetDirResolve { path, source } => {
                write!(f, "failed to resolve target dir {}: {source}", path.display())
            }
            ReceiveError::LocalState { source } => {
                write!(f, "failed to read local store state: {source}")
            }
            ReceiveError::Connect { source } => {
                write!(f, "failed to connect to the sender: {source}")
            }
            ReceiveError::ConnectTimeout => {
                write!(f, "connect to the sender timed out after {:?}", CONNECT_TIMEOUT)
            }
            ReceiveError::Sizes { source } => write!(f, "failed to fetch collection sizes: {source}"),
            ReceiveError::Download { source } => write!(f, "download failed: {source}"),
            ReceiveError::LoadCollection { source } => {
                write!(f, "failed to load downloaded collection: {source}")
            }
            ReceiveError::InvalidCollectionName { name } => write!(
                f,
                "collection name {name:?} cannot be mapped safely under the target dir"
            ),
            ReceiveError::Export { file, source } => {
                write!(f, "failed to export {file:?}: {source}")
            }
            ReceiveError::ExportStreamEnded { file } => {
                write!(f, "export of {file:?} ended without a result")
            }
            ReceiveError::RemoveExisting { path, source } => {
                write!(f, "failed to remove existing target {}: {source}", path.display())
            }
            ReceiveError::RecordSave { path, source } => {
                write!(f, "failed to save transfer record {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ReceiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReceiveError::TargetDir { source, .. }
            | ReceiveError::TargetDirResolve { source, .. }
            | ReceiveError::RemoveExisting { source, .. }
            | ReceiveError::RecordSave { source, .. } => Some(source),
            ReceiveError::LocalState { source } => Some(source.as_ref()),
            ReceiveError::Connect { source } => Some(source),
            ReceiveError::Sizes { source } | ReceiveError::Download { source } => Some(source),
            ReceiveError::LoadCollection { source } => Some(source.as_ref()),
            ReceiveError::Export { source, .. } => Some(source),
            ReceiveError::InvalidCollectionName { .. } | ReceiveError::ExportStreamEnded { .. } => {
                None
            }
            ReceiveError::ConnectTimeout => None,
        }
    }
}
