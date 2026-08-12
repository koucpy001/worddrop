//! Session API tests: lifecycle + cancel propagation + a full in-process
//! sender/receiver pair against the real rendezvous server on loopback
//! (RelayMode::Disabled — the CLI test pattern).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::RelayMode;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use worddrop_core::identity;

use super::*;
use crate::api::ENV_LOCK;
use crate::api::events::BridgeEvent;

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Point config/data dirs at a fresh temp base for the duration of `f`
/// (serialized by ENV_LOCK: env vars are process-global).
fn with_test_env<T>(f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let base = fresh_base("lifecycle");
    let config_dir = base.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    // SAFETY: serialized by ENV_LOCK; restored on drop.
    unsafe {
        std::env::set_var(identity::ENV_CONFIG_DIR, &config_dir);
        std::env::set_var(identity::ENV_DATA_DIR, &base);
    }
    let result = f();
    // SAFETY: serialized by ENV_LOCK.
    unsafe {
        std::env::remove_var(identity::ENV_CONFIG_DIR);
        std::env::remove_var(identity::ENV_DATA_DIR);
    }
    result
}

fn fresh_base(tag: &str) -> std::path::PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("worddrop-bridge-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Spawn the real axum rendezvous server on an ephemeral port and wait until
/// `/health` answers (same pattern as the CLI tests).
fn spawn_rendezvous() -> (String, tokio::task::JoinHandle<()>) {
    let addr = RUNTIME.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);
        addr
    });
    let url = format!("http://{addr}");
    let handle = RUNTIME.spawn(async move {
        let _ = worddrop_rendezvous::server::serve(addr).await;
    });
    let client = worddrop_cli::rendezvous_client::RvClient::new(&url);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let healthy = RUNTIME.block_on(async { client.health().await.is_ok() });
        if healthy {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "rendezvous not healthy");
        std::thread::sleep(Duration::from_millis(50));
    }
    (url, handle)
}

/// Set data/config dirs + rendezvous URL + relay-disabled for a full pair.
fn with_pair_env<T>(rendezvous_url: &str, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let base = fresh_base("pair");
    let config_dir = base.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    // SAFETY: serialized by ENV_LOCK; restored on drop.
    unsafe {
        std::env::set_var(identity::ENV_CONFIG_DIR, &config_dir);
        std::env::set_var(identity::ENV_DATA_DIR, &base);
        std::env::set_var(identity::ENV_RENDEZVOUS_URL, rendezvous_url);
        std::env::set_var(identity::ENV_RELAY_URL, "disabled");
    }
    let result = f();
    // SAFETY: serialized by ENV_LOCK.
    unsafe {
        std::env::remove_var(identity::ENV_CONFIG_DIR);
        std::env::remove_var(identity::ENV_DATA_DIR);
        std::env::remove_var(identity::ENV_RENDEZVOUS_URL);
        std::env::remove_var(identity::ENV_RELAY_URL);
    }
    result
}

fn wait_phase(handle: SessionHandle, expected: &str, timeout_ms: u64) -> Result<String, String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let phase = session_phase(handle)?;
        if phase == expected {
            return Ok(phase);
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("phase {phase:?}, expected {expected:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Drain a session bus until `kind` (or a phase event equal to `phase`) shows
/// up, or the deadline passes. Returns whether it was seen.
fn collect_until(
    mut rx: broadcast::Receiver<BridgeEvent>,
    stop_kind: &str,
    timeout_ms: u64,
) -> Vec<BridgeEvent> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        let event = RUNTIME.block_on(async {
            tokio::time::timeout(remaining, rx.recv()).await.ok().and_then(|r| r.ok())
        });
        match event {
            Some(event) => {
                let stop = event.kind == stop_kind;
                seen.push(event);
                if stop {
                    return seen;
                }
            }
            None => return seen,
        }
    }
}

#[test]
fn create_sender_session_starts_in_created_phase() {
    with_test_env(|| {
        let handle = create_session(SessionRole::Sender).unwrap();
        assert_eq!(session_phase(handle).unwrap(), "created");
        dispose_session(handle).unwrap();
    });
}

#[test]
fn cancel_from_api_reaches_core_cancel_watch() {
    with_test_env(|| {
        let handle = create_session(SessionRole::Sender).unwrap();
        cancel_session(handle).unwrap();
        let phase = wait_phase(handle, "cancelled", 5_000).unwrap();
        assert_eq!(phase, "cancelled");
        // The flow task has terminated the session: further API calls are
        // answered (idempotent cancel) and the session is disposable.
        assert!(cancel_session(handle).is_ok());
        dispose_session(handle).unwrap();
    });
}

#[test]
fn receiver_cancel_propagates_too() {
    with_test_env(|| {
        let handle = create_session(SessionRole::Receiver).unwrap();
        cancel_session(handle).unwrap();
        assert_eq!(wait_phase(handle, "cancelled", 5_000).unwrap(), "cancelled");
        dispose_session(handle).unwrap();
    });
}

#[test]
fn wrong_role_calls_fail_fast() {
    with_test_env(|| {
        let receiver = create_session(SessionRole::Receiver).unwrap();
        assert!(send_paths(receiver, vec!["/tmp/x".into()]).is_err());
        assert!(accept_offer(receiver, String::new()).is_err());

        let sender = create_session(SessionRole::Sender).unwrap();
        assert!(receive_ticket(sender, "1-aaa-bbb-ccc".into()).is_err());
        assert!(decline_offer(sender, "no".into()).is_err());
        assert!(accept_offer(sender, String::new()).is_err());

        dispose_session(sender).unwrap();
        dispose_session(receiver).unwrap();
    });
}

