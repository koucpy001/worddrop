//! Receive-side flow (T9): connect to the peer via the ticket address,
//! download the collection into the persistent store, stream progress
//! events, and export the collection into the target dir with a conflict
//! policy (skip existing targets unless `overwrite` is set).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use iroh_blobs::{
    api::{
        blobs::{ExportMode, ExportOptions, ExportProgressItem},
        remote::GetProgressItem,
    },
    format::collection::Collection,
    get::request::get_hash_seq_and_sizes,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use tracing::warn;

use super::engine::TransferEngine;

/// Upper bound for the collection blob (the hash seq root) when fetching
/// sizes from the peer: a larger root is rejected as a bad request (sendme
/// uses the same 32 MiB cap).
const MAX_HASH_SEQ_SIZE: u64 = 1024 * 1024 * 32;

/// Budget for the QUIC dial: noq has no handshake timeout (dead UDP peers
/// are retried forever), so the receive flow bounds the connect itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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

mod error;

/// Map a collection name to a target path under `root`, validating every
/// component: empty, `.`, `..` and backslash-containing parts are rejected so
/// a malicious collection cannot escape the target dir.
pub(crate) fn export_target(root: &Path, name: &str) -> Result<PathBuf, ReceiveError> {
    let mut target = root.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(ReceiveError::InvalidCollectionName { name: name.to_string() });
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
        tokio::fs::create_dir_all(&options.target_dir)
            .await
            .map_err(|source| ReceiveError::TargetDir {
                path: options.target_dir.clone(),
                source,
            })?;
        // The export API rejects relative targets (store/fs.rs export_path_impl).
        let root = std::path::absolute(&options.target_dir)
            .map_err(|source| ReceiveError::TargetDirResolve {
                path: options.target_dir.clone(),
                source,
            })?;

        let hash_and_format = ticket.hash_and_format();
        let local = self
            .store()
            .remote()
            .local(hash_and_format)
            .await
            .map_err(|source| ReceiveError::LocalState { source: Box::new(source) })?;

        if !local.is_complete() {
            progress(ReceiveProgress::Connecting);
            let connection = match tokio::time::timeout(
                CONNECT_TIMEOUT,
                self.endpoint().connect(ticket.addr().clone(), iroh_blobs::ALPN),
            )
            .await
            {
                Ok(Ok(connection)) => connection,
                Ok(Err(source)) => {
                    progress(ReceiveProgress::Error);
                    return Err(ReceiveError::Connect { source });
                }
                Err(_) => {
                    progress(ReceiveProgress::Error);
                    return Err(ReceiveError::ConnectTimeout);
                }
            };
            let (_hash_seq, sizes) = get_hash_seq_and_sizes(
                &connection,
                &hash_and_format.hash,
                MAX_HASH_SEQ_SIZE,
                None,
            )
            .await
            .map_err(|source| {
                progress(ReceiveProgress::Error);
                ReceiveError::Sizes { source }
            })?;
            // sizes[0] is the collection metadata blob: the iroh collection
            // format stores the meta blob as the first hash seq child, so the
            // payload total skips it (sendme's total_files = len - 1 quirk).
            let total = sizes.iter().skip(1).copied().sum::<u64>();
            let local_bytes = local.local_bytes();
            let mut stream = self
                .store()
                .remote()
                .execute_get(connection, local.missing())
                .stream();
            while let Some(item) = stream.next().await {
                match item {
                    GetProgressItem::Progress(offset) => {
                        // The offset also counts the collection root blob.
                        progress(ReceiveProgress::Downloading {
                            received: (local_bytes + offset).min(total),
                            total,
                        });
                    }
                    GetProgressItem::Done(_) => break,
                    GetProgressItem::Error(source) => {
                        progress(ReceiveProgress::Error);
                        return Err(ReceiveError::Download { source });
                    }
                }
            }
        }

        let collection = Collection::load(hash_and_format.hash, self.store())
            .await
            .map_err(|source| ReceiveError::LoadCollection { source: Box::new(source) })?;
        let mut bytes = 0u64;
        let mut files = 0usize;
        let mut skipped = Vec::new();
        for (name, hash) in collection.iter() {
            let target = export_target(&root, name)?;
            if target.exists() {
                if !options.overwrite {
                    warn!(name, target = %target.display(), "skipping existing target");
                    skipped.push(name.clone());
                    continue;
                }
                tokio::fs::remove_file(&target)
                    .await
                    .map_err(|source| ReceiveError::RemoveExisting {
                        path: target.clone(),
                        source,
                    })?;
            }
            progress(ReceiveProgress::Exporting { file: name.clone() });
            let mut stream = self
                .store()
                .export_with_opts(ExportOptions {
                    hash: *hash,
                    target: target.clone(),
                    mode: ExportMode::Copy,
                })
                .stream()
                .await;
            let mut file_size = 0u64;
            let mut done = false;
            while let Some(item) = stream.next().await {
                match item {
                    ExportProgressItem::Size(size) => file_size = size,
                    ExportProgressItem::CopyProgress(_) => {}
                    ExportProgressItem::Done => done = true,
                    ExportProgressItem::Error(source) => {
                        progress(ReceiveProgress::Error);
                        return Err(ReceiveError::Export {
                            file: name.clone(),
                            source,
                        });
                    }
                }
            }
            if !done {
                return Err(ReceiveError::ExportStreamEnded { file: name.clone() });
            }
            bytes += file_size;
            files += 1;
        }
        progress(ReceiveProgress::Done { bytes, files });
        Ok(TransferResult { bytes, files, skipped })
    }
}
