//! Session flow drivers (T17): the CLI's run_send_inner / run_receive_inner
//! re-implemented event-driven — `tokio::select!` on the session command
//! channel replaces the CLI's interrupt future, and core progress callbacks
//! fan into the session event bus instead of the progress UI. Every exit
//! path shuts the engine down.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::Connection;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};

use worddrop_core::pairing::wordcode::WordCode;
use worddrop_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, PROTOCOL_VERSION, recv_message_timeout, send_message,
};
use worddrop_core::session::state::Transition;
use worddrop_core::session::Session;
use worddrop_core::transfer::engine::TransferEngine;
use worddrop_core::transfer::receive::{ReceiveOptions, ReceiveProgress, TransferResult};
use worddrop_core::transfer::send::{PreparedTransfer, ProgressEvent};

use worddrop_cli::rendezvous_client::RvClient;
use worddrop_cli::wire::{self, CONTROL_ALPN, PAIR_TIMEOUT};

use super::session::{Stage, SessionCommand, cancel_flow, emit, finish_failed, reject_other};
use crate::api::events::BridgeEvent;
use crate::api::session::{FileMetaDto, OfferDto, PreparedSendDto};

type Bus = broadcast::Sender<BridgeEvent>;

/// Sender flow: prepare -> allocate + code -> wait for receiver -> hello +
/// SPAKE2 -> offer -> transfer (served-bytes progress) -> result/cancel.
pub(super) async fn run_sender_flow(
    engine: TransferEngine,
    mut control_rx: mpsc::UnboundedReceiver<Connection>,
    mut cmds: mpsc::UnboundedReceiver<SessionCommand>,
    events: Bus,
    session: Arc<Session>,
    stage: Arc<Mutex<Stage>>,
    rv: RvClient,
) {
    let first = match cmds.recv().await {
        Some(cmd) => cmd,
        None => {
            finish_sender_flow(engine, &rv, None).await;
            return;
        }
    };
    let (paths, reply) = match first {
        SessionCommand::Prepare { paths, reply } => (paths, reply),
        // Cancel is legal as the first command (a session cancelled before
        // anything was prepared).
        SessionCommand::Cancel { reply } => {
            cancel_flow(&session, &events, &stage, Some(reply)).await;
            finish_sender_flow(engine, &rv, None).await;
            return;
        }
        other => {
            reject_other(other, "send_paths must be the first call on a sender session");
            finish_sender_flow(engine, &rv, None).await;
            return;
        }
    };

    let (prepared, code) = match drive_prepare(&engine, &session, &events, &rv, &paths).await {
        Ok(prepared) => prepared,
        Err(err) => {
            *stage.lock().unwrap() = Stage::Idle; // retryable
            emit(&events, BridgeEvent::error(&err));
            let _ = reply.send(Err(err));
            finish_sender_flow(engine, &rv, None).await;
            return;
        }
    };
    let dto = PreparedSendDto {
        code: code.to_string(),
        files: prepared
            .files
            .iter()
            .map(|file| FileMetaDto { name: file.name.clone(), size: file.size, hash: file.hash.to_hex() })
            .collect(),
        total_bytes: prepared.total_bytes,
    };
    let total = prepared.total_bytes;
    let file_count = prepared.files.len() as u32;
    let _ = reply.send(Ok(dto));
    *stage.lock().unwrap() = Stage::AwaitingPeer;

    let conn = match wait_for_receiver(&mut cmds, &mut control_rx, &session, &stage, &events).await {
        Some(conn) => conn,
        None => {
            finish_sender_flow(engine, &rv, Some(&code)).await;
            return;
        }
    };
    let (mut send, mut recv) = match open_control(conn).await {
        Ok(stream) => stream,
        Err(err) => {
            emit(&events, BridgeEvent::error(err));
            finish_failed(&session, &stage, &events).await;
            finish_sender_flow(engine, &rv, Some(&code)).await;
            return;
        }
    };

    let paired = async {
        let hello = wire::recv_hello(&mut recv).await.map_err(|err| err.to_string())?;
        send_message(&mut send, &hello).await.map_err(|err| err.to_string())?;
        let _key = wire::spake_sender_side(&mut send, &mut recv, code.password().as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(err) = paired {
        emit(&events, BridgeEvent::error(err));
        finish_failed(&session, &stage, &events).await;
        finish_sender_flow(engine, &rv, Some(&code)).await;
        return;
    }
    if let Err(err) = session.transition(Transition::PairConfirmed).await {
        emit(&events, BridgeEvent::error(err.to_string()));
        finish_failed(&session, &stage, &events).await;
        finish_sender_flow(engine, &rv, Some(&code)).await;
        return;
    }
    emit(&events, BridgeEvent::phase("paired"));
    emit(&events, BridgeEvent::info("paired"));

    let offer = ControlMessage::Offer {
        files: prepared
            .files
            .iter()
            .map(|file| worddrop_core::session::control::FileMeta {
                name: file.name.clone(),
                size: file.size,
                hash: file.hash.to_hex(),
            })
            .collect(),
        total_bytes: total,
    };
    if let Err(err) = send_message(&mut send, &offer).await {
        emit(&events, BridgeEvent::error(err.to_string()));
        finish_failed(&session, &stage, &events).await;
        finish_sender_flow(engine, &rv, Some(&code)).await;
        return;
    }

    let response = match wait_response(&mut cmds, &mut send, &mut recv, &session, &stage, &events).await {
        Some(response) => response,
        None => {
            finish_sender_flow(engine, &rv, Some(&code)).await;
            return;
        }
    };
    match response {
        ControlMessage::Accept => {
            if let Err(err) = session.transition(Transition::TransferStarted).await {
                emit(&events, BridgeEvent::error(err.to_string()));
                finish_failed(&session, &stage, &events).await;
                finish_sender_flow(engine, &rv, Some(&code)).await;
                return;
            }
            emit(&events, BridgeEvent::phase("transferring"));
            *stage.lock().unwrap() = Stage::Transferring;
            transfer_phase(
                &engine,
                &mut cmds,
                &mut send,
                &mut recv,
                &session,
                &stage,
                &events,
                total,
                file_count,
            )
            .await;
            finish_sender_flow(engine, &rv, Some(&code)).await;
        }
        ControlMessage::Decline { reason } => {
            emit(&events, BridgeEvent::info(format!("declined: {reason}")));
            *stage.lock().unwrap() = Stage::Terminal;
            finish_sender_flow(engine, &rv, Some(&code)).await;
        }
        ControlMessage::Cancel => {
            cancel_flow(&session, &events, &stage, None).await;
            finish_sender_flow(engine, &rv, Some(&code)).await;
        }
        other => {
            emit(&events, BridgeEvent::error(format!("unexpected control message: {other:?}")));
            finish_failed(&session, &stage, &events).await;
            finish_sender_flow(engine, &rv, Some(&code)).await;
        }
    }
}

/// Best-effort release of the published pairing, then shut the engine down.
/// Every exit path of [`run_sender_flow`] funnels through here: MQTT mode
/// clears the retained ticket; HTTP mode is a no-op. `code` is `None` when
/// the flow ended before a pairing was published.
async fn finish_sender_flow(engine: TransferEngine, rv: &RvClient, code: Option<&WordCode>) {
    if let Some(code) = code {
        let _ = rv.cleanup(code.nameplate(), &code.password()).await;
    }
    let _ = engine.shutdown().await;
}

/// Sender prepare: walk + import + collection + ticket, then rendezvous
/// allocate + word-code generation. The prepared transfer (and its blob pin)
/// must outlive the whole flow.
async fn drive_prepare(
    engine: &TransferEngine,
    session: &Session,
    events: &Bus,
    rv: &RvClient,
    paths: &[PathBuf],
) -> Result<(PreparedTransfer, WordCode), String> {
    session.transition(Transition::StartPairing).await.map_err(|err| err.to_string())?;
    emit(events, BridgeEvent::phase("pending_pair"));
    let mut progress = |event: ProgressEvent| {
        if let Some(event) = map_progress(event) {
            emit(events, event);
        }
    };
    let prepared = engine
        .prepare_send(paths, &mut progress)
        .await
        .map_err(|err| err.to_string())?;
    let allocation = rv.allocate(&prepared.ticket.to_string()).await.map_err(|err| err.to_string())?;
    let code = WordCode::generate(allocation.nameplate, &mut rand::rng())
        .map_err(|err| err.to_string())?;
    rv.publish(&prepared.ticket.to_string(), allocation.nameplate, &code.password())
        .await
        .map_err(|err| err.to_string())?;
    Ok((prepared, code))
}

fn map_progress(event: ProgressEvent) -> Option<BridgeEvent> {
    match event {
        ProgressEvent::FileFound { name, size } => {
            Some(BridgeEvent::progress("file_found", Some(name), None, Some(size)))
        }
        ProgressEvent::FileImported { name } => {
            Some(BridgeEvent::progress("file_imported", Some(name), None, None))
        }
    }
}

/// Wait for the receiver's control connection (or a cancel command).
async fn wait_for_receiver(
    cmds: &mut mpsc::UnboundedReceiver<SessionCommand>,
    control_rx: &mut mpsc::UnboundedReceiver<Connection>,
    session: &Session,
    stage: &Mutex<Stage>,
    events: &Bus,
) -> Option<Connection> {
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => return None, // disposed
                Some(SessionCommand::Cancel { reply }) => {
                    cancel_flow(session, events, stage, Some(reply)).await;
                    return None;
                }
                Some(other) => reject_other(other, "sender session: unexpected command while waiting for the receiver"),
            },
            conn = tokio::time::timeout(PAIR_TIMEOUT, control_rx.recv()) => match conn {
                Err(_) => {
                    emit(events, BridgeEvent::error("timed out waiting for the receiver to pair"));
                    finish_failed(session, stage, events).await;
                    return None;
                }
                Ok(None) => {
                    emit(events, BridgeEvent::error("control acceptor closed before any receiver connected"));
                    finish_failed(session, stage, events).await;
                    return None;
                }
                Ok(Some(conn)) => return Some(conn),
            },
        }
    }
}

