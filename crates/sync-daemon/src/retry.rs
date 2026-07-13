//! Retry-with-backoff for RPC calls.
//!
//! The backend crash-respawn window is ~30–60 s (m1.md), so transient
//! failures — transport errors (connection refused while the JVM restarts)
//! and 5xx (the proxy answers 502 while the backend is down) — are retried
//! long enough to ride out a respawn. 4xx and protocol-shape errors are
//! permanent and returned immediately. An RPC failure must NEVER be
//! interpreted as "file deleted".

use std::future::Future;
use std::time::Duration;

/// Errors worth retrying: transport-level failures and server-side 5xx.
pub(crate) fn is_transient(err: &penpot_rpc::Error) -> bool {
    match err {
        penpot_rpc::Error::Transport(_) => true,
        penpot_rpc::Error::Rpc { status, .. } => *status >= 500,
        penpot_rpc::Error::Protocol(_) => false,
    }
}

/// Total attempts (1 initial + 9 retries) and the backoff ramp: 0.5 s
/// doubling, capped at 15 s → worst case ≈ 90 s of waiting, covering the
/// 30–60 s backend respawn window with margin.
const MAX_ATTEMPTS: u32 = 10;
const BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(15);

/// Run `op` until it succeeds, a permanent error occurs, or the attempt
/// budget is exhausted.
pub(crate) async fn with_retry<T, F, Fut>(what: &str, mut op: F) -> penpot_rpc::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = penpot_rpc::Result<T>>,
{
    let mut delay = BASE_DELAY;
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    what,
                    attempt,
                    error = %e,
                    retry_in = ?delay,
                    "transient rpc failure; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_DELAY);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn transport_err() -> penpot_rpc::Error {
        // Force a reqwest error via an invalid URL scheme.
        let err = reqwest_error();
        penpot_rpc::Error::Transport(err)
    }

    fn reqwest_error() -> reqwest::Error {
        // Build a genuine reqwest::Error without the network: an invalid
        // request construction.
        reqwest::Client::new()
            .get("this is not a url")
            .build()
            .unwrap_err()
    }

    fn rpc_err(status: u16) -> penpot_rpc::Error {
        penpot_rpc::Error::Rpc {
            status,
            error_type: None,
            code: None,
            body: serde_json::Value::Null,
        }
    }

    #[test]
    fn transience_classification() {
        assert!(is_transient(&transport_err()));
        assert!(is_transient(&rpc_err(500)));
        assert!(is_transient(&rpc_err(502)));
        assert!(!is_transient(&rpc_err(400)));
        assert!(!is_transient(&rpc_err(404)));
        assert!(!is_transient(&penpot_rpc::Error::Protocol("bad sse".into())));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_until_success() {
        let calls = AtomicU32::new(0);
        let out = with_retry("test", || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 3 {
                    Err(rpc_err(502))
                } else {
                    Ok(42u32)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_error_fails_fast() {
        let calls = AtomicU32::new(0);
        let out: penpot_rpc::Result<u32> = with_retry("test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(rpc_err(400)) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn budget_exhausts_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let out: penpot_rpc::Result<u32> = with_retry("test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(rpc_err(503)) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }
}
