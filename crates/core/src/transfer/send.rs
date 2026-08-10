//! Send-side preparation (T8): walk input paths into a stable, symlink-free
//! file list, import each file into the blob store, build the [`Collection`]
//! and produce a [`BlobTicket`] — everything a receiver needs to fetch the
//! data, without starting any connection (T9 wires the ticket to a peer).
//!
//! Symlink policy: symlinks are skipped with a warning (both nested and
//! top-level), documented in task-8 evidence.

use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
};

use iroh_blobs::{
    BlobFormat, Hash,
    api::{
        TempTag,
        blobs::{AddPathOptions, AddProgressItem, ImportMode},
    },
    format::collection::Collection,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use tracing::warn;

use super::engine::TransferEngine;

/// Progress events emitted while preparing a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// A file was found and its size determined (found bytes).
    FileFound { name: String, size: u64 },
    /// A file finished importing into the blob store (per-file done).
    FileImported { name: String },
}

/// A single file inside a prepared transfer.
#[derive(Debug, Clone)]
pub struct PreparedFile {
    /// Local source path on disk.
    pub path: PathBuf,
    /// Collection name (root-prefixed relative path).
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Hash of the imported blob.
    pub hash: Hash,
}

/// The result of preparing a send: a ticket plus the file inventory.
///
/// Holds the collection [`TempTag`] pin so the data stays alive for the
/// lifetime of the prepared transfer.
#[derive(Debug)]
pub struct PreparedTransfer {
    /// Ticket a receiver uses to fetch the collection.
    pub ticket: BlobTicket,
    /// Hash of the stored collection (equals the ticket hash).
    pub collection_hash: Hash,
    /// The files in collection order (sorted by name).
    pub files: Vec<PreparedFile>,
    /// Sum of all file sizes in bytes.
    pub total_bytes: u64,
    /// Pins the collection (not constructible from outside this module).
    _collection_tag: TempTag,
}

/// Errors from send preparation.
#[derive(Debug)]
pub enum SendError {
    /// The input path does not exist.
    MissingSource { path: PathBuf },
    /// Failed to read metadata for a path.
    ReadMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Failed to list a directory.
    ReadDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A path component is not valid UTF-8.
    InvalidUtf8PathComponent { path: PathBuf },
    /// The input path has no name component to derive collection names from.
    InvalidRootName { path: PathBuf },
    /// The path is neither a regular file nor a directory.
    UnsupportedFileType { path: PathBuf },
    /// Two inputs would produce the same collection name.
    DuplicateName { name: String },
    /// The store failed to import a file.
    Import {
        name: String,
        source: std::io::Error,
    },
    /// The import stream ended without a `Done` event.
    ImportStreamEnded { name: String },
    /// The collection could not be stored.
    StoreCollection {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::MissingSource { path } => {
                write!(f, "source path {} does not exist", path.display())
            }
            SendError::ReadMetadata { path, source } => {
                write!(
                    f,
                    "failed to read metadata for {}: {source}",
                    path.display()
                )
            }
            SendError::ReadDirectory { path, source } => {
                write!(f, "failed to read directory {}: {source}", path.display())
            }
            SendError::InvalidUtf8PathComponent { path } => {
                write!(f, "path component of {} is not valid UTF-8", path.display())
            }
            SendError::InvalidRootName { path } => {
                write!(f, "cannot derive a name for input path {}", path.display())
            }
            SendError::UnsupportedFileType { path } => {
                write!(f, "unsupported file type at {}", path.display())
            }
            SendError::DuplicateName { name } => write!(f, "duplicate collection name {name:?}"),
            SendError::Import { name, source } => {
                write!(f, "failed to import {name:?}: {source}")
            }
            SendError::ImportStreamEnded { name } => {
                write!(f, "import of {name:?} ended without a result")
            }
            SendError::StoreCollection { source } => {
                write!(f, "failed to store collection: {source}")
            }
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SendError::ReadMetadata { source, .. }
            | SendError::ReadDirectory { source, .. }
            | SendError::Import { source, .. } => Some(source),
            SendError::StoreCollection { source } => Some(source.as_ref()),
            SendError::MissingSource { .. }
            | SendError::InvalidUtf8PathComponent { .. }
            | SendError::InvalidRootName { .. }
            | SendError::UnsupportedFileType { .. }
            | SendError::DuplicateName { .. }
            | SendError::ImportStreamEnded { .. } => None,
        }
    }
}

