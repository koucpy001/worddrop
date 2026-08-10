//! Receive-side flow (T9): connect to the peer via the ticket address,
//! download the collection into the persistent store, stream progress
//! events, and export the collection into the target dir with a conflict
//! policy (skip existing targets unless `overwrite` is set).
//!
//! T10 adds the resumable entry point [`TransferEngine::receive_resumable`]:
//! the same flow, but a [`TransferRecord`] is persisted at
//! `<data_dir>/transfers/<hash>.json` (atomic write) so a crash mid-transfer
//! can be resumed — the FsStore bitfield skips already-stored chunks, the
//! record provides the UI offset and remembers exported files, and it is
//! deleted once the receive succeeds. The shared core lives in `core.rs`.

use std::path::{Path, PathBuf};

use iroh_blobs::ticket::BlobTicket;
use tracing::{info, warn};

use super::{
    engine::TransferEngine,
    record::{RecordStore, TransferRecord},
};

mod core;
mod error;

/// Progress events emitted while receiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveProgress {
    /// Dialing the sender via the ticket address.
    Connecting,
    /// Downloading payload bytes: `received` of `total` (received is clamped
    /// to total).
    Downloading { received: u64, total: u64 },
    /// Exporting a file from the store into the target dir.
    Exporting { file: String },
    /// The transfer finished: exported bytes and file count.
    Done { bytes: u64, files: usize },
    /// The transfer failed; the `receive` call returns `Err` after this.
    Error,
}

/// Options for a receive.
#[derive(Debug, Clone)]
pub struct ReceiveOptions {
    /// Directory the received files are exported into.
    pub target_dir: PathBuf,
    /// Replace an existing file at the target path; without this the file is
    /// skipped and recorded in [`TransferResult::skipped`].
    pub overwrite: bool,
}

/// The result of a completed receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResult {
    /// Payload bytes exported (sum of the exported files' sizes).
    pub bytes: u64,
    /// Number of files exported.
    pub files: usize,
    /// Collection names skipped because the target existed and `overwrite`
    /// was false.
    pub skipped: Vec<String>,
}

/// Errors from the receive flow.
pub use error::ReceiveError;

/// Map a collection name to a target path under `root`, validating every
/// component: empty, `.`, `..` and backslash-containing parts are rejected so
/// a malicious collection cannot escape the target dir.
pub(crate) fn export_target(root: &Path, name: &str) -> Result<PathBuf, ReceiveError> {
    let mut target = root.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(ReceiveError::InvalidCollectionName {
                name: name.to_string(),
            });
        }
        target.push(part);
    }
    Ok(target)
}

impl TransferEngine {
    /// Receive the collection referenced by `ticket` from the ticket's peer
    /// address: connect, download into the persistent store (skipping chunks
    /// already present — the T10 resume path), then export every file into
    /// `options.target_dir`.
    ///
    /// Conflict policy: an existing target file is skipped (recorded in
    /// [`TransferResult::skipped`]) unless `options.overwrite` is set, in
    /// which case it is replaced. A directory at the target path is an error
    /// even with overwrite.
    pub async fn receive(
        &self,
        ticket: &BlobTicket,
        options: ReceiveOptions,
        progress: &mut dyn FnMut(ReceiveProgress),
    ) -> Result<TransferResult, ReceiveError> {
        self.receive_impl(ticket, options, progress, None).await
    }

    /// Receive like [`receive`](Self::receive), with a persistent resume
    /// record (T10): the record at `<data_dir>/transfers/<hash>.json` is
    /// loaded or created before the download, updated with progress and
    /// exported files as the transfer runs, and deleted on success.
    ///
    /// Resume semantics: a fresh engine on the same data dir re-runs the
    /// download, but the FsStore bitfield skips already-stored chunks (only
    /// missing bytes cross the wire — the T7 spike), progress continues from
    /// the record's `bytes_received`, and files the record marks as exported
    /// are not re-exported. A missing, corrupt, or mismatched record is
    /// treated as a fresh receive: a full download is fine.
    pub async fn receive_resumable(
        &self,
        ticket: &BlobTicket,
        options: ReceiveOptions,
        progress: &mut dyn FnMut(ReceiveProgress),
    ) -> Result<TransferResult, ReceiveError> {
        let hash = ticket.hash();
        let records = RecordStore::new(self.data_dir());
        let mut record = match records.load(&hash, &options.target_dir).await {
            Some(record) => {
                info!(
                    hash = %record.collection_hash,
                    bytes = record.bytes_received,
                    "resuming transfer from record"
                );
                record
            }
            None => TransferRecord::new(hash, &options.target_dir),
        };
        let path = records.path(&hash);
        records
            .save(&record)
            .await
            .map_err(|source| ReceiveError::RecordSave { path, source })?;
        let result = self
            .receive_impl(ticket, options, progress, Some(&mut record))
            .await?;
        // Record deleted after success; a failed delete only leaves a stale
        // record that the next receive treats as done and removes.
        if let Err(source) = records.delete(&hash).await {
            warn!(hash = %hash, error = %source, "failed to delete transfer record after success");
        }
        Ok(result)
    }
}
