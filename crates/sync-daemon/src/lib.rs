//! sync-daemon — Milestone M2: one-way sync (DB → FS) + startup reconciliation.
//!
//! Direction A from PLAN.md on top of `sync-core` + `penpot-rpc`:
//!
//! - **Poll loop**: `get-projects` → `get-project-files` every
//!   [`SyncConfig::poll_interval`]; a file *changed* iff `(revn, modifiedAt)`
//!   differs from the last synced state. Changes are debounced per file
//!   ([`SyncConfig::debounce`], timer reset on further change), then run
//!   through the export pipeline: `export-binfile` (embed-assets, no
//!   libraries) → authenticated download → unzip to a staging sibling →
//!   normalize → semantic tree hash → **no-op discard** if the hash equals
//!   the manifest's `lastSyncedHash` (the target dir's mtimes are never
//!   touched) → otherwise record the hash in the manifest *first*, then
//!   two-phase-swap the staged tree into place.
//! - **Startup reconciliation** (before the first poll): sweep orphaned
//!   `.tmp-*`/`.old-*` leftovers, then walk disk vs manifest vs DB. On disk
//!   but not in DB → import (**in-place with the manifest's fileId** when the
//!   manifest knows it — this is the core-invariant path that resurrects
//!   files under the same id after a DB wipe; import-as-new is the fallback).
//!   In DB but not on disk → export. Both present and semantically equal →
//!   no-op. Both changed → M2 is one-way: the DB wins, logged loudly
//!   (TODO(M3 conflict rule): write a `.conflict-<ts>.penpot` copy instead).
//! - **Resilience**: every RPC failure is retried with backoff (the backend
//!   crash-respawn window is ~30–60 s); a failed poll cycle is skipped and
//!   NEVER interpreted as "file deleted". Reconciliation is idempotent.
//!
//! Invariants honored (CLAUDE.md): `revn` is advisory — all comparisons go
//! through the semantic tree hash; zip containers are never compared, only
//! extracted trees; neither side is ever silently overwritten *without a loud
//! conflict log* (M2 limitation, full conflict copies land in M3).

mod engine;
pub mod paths;
pub mod plan;
mod retry;
mod tracker;

use std::path::PathBuf;
use std::time::Duration;

use penpot_rpc::PenpotClient;
use tokio::sync::watch;
use tokio::task::JoinHandle;

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
    /// Per-file debounce after a detected change (default 3 s, reset on
    /// further change).
    pub debounce: Duration,
}

impl SyncConfig {
    pub fn new(sync_root: impl Into<PathBuf>, team_id: impl Into<String>) -> Self {
        SyncConfig {
            sync_root: sync_root.into(),
            team_id: team_id.into(),
            poll_interval: Duration::from_secs(2),
            debounce: Duration::from_secs(3),
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
/// [`SyncDaemonHandle::stop`] for an orderly shutdown (in-flight export
/// finishes at the next await point, then the task exits).
pub struct SyncDaemonHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl SyncDaemonHandle {
    /// Signal shutdown and wait for the daemon task to finish.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Spawn the sync daemon as a background tokio task: startup reconciliation
/// first (retried until it succeeds), then the poll loop.
///
/// `client` must be authenticated (access token) against the **backend** base
/// URL; export artifact downloads use the absolute URI from the SSE event
/// (which points at the public/proxy origin), so the proxy must be up.
pub fn spawn(client: PenpotClient, config: SyncConfig) -> SyncDaemonHandle {
    let (shutdown, rx) = watch::channel(false);
    let task = tokio::spawn(engine::run(client, config, rx));
    SyncDaemonHandle { shutdown, task }
}
