//! Status + control surface for the M3 tray/menubar UI.
//!
//! - [`SyncStatusSnapshot`] is broadcast on a `tokio::sync::watch` channel
//!   (get it from `SyncDaemonHandle::status()`): last successful sync time,
//!   per-file state keyed by the file dir's sync-root-relative path, the
//!   paused flag and the last error message. Snapshots are only published
//!   when something actually changed, so UI consumers can just `changed()`.
//! - [`SyncControl`] (from `SyncDaemonHandle::control()`) pauses/resumes the
//!   daemon. Paused = watcher events are dropped and poll cycles are skipped
//!   — nothing on disk or in the DB is ever half-applied; resume triggers a
//!   full rescan of the sync root (the semantic-hash ledger makes clean
//!   files no-ops), and the next poll cycle picks up DB-side changes.
//! - [`SyncControl::pause_and_wait_idle`] is `pause()` plus a barrier: it
//!   does not return until the engine loop itself confirms (via the idle-ack
//!   watch channel `engine::run` sends on) that nothing is mid-write. Plain
//!   `pause()` only stops FUTURE work — an operation already in flight (a
//!   poll cycle, startup reconciliation) keeps running against its
//!   pre-pause snapshot and can still write to disk/DB after `pause()`
//!   returns. Callers who need "nothing is touching the vault right now"
//!   (D2 delete) must use the barrier, not the bare flag.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Per-file sync state, keyed in [`SyncStatusSnapshot::files`] by the
/// `.penpot` dir's path relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// Disk, manifest and DB agree (as of the last look).
    Synced,
    /// A change was detected (either side) and is waiting out its debounce.
    Pending,
    /// Direction B: the on-disk tree is being imported into the DB.
    Importing,
    /// Direction A: the DB version is being exported to disk.
    Exporting,
    /// Both sides had changed since `lastSyncedHash`: the DB version was
    /// preserved at `copy_path` (relative to the sync root) and the disk
    /// version was imported. The copy is never watched, synced or deleted.
    Conflict { copy_path: String },
    /// The last operation on this file failed; retried when the file
    /// changes again (or at the next startup reconciliation).
    Error { message: String },
}

/// One observable snapshot of the whole daemon, for the status UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncStatusSnapshot {
    /// RFC 3339 UTC time of the last successful sync operation (export,
    /// import or verified no-op). `None` until the first one.
    pub last_sync_at: Option<String>,
    /// Per-file state, keyed by sync-root-relative `.penpot` dir path.
    pub files: BTreeMap<String, FileState>,
    /// True while [`SyncControl::pause`] is in effect.
    pub paused: bool,
    /// Most recent error message (sticky — informational only; per-file
    /// errors live in `files`).
    pub last_error: Option<String>,
}

/// Internal write side of the status channel. Cheap to clone; publishes a
/// new snapshot only when the update actually changed something.
#[derive(Clone)]
pub(crate) struct StatusHub {
    tx: Arc<watch::Sender<SyncStatusSnapshot>>,
}

impl StatusHub {
    pub fn new() -> (Self, watch::Receiver<SyncStatusSnapshot>) {
        let (tx, rx) = watch::channel(SyncStatusSnapshot::default());
        (StatusHub { tx: Arc::new(tx) }, rx)
    }

    fn update(&self, f: impl FnOnce(&mut SyncStatusSnapshot)) {
        self.tx.send_if_modified(|snap| {
            let before = snap.clone();
            f(snap);
            *snap != before
        });
    }

    pub fn set_file(&self, path: &str, state: FileState) {
        self.update(|s| {
            s.files.insert(path.to_string(), state);
        });
    }

    pub fn remove_file(&self, path: &str) {
        self.update(|s| {
            s.files.remove(path);
        });
    }

    /// Record a successful sync operation (bumps `last_sync_at` to now).
    pub fn record_success(&self) {
        let now = sync_core::manifest::now_rfc3339();
        self.update(|s| s.last_sync_at = Some(now));
    }

    pub fn set_paused(&self, paused: bool) {
        self.update(|s| s.paused = paused);
    }

    pub fn set_last_error(&self, message: impl Into<String>) {
        let message = message.into();
        self.update(|s| s.last_error = Some(message));
    }
}

