//! The send command flow (T13): prepare -> allocate nameplate -> generate
//! code -> SPAKE2 pair (words only, F1) -> offer -> transfer with a bytes +
//! ETA progress bar -> result/cancel.
//!
//! The flow drives the same control protocol the T11 e2e proves end to end;
//! the wire helpers live in [`crate::wire`]. SECURITY: only the ticket (and
//! later nothing at all) goes to the rendezvous; the three secret words are
//! generated here, used as the SPAKE2 password, and never leave the process.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use iroh::endpoint::Connection;

use my_croc_core::pairing::wordcode::{WordCode, WordCodeError};
use my_croc_core::session::control::{ControlMessage, FileMeta, SessionError, send_message};
use my_croc_core::session::state::{Transition, TransitionError};
use my_croc_core::session::Session;
use my_croc_core::transfer::engine::TransferEngine;
use my_croc_core::transfer::send::ProgressEvent;

use crate::rendezvous_client::{RvClient, RvError};
use crate::ui::SendUi;

pub(super) mod transfer;
use crate::wire::{
    PAIR_TIMEOUT, PairError, recv_hello, recv_message_idle, spake_sender_side,
};

/// How the send flow ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The receiver accepted and reported the transfer result.
    Completed { bytes: u64, files: u32 },
    /// The receiver refused the offer.
    Declined { reason: String },
    /// Either side cancelled (local Ctrl+C or remote Cancel).
    Cancelled,
}

/// Errors from the send flow.
#[derive(Debug)]
pub enum SendError {
    /// Preparing the transfer failed (missing paths, import errors, ...).
    Prepare(my_croc_core::transfer::send::SendError),
    /// Rendezvous allocate failed.
    Rv(RvError),
    /// Word-code generation failed.
    Word(WordCodeError),
    /// Pairing control exchange failed.
    Pair(PairError),
    /// Control message send failed (offer, cancel, ...).
    Control(SessionError),
    /// Illegal session transition.
    Transition(TransitionError),
    /// Opening the control stream failed.
    Connection(iroh::endpoint::ConnectionError),
    /// The control-acceptor channel closed before any receiver dialed in.
    AcceptorClosed,
    /// A message of the wrong kind arrived.
    UnexpectedMessage(ControlMessage),
    /// The receiver's Result disagrees with what we prepared.
    ResultMismatch {
        expected_bytes: u64,
        expected_files: u32,
        got_bytes: u64,
        got_files: u32,
    },
    /// Waiting for something timed out (no hang).
    Hung(&'static str),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(err) => write!(f, "{err}"),
            Self::Rv(err) => write!(f, "rendezvous error: {err}"),
            Self::Word(err) => write!(f, "word-code error: {err}"),
            Self::Pair(err) => write!(f, "pairing error: {err}"),
            Self::Control(err) => write!(f, "control error: {err}"),
            Self::Transition(err) => write!(f, "session error: {err}"),
            Self::Connection(err) => write!(f, "control stream error: {err}"),
            Self::AcceptorClosed => write!(f, "control acceptor closed before any receiver dialed in"),
            Self::UnexpectedMessage(message) => {
                write!(f, "unexpected control message: {message:?}")
            }
            Self::ResultMismatch { expected_bytes, expected_files, got_bytes, got_files } => {
                write!(
                    f,
                    "receiver result mismatch: expected {expected_bytes} bytes / {expected_files} \
                     files, got {got_bytes} / {got_files}"
                )
            }
            Self::Hung(what) => write!(f, "timed out waiting for {what}"),
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Prepare(err) => Some(err),
            Self::Rv(err) => Some(err),
            Self::Word(err) => Some(err),
            Self::Pair(err) => Some(err),
            Self::Control(err) => Some(err),
            Self::Transition(err) => Some(err),
            Self::Connection(err) => Some(err),
            Self::AcceptorClosed
            | Self::UnexpectedMessage(_)
            | Self::ResultMismatch { .. }
            | Self::Hung(_) => None,
        }
    }
}

impl From<my_croc_core::transfer::send::SendError> for SendError {
    fn from(err: my_croc_core::transfer::send::SendError) -> Self {
        Self::Prepare(err)
    }
}
impl From<RvError> for SendError {
    fn from(err: RvError) -> Self {
        Self::Rv(err)
    }
}
impl From<WordCodeError> for SendError {
    fn from(err: WordCodeError) -> Self {
        Self::Word(err)
    }
}
impl From<PairError> for SendError {
    fn from(err: PairError) -> Self {
        Self::Pair(err)
    }
}
impl From<SessionError> for SendError {
    fn from(err: SessionError) -> Self {
        Self::Control(err)
    }
}
impl From<TransitionError> for SendError {
    fn from(err: TransitionError) -> Self {
        Self::Transition(err)
    }
}
impl From<iroh::endpoint::ConnectionError> for SendError {
    fn from(err: iroh::endpoint::ConnectionError) -> Self {
        Self::Connection(err)
    }
}

