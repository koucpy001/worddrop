//! my-croc-rendezvous binary entry point.
//!
//! Listens on `MY_CROC_RENDEZVOUS_ADDR` (default `127.0.0.1:8080`, matching the
//! core config default).

use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("my_croc_rendezvous=info")),
        )
        .init();

    let addr: SocketAddr = std::env::var("MY_CROC_RENDEZVOUS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
        .parse()
        .expect("MY_CROC_RENDEZVOUS_ADDR must be a valid socket address");

    my_croc_rendezvous::server::serve(addr).await
}