async fn open_control(
    conn: Connection,
) -> Result<(impl AsyncWrite + Unpin, impl AsyncRead + Unpin), String> {
    tokio::time::timeout(PAIR_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| "timed out waiting for the control stream".to_string())?
        .map_err(|err| format!("control stream failed: {err}"))
}

/// Wait for the receiver's offer response (accept/decline/cancel).
async fn wait_response(
    cmds: &mut mpsc::UnboundedReceiver<SessionCommand>,
    send: &mut (impl AsyncWrite + Unpin),
    recv: &mut (impl AsyncRead + Unpin),
    session: &Session,
    stage: &Mutex<Stage>,
    events: &Bus,
) -> Option<ControlMessage> {
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => return None, // disposed
                Some(SessionCommand::Cancel { reply }) => {
                    let _ = send_message(send, &ControlMessage::Cancel).await;
                    cancel_flow(session, events, stage, Some(reply)).await;
                    return None;
                }
                Some(other) => reject_other(other, "sender session: unexpected command while waiting for the offer response"),
            },
            message = wire::recv_message_idle(recv, "offer response") => match message {
                Ok(message) => return Some(message),
                Err(err) => {
                    emit(events, BridgeEvent::error(err.to_string()));
                    finish_failed(session, stage, events).await;
                    return None;
                }
            },
        }
    }
}

