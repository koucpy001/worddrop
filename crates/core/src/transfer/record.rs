//! Transfer records (T10): one JSON record per in-progress receive at
//! `<data_dir>/transfers/<collection_hash>.json` (drift record.rs pattern).
//!
//! The record is convenience state for resume — it gives the UI a progress
//! offset and remembers which files were already exported — while the
//! durable partial data itself lives in the FsStore bitfield (partial blobs
//! persist as `<hash>.data` + `<hash>.bitfield`, reopened by a fresh store).
//! Records are never a security boundary; a corrupt or missing record is
//! simply treated as "no resume" and a fresh download is fine.

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use iroh_blobs::Hash;
use serde::{Deserialize, Serialize};

/// Subdirectory of the data dir holding transfer records.
pub const TRANSFERS_DIR: &str = "transfers";

/// Lifecycle state of a recorded receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferStatus {
    /// Download or export is not finished; a later run resumes it.
    InProgress,
    /// Data downloaded and every file exported; the record is deleted once
    /// the receive returns.
    Done,
}

/// Persistent state of one receive, keyed by the collection hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    /// Hash of the collection this record belongs to.
    pub collection_hash: Hash,
    /// Target dir the files are exported into (as given in `ReceiveOptions`).
    pub target_dir: String,
    /// Payload bytes the UI shows as received (clamped to the payload total).
    #[serde(default)]
    pub bytes_received: u64,
    /// Lifecycle state of the receive.
    pub status: TransferStatus,
    /// Collection names whose files were already exported.
    #[serde(default)]
    pub exported_files: Vec<String>,
    /// Last time the record was written.
    pub updated_at: SystemTime,
}

impl TransferRecord {
    /// A fresh record for a receive of `collection_hash` into `target_dir`.
    pub fn new(collection_hash: Hash, target_dir: &Path) -> Self {
        Self {
            collection_hash,
            target_dir: target_dir.to_string_lossy().into_owned(),
            bytes_received: 0,
            status: TransferStatus::InProgress,
            exported_files: Vec::new(),
            updated_at: SystemTime::now(),
        }
    }

    /// Whether the record belongs to the receive identified by the collection
    /// hash and target dir. A mismatched record (different target dir) is not
    /// resumed.
    pub fn matches(&self, collection_hash: Hash, target_dir: &Path) -> bool {
        self.collection_hash == collection_hash && self.target_dir == target_dir.to_string_lossy()
    }
}

/// File-backed store for transfer records under `<data_dir>/transfers/`.
///
/// Saves are atomic (write to a unique temp file in the same dir, sync,
/// rename), so a reader never observes a partial record. Loading is
/// best-effort: a missing, unreadable, corrupt, or mismatched file yields
/// `None` — "no resume", and a full re-download is fine.
#[derive(Debug, Clone)]
pub struct RecordStore {
    dir: PathBuf,
}

impl RecordStore {
    /// Store rooted at `<data_dir>/transfers`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join(TRANSFERS_DIR),
        }
    }

    /// The directory holding the records.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file path for `hash`'s record.
    pub fn path(&self, hash: &Hash) -> PathBuf {
        self.dir.join(format!("{}.json", hash.to_hex()))
    }

    /// Load the record for `hash` if one exists and matches `target_dir`.
    ///
    /// Any failure to read or parse the file, or a record for a different
    /// target dir, is treated as "no resume" (`None`).
    pub async fn load(&self, hash: &Hash, target_dir: &Path) -> Option<TransferRecord> {
        let content = tokio::fs::read_to_string(self.path(hash)).await.ok()?;
        let record: TransferRecord = serde_json::from_str(&content).ok()?;
        record.matches(*hash, target_dir).then_some(record)
    }

    /// Atomically persist `record`: write to a unique temp file in the same
    /// dir (`create_new`), sync, rename over the target. A stale temp file
    /// from a crashed run is removed and the write retried once.
    pub async fn save(&self, record: &TransferRecord) -> io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let content = serde_json::to_vec_pretty(record)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        let tmp = self.tmp_path(&record.collection_hash);
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                // Stale temp left by a crashed run: clear it and retry once.
                tokio::fs::remove_file(&tmp).await?;
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .await?
            }
            Err(source) => return Err(source),
        };
        use tokio::io::AsyncWriteExt;
        file.write_all(&content).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(tmp, self.path(&record.collection_hash)).await
    }

    /// Remove the record for `hash` (best-effort: a missing file is fine).
    pub async fn delete(&self, hash: &Hash) -> io::Result<()> {
        match tokio::fs::remove_file(self.path(hash)).await {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(source),
        }
    }

    /// Unique temp path per call: pid + process-local counter, so concurrent
    /// saves (and stale files from dead processes) never collide.
    fn tmp_path(&self, hash: &Hash) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.dir
            .join(format!(".{}.{}-{n}.tmp", hash.to_hex(), std::process::id()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn hash() -> Hash {
        [7u8; 32].into()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("my-croc-record-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn record_save_load_roundtrips_all_fields() {
        let dir = temp_dir("roundtrip");
        let store = RecordStore::new(&dir);
        let target = dir.join("out");
        let mut record = TransferRecord::new(hash(), &target);
        record.bytes_received = 42;
        record.exported_files.push("a.txt".to_string());
        record.status = TransferStatus::Done;

        store.save(&record).await.expect("save record");
        let loaded = store.load(&hash(), &target).await.expect("load record");
        assert_eq!(loaded, record, "all fields survive the JSON roundtrip");
        assert!(store.path(&hash()).is_file(), "record file exists");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn record_save_is_atomic_leaves_no_temp_files() {
        let dir = temp_dir("atomic");
        let store = RecordStore::new(&dir);
        let record = TransferRecord::new(hash(), &dir);

        store.save(&record).await.expect("save record");
        let leftovers: Vec<_> = fs::read_dir(store.dir())
            .expect("read records dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().into_string().unwrap_or_default())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files left behind: {leftovers:?}"
        );
        // The final file is a complete, parseable JSON record.
        let content = fs::read_to_string(store.path(&hash())).expect("read record");
        let parsed: TransferRecord = serde_json::from_str(&content).expect("valid JSON");

        fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(parsed.collection_hash, hash());
    }

    #[tokio::test]
    async fn record_load_treats_corrupt_file_as_no_resume() {
        let dir = temp_dir("corrupt");
        let store = RecordStore::new(&dir);
        fs::create_dir_all(store.dir()).expect("records dir");
        fs::write(store.path(&hash()), b"{ not json !!!").expect("write garbage");

        assert!(
            store.load(&hash(), &dir).await.is_none(),
            "corrupt record -> None"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn record_load_rejects_mismatched_target_dir() {
        let dir = temp_dir("mismatch");
        let store = RecordStore::new(&dir);
        let record = TransferRecord::new(hash(), &dir.join("out-a"));
        store.save(&record).await.expect("save record");

        assert!(store.load(&hash(), &dir.join("out-b")).await.is_none());
        // Same target dir loads fine.
        assert!(store.load(&hash(), &dir.join("out-a")).await.is_some());

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn record_delete_removes_file_and_tolerates_missing() {
        let dir = temp_dir("delete");
        let store = RecordStore::new(&dir);
        let record = TransferRecord::new(hash(), &dir);
        store.save(&record).await.expect("save record");

        store.delete(&hash()).await.expect("delete record");
        assert!(!store.path(&hash()).exists(), "record file removed");
        store
            .delete(&hash())
            .await
            .expect("deleting a missing record is fine");

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
