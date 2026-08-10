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

use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use iroh::{
    Endpoint, RelayMode, RelayUrl, RelayUrlParseError, SecretKey, endpoint::presets,
    protocol::Router,
};
use iroh_blobs::{BlobsProtocol, api::Store, provider::events::EventSender, store::fs::FsStore};

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
    DataDirNotDirectory { path: PathBuf },
    /// Failed to bind the QUIC endpoint.
    Bind { source: iroh::endpoint::BindError },
    /// Router shutdown failed (a protocol handler panicked).
    Shutdown { message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RelayUrl { url, source } => {
                write!(f, "invalid relay URL {url:?}: {source}")
            }
            Error::StoreLoad { dir, source } => {
                write!(
                    f,
                    "failed to load blob store at {}: {source}",
                    dir.display()
                )
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
    /// Cumulative payload bytes served to receiving peers (T13 progress
    /// plumbing). Only advances when the engine was built with
    /// [`EngineSpec::track_served_bytes`].
    served: Arc<AtomicU64>,
}

/// All knobs for constructing a [`TransferEngine`]. Bundled so the growing
/// constructor surface stays under the 3-parameter ceiling; the plain
/// constructors ([`TransferEngine::new`], [`TransferEngine::with_relay_mode`],
/// [`TransferEngine::new_local`], [`TransferEngine::new_local_n0`]) fill in
/// the defaults.
pub struct EngineSpec<'a> {
    /// Where the blob store lives (`<data_dir>/blobs`).
    pub data_dir: &'a Path,
    /// Relay mode for the endpoint.
    pub relay_mode: RelayMode,
    /// Pins the node id to a persisted identity (T4); `None` = ephemeral.
    pub secret_key: Option<&'a SecretKey>,
    /// An extra protocol handler registered on the router for `alpn`
    /// (the pairing CONTROL_ALPN acceptor).
    pub extra_handler: Option<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
    /// Enable provider serve-event tracking so [`TransferEngine::served_bytes`]
    /// reports payload bytes served to receiving peers (drives the send-side
    /// progress bar). Costs one spawned consumer task; the default
    /// constructors leave it off.
    pub track_served_bytes: bool,
}

mod events;

impl TransferEngine {
    /// Create an engine from the core [`Config`]: the blob store lives at
    /// `<data_dir>/blobs` and the endpoint uses the configured self-hosted
    /// relay URL.
    pub async fn new(config: &Config) -> Result<Self, Error> {
        let relay = RelayUrl::from_str(&config.relay_url).map_err(|source| Error::RelayUrl {
            url: config.relay_url.clone(),
            source,
        })?;
        Self::with_relay_mode(&config.data_dir, RelayMode::Custom(relay.into()), None).await
    }

    /// Create an engine with an explicit relay mode and optional persistent
    /// identity key.
    ///
    /// `secret_key` pins the node id to the persisted identity (T4); `None`
    /// yields an ephemeral id per engine, like sendme's default.
    pub async fn with_relay_mode(
        data_dir: &Path,
        relay_mode: RelayMode,
        secret_key: Option<&SecretKey>,
    ) -> Result<Self, Error> {
        Self::new_spec(EngineSpec {
            data_dir,
            relay_mode,
            secret_key,
            extra_handler: None,
            track_served_bytes: false,
        })
        .await
    }

    /// Create an engine for the local e2e (T11): `presets::Minimal` — no
    /// n0.computer address lookups, so a test run never touches the real
    /// network — with an optional extra protocol handler registered on the
    /// router for `alpn`.
    ///
    /// The extra ALPN exists because iroh-blobs consumes *every* incoming
    /// bidi stream on its own ALPN as a blob request, so pairing control
    /// traffic (T11: `open_bi`/`accept_bi` + wire framing) needs its own
    /// ALPN on the sender's router.
    pub async fn new_local(
        data_dir: &Path,
        relay_mode: RelayMode,
        extra_handler: Option<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
    ) -> Result<Self, Error> {
        Self::new_spec(EngineSpec {
            data_dir,
            relay_mode,
            secret_key: None,
            extra_handler,
            track_served_bytes: false,
        })
        .await
    }

