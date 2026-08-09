//! Server bootstrap: bind a TCP listener, spawn the expiry cleanup task, and
//! serve the axum router with `ConnectInfo` (per-IP rate limiting).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpListener;
use tokio::time::sleep;
use tracing::info;

use crate::{AppState, CLEANUP_INTERVAL, SharedState, app};

/// Bind `listen_addr` and serve the rendezvous API until shutdown.
pub async fn serve(listen_addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState::new());
    tokio::spawn(cleanup_task(state.clone()));

    let listener = TcpListener::bind(listen_addr).await?;
    info!(%listen_addr, "rendezvous server listening");

    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
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
