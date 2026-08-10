//! The receive command flow (T14): split code into nameplate + words (F1:
//! words never leave the client), claim the nameplate via rendezvous, dial
//! the sender, SPAKE2 pair with the words as password, review the offer
//! (interactive accept/decline), then download + export with a progress bar.
//!
//! The flow drives the same control protocol the T11 e2e proves end to end;
//! wire helpers live in [`crate::wire`]. SECURITY (F1): only the numeric
//! nameplate goes to the rendezvous; the words are used as the SPAKE2
//! password and never touch the network except over the encrypted control
//! stream.

use std::fmt;
use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use indicatif::ProgressBar;
use iroh::RelayMode;
use iroh_blobs::ticket::BlobTicket;

use my_croc_core::pairing::spake::SpakeError;
use my_croc_core::pairing::wordcode::{WordCode, WordCodeError};
use my_croc_core::session::Session;
use my_croc_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, PROTOCOL_VERSION, recv_message_timeout, send_message,
};
use my_croc_core::session::state::{Transition, TransitionError};
use my_croc_core::transfer::engine::{EngineSpec, TransferEngine};
use my_croc_core::transfer::receive::{ReceiveError, ReceiveOptions, ReceiveProgress};
use my_croc_core::transfer::record::RecordStore;

use crate::rendezvous_client::{RvClient, RvError};
use crate::ui::{bar_style, human_bytes, spinner_style, PlainBar, UiBar};
use crate::wire::{self, CONTROL_ALPN, PAIR_TIMEOUT};

/// How long to wait for the user offer prompt.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the code prompt (when not given via --code).
const CODE_PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// How the receive flow ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// Files downloaded and exported.
    Completed { bytes: u64, files: usize },
    /// The user declined the offer.
    Declined,
    /// Either side cancelled (local Ctrl+C or remote Cancel).
    Cancelled,
}

/// Errors from the receive flow.
#[derive(Debug)]
pub enum RecvError {
    /// Word-code parsing failed (bad code format).
    Word(WordCodeError),
    /// Rendezvous claim failed.
    Rv(RvError),
    /// Failed to parse the claim response as a ticket.
    Ticket(String),
    /// Engine construction failed.
    Engine(my_croc_core::transfer::engine::Error),
    /// The relay did not become contactable within the timeout.
    RelayHung,
    /// Pairing control exchange failed.
    Pair(wire::PairError),
    /// Control message send failed.
    Control(my_croc_core::session::control::SessionError),
    /// Illegal session transition.
    Transition(TransitionError),
    /// A message of the wrong kind arrived.
    UnexpectedMessage(ControlMessage),
    /// The download or export failed.
    Receive(ReceiveError),
    /// iroh connection failed (dial or open_bi).
    Connection(iroh::endpoint::ConnectionError),
    /// Waiting for something timed out (no hang).
    Hung(&'static str),
    /// The user failed to enter a code (stdin closed or timeout).
    NoCode,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // User-facing errors are bilingual (中文 + English) per the global
        // copy rule; the English half keeps the historical wording so
        // scripted checks on the English substring keep working.
        match self {
            Self::Word(err) => write!(f, "配对码格式错误 / word-code error: {err}"),
            Self::Rv(err) => write!(f, "服务器错误 / rendezvous error: {err}"),
            Self::Ticket(t) => write!(f, "无效的票据 / invalid ticket: {t}"),
            Self::Engine(err) => write!(f, "传输引擎错误 / engine error: {err}"),
            Self::RelayHung => write!(f, "连接中继服务器超时 / timed out contacting the relay"),
            Self::Pair(err) => write!(f, "配对失败 / pairing error: {err}"),
            Self::Control(err) => write!(f, "控制通道错误 / control error: {err}"),
            Self::Transition(err) => write!(f, "会话状态错误 / session error: {err}"),
            Self::UnexpectedMessage(m) => {
                write!(f, "意外的控制消息 / unexpected control message: {m:?}")
            }
            Self::Receive(err) => write!(f, "接收错误 / receive error: {err}"),
            Self::Connection(err) => write!(f, "连接错误 / connection error: {err}"),
            Self::Hung(what) => write!(f, "等待 {what} 超时 / timed out waiting for {what}"),
            Self::NoCode => write!(f, "未提供配对码 / no pairing code provided"),
        }
    }
}

impl std::error::Error for RecvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Word(err) => Some(err),
            Self::Rv(err) => Some(err),
            Self::Engine(err) => Some(err),
            Self::Pair(err) => Some(err),
            Self::Control(err) => Some(err),
            Self::Transition(err) => Some(err),
            Self::Receive(err) => Some(err),
            Self::Connection(err) => Some(err),
            Self::Ticket(_)
            | Self::RelayHung
            | Self::UnexpectedMessage(_)
            | Self::Hung(_)
            | Self::NoCode => None,
        }
    }
}

