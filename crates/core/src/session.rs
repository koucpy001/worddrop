//! Session handle for a my-croc transfer (T5).
//!
//! A [`Session`] owns the phase state machine ([`state`]) and a cancellation
//! watch channel (drift `send/session.rs` pattern). Every await in the
//! session driver must race [`CancelWatcher::wait_cancelled`] plus the peer
//! connection's close signal (T11 wiring); [`race`] is the structured helper
//! for the first of those two.

pub mod control;
pub mod state;

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use state::{SessionPhase, Transition, TransitionError};

/// Handle for a my-croc session: phase state machine + cancellation watch.
#[derive(Debug, Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

#[derive(Debug)]
struct SessionInner {
    phase: Mutex<SessionPhase>,
    cancel_tx: watch::Sender<bool>,
    /// Keeps the channel open: `watch::Sender::send` with zero receivers
    /// drops the value without storing it, losing the cancellation for late
    /// subscribers. One receiver is held for the session's lifetime.
    _keep_alive: watch::Receiver<bool>,
}

impl Session {
    /// A new session in [`SessionPhase::Created`].
    pub fn new() -> Self {
        let (cancel_tx, keep_alive) = watch::channel(false);
        Self {
            inner: Arc::new(SessionInner {
                phase: Mutex::new(SessionPhase::Created),
                cancel_tx,
                _keep_alive: keep_alive,
            }),
        }
    }

    /// Current phase.
    pub async fn phase(&self) -> SessionPhase {
        *self.inner.phase.lock().await
    }

    /// Apply `event` to the state machine, returning the new phase.
    pub async fn transition(&self, event: Transition) -> Result<SessionPhase, TransitionError> {
        let mut phase = self.inner.phase.lock().await;
        let next = phase.transition(event)?;
        *phase = next;
        Ok(next)
    }

    /// Cancel the session: moves it to [`SessionPhase::Cancelled`] and signals
    /// the watch channel so every raced await aborts. Errors if the session
    /// is already terminal.
    pub async fn cancel(&self) -> Result<SessionPhase, TransitionError> {
        let next = self.transition(Transition::Cancelled).await?;
        let _ = self.inner.cancel_tx.send(true);
        Ok(next)
    }

    /// A watcher that resolves when this session is cancelled.
    pub fn cancel_watcher(&self) -> CancelWatcher {
        CancelWatcher {
            rx: self.inner.cancel_tx.subscribe(),
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Observer of the session's cancellation watch channel.
#[derive(Debug, Clone)]
pub struct CancelWatcher {
    rx: watch::Receiver<bool>,
}

impl CancelWatcher {
    /// Whether cancellation has been signalled.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once cancellation is signalled (immediately when already
    /// signalled). Race this against every session await, together with the
    /// peer connection's close signal (T11 wires the latter via `select!`).
    pub async fn wait_cancelled(&mut self) {
        if *self.rx.borrow() {
            return;
        }
        let _ = self.rx.changed().await;
    }
}

/// Outcome of [`race`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Race<T> {
    /// The awaited future finished first.
    Value(T),
    /// Cancellation won the race.
    Cancelled,
}

/// Race an await against cancellation (drift's watch-cancellation pattern).
/// Cancellation wins ties (`biased`), so an already-signalled cancel is
/// observed deterministically.
pub async fn race<T>(cancel: &mut CancelWatcher, future: impl Future<Output = T>) -> Race<T> {
    tokio::select! {
        biased;
        _ = cancel.wait_cancelled() => Race::Cancelled,
        value = future => Race::Value(value),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::session::state::{SessionPhase, Transition, TransitionError};

    use super::{Race, Session, race};

    async fn drive_to(session: &Session, target: SessionPhase) -> Result<(), TransitionError> {
        match target {
            SessionPhase::Created => Ok(()),
            SessionPhase::PendingPair => session
                .transition(Transition::StartPairing)
                .await
                .map(|_| ()),
            SessionPhase::Paired => {
                session.transition(Transition::StartPairing).await?;
                session
                    .transition(Transition::PairConfirmed)
                    .await
                    .map(|_| ())
            }
            SessionPhase::Transferring => {
                session.transition(Transition::StartPairing).await?;
                session.transition(Transition::PairConfirmed).await?;
                session
                    .transition(Transition::TransferStarted)
                    .await
                    .map(|_| ())
            }
            terminal @ (SessionPhase::Done | SessionPhase::Cancelled | SessionPhase::Failed) => {
                unreachable!("terminal phases are not drive targets: {terminal:?}")
            }
        }
    }

    #[tokio::test]
    async fn session_created_initial_phase() {
        assert_eq!(Session::new().phase().await, SessionPhase::Created);
    }

    #[tokio::test]
    async fn session_transition_updates_phase() {
        let session = Session::new();
        session.transition(Transition::StartPairing).await.unwrap();
        assert_eq!(session.phase().await, SessionPhase::PendingPair);
    }

    #[tokio::test]
    async fn session_cancel_signals_watch_channel() {
        let session = Session::new();
        let watcher = session.cancel_watcher();
        assert!(!watcher.is_cancelled());

        let phase = session
            .cancel()
            .await
            .expect("cancel from Created succeeds");
        assert_eq!(phase, SessionPhase::Cancelled);
        assert!(watcher.is_cancelled());
    }

    #[tokio::test]
    async fn session_cancel_from_each_phase_converges_to_cancelled() {
        for phase in [
            SessionPhase::Created,
            SessionPhase::PendingPair,
            SessionPhase::Paired,
            SessionPhase::Transferring,
        ] {
            let session = Session::new();
            drive_to(&session, phase).await.expect("drives to phase");
            let cancelled = session.cancel().await.expect("cancel succeeds");
            assert_eq!(cancelled, SessionPhase::Cancelled);
            assert_eq!(session.phase().await, SessionPhase::Cancelled);
        }
    }

    #[tokio::test]
    async fn session_cancel_from_terminal_errors() {
        let session = Session::new();
        drive_to(&session, SessionPhase::Transferring)
            .await
            .unwrap();
        session.transition(Transition::Completed).await.unwrap();
        assert!(session.cancel().await.is_err());
    }

    #[tokio::test]
    async fn session_watcher_wait_resolves_after_cancel() {
        let session = Session::new();
        let mut watcher = session.cancel_watcher();
        let waiter = tokio::spawn(async move { watcher.wait_cancelled().await });
        session.cancel().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait_cancelled resolves once cancelled")
            .expect("waiter task completes");
    }

    #[tokio::test]
    async fn session_race_returns_value_when_not_cancelled() {
        let session = Session::new();
        let mut watcher = session.cancel_watcher();
        let outcome = race(&mut watcher, async { 42 }).await;
        assert_eq!(outcome, Race::Value(42));
    }

    #[tokio::test]
    async fn session_race_returns_cancelled_when_signalled() {
        let session = Session::new();
        session.cancel().await.unwrap();
        let mut watcher = session.cancel_watcher();
        assert!(
            watcher.is_cancelled(),
            "fresh subscribe sees the signalled value"
        );
        let outcome = race(&mut watcher, async { 42 }).await;
        assert_eq!(outcome, Race::Cancelled);
    }
}
