//! FRB session API: a thin facade over my-croc-core driving the same flows
//! as the CLI (send/mod.rs, receive.rs), reusing the CLI's wire helpers,
//! rendezvous client, and config. Each session owns a TransferEngine, a core
//! Session state machine, and a per-session event bus fanned into
//! `watch_transfer` sinks. All iroh types stay behind the bridge — Dart only
//! sees plain DTOs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, LazyLock};
use std::time::Duration;

use iroh::RelayMode;
use tokio::sync::{broadcast, mpsc, oneshot};

use my_croc_core::identity::{self, Identity};
use my_croc_core::session::Session;
use my_croc_core::session::state::Transition;
use my_croc_core::transfer::engine::{EngineSpec, TransferEngine};

use my_croc_cli::config::Config;
use my_croc_cli::rendezvous_client::RvClient;
use my_croc_cli::wire::{CONTROL_ALPN, ControlAcceptor};

use crate::api::events::{BridgeEvent, fan_out};
use crate::api::RUNTIME;
use crate::frb_generated::StreamSink;

/// Role of a session: the sender prepares + serves, the receiver claims +
/// downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Sender,
    Receiver,
}

/// Opaque handle identifying a session to Dart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionHandle {
    pub id: u64,
}

/// One file in an offer or prepared-send inventory (hash is the hex blob
/// hash — a plain String, never the iroh type).
#[derive(Debug, Clone)]
pub struct FileMetaDto {
    pub name: String,
    pub size: u64,
    pub hash: String,
}

/// A prepared send: the pairing code plus the file inventory.
#[derive(Debug, Clone)]
pub struct PreparedSendDto {
    pub code: String,
    pub files: Vec<FileMetaDto>,
    pub total_bytes: u64,
}

/// The offer a receiver reviews before accepting.
#[derive(Debug, Clone)]
pub struct OfferDto {
    pub files: Vec<FileMetaDto>,
    pub total_bytes: u64,
}

/// Flow stage of a session, used to fast-fail API calls out of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Idle,
    AwaitingPeer,
    OfferPending,
    Transferring,
    Terminal,
}

/// Commands the API sends into a session's flow task; the task replies once
/// the command has been consumed (accept replies immediately, before the
/// transfer runs — progress is observed via `watch_transfer`).
pub(super) enum SessionCommand {
    Prepare { paths: Vec<PathBuf>, reply: oneshot::Sender<Result<PreparedSendDto, String>> },
    Claim { code: String, reply: oneshot::Sender<Result<OfferDto, String>> },
    Accept { target_dir: PathBuf, reply: oneshot::Sender<Result<(), String>> },
    Decline { reason: String, reply: oneshot::Sender<Result<(), String>> },
    Cancel { reply: oneshot::Sender<Result<(), String>> },
}

struct SessionState {
    id: u64,
    role: SessionRole,
    session: Arc<Session>,
    stage: Arc<Mutex<Stage>>,
    events: broadcast::Sender<BridgeEvent>,
    cmds: mpsc::UnboundedSender<SessionCommand>,
}