impl From<WordCodeError> for RecvError {
    fn from(err: WordCodeError) -> Self {
        Self::Word(err)
    }
}
impl From<RvError> for RecvError {
    fn from(err: RvError) -> Self {
        Self::Rv(err)
    }
}
impl From<my_croc_core::transfer::engine::Error> for RecvError {
    fn from(err: my_croc_core::transfer::engine::Error) -> Self {
        Self::Engine(err)
    }
}
impl From<wire::PairError> for RecvError {
    fn from(err: wire::PairError) -> Self {
        Self::Pair(err)
    }
}
impl From<my_croc_core::session::control::SessionError> for RecvError {
    fn from(err: my_croc_core::session::control::SessionError) -> Self {
        Self::Control(err)
    }
}
impl From<TransitionError> for RecvError {
    fn from(err: TransitionError) -> Self {
        Self::Transition(err)
    }
}
impl From<ReceiveError> for RecvError {
    fn from(err: ReceiveError) -> Self {
        Self::Receive(err)
    }
}

impl From<iroh::endpoint::ConnectionError> for RecvError {
    fn from(err: iroh::endpoint::ConnectionError) -> Self {
        Self::Connection(err)
    }
}

/// Bundled options for the receive flow.
pub struct ReceiveOpts {
    /// Directory to save received files into (defaults to cwd).
    pub output: Option<PathBuf>,
    /// Data dir for the transfer engine.
    pub data_dir: PathBuf,
    /// Rendezvous server URL.
    pub rendezvous_url: String,
    /// Relay mode for the iroh endpoint.
    pub relay_mode: RelayMode,
    /// Overwrite existing files instead of skipping.
    pub overwrite: bool,
    /// `Some(true)` = auto-accept offer, `Some(false)` = auto-decline,
    /// `None` = interactive prompt (60s default-no).
    pub auto_accept: Option<bool>,
}

/// Run the whole receiver flow.
///
/// `code` is from `--code CODE`; when `None` the code is read from stdin.
/// `interrupt` resolves on Ctrl+C.
pub async fn run_receive<I>(
    code: Option<String>,
    opts: ReceiveOpts,
    interrupt: I,
) -> Result<ReceiveOutcome, RecvError>
where
    I: Future<Output = ()> + Unpin,
{
    let mut interrupt = interrupt;
    let ReceiveOpts { output, data_dir, rendezvous_url, relay_mode, overwrite, auto_accept } = opts;

    // 1. Get the code.
    let code = match code {
        Some(code) => code,
        None => {
            eprint!("输入配对码 / Enter pairing code: ");
            match tokio::select! {
                biased;
                _ = &mut interrupt => return Ok(ReceiveOutcome::Cancelled),
                result = read_line_timeout(CODE_PROMPT_TIMEOUT) => {
                    result.map_err(|_| RecvError::NoCode)?
                }
            } {
                Some(line) => line,
                None => return Err(RecvError::NoCode),
            }
        }
    };
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err(RecvError::NoCode);
    }

    // 2. Split code into nameplate + words (F1: words stay local).
    let (nameplate, words) = WordCode::split(&code)?;

    // 3. Claim the nameplate via rendezvous (nameplate only, words never leave).
    let rv = RvClient::new(&rendezvous_url);
    let ticket_str = tokio::select! {
        biased;
        _ = &mut interrupt => return Ok(ReceiveOutcome::Cancelled),
        result = rv.claim(nameplate) => result?,
    };
    let ticket = BlobTicket::from_str(&ticket_str)
        .map_err(|_| RecvError::Ticket(ticket_str.clone()))?;

    // 4. Build the engine (receivers download blobs; no extra handler needed).
    let relay_enabled = !matches!(relay_mode, RelayMode::Disabled);
    let engine = TransferEngine::new_spec(EngineSpec {
        data_dir: &data_dir,
        relay_mode,
        secret_key: None,
        extra_handler: None,
        track_served_bytes: false,
    })
    .await?;

    // Wait for relay contact before dialing (only when a relay is configured).
    // Restricted networks can stall this up to 15 s — announce it up front.
    if relay_enabled {
        eprintln!("正在连接中继服务器...");
        tokio::time::timeout(Duration::from_secs(15), engine.endpoint().online())
            .await
            .map_err(|_| RecvError::RelayHung)?;
    }

    let outcome = run_receive_inner(
        &engine,
        ticket,
        &words,
        InnerOpts { output, overwrite, auto_accept },
        &data_dir,
        &mut interrupt,
    )
    .await;
    let _ = engine.shutdown().await;
    outcome
}

/// The inner flow after engine construction and code parsing.
/// Inner flow options (reduced from ReceiveOpts for the inner function).
struct InnerOpts {
    output: Option<PathBuf>,
    overwrite: bool,
    auto_accept: Option<bool>,
}

