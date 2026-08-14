//! Cache cleanup bridge API: `cleanup_cache()`.
//!
//! Clears the blob caches of both role stores (`<data_dir>/send` and
//! `<data_dir>/receive`) the same way the CLI `cleanup` command does: the
//! whole `<data_dir>/<role>/blobs` directory (blob contents + redb database)
//! is removed and rebuilt empty on the next engine open. Resume records and
//! exported files are untouched. Refuses to run while any transfer session is
//! live — the redb blob store is single-process-exclusive, and a concurrent
//! engine would deadlock on the file lock (D3 lesson).

use iroh::RelayMode;

use worddrop_cli::config::Config;
use worddrop_core::transfer::engine::{EngineSpec, TransferEngine};

use crate::api::RUNTIME;

/// Clear both role blob caches. Returns a bilingual summary of the cleared
/// blob counts, e.g. `已清空发送缓存 2 个 blob / Cleared send cache (2 blobs)
/// \n已清空接收缓存 0 个 blob / Cleared receive cache (0 blobs)`.
///
/// Errors (without touching the cache) when any session is active, or when a
/// role store cannot be opened, read, cleared, or closed. On Windows the
/// removal fails while a store handle is still open — retry after fully
/// closing the app and any other transfer process.
pub fn cleanup_cache() -> Result<String, String> {
    let active = crate::api::session::live_session_count()
        .map_err(|_| "session registry poisoned".to_string())?;
    if active != 0 {
        return Err("有活跃传输，请完成后再清理 / active transfer in progress".to_string());
    }
    RUNTIME.block_on(async {
        crate::api::config::ensure_android_config_dir();
        let cfg = Config::load().map_err(|err| err.to_string())?;
        let mut lines = Vec::with_capacity(2);
        for role in ["send", "receive"] {
            let engine = TransferEngine::new_spec(EngineSpec {
                data_dir: &cfg.data_dir.join(role),
                relay_mode: RelayMode::Disabled,
                secret_key: None,
                extra_handler: None,
                track_served_bytes: false,
            })
            .await
            .map_err(|err| {
                format!("无法打开{role}缓存 / failed to open {role} cache: {err}")
            })?;
            let count = engine
                .store()
                .blobs()
                .list()
                .hashes()
                .await
                .map_err(|err| {
                    format!("无法读取{role}缓存 / failed to read {role} cache: {err}")
                })?
                .len();
            engine.clear_cache().await.map_err(|err| {
                format!(
                    "无法清空{role}缓存，请确认没有进行中的传输后重试（Windows 下文件被占用会失败）/ failed to clear {role} cache, retry after closing any active transfer (Windows may fail while files are in use): {err}"
                )
            })?;
            engine.shutdown().await.map_err(|err| {
                format!("无法关闭{role}引擎 / failed to shut down {role} engine: {err}")
            })?;
            lines.push(format!(
                "已清空{}缓存 {} 个 blob / Cleared {} cache ({} blobs)",
                if role == "send" { "发送" } else { "接收" },
                count,
                role,
                count
            ));
        }
        Ok(lines.join("\n"))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use worddrop_core::identity;

    use super::*;
    use crate::api::session::{SessionRole, create_session, dispose_session};
    use crate::api::ENV_LOCK;

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Point config/data dirs at a fresh temp base and disable the relay for
    /// the duration of `f` (serialized by ENV_LOCK: env vars are
    /// process-global).
    fn with_cache_env<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "worddrop-bridge-cache-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: serialized by ENV_LOCK; restored on drop.
        unsafe {
            std::env::set_var(identity::ENV_CONFIG_DIR, base.join("config"));
            std::env::set_var(identity::ENV_DATA_DIR, &base);
            std::env::set_var(identity::ENV_RELAY_URL, "disabled");
        }
        let result = f();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var(identity::ENV_CONFIG_DIR);
            std::env::remove_var(identity::ENV_DATA_DIR);
            std::env::remove_var(identity::ENV_RELAY_URL);
        }
        let _ = std::fs::remove_dir_all(&base);
        result
    }

    /// Empty caches report 0 blobs cleared (no error).
    #[test]
    fn cleanup_cache_reports_zero_on_empty_caches() {
        with_cache_env(|| {
            let out = cleanup_cache().expect("cleanup runs on empty caches");
            assert_eq!(
                out,
                "已清空发送缓存 0 个 blob / Cleared send cache (0 blobs)\n已清空接收缓存 0 个 blob / Cleared receive cache (0 blobs)"
            );
        });
    }

    /// An active session makes cleanup refuse (Err) and nothing is cleared.
    #[test]
    fn cleanup_cache_refuses_while_session_is_active() {
        with_cache_env(|| {
            let handle =
                create_session(SessionRole::Receiver).expect("session starts with relay disabled");
            let err = cleanup_cache().expect_err("cleanup must refuse while a session is live");
            assert!(
                err.contains("有活跃传输"),
                "err mentions active transfer: {err}"
            );
            dispose_session(handle).expect("dispose session");
        });
    }
}