#[test]
fn double_dispose_or_unknown_handle_fails() {
    with_test_env(|| {
        let handle = create_session(SessionRole::Sender).unwrap();
        dispose_session(handle).unwrap();
        assert!(dispose_session(handle).is_err());
        assert!(session_phase(handle).is_err());
    });
}

#[test]
fn relay_mode_parsing() {
    assert!(matches!(relay_mode_from_url("disabled").unwrap(), RelayMode::Disabled));
    assert!(matches!(relay_mode_from_url("OFF").unwrap(), RelayMode::Disabled));
    assert!(matches!(relay_mode_from_url("http://127.0.0.1:3340").unwrap(), RelayMode::Custom(_)));
    assert!(relay_mode_from_url("not a url").is_err());
}

#[test]
fn e2e_pair_transfers_file_end_to_end() {
    let (rendezvous_url, _server) = spawn_rendezvous();
    with_pair_env(&rendezvous_url, || {
        let base = std::env::var(identity::ENV_DATA_DIR).unwrap();
        let base = std::path::PathBuf::from(base);
        let content = b"hello from the bridge e2e\x00\xff";
        let src = base.join("src.txt");
        std::fs::write(&src, content).unwrap();

        let sender = create_session(SessionRole::Sender).unwrap();
        let receiver = create_session(SessionRole::Receiver).unwrap();
        let sender_events = events_sub(sender);
        let receiver_events = events_sub(receiver);

        let prepared = send_paths(sender, vec![src.to_string_lossy().to_string()]).unwrap();
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.total_bytes, content.len() as u64);
        assert_eq!(prepared.files[0].name, "src.txt");
        assert!(prepared.code.contains('-'), "code {code:?} must be nameplate-word-word-word", code = prepared.code);
        assert_eq!(wait_phase(sender, "pending_pair", 5_000).unwrap(), "pending_pair");

        let offer = receive_ticket(receiver, prepared.code.clone()).unwrap();
        assert_eq!(offer.files.len(), 1);
        assert_eq!(offer.files[0].name, "src.txt");
        assert_eq!(offer.total_bytes, content.len() as u64);
        assert_eq!(wait_phase(receiver, "paired", 5_000).unwrap(), "paired");

        let target = base.join("out");
        accept_offer(receiver, target.to_string_lossy().to_string()).unwrap();
        assert_eq!(wait_phase(receiver, "done", 30_000).unwrap(), "done");
        assert_eq!(wait_phase(sender, "done", 30_000).unwrap(), "done");

        let landed = std::fs::read(target.join("src.txt")).unwrap();
        assert_eq!(landed, content);

        // The event stream carried progress on both sides (the receiver's
        // downloading events are guaranteed; the sender's served tick may be
        // skipped on loopback when the transfer outruns the 200ms tick).
        let receiver_events = collect_until(receiver_events, "done", 2_000);
        assert!(receiver_events.iter().any(|event| event.kind == "downloading"));
        assert!(receiver_events.iter().any(|event| event.kind == "done"));
        assert!(receiver_events.iter().any(|event| event.kind == "phase" && event.phase.as_deref() == Some("done")));
        let sender_events = collect_until(sender_events, "done", 2_000);
        assert!(sender_events.iter().any(|event| event.kind == "served" || event.kind == "done"));

        dispose_session(receiver).unwrap();
        dispose_session(sender).unwrap();
    });
}

#[test]
fn e2e_cancel_from_api_propagates_to_both_sides() {
    let (rendezvous_url, _server) = spawn_rendezvous();
    with_pair_env(&rendezvous_url, || {
        let base = std::env::var(identity::ENV_DATA_DIR).unwrap();
        let base = std::path::PathBuf::from(base);
        let src = base.join("cancel.txt");
        std::fs::write(&src, b"cancellable payload").unwrap();

        let sender = create_session(SessionRole::Sender).unwrap();
        let receiver = create_session(SessionRole::Receiver).unwrap();
        let sender_events = events_sub(sender);
        let receiver_events = events_sub(receiver);

        let prepared = send_paths(sender, vec![src.to_string_lossy().to_string()]).unwrap();
        let _offer = receive_ticket(receiver, prepared.code.clone()).unwrap();

        cancel_session(receiver).unwrap();
        assert_eq!(wait_phase(receiver, "cancelled", 10_000).unwrap(), "cancelled");
        // The peer Cancel travels over the control stream: the sender's flow
        // unwinds too.
        assert_eq!(wait_phase(sender, "cancelled", 10_000).unwrap(), "cancelled");
        let receiver_events = collect_until(receiver_events, "cancelled", 2_000);
        assert!(receiver_events.iter().any(|event| event.kind == "phase" && event.phase.as_deref() == Some("cancelled")));
        let sender_events = collect_until(sender_events, "cancelled", 2_000);
        assert!(sender_events.iter().any(|event| event.kind == "phase" && event.phase.as_deref() == Some("cancelled")));

        dispose_session(receiver).unwrap();
        dispose_session(sender).unwrap();
    });
}
