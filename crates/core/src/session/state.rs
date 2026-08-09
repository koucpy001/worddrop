//! Session phase state machine (T5): Created -> PendingPair -> Paired ->
//! Transferring -> Done | Cancelled | Failed. Pure and sync; every illegal
//! transition returns an explicit [`TransitionError`].

use core::fmt;

/// Phases a session moves through over its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    /// Session constructed, nothing started yet.
    Created,
    /// Pairing handshake in flight (nameplate claim + SPAKE2 exchange).
    PendingPair,
    /// Pairing confirmed; waiting for / negotiating the transfer.
    Paired,
    /// Bytes are flowing.
    Transferring,
    /// Transfer finished successfully.
    Done,
    /// Transfer aborted (local or remote cancel).
    Cancelled,
    /// Transfer failed.
    Failed,
}

impl SessionPhase {
    /// Whether the session can still transition (Done/Cancelled/Failed are final).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Failed)
    }

    /// Apply `event`, returning the next phase. Illegal transitions (including
    /// any event on a terminal phase) return an explicit [`TransitionError`].
    pub fn transition(self, event: Transition) -> Result<Self, TransitionError> {
        match (self, event) {
            (Self::Created, Transition::StartPairing) => Ok(Self::PendingPair),
            (Self::PendingPair, Transition::PairConfirmed) => Ok(Self::Paired),
            (Self::Paired, Transition::TransferStarted) => Ok(Self::Transferring),
            (Self::Transferring, Transition::Completed) => Ok(Self::Done),
            (Self::Created | Self::PendingPair | Self::Paired | Self::Transferring, Transition::Cancelled) => {
                Ok(Self::Cancelled)
            }
            (Self::Created | Self::PendingPair | Self::Paired | Self::Transferring, Transition::Failed) => {
                Ok(Self::Failed)
            }
            (from, event) => Err(TransitionError { from, event }),
        }
    }
}

impl fmt::Display for SessionPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Created => "created",
            Self::PendingPair => "pending_pair",
            Self::Paired => "paired",
            Self::Transferring => "transferring",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        };
        f.write_str(name)
    }
}

/// Events that drive a session between phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Pairing handshake begins.
    StartPairing,
    /// SPAKE2 key confirmation succeeded.
    PairConfirmed,
    /// Transfer negotiation done, bytes about to flow.
    TransferStarted,
    /// All bytes transferred and verified.
    Completed,
    /// Either side aborted.
    Cancelled,
    /// Fatal error.
    Failed,
}

/// A transition that is not allowed from the current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    pub from: SessionPhase,
    pub event: Transition,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal transition {} from phase {}",
            event_name(self.event),
            self.from
        )
    }
}

impl std::error::Error for TransitionError {}

fn event_name(event: Transition) -> &'static str {
    match event {
        Transition::StartPairing => "start_pairing",
        Transition::PairConfirmed => "pair_confirmed",
        Transition::TransferStarted => "transfer_started",
        Transition::Completed => "completed",
        Transition::Cancelled => "cancelled",
        Transition::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionPhase, Transition, TransitionError};

    fn error_for(phase: SessionPhase, event: Transition) -> TransitionError {
        TransitionError { from: phase, event }
    }

    #[test]
    fn session_state_start_pairing_advances_created_to_pending_pair() {
        assert_eq!(
            SessionPhase::Created.transition(Transition::StartPairing).unwrap(),
            SessionPhase::PendingPair
        );
    }

    #[test]
    fn session_state_legal_path_reaches_done() {
        let phase = SessionPhase::Created
            .transition(Transition::StartPairing).unwrap()
            .transition(Transition::PairConfirmed).unwrap()
            .transition(Transition::TransferStarted).unwrap()
            .transition(Transition::Completed).unwrap();
        assert_eq!(phase, SessionPhase::Done);
    }

    #[test]
    fn session_state_skip_pair_stage_is_illegal() {
        assert_eq!(
            SessionPhase::Created.transition(Transition::PairConfirmed).unwrap_err(),
            error_for(SessionPhase::Created, Transition::PairConfirmed)
        );
        assert_eq!(
            SessionPhase::PendingPair.transition(Transition::TransferStarted).unwrap_err(),
            error_for(SessionPhase::PendingPair, Transition::TransferStarted)
        );
        assert_eq!(
            SessionPhase::Paired.transition(Transition::Completed).unwrap_err(),
            error_for(SessionPhase::Paired, Transition::Completed)
        );
    }

    #[test]
    fn session_state_done_is_terminal_and_rejects_all_events() {
        let done = SessionPhase::Done;
        assert!(done.is_terminal());
        for event in [
            Transition::StartPairing,
            Transition::PairConfirmed,
            Transition::TransferStarted,
            Transition::Completed,
            Transition::Cancelled,
            Transition::Failed,
        ] {
            assert_eq!(done.transition(event).unwrap_err(), error_for(done, event));
        }
    }

    #[test]
    fn session_state_cancelled_and_failed_are_terminal() {
        assert!(SessionPhase::Cancelled.is_terminal());
        assert!(SessionPhase::Failed.is_terminal());
        assert!(!SessionPhase::Transferring.is_terminal());
    }

    #[test]
    fn session_state_cancel_from_any_phase_converges_to_cancelled() {
        for from in [
            SessionPhase::Created,
            SessionPhase::PendingPair,
            SessionPhase::Paired,
            SessionPhase::Transferring,
        ] {
            assert_eq!(
                from.transition(Transition::Cancelled).unwrap(),
                SessionPhase::Cancelled
            );
        }
    }

    #[test]
    fn session_state_failed_from_any_phase_converges_to_failed() {
        for from in [
            SessionPhase::Created,
            SessionPhase::PendingPair,
            SessionPhase::Paired,
            SessionPhase::Transferring,
        ] {
            assert_eq!(from.transition(Transition::Failed).unwrap(), SessionPhase::Failed);
        }
    }

    #[test]
    fn session_state_display_is_snake_case() {
        assert_eq!(SessionPhase::Created.to_string(), "created");
        assert_eq!(SessionPhase::PendingPair.to_string(), "pending_pair");
        assert_eq!(SessionPhase::Paired.to_string(), "paired");
        assert_eq!(SessionPhase::Transferring.to_string(), "transferring");
        assert_eq!(SessionPhase::Done.to_string(), "done");
        assert_eq!(SessionPhase::Cancelled.to_string(), "cancelled");
        assert_eq!(SessionPhase::Failed.to_string(), "failed");
    }

    #[test]
    fn session_state_transition_error_display_names_both_sides() {
        let err = SessionPhase::Done.transition(Transition::TransferStarted).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("done"), "message: {message}");
        assert!(message.contains("transfer_started"), "message: {message}");
    }
}