/// Pause/resume handle for the UI (`SyncDaemonHandle::control()`).
///
/// Pausing is coarse and safe: the daemon finishes the operation currently
/// in flight (operations are never half-applied), then processes nothing —
/// watcher events are dropped, poll cycles skipped. Resuming rescans every
/// `.penpot` dir on disk (dropped events are thereby recovered; the hash
/// ledger turns unchanged dirs into silent no-ops) and lets the poll loop
/// pick up DB-side changes on its next cycle.
#[derive(Clone)]
pub struct SyncControl {
    pause: Arc<watch::Sender<bool>>,
    /// The engine loop sends on this every time it returns to the top of
    /// its `select!` with the pause flag observed true — i.e. every branch
    /// that could touch disk/DB has already run to completion. Never sent
    /// while unpaused, so the first publish after [`Self::pause`] flips the
    /// flag is authoritative proof that whatever was mid-flight when the
    /// flag flipped has since finished. See [`Self::pause_and_wait_idle`].
    idle: Arc<watch::Sender<()>>,
    status: StatusHub,
}

/// How long [`SyncControl::pause_and_wait_idle`] waits for the engine's idle
/// ack before giving up. Long enough to cover a normal in-flight poll cycle
/// or single reconciliation pass; short enough that a genuinely stuck daemon
/// (e.g. reconciliation wedged retrying against an unreachable backend)
/// fails the caller fast instead of hanging the request indefinitely.
const PAUSE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Error from [`SyncControl::pause_and_wait_idle`].
#[derive(Debug)]
pub enum PauseAckError {
    /// No idle ack arrived within [`PAUSE_ACK_TIMEOUT`]. An operation may
    /// still be mid-flight (or the daemon may be genuinely stuck). Callers
    /// MUST treat this as a hard failure — proceeding without the ack is
    /// exactly the unprotected state this barrier exists to prevent.
    Timeout,
    /// The engine loop has stopped (its idle-ack sender was dropped); there
    /// is nothing left to wait for.
    DaemonStopped,
}

impl std::fmt::Display for PauseAckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PauseAckError::Timeout => {
                write!(f, "sync daemon did not acknowledge pause within {PAUSE_ACK_TIMEOUT:?}")
            }
            PauseAckError::DaemonStopped => write!(f, "sync daemon is no longer running"),
        }
    }
}

impl std::error::Error for PauseAckError {}

impl SyncControl {
    pub(crate) fn new(
        pause: Arc<watch::Sender<bool>>,
        idle: Arc<watch::Sender<()>>,
        status: StatusHub,
    ) -> Self {
        SyncControl { pause, idle, status }
    }

    pub fn pause(&self) {
        let _ = self.pause.send(true);
        self.status.set_paused(true);
    }

