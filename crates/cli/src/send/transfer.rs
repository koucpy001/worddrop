//! The transfer phase of the send flow: progress bar fed by the engine's
//! served-bytes counter while waiting for the receiver's Result (or a Cancel
//! — the receiver may abort mid-transfer).

use std::{future::Future, time::Duration};

use tokio::io::{AsyncRead, AsyncWrite};

use worddrop_core::session::Session;
use worddrop_core::session::control::{ControlMessage, send_message};
use worddrop_core::session::state::Transition;
use worddrop_core::transfer::engine::TransferEngine;

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
    let TransferContext {
        session,
        engine,
        ui,
        total,
        file_count,
    } = ctx;
    let served_baseline = engine.served_bytes();
    let mut bar = ui.transfer_bar(total);
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
                    ControlMessage::Result {
                        bytes,
                        files,
                        skipped_bytes,
                        skipped_files,
                    } => {
                        if !result_matches(bytes, files, skipped_bytes, skipped_files, total, file_count) {
                            return Err(SendError::ResultMismatch {
                                expected_bytes: total,
                                expected_files: file_count,
                                got_bytes: bytes,
                                got_files: files,
                            });
                        }
                        bar.finish_and_clear();
                        session.transition(Transition::Completed).await?;
                        return Ok(SendOutcome::Completed {
                            bytes,
                            files,
                            skipped_bytes,
                            skipped_files,
                        });
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

/// Whether a receiver [`ControlMessage::Result`] accounts for the prepared
/// totals: skipped files (targets the receiver did not re-export) count as
/// delivered, so a retransmit of an already-received collection reconciles.
fn result_matches(
    bytes: u64,
    files: u32,
    skipped_bytes: u64,
    skipped_files: u32,
    total: u64,
    file_count: u32,
) -> bool {
    bytes + skipped_bytes == total && files + skipped_files == file_count
}

#[cfg(test)]
mod tests {
    use super::result_matches;

    #[test]
    fn result_matches_counts_skipped_as_delivered() {
        // Split delivery: 100/1 exported + 100/1 skipped == 200/2 prepared.
        assert!(result_matches(100, 1, 100, 1, 200, 2));
        // Pure-skip retransmit: everything already existed, nothing exported.
        assert!(result_matches(0, 0, 18_369_543, 1, 18_369_543, 1));
        // No skips: plain full transfer still reconciles.
        assert!(result_matches(200, 2, 0, 0, 200, 2));
    }

    #[test]
    fn result_matches_rejects_partial_accounting() {
        // Skipped bytes claimed but files do not add up.
        assert!(!result_matches(100, 1, 50, 1, 200, 2));
        // Files add up but skipped bytes do not.
        assert!(!result_matches(100, 1, 100, 0, 200, 2));
        // Nothing delivered at all.
        assert!(!result_matches(0, 0, 0, 0, 200, 2));
    }
}
