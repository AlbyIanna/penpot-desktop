//! Retry-with-backoff, consistent with `sync-daemon/src/retry.rs`: 10 total
//! attempts, 0.5 s doubling capped at 15 s (worst case ≈ 90 s of waiting —
//! rides out the backend/exporter crash-respawn window). Transience is
//! caller-defined so the same helper covers `penpot_rpc::Error` (login,
//! get-file) and [`crate::exporter::RenderError`].

use std::future::Future;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 10;
const BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(15);

/// Transience classifier for RPC errors, mirroring the sync daemon's:
/// transport failures and 5xx retry; 4xx / protocol errors are permanent.
pub(crate) fn rpc_is_transient(err: &penpot_rpc::Error) -> bool {
    match err {
        penpot_rpc::Error::Transport(_) => true,
        penpot_rpc::Error::Rpc { status, .. } => *status >= 500,
        penpot_rpc::Error::Protocol(_) => false,
    }
}

/// Run `op` until it succeeds, a permanent error occurs, or the attempt
/// budget is exhausted.
pub(crate) async fn with_retry<T, E, F, Fut>(
    what: &str,
    is_transient: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
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
                    "transient failure; retrying"
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

    #[derive(Debug)]
    struct Err5xx;
    impl std::fmt::Display for Err5xx {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "503")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_success() {
        let calls = AtomicU32::new(0);
        let out = with_retry("t", |_e: &Err5xx| true, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { if n < 3 { Err(Err5xx) } else { Ok(7u8) } }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_fails_fast_and_budget_exhausts() {
        let calls = AtomicU32::new(0);
        let out: Result<u8, Err5xx> = with_retry("t", |_| false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(Err5xx) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let calls = AtomicU32::new(0);
        let out: Result<u8, Err5xx> = with_retry("t", |_| true, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(Err5xx) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }
}