async fn run_receive_inner<I>(
    engine: &TransferEngine,
    ticket: BlobTicket,
    words: &str,
    inner: InnerOpts,
    data_dir: &Path,
    interrupt: &mut I,
) -> Result<ReceiveOutcome, RecvError>
where
    I: Future<Output = ()> + Unpin,
{
    let InnerOpts { output, overwrite, auto_accept } = inner;
    let output = output.unwrap_or_else(|| PathBuf::from("."));
    let output = std::path::absolute(&output).map_err(|source| {
        RecvError::Receive(ReceiveError::TargetDirResolve {
            path: output.clone(),
            source,
        })
    })?;

    // 5. Dial the sender on the control ALPN.
    let mut connecting = spinner("正在连接发送方...");
    let conn = tokio::select! {
        biased;
        _ = &mut *interrupt => {
            connecting.finish_and_clear();
            return Ok(ReceiveOutcome::Cancelled);
        }
        result = tokio::time::timeout(
            PAIR_TIMEOUT,
            engine.endpoint().connect(ticket.addr().clone(), CONTROL_ALPN),
        ) => match result {
            Err(_) => return Err(RecvError::Hung("connect to sender")),
            Ok(Err(source)) => return Err(RecvError::Pair(wire::PairError::Io(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, source.to_string()),
            ))),
            Ok(Ok(conn)) => conn,
        },
    };
    let (mut send, mut recv) = conn.open_bi().await?;
    connecting.finish_and_clear();

    // 6. Hello exchange: receiver sends first, sender echoes back.
    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let _ = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "sender hello").await?;

    // 7. SPAKE2 pairing with the words as password (F1: words used here only).
    let mut pairing = spinner("正在配对...");
    let spake_result = tokio::select! {
        biased;
        _ = &mut *interrupt => {
            pairing.finish_and_clear();
            let _ = send_message(&mut send, &ControlMessage::Cancel).await;
            return Ok(ReceiveOutcome::Cancelled);
        }
        result = wire::spake_receiver_side(&mut send, &mut recv, words.as_bytes()) => result,
    };
    match spake_result {
        Ok(_key) => {}
        Err(wire::PairError::Spake(SpakeError::ConfirmationMismatch)) => {
            pairing.finish_and_clear();
            return Err(RecvError::Pair(wire::PairError::Spake(
                SpakeError::ConfirmationMismatch,
            )));
        }
        Err(err) => {
            pairing.finish_and_clear();
            return Err(RecvError::Pair(err));
        }
    }
    pairing.finish_and_clear();
    eprintln!("配对成功 / Paired successfully");

    let session = Session::new();
    session.transition(Transition::StartPairing).await?;
    session.transition(Transition::PairConfirmed).await?;

    // 8. Receive the Offer.
    let offer = tokio::select! {
        biased;
        _ = &mut *interrupt => {
            let _ = send_message(&mut send, &ControlMessage::Cancel).await;
            session.cancel().await?;
            return Ok(ReceiveOutcome::Cancelled);
        }
        result = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "sender offer") => match result {
            Ok(ControlMessage::Offer { files, total_bytes }) => (files, total_bytes),
            Ok(other) => return Err(RecvError::UnexpectedMessage(other)),
            Err(err) => return Err(RecvError::Control(err)),
        },
    };
    let (files, total_bytes) = offer;

    // 9. Display the offer and prompt accept/decline.
    display_offer(&files, total_bytes);
    let accepted = match auto_accept {
        Some(accept) => accept,
        None => {
            tokio::select! {
                biased;
                _ = &mut *interrupt => {
                    let _ = send_message(&mut send, &ControlMessage::Cancel).await;
                    session.cancel().await?;
                    return Ok(ReceiveOutcome::Cancelled);
                }
                result = prompt_accept() => match result {
                    Ok(true) => true,
                    Ok(false) => false,
                    Err(_) => false,
                },
            }
        }
    };

    if !accepted {
        send_message(
            &mut send,
            &ControlMessage::Decline {
                reason: "user declined".to_string(),
            },
        )
        .await?;
        wire::await_peer_close(&mut recv, "sender to close after decline").await?;
        return Ok(ReceiveOutcome::Declined);
    }

    // 10. Accept and download.
    send_message(&mut send, &ControlMessage::Accept).await?;
    session.transition(Transition::TransferStarted).await?;

    // Prompt for resume when a record exists; check_resume also drops a
    // declined record so the download below starts fresh.
    check_resume(data_dir, &ticket, &output).await;

    let mut bar = progress_bar(total_bytes);
    let mut bar_for_cb = bar.clone();
    let mut progress_cb = move |ev: ReceiveProgress| {
        if let ReceiveProgress::Downloading { received, total } = ev {
            let clamped = received.min(total);
            bar_for_cb.set_position(clamped);
        }
    };

    // Always download through the record-backed path: an interrupted first
    // transfer must leave a resume record on disk (the resume prompt needs
    // one; plain `receive` never writes a record). The record loaded by
    // `receive_resumable` continues the old transfer when the user accepted
    // the resume prompt, and a fresh one otherwise.
    let receive_result = engine
        .receive_resumable(
            &ticket,
            ReceiveOptions {
                target_dir: output,
                overwrite,
            },
            &mut progress_cb,
        )
        .await;
    bar.finish_and_clear();

    match receive_result {
            Ok(result) => {
                send_message(
                    &mut send,
                    &ControlMessage::Result {
                        bytes: result.bytes,
                        files: result.files as u32,
                    },
                )
                .await?;
                wire::await_peer_close(&mut recv, "sender to close after result").await?;
                session.transition(Transition::Completed).await?;
                Ok(ReceiveOutcome::Completed {
                    bytes: result.bytes,
                    files: result.files,
                })
            }
            Err(err) => {
                let _ = send_message(&mut send, &ControlMessage::Cancel).await;
                session.cancel().await?;
                Err(RecvError::Receive(err))
            }
        }
}