/// Serve blobs while the receiver downloads; emit served-bytes progress and
/// handle the final Result (or a Cancel from either side).
async fn transfer_phase(
    engine: &TransferEngine,
    cmds: &mut mpsc::UnboundedReceiver<SessionCommand>,
    send: &mut (impl AsyncWrite + Unpin),
    recv: &mut (impl AsyncRead + Unpin),
    session: &Session,
    stage: &Mutex<Stage>,
    events: &Bus,
    total: u64,
    file_count: u32,
) {
    let baseline = engine.served_bytes();
    let mut last_served = 0u64;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => {
                    let _ = send_message(send, &ControlMessage::Cancel).await;
                    cancel_flow(session, events, stage, None).await;
                    return;
                }
                Some(SessionCommand::Cancel { reply }) => {
                    let _ = send_message(send, &ControlMessage::Cancel).await;
                    cancel_flow(session, events, stage, Some(reply)).await;
                    return;
                }
                Some(other) => reject_other(other, "sender session: unexpected command during transfer"),
            },
            message = wire::recv_message_idle(recv, "transfer result or cancel") => match message {
                Ok(ControlMessage::Result { bytes, files, skipped_bytes, skipped_files }) => {
                    if bytes + skipped_bytes != total || files + skipped_files != file_count {
                        emit(events, BridgeEvent::error(format!(
                            "receiver result mismatch: expected {total} bytes / {file_count} files, got {bytes} / {files} (skipped {skipped_bytes} / {skipped_files})"
                        )));
                        finish_failed(session, stage, events).await;
                    } else if let Err(err) = session.transition(Transition::Completed).await {
                        emit(events, BridgeEvent::error(err.to_string()));
                        finish_failed(session, stage, events).await;
                    } else {
                        if skipped_files > 0 {
                            emit(events, BridgeEvent::skipped(skipped_files as u64, skipped_bytes));
                        }
                        emit(events, BridgeEvent::phase("done"));
                        emit(events, BridgeEvent::done(bytes, files as u64));
                        *stage.lock().unwrap() = Stage::Terminal;
                    }
                    return;
                }
                Ok(ControlMessage::Cancel) => {
                    cancel_flow(session, events, stage, None).await;
                    return;
                }
                Ok(other) => emit(events, BridgeEvent::error(format!("unexpected control message: {other:?}"))),
                Err(err) => {
                    emit(events, BridgeEvent::error(err.to_string()));
                    finish_failed(session, stage, events).await;
                    return;
                }
            },
            _ = tick.tick() => {
                let served = engine.served_bytes().saturating_sub(baseline).min(total);
                if served != last_served {
                    last_served = served;
                    emit(events, BridgeEvent::progress("served", None, Some(served), Some(total)));
                }
            }
        }
    }
}

