//! Server bootstrap: bind a TCP listener, spawn the expiry cleanup task, and
//! serve the axum router with `ConnectInfo` (per-IP rate limiting). Shuts down
//! gracefully on SIGTERM/Ctrl+C (or an injected test trigger), draining
//! in-flight requests within a bounded window before forcing exit.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{AppState, CLEANUP_INTERVAL, SharedState, app};

/// How long in-flight requests may drain after the shutdown signal fires
/// before the server force-exits.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Bind `listen_addr` and serve the rendezvous API until shutdown.
pub async fn serve(listen_addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(listen_addr).await?;
    serve_on(listener).await
}

/// Serve the rendezvous API on an already-bound listener. Used by tests that
/// bind an ephemeral port once (avoiding a bind/drop/rebind race on Windows,
/// where a just-closed TCP port may reject immediate rebinding).
pub async fn serve_on(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    serve_on_with(listener, None).await
}

/// Serve the rendezvous API with graceful shutdown.
///
/// `trigger` is the test seam: when `Some`, shutdown fires as soon as the
/// sender completes — no real OS signals are involved. When `None`, the
/// production SIGTERM/Ctrl+C handlers are installed. After the signal fires,
/// in-flight requests get a bounded [`DRAIN_TIMEOUT`] window to finish before
/// the server force-exits.
pub async fn serve_on_with(
    listener: TcpListener,
    trigger: Option<oneshot::Receiver<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState::new());
    tokio::spawn(cleanup_task(state.clone()));

    let listen_addr = listener.local_addr()?;
    info!(%listen_addr, "rendezvous server listening");

    // The drain clock starts only once the shutdown signal fires.
    let (drained_tx, drained_rx) = oneshot::channel::<()>();
    let serve = axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal(trigger).await;
        info!("shutting down: waiting for in-flight requests to drain");
        let _ = drained_tx.send(());
    });

    tokio::select! {
        result = serve => match result {
            Ok(()) => info!("shutting down: server stopped, requests drained"),
            Err(err) => return Err(Box::new(err)),
        },
        _ = drain_deadline(drained_rx) => {
            info!("shutting down: drain timed out, forcing exit");
        }
    }
    Ok(())
}

/// Resolves once the shutdown signal has fired plus the bounded drain window.
async fn drain_deadline(drained_rx: oneshot::Receiver<()>) {
    let _ = drained_rx.await;
    sleep(DRAIN_TIMEOUT).await;
}

/// Wait for a shutdown request.
///
/// `trigger` is the test seam: when `Some`, shutdown fires when the sender
/// completes — no real OS signals are involved. When `None`, the production
/// handlers are installed: SIGTERM (unix) and Ctrl+C.
pub async fn shutdown_signal(trigger: Option<oneshot::Receiver<()>>) {
    if let Some(receiver) = trigger {
        let _ = receiver.await;
        return;
    }
    shutdown_signals_production().await;
}

#[cfg(unix)]
async fn shutdown_signals_production() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => info!("shutdown signal: SIGTERM"),
                _ = tokio::signal::ctrl_c() => info!("shutdown signal: Ctrl+C"),
            }
        }
        Err(err) => {
            warn!(%err, "failed to install SIGTERM handler; falling back to Ctrl+C only");
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal: Ctrl+C");
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signals_production() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal: Ctrl+C");
}

/// Periodically remove expired nameplate entries (drift DISCOVERY_TTL pattern).
async fn cleanup_task(state: SharedState) {
    loop {
        sleep(CLEANUP_INTERVAL).await;
        let now = Instant::now();
        if let Ok(mut pairs) = state.pairs.lock() {
            let removed = pairs.purge_expired(now);
            if removed > 0 {
                info!(removed, "expired pairs cleaned up");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    #[tokio::test]
    async fn shutdown_signal_resolves_on_injected_trigger() {
        let (tx, rx) = oneshot::channel::<()>();
        tokio::pin!(let sig = shutdown_signal(Some(rx)););

        // Stays pending until the trigger fires (no real signals involved).
        tokio::select! {
            _ = &mut sig => panic!("resolved before the trigger fired"),
            _ = sleep(Duration::from_millis(50)) => {}
        }

        tx.send(()).expect("trigger send");

        tokio::select! {
            _ = &mut sig => {}
            _ = sleep(Duration::from_secs(5)) => panic!("did not resolve after the trigger fired"),
        }
    }

    #[tokio::test]
    async fn serve_on_with_stops_cleanly_on_injected_trigger() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let (tx, rx) = oneshot::channel::<()>();

        let serve = serve_on_with(listener, Some(rx));
        let handle = tokio::spawn(async move { serve.await.map_err(|e| e.to_string()) });

        // Give the server a moment to start accepting, then trigger shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).expect("trigger send");

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve must finish after trigger")
            .expect("serve task must not panic");
        assert!(result.is_ok(), "graceful shutdown returns Ok");
    }
}
