//! Provider serve-event plumbing: turns iroh-blobs' per-request transfer
//! updates into a cumulative "payload bytes served" counter (T13 progress).

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use iroh_blobs::provider::events::{
    ConnectMode, EventMask, EventSender, ObserveMode, ProviderMessage, RequestMode, RequestUpdate,
    ThrottleMode,
};

/// Build an [`EventSender`] that notifies us of every blob request the
/// receiver makes, and spawn the consumer task that folds each request's
/// chunk-progress stream into the cumulative `served` counter.
///
/// The iroh-blobs provider emits one `RequestUpdate` stream per request
/// (concurrent requests interleave), so each stream gets its own task; the
/// per-request offset deltas (plus the final `payload_bytes_sent` from
/// `Completed`, which covers the last partial chunk) accumulate into the
/// shared counter. The task exits when the provider drops the channel
/// (router shutdown / engine drop).
pub(super) fn make_event_sender(served: Arc<AtomicU64>) -> EventSender {
    let (sender, mut events) = EventSender::channel(
        256,
        EventMask {
            connected: ConnectMode::None,
            get: RequestMode::NotifyLog,
            get_many: RequestMode::NotifyLog,
            push: RequestMode::Disabled,
            throttle: ThrottleMode::None,
            observe: ObserveMode::None,
        },
    );
    tokio::spawn(async move {
        while let Some(message) = events.recv().await {
            let updates = match message {
                ProviderMessage::GetRequestReceivedNotify(request) => Some(request.rx),
                ProviderMessage::GetManyRequestReceivedNotify(request) => Some(request.rx),
                _ => None,
            };
            let Some(mut updates) = updates else { continue };
            let served = served.clone();
            tokio::spawn(async move {
                let mut last_offset = 0u64;
                while let Ok(Some(update)) = updates.recv().await {
                    match update {
                        RequestUpdate::Progress(progress) => {
                            let delta = progress.end_offset.saturating_sub(last_offset);
                            served.fetch_add(delta, Ordering::Relaxed);
                            last_offset = progress.end_offset;
                        }
                        RequestUpdate::Completed(completed) => {
                            let delta = completed
                                .stats
                                .payload_bytes_sent
                                .saturating_sub(last_offset);
                            served.fetch_add(delta, Ordering::Relaxed);
                        }
                        // Aborted requests stop at their last Progress; the
                        // Started event carries no byte count.
                        RequestUpdate::Aborted(_) | RequestUpdate::Started(_) => {}
                    }
                }
            });
        }
    });
    sender
}
