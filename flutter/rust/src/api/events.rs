//! Bridge event DTO + fan-out plumbing (drift map_event pattern).
//!
//! Two buses: a global debug/test bus (T16 contract: `subscribeEvents` /
//! `emitEvent`, still used by the Dart smoke test) and per-session buses
//! fanned into `watch_transfer` sinks by the real session flows (T17).

use std::sync::LazyLock;

use tokio::sync::broadcast;

use crate::api::RUNTIME;
use crate::frb_generated::StreamSink;

/// One event on a transfer stream. `kind` selects the shape; the optional
/// payload fields carry that kind's values.
///
/// Kinds emitted by the session flows (api/session.rs):
/// - "phase"         {phase}: session state machine advanced
/// - "info"          {message}: noteworthy step (paired, declined, ...)
/// - "file_found"    {name, total}: sender preparing, a file's size known
/// - "file_imported" {name}: sender preparing, a file stored
/// - "connecting"    {}: receiver dialing the sender
/// - "downloading"   {received, total}: receiver payload progress
/// - "exporting"     {name}: receiver writing a file to disk
/// - "served"        {received, total}: sender-side bytes served
/// - "skipped"       {files, bytes}: files the receiver did not re-export
///   because they already existed (emitted right before "done")
/// - "done"          {bytes, files}: transfer finished successfully
/// - "error"         {message}: the flow failed
/// - "test"          {message}: debug bus only (T16 `emitEvent` helper)
#[derive(Debug, Clone)]
pub struct BridgeEvent {
    pub kind: String,
    pub phase: Option<String>,
    pub message: Option<String>,
    pub name: Option<String>,
    pub received: Option<u64>,
    pub total: Option<u64>,
    pub bytes: Option<u64>,
    pub files: Option<u64>,
}

impl BridgeEvent {
    pub fn phase(phase: impl Into<String>) -> Self {
        Self { kind: "phase".to_owned(), phase: Some(phase.into()), ..Self::empty() }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self { kind: "info".to_owned(), message: Some(message.into()), ..Self::empty() }
    }

    pub fn progress(kind: impl Into<String>, name: Option<String>, received: Option<u64>, total: Option<u64>) -> Self {
        Self { kind: kind.into(), name, received, total, ..Self::empty() }
    }

    pub fn done(bytes: u64, files: u64) -> Self {
        Self { kind: "done".to_owned(), bytes: Some(bytes), files: Some(files), ..Self::empty() }
    }

    pub fn skipped(files: u64, bytes: u64) -> Self {
        Self { kind: "skipped".to_owned(), files: Some(files), bytes: Some(bytes), ..Self::empty() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { kind: "error".to_owned(), message: Some(message.into()), ..Self::empty() }
    }

    pub fn cancelled() -> Self {
        Self { kind: "cancelled".to_owned(), ..Self::empty() }
    }

    fn empty() -> Self {
        Self {
            kind: String::new(),
            phase: None,
            message: None,
            name: None,
            received: None,
            total: None,
            bytes: None,
            files: None,
        }
    }
}

/// Global debug/test bus (T16): `subscribeEvents` streams everything pushed
/// by `emitEvent`. Real sessions use their own per-session bus instead.
static EVENT_BUS: LazyLock<broadcast::Sender<BridgeEvent>> =
    LazyLock::new(|| broadcast::channel(64).0);

/// Subscribe to the global event stream. Kept open (and fanned) until the
/// Dart side cancels it.
pub fn subscribe_events(updates: StreamSink<BridgeEvent>) -> Result<(), String> {
    fan_out(updates, EVENT_BUS.subscribe());
    Ok(())
}

/// Debug/test helper: push an event onto the global bus.
pub fn emit_event(kind: String, message: String) -> Result<(), String> {
    let event = BridgeEvent { kind, message: Some(message), ..BridgeEvent::empty() };
    // Ignore a full-bus lag error; the bus is a debug channel.
    let _ = EVENT_BUS.send(event);
    Ok(())
}

/// Spawn a task forwarding `events` into `sink` until the Dart side closes
/// the stream (T16 pattern: a failed `add` means the sink is gone).
pub(crate) fn fan_out(
    updates: StreamSink<BridgeEvent>,
    mut events: broadcast::Receiver<BridgeEvent>,
) {
    RUNTIME.spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if updates.add(event).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_fill_kind_and_payload() {
        let phase = BridgeEvent::phase("paired");
        assert_eq!(phase.kind, "phase");
        assert_eq!(phase.phase.as_deref(), Some("paired"));

        let progress = BridgeEvent::progress("downloading", Some("a.txt".into()), Some(5), Some(10));
        assert_eq!(progress.kind, "downloading");
        assert_eq!(progress.name.as_deref(), Some("a.txt"));
        assert_eq!(progress.received, Some(5));
        assert_eq!(progress.total, Some(10));

        let done = BridgeEvent::done(12, 2);
        assert_eq!(done.bytes, Some(12));
        assert_eq!(done.files, Some(2));

        let skipped = BridgeEvent::skipped(3, 4096);
        assert_eq!(skipped.kind, "skipped");
        assert_eq!(skipped.files, Some(3));
        assert_eq!(skipped.bytes, Some(4096));

        assert_eq!(BridgeEvent::error("boom").message.as_deref(), Some("boom"));
        assert_eq!(BridgeEvent::info("paired").kind, "info");
        assert_eq!(BridgeEvent::cancelled().kind, "cancelled");
    }

    #[test]
    fn emit_event_reaches_subscriber() {
        let mut rx = EVENT_BUS.subscribe();
        emit_event("test".to_owned(), "payload".to_owned()).unwrap();
        let event = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        assert_eq!(event.kind, "test");
        assert_eq!(event.message.as_deref(), Some("payload"));
    }

    #[test]
    fn late_subscriber_sees_next_event() {
        emit_event("before".to_owned(), "dropped".to_owned());
        let mut rx = EVENT_BUS.subscribe();
        emit_event("after".to_owned(), "seen".to_owned());
        let event = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        assert_eq!(event.kind, "after");
    }

    #[test]
    fn per_session_bus_delivers_in_order() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(BridgeEvent::phase("created")).unwrap();
        tx.send(BridgeEvent::info("paired")).unwrap();
        let first = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        let second = RUNTIME.block_on(async { rx.recv().await.unwrap() });
        assert_eq!(first.kind, "phase");
        assert_eq!(second.kind, "info");
    }
}