    /// Like [`new_local`](Self::new_local) but uses `presets::N0` (full
    /// n0.computer stack) for e2e tests where the relay transport needs the
    /// full preset. Still uses `RelayMode::Custom` so no public relay is
    /// contacted — only the configured relay URL.
    pub async fn new_local_n0(
        data_dir: &Path,
        relay_mode: RelayMode,
        extra_handler: Option<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
    ) -> Result<Self, Error> {
        Self::new_spec(EngineSpec {
            data_dir,
            relay_mode,
            secret_key: None,
            extra_handler,
            track_served_bytes: false,
        })
        .await
    }

    /// The full engine constructor (T13 CLI): persisted identity, custom
    /// relay, a CONTROL_ALPN acceptor, and optional serve-event tracking for
    /// the send progress bar — every knob the CLI/GUI send flow needs.
    pub async fn new_spec(spec: EngineSpec<'_>) -> Result<Self, Error> {
        let served = Arc::new(AtomicU64::new(0));
        let events = if spec.track_served_bytes {
            Some(events::make_event_sender(served.clone()))
        } else {
            None
        };
        let engine = Self::build(
            spec.data_dir,
            spec.relay_mode,
            spec.secret_key,
            spec.extra_handler,
            events,
            presets::N0,
        )
        .await?;
        Ok(Self { served, ..engine })
    }

    async fn build(
        data_dir: &Path,
        relay_mode: RelayMode,
        secret_key: Option<&SecretKey>,
        extra_handler: Option<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
        events: Option<EventSender>,
        preset: impl presets::Preset,
    ) -> Result<Self, Error> {
        let store_dir = data_dir.join(BLOBS_DIR);
        for path in [data_dir, store_dir.as_path()] {
            if path.exists() && !path.is_dir() {
                return Err(Error::DataDirNotDirectory {
                    path: path.to_path_buf(),
                });
            }
        }
        let store = FsStore::load(&store_dir)
            .await
            .map_err(|source| Error::StoreLoad {
                dir: store_dir,
                source: Box::new(source),
            })?;
        let mut alpns = vec![iroh_blobs::ALPN.to_vec()];
        if let Some((ref alpn, _)) = extra_handler {
            alpns.push(alpn.clone());
        }
        let mut builder = Endpoint::builder(preset)
            .alpns(alpns)
            .relay_mode(relay_mode);
        if let Some(key) = secret_key {
            builder = builder.secret_key(key.clone());
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|source| Error::Bind { source })?;
        let blobs = BlobsProtocol::new(&store, events);
        let mut router_builder = Router::builder(endpoint).accept(iroh_blobs::ALPN, blobs);
        if let Some((alpn, handler)) = extra_handler {
            router_builder = router_builder.accept(alpn, handler);
        }
        let router = router_builder.spawn();
        Ok(Self {
            router,
            store,
            data_dir: data_dir.to_path_buf(),
            served: Arc::new(AtomicU64::new(0)),
        })
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

    /// Cumulative payload bytes served to receiving peers since the engine
    /// was built (0 when [`EngineSpec::track_served_bytes`] was off, or
    /// before the first request lands). Drives the send-side progress bar
    /// (T13): the CLI takes a baseline at Accept and renders
    /// `served - baseline` over the prepared total.
    pub fn served_bytes(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// Shut down the router (which shuts down the blobs protocol and closes
    /// the endpoint).
    pub async fn shutdown(self) -> Result<(), Error> {
        self.router
            .shutdown()
            .await
            .map_err(|source| Error::Shutdown {
                message: source.to_string(),
            })?;
        Ok(())
    }
}