    pub fn resume(&self) {
        let _ = self.pause.send(false);
        self.status.set_paused(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.pause.borrow()
    }

    /// [`Self::pause`] plus a barrier: does not return until the engine
    /// loop's idle ack proves no disk/DB-touching work is in flight. See the
    /// module docs and the `idle` field doc for why the bare flag isn't
    /// enough. A timeout is `Err`, never a silent "probably paused by now".
    pub async fn pause_and_wait_idle(&self) -> Result<(), PauseAckError> {
        self.pause_and_wait_idle_with_timeout(PAUSE_ACK_TIMEOUT).await
    }

    /// The timeout-parameterised implementation, so tests can exercise the
    /// timeout path without a real multi-second wait. Production code always
    /// goes through [`Self::pause_and_wait_idle`].
    async fn pause_and_wait_idle_with_timeout(&self, wait: Duration) -> Result<(), PauseAckError> {
        self.pause();
        // Subscribe AFTER flipping the flag, not before: a fresh receiver
        // treats the sender's current value as already-seen, so this only
        // resolves on a publish that happens from here on — i.e. one the
        // engine sent after observing (or about to observe) `paused = true`.
        let mut ack_rx = self.idle.subscribe();
        match tokio::time::timeout(wait, ack_rx.changed()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(PauseAckError::DaemonStopped),
            Err(_) => Err(PauseAckError::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_publish_only_on_actual_change() {
        let (hub, rx) = StatusHub::new();
        assert!(!rx.has_changed().unwrap());

        hub.set_file("a/x.penpot", FileState::Pending);
        assert!(rx.has_changed().unwrap());
        let mut rx2 = rx.clone();
        rx2.borrow_and_update();

        // Same state again: no new snapshot.
        hub.set_file("a/x.penpot", FileState::Pending);
        assert!(!rx2.has_changed().unwrap());

        // Different state: published.
        hub.set_file("a/x.penpot", FileState::Importing);
        assert!(rx2.has_changed().unwrap());
        assert_eq!(
            rx2.borrow_and_update().files["a/x.penpot"],
            FileState::Importing
        );

        // Removing a missing file: no publish.
        hub.remove_file("nope");
        assert!(!rx2.has_changed().unwrap());
        hub.remove_file("a/x.penpot");
        assert!(rx2.has_changed().unwrap());
        assert!(rx2.borrow_and_update().files.is_empty());
    }

    #[test]
    fn conflict_state_carries_copy_path_and_errors_are_sticky() {
        let (hub, rx) = StatusHub::new();
        hub.set_file(
            "a/x.penpot",
            FileState::Conflict {
                copy_path: "a/x.conflict-2026-07-13T09-04-42Z.penpot".into(),
            },
        );
        hub.set_last_error("boom");
        let snap = rx.borrow().clone();
        assert_eq!(
            snap.files["a/x.penpot"],
            FileState::Conflict {
                copy_path: "a/x.conflict-2026-07-13T09-04-42Z.penpot".into()
            }
        );
        assert_eq!(snap.last_error.as_deref(), Some("boom"));
        // Success elsewhere does not clear the sticky last_error.
        hub.record_success();
        let snap = rx.borrow().clone();
        assert!(snap.last_sync_at.is_some());
        assert_eq!(snap.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn control_pause_resume_roundtrip_and_status_mirrors_it() {
        let (hub, rx) = StatusHub::new();
        let (tx, _prx) = watch::channel(false);
        let (idle_tx, _idle_rx) = watch::channel(());
        let control = SyncControl::new(Arc::new(tx), Arc::new(idle_tx), hub);
        assert!(!control.is_paused());
        control.pause();
        assert!(control.is_paused());
        assert!(rx.borrow().paused);
        control.resume();
        assert!(!control.is_paused());
        assert!(!rx.borrow().paused);
    }

    /// Test-only double for the engine's side of the idle-ack channel: holds
    /// the same `Sender` `SyncControl` subscribes to, so a test can play the
    /// engine's part ("I observed the pause, here's my ack") without
    /// spinning up a real `Engine`/`run` loop.
    ///
    /// Also returns the `pause` channel's `Receiver`: `watch::Sender::send`
    /// silently no-ops (returns `Err`, value NOT stored — see its doc
    /// comment) once its last receiver is dropped, and in production that
    /// receiver is the engine's `pause_rx`, held for the daemon's whole
    /// lifetime. The caller MUST keep this alive for as long as it calls
    /// `pause()`/`is_paused()`, exactly like `engine::run` does — mirrors
    /// `control_pause_resume_roundtrip_and_status_mirrors_it`'s `_prx`.
    fn control_with_fake_engine() -> (SyncControl, Arc<watch::Sender<()>>, watch::Receiver<bool>) {
        let (hub, _rx) = StatusHub::new();
        let (pause_tx, pause_rx) = watch::channel(false);
        let (idle_tx, _idle_rx) = watch::channel(());
        let idle_tx = Arc::new(idle_tx);
        let control = SyncControl::new(Arc::new(pause_tx), idle_tx.clone(), hub);
        (control, idle_tx, pause_rx)
    }

    #[tokio::test]
    async fn pause_and_wait_idle_returns_only_after_the_engine_acks() {
        let (control, fake_engine_idle, _pause_rx) = control_with_fake_engine();

        let waiting = tokio::spawn({
            let control = control.clone();
            async move { control.pause_and_wait_idle().await }
        });

        // Give the spawned task real wall-clock time to reach its await
        // point (pause() + the idle-channel subscribe both happen before
        // that await) without a real engine ever acking. A cooperative
        // `yield_now()` is not enough to guarantee tokio's scheduler has
        // actually polled the freshly spawned task by the time we check.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(control.is_paused(), "pause_and_wait_idle must flip the flag before waiting");
        assert!(!waiting.is_finished(), "must not resolve before the engine's ack arrives");

        // Now play the engine's part: it observed the pause and is idle.
        let _ = fake_engine_idle.send(());

        let result = waiting.await.expect("waiting task must not panic");
        assert!(result.is_ok(), "must resolve once the ack arrives: {result:?}");
    }

    #[tokio::test]
    async fn pause_and_wait_idle_errors_when_the_engine_never_acks() {
        let (control, _fake_engine_idle, _pause_rx) = control_with_fake_engine();

        // Nobody ever sends on the idle channel — simulates a stuck engine
        // (mid-poll-cycle, or wedged retrying against an unreachable
        // backend). A short timeout keeps this test fast; production uses
        // `PAUSE_ACK_TIMEOUT`.
        let result = control.pause_and_wait_idle_with_timeout(Duration::from_millis(20)).await;
        assert!(matches!(result, Err(PauseAckError::Timeout)), "expected a timeout, got {result:?}");
        // The flag stays set: a timeout must not silently behave as if the
        // daemon had paused successfully. The caller (manage.rs's
        // PauseGuard) is responsible for resuming on this error path.
        assert!(control.is_paused());
    }
}
