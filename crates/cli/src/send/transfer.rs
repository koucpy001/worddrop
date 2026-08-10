//! The transfer phase of the send flow: progress bar fed by the engine's
//! served-bytes counter while waiting for the receiver's Result (or a Cancel
//! — the receiver may abort mid-transfer).

use std::{future::Future, time::Duration};

use tokio::io::{AsyncRead, AsyncWrite};

use my_croc_core::session::control::{ControlMessage, send_message};
use my_croc_core::session::state::Transition;
use my_croc_core::session::Session;
use my_croc_core::transfer::engine::TransferEngine;

use crate::send::{SendError, SendOutcome};
use crate::ui::SendUi;
use crate::wire::recv_message_idle;

/// Everything the transfer phase reads: the session state machine, the
/// engine's served-bytes counter, the progress UI, and the expected totals.
pub(super) struct TransferContext<'a> {
    pub(super) session: &'a Session,
    pub(super) engine: &'a TransferEngine,
    pub(super) ui: &'a SendUi,
    pub(super) total: u64,
    pub(super) file_count: u32,
}

/// The transfer phase: progress bar fed by the engine's served-bytes counter,
/// while waiting for the receiver's Result (or a Cancel — the receiver may
/// abort mid-transfer).
pub(super) async fn transfer_phase<I>(
    ctx: TransferContext<'_>,
    send: &mut (impl AsyncWrite + Unpin),
    recv: &mut (impl AsyncRead + Unpin),
    interrupt: &mut I,
) -> Result<SendOutcome, SendError>
where
    I: Future<Output = ()> + Unpin,
{
    let TransferContext { session, engine, ui, total, file_count } = ctx;
    let served_baseline = engine.served_bytes();
    let bar = ui.transfer_bar(total);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            _ = &mut *interrupt => {
                let _ = send_message(send, &ControlMessage::Cancel).await;
                bar.finish_and_clear();
                session.cancel().await?;
                return Ok(SendOutcome::Cancelled);
            }
            message = recv_message_idle(recv, "transfer result or cancel") => {
                match message? {
                    ControlMessage::Result { bytes, files } => {
                        if bytes != total || files != file_count {
                            return Err(SendError::ResultMismatch {
                                expected_bytes: total,
                                expected_files: file_count,
                                got_bytes: bytes,
                                got_files: files,
                            });
                        }
                        bar.finish_and_clear();
                        session.transition(Transition::Completed).await?;
                        return Ok(SendOutcome::Completed { bytes, files });
                    }
                    ControlMessage::Cancel => {
                        bar.finish_and_clear();
                        session.cancel().await?;
                        return Ok(SendOutcome::Cancelled);
                    }
                    other => return Err(SendError::UnexpectedMessage(other)),
                }
            }
            _ = tick.tick() => {
                let served = engine
                    .served_bytes()
                    .saturating_sub(served_baseline)
                    .min(total);
                bar.set_position(served);
            }
        }
    }
}
