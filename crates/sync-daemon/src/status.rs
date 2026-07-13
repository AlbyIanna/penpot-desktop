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

use std::collections::BTreeMap;
use std::sync::Arc;

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
    status: StatusHub,
}

impl SyncControl {
    pub(crate) fn new(pause: Arc<watch::Sender<bool>>, status: StatusHub) -> Self {
        SyncControl { pause, status }
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
        let control = SyncControl::new(Arc::new(tx), hub);
        assert!(!control.is_paused());
        control.pause();
        assert!(control.is_paused());
        assert!(rx.borrow().paused);
        control.resume();
        assert!(!control.is_paused());
        assert!(!rx.borrow().paused);
    }
}