/// Receiver flow: claim + dial + hello + SPAKE2 + offer -> wait for
/// accept/decline/cancel -> download + export with progress -> result.
pub(super) async fn run_receiver_flow(
    engine: TransferEngine,
    mut cmds: mpsc::UnboundedReceiver<SessionCommand>,
    events: Bus,
    session: Arc<Session>,
    stage: Arc<Mutex<Stage>>,
    rv: RvClient,
    data_dir: PathBuf,
    overwrite: bool,
) {
    let first = match cmds.recv().await {
        Some(cmd) => cmd,
        None => {
            let _ = engine.shutdown().await;
            return;
        }
    };
    let (code, reply) = match first {
        SessionCommand::Claim { code, reply } => (code, reply),
        // Cancel is legal as the first command (a session cancelled before
        // anything was claimed).
        SessionCommand::Cancel { reply } => {
            cancel_flow(&session, &events, &stage, Some(reply)).await;
            let _ = engine.shutdown().await;
            return;
        }
        other => {
            reject_other(other, "receive_ticket must be the first call on a receiver session");
            let _ = engine.shutdown().await;
            return;
        }
    };

    let claimed = drive_claim(&engine, &session, &events, &rv, &code).await;
    let (dto, ticket, mut send, mut recv) = match claimed {
        Ok(claimed) => claimed,
        Err(err) => {
            emit(&events, BridgeEvent::error(&err));
            finish_failed(&session, &stage, &events).await;
            let _ = reply.send(Err(err));
            let _ = engine.shutdown().await;
            return;
        }
    };
    let _ = reply.send(Ok(dto));
    *stage.lock().unwrap() = Stage::OfferPending;

    let decision = match cmds.recv().await {
        Some(cmd) => cmd,
        None => {
            let _ = send_message(&mut send, &ControlMessage::Cancel).await;
            cancel_flow(&session, &events, &stage, None).await;
            let _ = engine.shutdown().await;
            return;
        }
    };
    match decision {
        SessionCommand::Accept { target_dir, reply } => {
            let target = if target_dir.as_os_str().is_empty() {
                data_dir.join("received")
            } else {
                target_dir
            };
            let _ = reply.send(Ok(()));
            accept_transfer(
                engine,
                &mut cmds,
                &mut send,
                &mut recv,
                &session,
                &stage,
                &events,
                &ticket,
                target,
                overwrite,
            )
            .await;
        }
        SessionCommand::Decline { reason, reply } => {
            let result: Result<(), String> = async {
                send_message(&mut send, &ControlMessage::Decline { reason: reason.clone() })
                    .await
                    .map_err(|err| err.to_string())?;
                wire::await_peer_close(&mut recv, "sender close after decline")
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => {
                    emit(&events, BridgeEvent::info(format!("declined: {reason}")));
                    let _ = reply.send(Ok(()));
                }
                Err(err) => {
                    emit(&events, BridgeEvent::error(&err));
                    let _ = reply.send(Err(err));
                }
            }
            *stage.lock().unwrap() = Stage::Terminal;
            let _ = engine.shutdown().await;
        }
        SessionCommand::Cancel { reply } => {
            let _ = send_message(&mut send, &ControlMessage::Cancel).await;
            cancel_flow(&session, &events, &stage, Some(reply)).await;
            let _ = engine.shutdown().await;
        }
        other => {
            reject_other(other, "receiver session: unexpected command while waiting for the offer decision");
            let _ = engine.shutdown().await;
        }
    }
}

