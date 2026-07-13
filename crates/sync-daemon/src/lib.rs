//! sync-daemon — Milestone M3: two-way sync + conflicts.
//!
//! Directions A and B from PLAN.md on top of `sync-core` + `penpot-rpc`:
//!
//! - **Direction A (DB → FS), poll loop**: `get-projects` →
//!   `get-project-files` every [`SyncConfig::poll_interval`]; a file
//!   *changed* iff `(revn, modifiedAt)` differs from the last synced state.
//!   Changes are debounced per file ([`SyncConfig::debounce`], timer reset on
//!   further change), then run through the export pipeline: `export-binfile`
//!   → authenticated download → unzip to a staging sibling → normalize →
//!   semantic tree hash → **no-op discard** if the hash equals the manifest's
//!   `lastSyncedHash` (the target dir's mtimes are never touched) →
//!   otherwise record the hash in the manifest *first*, then two-phase-swap
//!   the staged tree into place.
//! - **Direction B (FS → DB), watcher loop**: a recursive `notify` watcher
//!   on the sync root maps raw events to their owning
//!   `<project>/<name>.penpot` dir and debounces per dir
//!   ([`SyncConfig::fs_debounce`], quiescence-based — event storms from
//!   editors/`git checkout` fire once). On fire: semantic-hash the tree — if
//!   it equals `lastSyncedHash` the event was our own export (or semantic
//!   noise) and is skipped silently (**loop prevention**: Direction A saves
//!   the hash before its swap lands). Otherwise validate (every `.json`
//!   parses; binfile manifest sane — on failure surface a per-file error,
//!   touch nothing), then: DB unmoved since last sync → deterministic-zip →
//!   **in-place import** (`file-id`, kebab-case multipart) → read back
//!   `(revn, modifiedAt)` into manifest + poll tracker so no phantom change
//!   bounces back. New dirs → import-as-new. Deleted dirs → logged loudly,
//!   NEVER deleted DB-side (the next startup reconciliation re-exports).
//! - **Conflict rule** (CLAUDE.md, non-negotiable): both sides changed since
//!   `lastSyncedHash` → export the DB version as
//!   `<name>.conflict-<timestamp>.penpot/` NEXT TO the file, then import the
//!   disk version in place (the folder tree is the source of truth; the DB
//!   version survives in the copy). Applies to the watcher path, the poll
//!   path (a dirty disk blocks any swap) and startup reconciliation.
//!   Conflict copies are never watched, never synced, never auto-deleted,
//!   and are surfaced as [`FileState::Conflict`].
//! - **Startup reconciliation** (before the loops): sweep orphaned
//!   `.tmp-*`/`.old-*` leftovers, then walk disk vs manifest vs DB. On disk
//!   but not in DB → import (**in-place with the manifest's fileId** when the
//!   manifest knows it — the core-invariant path; import-as-new is the
//!   fallback). In DB but not on disk → export. Disk moved, DB unmoved →
//!   import. Both moved → conflict rule. Both present and equal → no-op.
//! - **Status & control**: a `watch` channel broadcasts
//!   [`SyncStatusSnapshot`] (last sync time, per-file [`FileState`], paused,
//!   last error) — see [`SyncDaemonHandle::status`]; [`SyncControl`]
//!   (from [`SyncDaemonHandle::control`]) pauses/resumes — paused = nothing
//!   is touched on disk or in the DB; resume rescans the root.
//! - **Resilience**: every RPC failure is retried with backoff (the backend
//!   crash-respawn window is ~30–60 s); a failed poll cycle is skipped and
//!   NEVER interpreted as "file deleted". Reconciliation is idempotent.
//!
//! Invariants honored (CLAUDE.md): `revn` is advisory — all conflict
//! decisions go through the semantic tree hash; zip containers are never
//! compared, only extracted trees; neither side is ever silently
//! overwritten.

mod engine;
pub mod paths;
pub mod plan;
mod retry;
mod status;
mod tracker;
mod validate;
mod watcher;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use penpot_rpc::PenpotClient;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use status::{FileState, SyncControl, SyncStatusSnapshot};

/// Sync daemon configuration.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Root of the user's designs folder (`<root>/<project>/<file>.penpot/`,
    /// manifest `.penpot-sync.json` at the root). Created if missing.
    pub sync_root: PathBuf,
    /// Team whose projects are synced (the single user's default team).
    pub team_id: String,
    /// DB poll interval (default 2 s).
    pub poll_interval: Duration,
    /// Per-file debounce after a detected DB change (default 3 s, reset on
    /// further change).
    pub debounce: Duration,
    /// Per-file-dir debounce for filesystem events (default 2 s,
    /// quiescence-based: re-armed on every event, so an editor/git event
    /// storm fires once after it settles).
    pub fs_debounce: Duration,
}

impl SyncConfig {
    pub fn new(sync_root: impl Into<PathBuf>, team_id: impl Into<String>) -> Self {
        SyncConfig {
            sync_root: sync_root.into(),
            team_id: team_id.into(),
            poll_interval: Duration::from_secs(2),
            debounce: Duration::from_secs(3),
            fs_debounce: Duration::from_secs(2),
        }
    }
}

/// A snapshot of one file's state in the DB, as seen by the poll surface
/// (`get-project-files` joined with its project's name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFileState {
    pub id: String,
    pub name: String,
    pub project_id: String,
    pub project_name: String,
    /// Advisory only (in-place import resets it) — change *detection* uses it
    /// together with `modified_at`, but never conflict decisions.
    pub revn: i64,
    pub modified_at: String,
}

/// Handle to a spawned daemon. Dropping it does NOT stop the daemon; call
/// [`SyncDaemonHandle::stop`] for an orderly shutdown (in-flight work
/// finishes at the next await point, then the task exits).
pub struct SyncDaemonHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    status_rx: watch::Receiver<SyncStatusSnapshot>,
    control: SyncControl,
}

impl SyncDaemonHandle {
    /// Subscribe to status snapshots (for the tray/menubar UI). The receiver
    /// always holds the latest [`SyncStatusSnapshot`]; `changed()` resolves
    /// whenever a new one is published (published only on actual change).
    pub fn status(&self) -> watch::Receiver<SyncStatusSnapshot> {
        self.status_rx.clone()
    }

    /// Pause/resume handle (cloneable; safe to hand to the UI thread).
    pub fn control(&self) -> SyncControl {
        self.control.clone()
    }

    /// Signal shutdown and wait for the daemon task to finish.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Spawn the sync daemon as a background tokio task: startup reconciliation
/// first (retried until it succeeds), then the poll + watcher loops.
///
/// `client` must be authenticated (access token) against the **backend** base
/// URL; export artifact downloads use the absolute URI from the SSE event
/// (which points at the public/proxy origin), so the proxy must be up.
pub fn spawn(client: PenpotClient, config: SyncConfig) -> SyncDaemonHandle {
    let (shutdown, rx) = watch::channel(false);
    let (hub, status_rx) = status::StatusHub::new();
    let (pause_tx, pause_rx) = watch::channel(false);
    let control = SyncControl::new(Arc::new(pause_tx), hub.clone());
    let task = tokio::spawn(engine::run(client, config, rx, hub, pause_rx));
    SyncDaemonHandle {
        shutdown,
        task,
        status_rx,
        control,
    }
}
