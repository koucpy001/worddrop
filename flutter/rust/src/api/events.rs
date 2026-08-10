//! StreamSink event channel skeleton (drift pattern).
//!
//! The Dart side subscribes once via `subscribeEvents()` (Rust fn taking a
//! `StreamSink<BridgeEvent>` -> Dart `Stream<BridgeEvent>`); a spawned task
//! fans events from a tokio broadcast bus into the sink. T17 replaces the
//! broadcast source with real my-croc-core event channels and richer DTOs.

use std::sync::LazyLock;

use tokio::sync::broadcast;

use crate::api::RUNTIME;
use crate::frb_generated::StreamSink;

/// Bridge-level event DTO (skeleton: kind + message; T17 adds typed payloads).
#[derive(Debug, Clone)]
pub struct BridgeEvent {
    pub kind: String,
    pub message: String,
}

/// Event bus backing every `subscribeEvents` stream.
static EVENT_BUS: LazyLock<broadcast::Sender<BridgeEvent>> =
    LazyLock::new(|| broadcast::channel(64).0);

/// Subscribe to the bridge event stream. The returned stream is kept open
/// (and fanned) until the Dart side cancels it.
pub fn subscribe_events(updates: StreamSink<BridgeEvent>) -> Result<(), String> {
    RUNTIME.spawn(async move {
        let mut rx = EVENT_BUS.subscribe();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if updates.add(event).is_err() {
                        // Dart side closed the stream.
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

/// Debug/test helper: push an event onto the bus (T17's real events fan in
/// from core channels instead).
pub fn emit_event(kind: String, message: String) -> Result<(), String> {
    let event = BridgeEvent { kind, message };
    // Ignore a full-bus lag error; the bus is a debug channel.
    let _ = EVENT_BUS.send(event);
    Ok(())
}

pub(crate) fn event_rx() -> broadcast::Receiver<BridgeEvent> {
    EVENT_BUS.subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_event_reaches_subscriber() {
        let mut rx = event_rx();
        emit_event("test".to_owned(), "payload".to_owned()).unwrap();
        let event = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        assert_eq!(event.kind, "test");
        assert_eq!(event.message, "payload");
    }

    #[test]
    fn late_subscriber_sees_next_event() {
        emit_event("before".to_owned(), "dropped".to_owned());
        let mut rx = event_rx();
        emit_event("after".to_owned(), "seen".to_owned());
        let event = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        assert_eq!(event.kind, "after");
    }
}
