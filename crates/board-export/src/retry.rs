//! Retry-with-backoff, consistent with `sync-daemon/src/retry.rs`: 10 total
//! attempts, 0.5 s doubling capped at 15 s (worst case ≈ 90 s of waiting —
//! rides out the backend/exporter crash-respawn window). Transience is
//! caller-defined so the same helper covers `penpot_rpc::Error` (login,
//! get-file) and [`crate::exporter::RenderError`].
//!
//! N2 (post-M5 debt #2): the ladder is **cancellation-aware**. Every retry
//! sleep — and the in-flight operation itself — races a `watch::Receiver<
//! bool>` shutdown flag in a `biased` select with the shutdown branch first,
//! so SIGTERM during a failing render batch aborts within one poll instead
//! of riding out ~90 s of backoff per board (observed 5–7 min exits in M5).

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;

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

/// Why a retried operation did not return a value.
#[derive(Debug)]
pub(crate) enum RetryError<E> {
    /// The shutdown flag flipped (or its sender dropped) — the caller is
    /// winding down; abandon the operation immediately.
    Cancelled,
    /// The operation failed permanently (or the attempt budget ran out).
    Op(E),
}

impl<E: std::fmt::Display> std::fmt::Display for RetryError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryError::Cancelled => write!(f, "cancelled by shutdown"),
            RetryError::Op(e) => e.fmt(f),
        }
    }
}

/// Resolve when the shutdown flag is (or becomes) true. A closed channel
/// (sender dropped) also counts as shutdown — the service never outlives
/// its handle.
async fn cancelled(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Run `op` until it succeeds, a permanent error occurs, the attempt budget
/// is exhausted, or `cancel` flips to true. Cancellation is checked with a
/// `biased` select around BOTH the in-flight operation and every backoff
/// sleep, so shutdown never waits for the ladder.
pub(crate) async fn with_retry<T, E, F, Fut>(
    what: &str,
    is_transient: impl Fn(&E) -> bool,
    cancel: &mut watch::Receiver<bool>,
    mut op: F,
) -> Result<T, RetryError<E>>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut delay = BASE_DELAY;
    let mut attempt = 1;
    loop {
        let result = tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(RetryError::Cancelled),
            result = op() => result,
        };
        match result {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    what,
                    attempt,
                    error = %e,
                    retry_in = ?delay,
                    "transient failure; retrying"
                );
                tokio::select! {
                    biased;
                    _ = cancelled(cancel) => return Err(RetryError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(MAX_DELAY);
                attempt += 1;
            }
            Err(e) => return Err(RetryError::Op(e)),
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

    fn never_cancel() -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        // Leak the sender so the channel stays open for the test's lifetime.
        std::mem::forget(tx);
        rx
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_success() {
        let calls = AtomicU32::new(0);
        let mut cancel = never_cancel();
        let out = with_retry("t", |_e: &Err5xx| true, &mut cancel, || {
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
        let mut cancel = never_cancel();
        let out: Result<u8, RetryError<Err5xx>> = with_retry("t", |_| false, &mut cancel, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(Err5xx) }
        })
        .await;
        assert!(matches!(out, Err(RetryError::Op(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let calls = AtomicU32::new(0);
        let mut cancel = never_cancel();
        let out: Result<u8, RetryError<Err5xx>> = with_retry("t", |_| true, &mut cancel, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(Err5xx) }
        })
        .await;
        assert!(matches!(out, Err(RetryError::Op(_))));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }

    /// N2 bug-B regression: a shutdown signal mid-backoff aborts the ladder
    /// immediately instead of sleeping out the remaining attempts.
    #[tokio::test(start_paused = true)]
    async fn cancel_interrupts_a_retry_sleep() {
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let calls_in = std::sync::Arc::clone(&calls);
        let task = tokio::spawn(async move {
            with_retry::<u8, _, _, _>("t", |_: &Err5xx| true, &mut cancel_rx, move || {
                calls_in.fetch_add(1, Ordering::SeqCst);
                async { Err(Err5xx) }
            })
            .await
        });
        // Let the first attempt fail and the first 500 ms sleep start.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cancel_tx.send(true).unwrap();
        let out = task.await.unwrap();
        assert!(matches!(out, Err(RetryError::Cancelled)));
        // No further attempts happened after cancellation.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Cancellation also wins while an operation itself is in flight (a hung
    /// HTTP call must not block shutdown).
    #[tokio::test(start_paused = true)]
    async fn cancel_interrupts_an_inflight_op() {
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            with_retry::<u8, Err5xx, _, _>("t", |_| true, &mut cancel_rx, || async {
                tokio::time::sleep(Duration::from_secs(3600)).await; // hangs
                Err(Err5xx)
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_tx.send(true).unwrap();
        let out = task.await.unwrap();
        assert!(matches!(out, Err(RetryError::Cancelled)));
    }

    /// A dropped sender (service handle gone) counts as cancellation.
    #[tokio::test(start_paused = true)]
    async fn dropped_sender_counts_as_cancel() {
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        drop(cancel_tx);
        let out = with_retry::<u8, Err5xx, _, _>("t", |_| true, &mut cancel_rx, || async {
            Err(Err5xx)
        })
        .await;
        assert!(matches!(out, Err(RetryError::Cancelled)));
    }
}
