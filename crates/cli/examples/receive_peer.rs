//! QA receive-side peer for the T13 manual two-terminal test.
//!
//! Stands in for the T14 receive command: claim the nameplate, dial the
//! sender on CONTROL_ALPN, run SPAKE2 with the words from the code, accept
//! the offer, download + export, and report the Result back to the sender.
//!
//! Usage:
//!   cargo run -p worddrop-cli --example receive_peer -- --code N-word-word-word [--output DIR]
//!
//! This is test scaffolding, not the shipped receive command (T14).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use iroh::RelayMode;
use iroh_blobs::ticket::BlobTicket;
use tokio::time::timeout;

use worddrop_core::pairing::wordcode::WordCode;
use worddrop_core::session::control::{
    ControlMessage, HANDSHAKE_TIMEOUT, PROTOCOL_VERSION, recv_message_timeout, send_message,
};
use worddrop_core::transfer::engine::TransferEngine;
use worddrop_core::transfer::receive::ReceiveOptions;

use worddrop_cli::rendezvous_client::RvClient;
use worddrop_cli::wire::{self, CONTROL_ALPN, PAIR_TIMEOUT};

/// Upper bound for the whole receive side.
const FLOW_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
struct Args {
    /// The full pairing code, e.g. `7-correct-horse-battery`.
    #[arg(long)]
    code: String,
    /// Directory to save received files into.
    #[arg(long, default_value = "received")]
    output: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let rendezvous_url = std::env::var("WORDDROP_RENDEZVOUS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let relay_url =
        std::env::var("WORDDROP_RELAY_URL").unwrap_or_else(|_| "http://127.0.0.1:3340".to_string());

    let (nameplate, words) = WordCode::split(&args.code)?;
    println!("nameplate = {nameplate}, words = {words}");

    let rv = RvClient::new(&rendezvous_url);
    let ticket_str = timeout(Duration::from_secs(15), rv.claim(nameplate, &words)).await??;
    let ticket = BlobTicket::from_str(&ticket_str)?;
    println!("claimed ticket: {ticket_str}");

    let data_dir = std::env::temp_dir().join(format!("receive-peer-{}", std::process::id()));
    let relay_url: iroh::RelayUrl = relay_url.parse()?;
    let engine =
        TransferEngine::with_relay_mode(&data_dir, RelayMode::Custom(relay_url.into()), None)
            .await?;
    timeout(Duration::from_secs(15), engine.endpoint().online()).await?;

    let conn = timeout(
        PAIR_TIMEOUT,
        engine
            .endpoint()
            .connect(ticket.addr().clone(), CONTROL_ALPN),
    )
    .await??;
    let (mut send, mut recv) = conn.open_bi().await?;

    send_message(
        &mut send,
        &ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let _hello = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "hello").await?;
    let _key = wire::spake_receiver_side(&mut send, &mut recv, words.as_bytes()).await?;
    println!("paired!");

    let offer = recv_message_timeout(&mut recv, HANDSHAKE_TIMEOUT, "offer").await?;
    let ControlMessage::Offer { files, total_bytes } = &offer else {
        return Err(format!("expected offer, got {offer:?}").into());
    };
    println!("offer: {} files, {} bytes", files.len(), total_bytes);
    for file in files {
        println!("  {} ({} bytes)", file.name, file.size);
    }

    send_message(&mut send, &ControlMessage::Accept).await?;
    let result = timeout(
        FLOW_TIMEOUT,
        engine.receive(
            &ticket,
            ReceiveOptions {
                target_dir: args.output.clone(),
                overwrite: false,
            },
            &mut |_| {},
        ),
    )
    .await??;
    println!(
        "downloaded {} bytes in {} files ({} skipped)",
        result.bytes,
        result.files,
        result.skipped.len()
    );

    send_message(
        &mut send,
        &ControlMessage::Result {
            bytes: result.bytes,
            files: result.files as u32,
            skipped_bytes: result.skipped_bytes,
            skipped_files: result.skipped.len() as u32,
        },
    )
    .await?;
    wire::await_peer_close(&mut recv, "sender close after result").await?;
    println!("done: files landed in {}", args.output.display());
    let _ = engine.shutdown().await;
    Ok(())
}