/// Check whether a resume record exists for this collection + target dir,
/// and if so, prompt the user. Returns true only when the user accepts the
/// resume; a declined resume (or a timeout/EOF default-no) deletes the stale
/// record so the download restarts fresh (and re-persists progress).
async fn check_resume(
    data_dir: &Path,
    ticket: &BlobTicket,
    target_dir: &Path,
) -> bool {
    let records = RecordStore::new(data_dir);
    let hash = ticket.hash();
    if records.load(&hash, target_dir).await.is_none() {
        return false;
    }
    eprint!("继续上次传输? [y/N] ");
    match read_line_timeout(PROMPT_TIMEOUT).await {
        Ok(Some(line)) => {
            if !std::io::stderr().is_terminal() {
                eprintln!();
            }
            let trimmed = line.trim().to_lowercase();
            let resume = trimmed == "y" || trimmed == "yes";
            if !resume {
                let _ = records.delete(&hash).await;
            }
            resume
        }
        _ => {
            let _ = records.delete(&hash).await;
            false
        }
    }
}

/// Display the offer to the user: file names + total bytes.
fn display_offer(
    files: &[my_croc_core::session::control::FileMeta],
    total_bytes: u64,
) {
    eprintln!(
        "发送方发来了 {} 个文件（{}） / Sender offers {} files ({} total):",
        files.len(),
        human_bytes(total_bytes),
        files.len(),
        human_bytes(total_bytes),
    );
    for file in files {
        eprintln!("  {} ({})", file.name, human_bytes(file.size));
    }
}

/// Prompt the user to accept or decline, with a 60s timeout defaulting to no.
async fn prompt_accept() -> Result<bool, RecvError> {
    eprint!(
        "接受传输? [y/N] ({}s 超时自动拒绝) ",
        PROMPT_TIMEOUT.as_secs()
    );
    match read_line_timeout(PROMPT_TIMEOUT).await {
        Ok(Some(line)) => {
            // Piped stdin (agents): close the prompt line so the next plain
            // progress line does not run into it.
            if !std::io::stderr().is_terminal() {
                eprintln!();
            }
            let trimmed = line.trim().to_lowercase();
            Ok(trimmed == "y" || trimmed == "yes")
        }
        Ok(None) => Ok(false),
        Err(_) => Ok(false),
    }
}

/// Read a line from stdin with a timeout. Returns None on EOF, or on timeout.
async fn read_line_timeout(timeout: Duration) -> Result<Option<String>, RecvError> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    match tokio::time::timeout(timeout, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(_)) => Ok(Some(line)),
        Ok(Err(source)) => Err(RecvError::Pair(wire::PairError::Io(source))),
        Err(_) => Ok(None),
    }
}

/// Create a state element (spinner): indicatif when stderr is a terminal,
/// a plain one-shot line otherwise (indicatif hides everything non-TTY).
fn spinner(msg: &str) -> UiBar {
    if std::io::stderr().is_terminal() {
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.set_message(msg.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        UiBar::Tty(bar)
    } else {
        UiBar::Plain(PlainBar::state(msg))
    }
}

/// Create a transfer progress bar (tty-aware, same fallback as [`spinner`]).
fn progress_bar(total_bytes: u64) -> UiBar {
    if std::io::stderr().is_terminal() {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(bar_style());
        bar.set_message("正在传输...".to_string());
        UiBar::Tty(bar)
    } else {
        UiBar::Plain(PlainBar::transfer("正在传输...", total_bytes))
    }
}