/// Claim the code's nameplate, dial the sender, SPAKE2 pair, and read the
/// offer. Returns the offer DTO plus the open control stream and ticket the
/// accept path reuses.
async fn drive_claim(
    engine: &TransferEngine,
    session: &Session,
    events: &Bus,
    rv: &RvClient,
    code: &str,
) -> Result<
    (
        OfferDto,
        iroh_blobs::ticket::BlobTicket,
        impl AsyncWrite + Unpin + use<>,
        impl AsyncRead + Unpin + use<>,
    ),
    String,
> {
    session.transition(Transition::StartPairing).await.map_err(|err| err.to_string())?;
    emit(events, BridgeEvent::phase("pending_pair"));

    let (nameplate, words) = WordCode::split(code).map_err(|err| err.to_string())?;
    let ticket_str = rv.claim(nameplate, &words).await.map_err(|err| err.to_string())?;
    let ticket = iroh_blobs::ticket::BlobTicket::from_str(&ticket_str)
        .map_err(|_| "invalid ticket from rendezvous".to_string())?;

    emit(events, BridgeEvent::progress("connecting", None, None, None));
    let conn = tokio::time::timeout(
        PAIR_TIMEOUT,
        engine.endpoint().connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await
    .map_err(|_| "timed out dialing the sender".to_string())?
    .map_err(|err| format!("dial failed: {err}"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|err| err.to_string())?;

    send_message(&mut send, &ControlMessage::Hello { version: PROTOCOL_VERSION })
        .await
        .map_err(|err| err.to_string())?;
    let _ = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "sender hello")
        .await
        .map_err(|err| err.to_string())?;
    let _key = wire::spake_receiver_side(&mut send, &mut recv, words.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    session.transition(Transition::PairConfirmed).await.map_err(|err| err.to_string())?;
    emit(events, BridgeEvent::phase("paired"));
    emit(events, BridgeEvent::info("paired"));

    let message = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "sender offer")
        .await
        .map_err(|err| err.to_string())?;
    let ControlMessage::Offer { files, total_bytes } = message else {
        return Err(format!("expected an offer, got {message:?}"));
    };
    let dto = OfferDto {
        files: files
            .into_iter()
            .map(|file| FileMetaDto { name: file.name, size: file.size, hash: file.hash })
            .collect(),
        total_bytes,
    };
    Ok((dto, ticket, send, recv))
}

/// Accept path: send Accept, download with resume record + progress events,
/// report the result to the sender, mark done. A cancel command (or dispose)
/// mid-transfer aborts the download.
async fn accept_transfer(
    engine: TransferEngine,
    cmds: &mut mpsc::UnboundedReceiver<SessionCommand>,
    send: &mut (impl AsyncWrite + Unpin),
    recv: &mut (impl AsyncRead + Unpin),
    session: &Session,
    stage: &Mutex<Stage>,
    events: &Bus,
    ticket: &iroh_blobs::ticket::BlobTicket,
    target_dir: PathBuf,
    overwrite: bool,
) {
    if let Err(err) = send_message(send, &ControlMessage::Accept).await {
        emit(events, BridgeEvent::error(err.to_string()));
        finish_failed(session, stage, events).await;
        let _ = engine.shutdown().await;
        return;
    }
    if let Err(err) = session.transition(Transition::TransferStarted).await {
        emit(events, BridgeEvent::error(err.to_string()));
        finish_failed(session, stage, events).await;
        let _ = engine.shutdown().await;
        return;
    }
    emit(events, BridgeEvent::phase("transferring"));
    *stage.lock().unwrap() = Stage::Transferring;

    let mut progress = |event: ReceiveProgress| {
        if let Some(event) = map_receive_progress(event) {
            emit(events, event);
        }
    };
    enum TransferEvent {
        Command(Option<SessionCommand>),
        Finished(Result<TransferResult, String>),
    }
    // The transfer future borrows the engine; scope it so the borrow ends
    // before the engine is shut down on every exit path.
    let outcome = {
        let mut transfer = Box::pin(engine.receive_resumable(
            ticket,
            ReceiveOptions { target_dir, overwrite },
            &mut progress,
        ));
        let event = tokio::select! {
            biased;
            cmd = cmds.recv() => TransferEvent::Command(cmd),
            result = &mut transfer => TransferEvent::Finished(result.map_err(|err| err.to_string())),
        };
        match event {
            TransferEvent::Command(None) => {
                let _ = send_message(send, &ControlMessage::Cancel).await;
                cancel_flow(session, events, stage, None).await;
                None
            }
            TransferEvent::Command(Some(SessionCommand::Cancel { reply })) => {
                let _ = send_message(send, &ControlMessage::Cancel).await;
                cancel_flow(session, events, stage, Some(reply)).await;
                None
            }
            TransferEvent::Command(Some(other)) => {
                reject_other(other, "receiver session: unexpected command during transfer");
                Some((&mut transfer).await.map_err(|err| err.to_string()))
            }
            TransferEvent::Finished(result) => Some(result),
        }
    };
    let Some(outcome) = outcome else {
        let _ = engine.shutdown().await;
        return;
    };

    match outcome {
        Ok(result) => {
            let result_msg = ControlMessage::Result {
                bytes: result.bytes,
                files: result.files as u32,
                skipped_bytes: result.skipped_bytes,
                skipped_files: result.skipped.len() as u32,
            };
            if let Err(err) = send_message(send, &result_msg).await {
                emit(events, BridgeEvent::error(err.to_string()));
                finish_failed(session, stage, events).await;
            } else if let Err(err) = wire::await_peer_close(recv, "sender close after result").await {
                emit(events, BridgeEvent::error(err.to_string()));
                finish_failed(session, stage, events).await;
            } else if let Err(err) = session.transition(Transition::Completed).await {
                emit(events, BridgeEvent::error(err.to_string()));
                finish_failed(session, stage, events).await;
            } else {
                if !result.skipped.is_empty() {
                    emit(events, BridgeEvent::skipped(result.skipped.len() as u64, result.skipped_bytes));
                }
                emit(events, BridgeEvent::phase("done"));
                emit(events, BridgeEvent::done(result.bytes, result.files as u64));
                *stage.lock().unwrap() = Stage::Terminal;
            }
        }
        Err(err) => {
            let _ = send_message(send, &ControlMessage::Cancel).await;
            let _ = session.cancel().await;
            emit(events, BridgeEvent::phase("cancelled"));
            emit(events, BridgeEvent::error(err));
            *stage.lock().unwrap() = Stage::Terminal;
        }
    }
    let _ = engine.shutdown().await;
}

fn map_receive_progress(event: ReceiveProgress) -> Option<BridgeEvent> {
    match event {
        ReceiveProgress::Connecting => Some(BridgeEvent::progress("connecting", None, None, None)),
        ReceiveProgress::Downloading { received, total } => {
            Some(BridgeEvent::progress("downloading", None, Some(received), Some(total)))
        }
        ReceiveProgress::Exporting { file } => {
            Some(BridgeEvent::progress("exporting", Some(file), None, None))
        }
        // The flow emits its own "done" (with the TransferResult) after the
        // result is reported; the callback's Done is redundant.
        ReceiveProgress::Done { .. } => None,
        ReceiveProgress::Error => Some(BridgeEvent::error("receive failed")),
    }
}