/// Run the whole sender flow. The engine is consumed: its router serves blob
/// requests while the flow drives the control stream; it is shut down on
/// every exit path.
///
/// `interrupt` resolves on local Ctrl+C and is re-polled at every wait point
/// (after firing once it stays resolved, so the flow unwinds promptly).
/// `code_tx` optionally receives the pairing code as soon as it is generated
/// (the e2e PairInfo pattern — used by tests to drive a fake peer; the CLI
/// passes `None`).
pub async fn run_send<I>(
    engine: TransferEngine,
    control_rx: mpsc::UnboundedReceiver<Connection>,
    rv: RvClient,
    paths: Vec<PathBuf>,
    ui: SendUi,
    interrupt: I,
    code_tx: Option<mpsc::Sender<String>>,
) -> Result<SendOutcome, SendError>
where
    I: Future<Output = ()> + Unpin,
{
    let mut interrupt = interrupt;
    let outcome =
        run_send_inner(&engine, control_rx, &rv, &paths, &ui, &mut interrupt, code_tx).await;
    let _ = engine.shutdown().await;
    outcome
}

async fn run_send_inner<I>(
    engine: &TransferEngine,
    mut control_rx: mpsc::UnboundedReceiver<Connection>,
    rv: &RvClient,
    paths: &[PathBuf],
    ui: &SendUi,
    interrupt: &mut I,
    code_tx: Option<mpsc::Sender<String>>,
) -> Result<SendOutcome, SendError>
where
    I: Future<Output = ()> + Unpin,
{
    let session = Session::new();
    session.transition(Transition::StartPairing).await?;

    // 1. Walk + import + collection + ticket.
    let preparing = ui.preparing();
    let mut progress: Box<dyn FnMut(ProgressEvent) + Send> = Box::new(|_| {});
    let prepared = engine.prepare_send(paths, progress.as_mut()).await?;
    preparing.finish_and_clear();
    let total = prepared.total_bytes;
    let file_count = prepared.files.len() as u32;

    // 2. Allocate a nameplate; only the ticket goes to the rendezvous.
    let allocation = rv.allocate(&prepared.ticket.to_string()).await?;

    // 3. Generate the code; the words stay local (SPAKE2 password only).
    let code = WordCode::generate(allocation.nameplate, &mut rand::rng())?;
    let words = code.password().to_owned();
    if let Some(code_tx) = code_tx {
        let _ = code_tx.send(code.to_string()).await;
    }

    // 4. Display the code and wait for the receiver's control connection.
    ui.show_code(&code.to_string());
    let waiting = ui.waiting_pair();
    let conn_fut = tokio::time::timeout(PAIR_TIMEOUT, control_rx.recv());
    let conn = tokio::select! {
        biased;
        _ = &mut *interrupt => {
            waiting.finish_and_clear();
            return Ok(SendOutcome::Cancelled);
        }
        conn = conn_fut => match conn {
            Err(_) => return Err(SendError::Hung("receiver to dial in with the pairing code")),
            Ok(None) => return Err(SendError::AcceptorClosed),
            Ok(Some(conn)) => conn,
        },
    };
    let (mut send, mut recv) = accept_control_stream(conn).await?;

    // 5. Hello + SPAKE2 + key confirmation (words only — F1).
    let hello = recv_hello(&mut recv).await?;
    send_message(&mut send, &hello).await?;
    let _key = spake_sender_side(&mut send, &mut recv, words.as_bytes()).await?;
    session.transition(Transition::PairConfirmed).await?;
    waiting.finish_and_clear();

    // 6. Offer.
    let offer = ControlMessage::Offer {
        files: prepared
            .files
            .iter()
            .map(|file| FileMeta {
                name: file.name.clone(),
                size: file.size,
                hash: file.hash.to_hex(),
            })
            .collect(),
        total_bytes: total,
    };
    send_message(&mut send, &offer).await?;
    let accept_spinner = ui.waiting_accept();
    let response = tokio::select! {
        biased;
        _ = &mut *interrupt => {
            let _ = send_message(&mut send, &ControlMessage::Cancel).await;
            accept_spinner.finish_and_clear();
            session.cancel().await?;
            return Ok(SendOutcome::Cancelled);
        }
        response = recv_message_idle(&mut recv, "offer response") => response?,
    };
    accept_spinner.finish_and_clear();

    // 7. Outcome.
    match response {
        ControlMessage::Accept => {
            session.transition(Transition::TransferStarted).await?;
            transfer::transfer_phase(
                transfer::TransferContext { session: &session, engine, ui, total, file_count },
                &mut send,
                &mut recv,
                interrupt,
            )
            .await
        }
        ControlMessage::Decline { reason } => Ok(SendOutcome::Declined { reason }),
        ControlMessage::Cancel => {
            session.cancel().await?;
            Ok(SendOutcome::Cancelled)
        }
        other => Err(SendError::UnexpectedMessage(other)),
    }
}

/// Accept the receiver's control stream, bounded (a dialed connection that
/// never opens a stream must not hang the flow).
async fn accept_control_stream(
    conn: Connection,
) -> Result<(impl AsyncWrite + Unpin, impl AsyncRead + Unpin), SendError> {
    tokio::time::timeout(PAIR_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| SendError::Hung("receiver to open the control stream"))?
        .map_err(SendError::from)
}

#[cfg(test)]
mod tests;
