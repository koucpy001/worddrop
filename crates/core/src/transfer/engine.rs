//! iroh transfer engine (T7).
//!
//! Owns the iroh [`Endpoint`] (QUIC transport, NAT hole-punching, relay
//! fallback), a persistent [`FsStore`] for blob data, and serves the iroh-blobs
//! protocol on the wire so peers can fetch blobs from this node.
//!
//! The relay mode comes from the core [`Config`]: production uses
//! [`RelayMode::Custom`] with the self-hosted relay URL; tests use
//! [`RelayMode::Disabled`] so local in-process pairs never touch a public
//! relay. No send/receive flow lives here — that is T8/T9.

use std::{fmt, path::Path, path::PathBuf, str::FromStr};

use iroh::{
    endpoint::presets,
    protocol::Router,
    Endpoint, RelayMode, RelayUrl, RelayUrlParseError, SecretKey,
};
use iroh_blobs::{api::Store, store::fs::FsStore, BlobsProtocol};

use crate::identity::Config;

/// Subdirectory of the data dir holding the blob store.
pub const BLOBS_DIR: &str = "blobs";

/// Errors from engine construction and teardown.
#[derive(Debug)]
pub enum Error {
    /// `config.relay_url` is not a valid URL.
    RelayUrl {
        url: String,
        source: RelayUrlParseError,
    },
    /// Failed to load or create the blob store under the data dir.
    StoreLoad {
        dir: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The data dir (or its `blobs` subdir) exists but is not a directory.
    ///
    /// Guarded in-engine: `FsStore::load` blocks forever on a non-directory
    /// root (iroh-blobs 0.103), so we fail fast with a clear error instead.
    DataDirNotDirectory {
        path: PathBuf,
    },
    /// Failed to bind the QUIC endpoint.
    Bind {
        source: iroh::endpoint::BindError,
    },
    /// Router shutdown failed (a protocol handler panicked).
    Shutdown {
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RelayUrl { url, source } => {
                write!(f, "invalid relay URL {url:?}: {source}")
            }
            Error::StoreLoad { dir, source } => {
                write!(f, "failed to load blob store at {}: {source}", dir.display())
            }
            Error::Bind { source } => write!(f, "failed to bind iroh endpoint: {source}"),
            Error::DataDirNotDirectory { path } => write!(
                f,
                "data dir {} exists but is not a directory",
                path.display()
            ),
            Error::Shutdown { message } => write!(f, "engine shutdown failed: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::RelayUrl { source, .. } => Some(source),
            Error::StoreLoad { source, .. } => Some(source.as_ref()),
            Error::Bind { source, .. } => Some(source),
            Error::DataDirNotDirectory { .. } => None,
            Error::Shutdown { .. } => None,
        }
    }
}

/// A running iroh endpoint serving the blobs protocol over a persistent
/// [`FsStore`]. One per process; drop or [`shutdown`](Self::shutdown) to stop.
pub struct TransferEngine {
    router: Router,
    store: FsStore,
    /// The data dir the engine was created with: home of the blob store and
    /// of the transfer records (`<data_dir>/transfers/`, T10).
    data_dir: PathBuf,
}

impl TransferEngine {
    /// Create an engine from the core [`Config`]: the blob store lives at
    /// `<data_dir>/blobs` and the endpoint uses the configured self-hosted
    /// relay URL.
    pub async fn new(config: &Config) -> Result<Self, Error> {
        let relay = RelayUrl::from_str(&config.relay_url)
            .map_err(|source| Error::RelayUrl { url: config.relay_url.clone(), source })?;
        Self::with_relay_mode(&config.data_dir, RelayMode::Custom(relay.into()), None).await
    }

    /// Create an engine with an explicit relay mode and optional persistent
    /// identity key.
    ///
    /// `secret_key` pins the node id to the persisted identity (T4); `None`
    /// yields an ephemeral id per engine, like sendme's default.
    pub(crate) async fn with_relay_mode(
        data_dir: &Path,
        relay_mode: RelayMode,
        secret_key: Option<&SecretKey>,
    ) -> Result<Self, Error> {
        let store_dir = data_dir.join(BLOBS_DIR);
        for path in [data_dir, store_dir.as_path()] {
            if path.exists() && !path.is_dir() {
                return Err(Error::DataDirNotDirectory { path: path.to_path_buf() });
            }
        }
        let store = FsStore::load(&store_dir).await.map_err(|source| Error::StoreLoad {
            dir: store_dir,
            source: Box::new(source),
        })?;
        let mut builder = Endpoint::builder(presets::N0)
            .alpns(vec![iroh_blobs::ALPN.to_vec()])
            .relay_mode(relay_mode);
        if let Some(key) = secret_key {
            builder = builder.secret_key(key.clone());
        }
        let endpoint = builder.bind().await.map_err(|source| Error::Bind { source })?;
        let blobs = BlobsProtocol::new(&store, None);
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, blobs)
            .spawn();
        Ok(Self { router, store, data_dir: data_dir.to_path_buf() })
    }

    /// The bound endpoint (node id, direct addresses, connections).
    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// The blob store handle.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The data dir the engine was created with (home of the transfer
    /// records at `<data_dir>/transfers/`).
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Shut down the router (which shuts down the blobs protocol and closes
    /// the endpoint).
    pub async fn shutdown(self) -> Result<(), Error> {
        self.router
            .shutdown()
            .await
            .map_err(|source| Error::Shutdown { message: source.to_string() })?;
        Ok(())
    }
}
