//! Shared receive core (T9 happy path + T10 resume), split out of
//! `receive.rs` to keep both files under the 250 pure-LOC ceiling.
//!
//! With `record: None` the flow is byte-for-byte the T9 semantics; with a
//! record, progress and exports are persisted so a crash can be resumed.

use std::time::{Duration, SystemTime};

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

use super::{error::ReceiveError, ReceiveOptions, ReceiveProgress, TransferResult};
use crate::transfer::{
    engine::TransferEngine,
    record::{RecordStore, TransferRecord, TransferStatus},
};

/// Upper bound for the collection blob (the hash seq root) when fetching
/// sizes from the peer: a larger root is rejected as a bad request (sendme
/// uses the same 32 MiB cap).
const MAX_HASH_SEQ_SIZE: u64 = 1024 * 1024 * 32;

/// Budget for the QUIC dial: noq has no handshake timeout (dead UDP peers
/// are retried forever), so the receive flow bounds the connect itself.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

impl TransferEngine {
    /// Shared receive core: download into the persistent store (skipping
    /// already-stored chunks), then export every file into the target dir.
    /// With a record, download progress and exported files are persisted.
    pub(super) async fn receive_impl(
        &self,
        ticket: &BlobTicket,
        options: ReceiveOptions,
        progress: &mut dyn FnMut(ReceiveProgress),
        mut record: Option<&mut TransferRecord>,
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
        let records = RecordStore::new(self.data_dir());

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
                        let received = (local_bytes + offset).min(total);
                        if let Some(record) = record
                            .as_deref_mut()
                            .filter(|record| record.bytes_received != received)
                        {
                            record.bytes_received = received;
                            record.updated_at = SystemTime::now();
                            let path = records.path(&record.collection_hash);
                            records
                                .save(record)
                                .await
                                .map_err(|source| ReceiveError::RecordSave { path, source })?;
                        }
                        progress(ReceiveProgress::Downloading { received, total });
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
            if record
                .as_deref()
                .filter(|record| record.exported_files.contains(name))
                .is_some()
            {
                warn!(name, "resume: skipping already exported file");
                continue;
            }
            let target = super::export_target(&root, name)?;
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
            if let Some(record) = record.as_deref_mut() {
                record.exported_files.push(name.clone());
                record.updated_at = SystemTime::now();
                let path = records.path(&record.collection_hash);
                records
                    .save(record)
                    .await
                    .map_err(|source| ReceiveError::RecordSave { path, source })?;
            }
        }
        if let Some(record) = record {
            record.status = TransferStatus::Done;
            record.updated_at = SystemTime::now();
            let path = records.path(&record.collection_hash);
            records
                .save(record)
                .await
                .map_err(|source| ReceiveError::RecordSave { path, source })?;
        }
        progress(ReceiveProgress::Done { bytes, files });
        Ok(TransferResult { bytes, files, skipped })
    }
}