/// Walk a file or directory input into `(collection_name, local_path)` pairs,
/// sorted by name for a deterministic collection hash.
///
/// Symlinks are skipped with a warning. The collection name is the input's
/// file name, with nested entries joined by `/` below it.
pub(crate) fn walk_files(path: &Path) -> Result<Vec<(String, PathBuf)>, SendError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            SendError::MissingSource {
                path: path.to_path_buf(),
            }
        } else {
            SendError::ReadMetadata {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        warn!(path = %path.display(), "skipping symlink input");
        return Ok(Vec::new());
    }
    let root_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SendError::InvalidRootName {
            path: path.to_path_buf(),
        })?;
    let mut discovered = Vec::new();
    if file_type.is_file() {
        discovered.push((root_name.to_string(), path.to_path_buf()));
    } else if file_type.is_dir() {
        let mut stack = vec![(path.to_path_buf(), root_name.to_string())];
        while let Some((current, name)) = stack.pop() {
            let current_type = std::fs::symlink_metadata(&current)
                .map_err(|source| SendError::ReadMetadata {
                    path: current.clone(),
                    source,
                })?
                .file_type();
            if current_type.is_symlink() {
                warn!(path = %current.display(), "skipping symlink");
                continue;
            }
            if current_type.is_file() {
                discovered.push((name, current));
                continue;
            }
            if !current_type.is_dir() {
                return Err(SendError::UnsupportedFileType { path: current });
            }
            let entries =
                std::fs::read_dir(&current).map_err(|source| SendError::ReadDirectory {
                    path: current.clone(),
                    source,
                })?;
            for entry in entries {
                let entry = entry.map_err(|source| SendError::ReadDirectory {
                    path: current.clone(),
                    source,
                })?;
                let child = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| SendError::InvalidUtf8PathComponent { path: entry.path() })?;
                stack.push((entry.path(), format!("{name}/{child}")));
            }
        }
    } else {
        return Err(SendError::UnsupportedFileType {
            path: path.to_path_buf(),
        });
    }
    discovered.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(discovered)
}

impl TransferEngine {
    /// Prepare a send for the given input paths (files and/or directories):
    /// walk them, import every file with [`ImportMode::TryReference`], build
    /// and store the collection, and return a pinned ticket. No connection is
    /// made — purely local preparation.
    ///
    /// The progress callback is `Send` so callers can drive a send flow from
    /// a spawned task (e.g. the CLI and the e2e tests run the sender
    /// concurrently with the receiver side of a pairing).
    pub async fn prepare_send(
        &self,
        paths: &[PathBuf],
        progress: &mut (dyn FnMut(ProgressEvent) + Send),
    ) -> Result<PreparedTransfer, SendError> {
        let mut discovered = Vec::new();
        for input in paths {
            discovered.extend(walk_files(input)?);
        }
        let mut seen = HashSet::with_capacity(discovered.len());
        for (name, _) in &discovered {
            if !seen.insert(name.clone()) {
                return Err(SendError::DuplicateName { name: name.clone() });
            }
        }
        discovered.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut files = Vec::with_capacity(discovered.len());
        let mut file_tags = Vec::with_capacity(discovered.len());
        let mut total_bytes = 0u64;
        for (name, local_path) in discovered {
            let mut stream = self
                .store()
                .add_path_with_opts(AddPathOptions {
                    path: local_path.clone(),
                    format: BlobFormat::Raw,
                    mode: ImportMode::TryReference,
                })
                .stream()
                .await;
            let mut size = 0u64;
            let temp_tag = loop {
                match stream.next().await {
                    Some(AddProgressItem::Size(reported)) => {
                        size = reported;
                        progress(ProgressEvent::FileFound {
                            name: name.clone(),
                            size: reported,
                        });
                    }
                    Some(AddProgressItem::Done(tag)) => {
                        progress(ProgressEvent::FileImported { name: name.clone() });
                        break tag;
                    }
                    Some(AddProgressItem::Error(source)) => {
                        return Err(SendError::Import { name, source });
                    }
                    // Ephemeral copy/outboard progress: no semantic weight here.
                    Some(
                        AddProgressItem::CopyProgress(_)
                        | AddProgressItem::CopyDone
                        | AddProgressItem::OutboardProgress(_),
                    ) => {}
                    None => return Err(SendError::ImportStreamEnded { name }),
                }
            };
            files.push(PreparedFile {
                path: local_path,
                name,
                size,
                hash: temp_tag.hash(),
            });
            file_tags.push(temp_tag);
            total_bytes += size;
        }

        let mut collection = Collection::default();
        for file in &files {
            collection.push(file.name.clone(), file.hash);
        }
        let collection_tag =
            collection
                .store(self.store())
                .await
                .map_err(|source| SendError::StoreCollection {
                    source: Box::new(source),
                })?;
        drop(file_tags);
        let ticket = BlobTicket::new(
            self.endpoint().addr(),
            collection_tag.hash(),
            BlobFormat::HashSeq,
        );
        Ok(PreparedTransfer {
            ticket,
            collection_hash: collection_tag.hash(),
            files,
            total_bytes,
            _collection_tag: collection_tag,
        })
    }
}