static SESSIONS: LazyLock<Mutex<HashMap<u64, Arc<SessionState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Create a session for `role`. The engine (and for the sender the control
/// acceptor) is built now; the flow task waits for the first command.
pub fn create_session(role: SessionRole) -> Result<SessionHandle, String> {
    let state = RUNTIME.block_on(async { build_session(role).await })?;
    let handle = SessionHandle { id: state.id };
    SESSIONS
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?
        .insert(state.id, state);
    Ok(handle)
}

/// Prepare `paths` and allocate a pairing code. Valid once, first, on a
/// sender session. Blocks until the code exists (the file inventory and
/// progress events stream out on `watch_transfer` meanwhile).
pub fn send_paths(handle: SessionHandle, paths: Vec<String>) -> Result<PreparedSendDto, String> {
    let state = lookup(handle)?;
    {
        let mut stage = state.stage.lock().unwrap();
        if state.role != SessionRole::Sender || *stage != Stage::Idle {
            return Err("send_paths requires a fresh sender session".to_string());
        }
        *stage = Stage::AwaitingPeer;
    }
    let paths = paths.into_iter().map(PathBuf::from).collect();
    let (reply, rx) = oneshot::channel();
    state
        .cmds
        .send(SessionCommand::Prepare { paths, reply })
        .map_err(|_| "session closed".to_string())?;
    RUNTIME.block_on(async { rx.await.map_err(|_| "session closed".to_string())? })
}

/// Claim `code`'s nameplate, pair with the sender, and return the pending
/// offer. Valid once, first, on a receiver session. Blocks until the offer
/// arrives; the flow then waits for accept/decline/cancel.
pub fn receive_ticket(handle: SessionHandle, code: String) -> Result<OfferDto, String> {
    let state = lookup(handle)?;
    {
        let mut stage = state.stage.lock().unwrap();
        if state.role != SessionRole::Receiver || *stage != Stage::Idle {
            return Err("receive_ticket requires a fresh receiver session".to_string());
        }
        *stage = Stage::OfferPending;
    }
    let (reply, rx) = oneshot::channel();
    state
        .cmds
        .send(SessionCommand::Claim { code, reply })
        .map_err(|_| "session closed".to_string())?;
    RUNTIME.block_on(async { rx.await.map_err(|_| "session closed".to_string())? })
}

/// Accept the pending offer and download into `target_dir` (empty = the
/// config data dir's `received/` subdir). Returns as soon as the transfer is
/// dispatched; progress and completion arrive on `watch_transfer`.
pub fn accept_offer(handle: SessionHandle, target_dir: String) -> Result<(), String> {
    let state = lookup(handle)?;
    {
        let stage = state.stage.lock().unwrap();
        if state.role != SessionRole::Receiver || *stage != Stage::OfferPending {
            return Err("accept_offer requires a receiver session with a pending offer".to_string());
        }
    }
    let (reply, rx) = oneshot::channel();
    state
        .cmds
        .send(SessionCommand::Accept { target_dir: PathBuf::from(target_dir), reply })
        .map_err(|_| "session closed".to_string())?;
    RUNTIME.block_on(async { rx.await.map_err(|_| "session closed".to_string())? })
}

/// Decline the pending offer.
pub fn decline_offer(handle: SessionHandle, reason: String) -> Result<(), String> {
    let state = lookup(handle)?;
    {
        let stage = state.stage.lock().unwrap();
        if state.role != SessionRole::Receiver || *stage != Stage::OfferPending {
            return Err("decline_offer requires a receiver session with a pending offer".to_string());
        }
    }
    let (reply, rx) = oneshot::channel();
    state
        .cmds
        .send(SessionCommand::Decline { reason, reply })
        .map_err(|_| "session closed".to_string())?;
    RUNTIME.block_on(async { rx.await.map_err(|_| "session closed".to_string())? })
}

/// Cancel the session from any non-terminal stage: the flow tells the peer,
/// then drives the core session to Cancelled (firing its cancel watch).
pub fn cancel_session(handle: SessionHandle) -> Result<(), String> {
    let state = lookup(handle)?;
    if *state.stage.lock().unwrap() == Stage::Terminal {
        return Ok(());
    }
    let (reply, rx) = oneshot::channel();
    state
        .cmds
        .send(SessionCommand::Cancel { reply })
        .map_err(|_| "session closed".to_string())?;
    RUNTIME.block_on(async { rx.await.map_err(|_| "session closed".to_string())? })
}

/// Drop the session handle and stop its flow (aborts a running transfer).
pub fn dispose_session(handle: SessionHandle) -> Result<(), String> {
    let mut registry = SESSIONS.lock().map_err(|_| "session registry poisoned".to_string())?;
    registry.remove(&handle.id).ok_or("unknown session handle")?;
    Ok(())
}

/// Current phase of the session's state machine
/// (created/pending_pair/paired/transferring/done/cancelled/failed).
pub fn session_phase(handle: SessionHandle) -> Result<String, String> {
    let state = lookup(handle)?;
    let phase = RUNTIME.block_on(async { state.session.phase().await });
    Ok(phase.to_string())
}

/// Stream this session's events (progress + phase) into `updates` until the
/// Dart side cancels the stream. Subscribe early to avoid missing events.
pub fn watch_transfer(handle: SessionHandle, updates: StreamSink<BridgeEvent>) -> Result<(), String> {
    let state = lookup(handle)?;
    fan_out(updates, state.events.subscribe());
    Ok(())
}

fn lookup(handle: SessionHandle) -> Result<Arc<SessionState>, String> {
    SESSIONS
        .lock()
        .map_err(|_| "session registry poisoned".to_string())?
        .get(&handle.id)
        .cloned()
        .ok_or_else(|| "unknown session handle".to_string())
}

/// The relay URL "disabled"/"off"/"none" turns the relay off (loopback
/// direct) — the escape hatch the headless smoke test uses.
fn relay_mode_from_url(url: &str) -> Result<RelayMode, String> {
    if matches!(url.to_ascii_lowercase().as_str(), "disabled" | "off" | "none") {
        Ok(RelayMode::Disabled)
    } else {
        let relay = iroh::RelayUrl::from_str(url)
            .map_err(|err| format!("invalid relay URL {url:?}: {err}"))?;
        Ok(RelayMode::Custom(relay.into()))
    }
}

/// Per-role engine data dir under the config base (mirrors the CLI: the redb
/// blob store is single-process-exclusive, so two roles on one machine must
/// never share a db — D3 lesson).
fn role_data_dir(base: &Path, role: SessionRole) -> PathBuf {
    base.join(match role {
        SessionRole::Sender => "send",
        SessionRole::Receiver => "receive",
    })
}

async fn build_session(role: SessionRole) -> Result<Arc<SessionState>, String> {
    let cfg = Config::load().map_err(|err| err.to_string())?;
    let relay_mode = relay_mode_from_url(&cfg.relay_url)?;
    let (events, _) = broadcast::channel(128);
    let (cmds, cmds_rx) = mpsc::unbounded_channel();
    let session = Arc::new(Session::new());
    let stage = Arc::new(Mutex::new(Stage::Idle));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    let (engine, control_rx) = match role {
        SessionRole::Sender => {
            let (control_tx, control_rx) = mpsc::unbounded_channel();
            let acceptor: Box<dyn iroh::protocol::DynProtocolHandler> =
                ControlAcceptor::new(control_tx).into();
            let config_dir = identity::Config::config_dir().map_err(|err| err.to_string())?;
            let identity = Identity::load_or_create(&config_dir).map_err(|err| err.to_string())?;
            let engine = TransferEngine::new_spec(EngineSpec {
                data_dir: &role_data_dir(&cfg.data_dir, role),
                relay_mode: relay_mode.clone(),
                secret_key: Some(identity.secret_key()),
                extra_handler: Some((CONTROL_ALPN.to_vec(), acceptor)),
                track_served_bytes: true,
            })
            .await
            .map_err(|err| err.to_string())?;
            (engine, Some(control_rx))
        }
        SessionRole::Receiver => {
            let engine = TransferEngine::new_spec(EngineSpec {
                data_dir: &role_data_dir(&cfg.data_dir, role),
                relay_mode: relay_mode.clone(),
                secret_key: None,
                extra_handler: None,
                track_served_bytes: false,
            })
            .await
            .map_err(|err| err.to_string())?;
            (engine, None)
        }
    };

    // The ticket must carry the relay URL: wait for relay contact before any
    // dial (mirror the CLI; skipped when the relay is disabled).
    if !matches!(relay_mode, RelayMode::Disabled) {
        tokio::time::timeout(Duration::from_secs(15), engine.endpoint().online())
            .await
            .map_err(|_| "timed out contacting the relay server".to_string())?;
    }

    let rv = RvClient::new(&cfg.rendezvous_url);
    let events_for_task = events.clone();
    let session_for_task = session.clone();
    let stage_for_task = stage.clone();
    let _task = match role {
        SessionRole::Sender => {
            let engine_for_task = engine;
            let control_rx = control_rx.expect("sender control channel");
            let _ = std::thread::Builder::new()
                .name(format!("my-croc-sender-{id}"))
                .spawn(move || {
                    RUNTIME.block_on(crate::api::flows::run_sender_flow(
                        engine_for_task,
                        control_rx,
                        cmds_rx,
                        events_for_task,
                        session_for_task,
                        stage_for_task,
                        rv,
                    ));
                })
                .map_err(|err| format!("failed to spawn session thread: {err}"))?;
            ()
        }
        SessionRole::Receiver => {
            let engine_for_task = engine;
            let data_dir = cfg.data_dir.clone();
            let overwrite = cfg.overwrite;
            let _ = std::thread::Builder::new()
                .name(format!("my-croc-receiver-{id}"))
                .spawn(move || {
                    RUNTIME.block_on(crate::api::flows::run_receiver_flow(
                        engine_for_task,
                        cmds_rx,
                        events_for_task,
                        session_for_task,
                        stage_for_task,
                        rv,
                        data_dir,
                        overwrite,
                    ));
                })
                .map_err(|err| format!("failed to spawn session thread: {err}"))?;
            ()
        }
    };

    Ok(Arc::new(SessionState { id, role, session, stage, events, cmds }))
}

/// Drive the core session to Cancelled (fires the cancel watch) and emit the
/// cancelled events. Idempotent on an already-terminal session.
pub(super) async fn cancel_flow(
    session: &Session,
    events: &broadcast::Sender<BridgeEvent>,
    stage: &Mutex<Stage>,
    reply: Option<oneshot::Sender<Result<(), String>>>,
) {
    let _ = session.cancel().await;
    emit(events, BridgeEvent::phase("cancelled"));
    emit(events, BridgeEvent::cancelled());
    *stage.lock().unwrap() = Stage::Terminal;
    if let Some(reply) = reply {
        let _ = reply.send(Ok(()));
    }
}

pub(super) async fn finish_failed(
    session: &Session,
    stage: &Mutex<Stage>,
    events: &broadcast::Sender<BridgeEvent>,
) {
    let _ = session.transition(Transition::Failed).await;
    emit(events, BridgeEvent::phase("failed"));
    *stage.lock().unwrap() = Stage::Terminal;
}

pub(super) fn emit(events: &broadcast::Sender<BridgeEvent>, event: BridgeEvent) {
    // No receivers (nobody subscribed) is fine.
    let _ = events.send(event);
}

pub(super) fn reject_other(cmd: SessionCommand, message: &str) {
    match cmd {
        SessionCommand::Prepare { reply, .. } => {
            let _ = reply.send(Err(message.to_owned()));
        }
        SessionCommand::Claim { reply, .. } => {
            let _ = reply.send(Err(message.to_owned()));
        }
        SessionCommand::Accept { reply, .. }
        | SessionCommand::Decline { reply, .. }
        | SessionCommand::Cancel { reply } => {
            let _ = reply.send(Err(message.to_owned()));
        }
    }
}

#[cfg(test)]
pub(crate) fn events_sub(handle: SessionHandle) -> broadcast::Receiver<BridgeEvent> {
    lookup(handle).unwrap().events.subscribe()
}

#[cfg(test)]
mod tests;
