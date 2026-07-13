//! board-export — Milestone M5: per-board SVG/PNG auto-export next to the
//! sources.
//!
//! A self-contained service that **consumes the sync daemon's outputs and
//! never talks to it**: it polls the `.penpot-sync.json` manifest
//! (read-only) and re-renders a file's boards exactly when that file's
//! `lastSyncedHash` moved past the hash recorded in the exports dir's
//! `.exports-state.json` ([`state::needs_render`]).
//!
//! Layout (PLAN.md): for `client-x/homepage.penpot/` the renders live in
//! `client-x/homepage.exports/` — `<board-name>.svg` + `<board-name>.png`
//! per board plus the `.exports-state.json` record, all replaced together in
//! one atomic two-phase directory swap (`sync-core::commit_dir_swap`), so
//! outputs and state can never disagree and a crash never leaves a
//! half-written exports dir (orphaned `.tmp-*`/`.old-*` siblings are swept
//! at startup, mirroring the sync daemon's recovery).
//!
//! No-churn guarantees:
//! - idle poll cycles only *read* (manifest + tiny state files) — nothing on
//!   disk is written or touched unless a hash moved;
//! - renders are debounced ([`ExportConfig::debounce`], re-armed while the
//!   hash keeps moving) because a render costs seconds per board;
//! - renders are serialized (one at a time, deterministic file order).
//!
//! Sync-daemon interplay (verified against its current code, no changes
//! needed): `*.exports/` dirs are invisible to both its watcher
//! (`map_event_path` only maps paths inside `*.penpot` dirs) and its disk
//! walker (`walk_penpot_dirs` only collects `*.penpot` dirs), so writing
//! renders never triggers sync work.
//!
//! Auth (spike-verified): the exporter accepts **only session-cookie auth**
//! (access tokens time out in its headless browser). The service mints a
//! session via `login-with-password` lazily and re-mints it after any render
//! failure. RPC reads (`get-file`) use the regular access token.

mod boards;
mod exporter;
mod names;
mod retry;
mod state;

pub use boards::{list_boards, Board, ROOT_FRAME_ID};
pub use exporter::{artifact_uri, export_payload, ExporterClient, Format, RenderError};
pub use names::{sanitize_stem, unique_stems};
pub use state::{needs_render, BoardRecord, ExportsState, STATE_FILE_NAME};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use penpot_rpc::PenpotClient;
use sync_core::{commit_dir_swap, stage_path_for, Manifest};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Directory suffix for a file's rendered exports (`homepage.exports/`).
pub const EXPORTS_DIR_SUFFIX: &str = ".exports";

/// Live status snapshot published by the service — the tray's "Exports:"
/// line subscribes to this (M5 integration of the `TRAY-HOOK(M5)` note).
/// Published only when something actually changed (`watch::send_if_modified`
/// with an equality guard), so idle poll cycles stay silent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportStatusSnapshot {
    /// Manifest files whose exports dir is up to date with `lastSyncedHash`.
    pub files_up_to_date: usize,
    /// Files armed for a render (debounce window or retry cooldown).
    pub files_pending: usize,
    /// Relative `.penpot` path currently being rendered, if any.
    pub rendering: Option<String>,
    /// RFC 3339 UTC time of the last successful render batch.
    pub last_render_at: Option<String>,
    /// Last render failure (cleared by the next successful render).
    pub last_error: Option<String>,
}

const PENPOT_DIR_SUFFIX: &str = ".penpot";

/// Board-export service configuration.
#[derive(Debug, Clone)]
pub struct ExportConfig {
    /// The user's designs root (same as the sync daemon's `sync_root`).
    pub sync_root: PathBuf,
    /// Exporter service base, e.g. `http://127.0.0.1:6467`.
    pub exporter_base: String,
    /// Backend base URL (for `login-with-password` session minting).
    pub backend_base: String,
    /// Single-user credentials (the exporter needs a session cookie; the
    /// access token deliberately does NOT work — see module docs).
    pub email: String,
    pub password: String,
    /// Profile uuid for the transit payload's `~:profile-id`.
    pub profile_id: String,
    /// Formats rendered per board.
    pub formats: Vec<Format>,
    /// Manifest poll interval (default 2 s, like the sync daemon's DB poll).
    pub poll_interval: Duration,
    /// Debounce after a hash change before rendering (default 3 s, re-armed
    /// while the hash keeps moving — renders are expensive).
    pub debounce: Duration,
    /// Cool-down before re-attempting a file whose render failed after all
    /// in-line retries (default 60 s).
    pub retry_cooldown: Duration,
}

impl ExportConfig {
    pub fn new(
        sync_root: impl Into<PathBuf>,
        exporter_base: impl Into<String>,
        backend_base: impl Into<String>,
        email: impl Into<String>,
        password: impl Into<String>,
        profile_id: impl Into<String>,
    ) -> Self {
        ExportConfig {
            sync_root: sync_root.into(),
            exporter_base: exporter_base.into(),
            backend_base: backend_base.into(),
            email: email.into(),
            password: password.into(),
            profile_id: profile_id.into(),
            formats: vec![Format::Svg, Format::Png],
            poll_interval: Duration::from_secs(2),
            debounce: Duration::from_secs(3),
            retry_cooldown: Duration::from_secs(60),
        }
    }
}

/// Exports dir path (relative, `/`-separators) for a manifest `.penpot`
/// path: `client-x/homepage.penpot` → `client-x/homepage.exports`.
pub fn exports_rel_path(penpot_rel_path: &str) -> String {
    let stem = penpot_rel_path
        .strip_suffix(PENPOT_DIR_SUFFIX)
        .unwrap_or(penpot_rel_path);
    format!("{stem}{EXPORTS_DIR_SUFFIX}")
}

// ---------------------------------------------------------------------------
// Debounce (pure, time-driven — tested with tokio::time::pause)
// ---------------------------------------------------------------------------

/// One armed render: the hash that triggered it and where the file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRender {
    hash: String,
    rel_path: String,
    deadline: Instant,
}

/// Per-file render debounce. `observe` (re)arms when the hash *changes*;
/// an unchanged pending hash keeps its deadline (so a stable-but-dirty file
/// renders `debounce` after the last actual movement, not never).
#[derive(Debug, Default)]
struct RenderQueue {
    pending: HashMap<String, PendingRender>,
}

impl RenderQueue {
    fn observe(&mut self, file_id: &str, hash: &str, rel_path: &str, deadline: Instant) {
        match self.pending.get_mut(file_id) {
            Some(p) if p.hash == hash => {
                // Same dirty hash as already armed: keep the deadline, but
                // track path renames (the sync daemon may re-path entries).
                if p.rel_path != rel_path {
                    p.rel_path = rel_path.to_string();
                }
            }
            _ => {
                self.pending.insert(
                    file_id.to_string(),
                    PendingRender {
                        hash: hash.to_string(),
                        rel_path: rel_path.to_string(),
                        deadline,
                    },
                );
            }
        }
    }

    /// The file no longer needs a render (state caught up / entry gone).
    fn clear(&mut self, file_id: &str) {
        self.pending.remove(file_id);
    }

    /// Drain everything due, sorted by path for deterministic processing.
    fn take_due(&mut self, now: Instant) -> Vec<(String, PendingRender)> {
        let mut due: Vec<(String, PendingRender)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect();
        due.sort_by(|a, b| a.1.rel_path.cmp(&b.1.rel_path));
        for (id, _) in &due {
            self.pending.remove(id);
        }
        due
    }
}

// ---------------------------------------------------------------------------
// Staging + orphan sweep
// ---------------------------------------------------------------------------

/// Write a fully rendered exports tree into `staging`: every `(file_name,
/// bytes)` plus the state record. Files are fsynced; the caller then swaps
/// the directory into place atomically.
fn write_staging(
    staging: &Path,
    files: &[(String, Vec<u8>)],
    state: &ExportsState,
) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(staging)?;
    let write_one = |name: &str, bytes: &[u8]| -> std::io::Result<()> {
        let mut f = std::fs::File::create(staging.join(name))?;
        f.write_all(bytes)?;
        f.sync_all()
    };
    for (name, bytes) in files {
        write_one(name, bytes)?;
    }
    write_one(STATE_FILE_NAME, &state.to_bytes())
}

/// True iff `s` is 12 lowercase hex chars (the sync-core unique-suffix shape
/// used by `stage_path_for`).
fn is_swap_suffix(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// If `name` is `<base>.exports.{tmp|old}-<12hex>`, return `(base, kind)`
/// where base ends in `.exports`.
fn parse_exports_orphan(name: &str) -> Option<(&str, &str)> {
    for kind in ["tmp", "old"] {
        let marker = format!(".{kind}-");
        if let Some(pos) = name.rfind(&marker) {
            let (base, rest) = name.split_at(pos);
            let suffix = &rest[marker.len()..];
            if is_swap_suffix(suffix) && base.ends_with(EXPORTS_DIR_SUFFIX) {
                return Some((base, kind));
            }
        }
    }
    None
}

/// Startup sweep for interrupted exports swaps, mirroring
/// `sync-core::cleanup_orphans` (which only handles `.penpot` bases):
/// `tmp` dirs are always safe to drop (the renders can be regenerated);
/// `old` dirs are restored when their target is missing, dropped otherwise.
/// Does not descend into `.penpot`/`.exports` payload dirs or dot dirs.
fn sweep_exports_orphans(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            match parse_exports_orphan(&name) {
                Some((base, "old")) => {
                    let target = dir.join(base);
                    if target.symlink_metadata().is_ok() {
                        let _ = std::fs::remove_dir_all(&path);
                        tracing::info!(path = %path.display(), "removed orphaned exports .old dir");
                    } else if std::fs::rename(&path, &target).is_ok() {
                        tracing::info!(
                            from = %path.display(),
                            to = %target.display(),
                            "restored interrupted exports swap"
                        );
                    }
                }
                Some((_, _)) => {
                    let _ = std::fs::remove_dir_all(&path);
                    tracing::info!(path = %path.display(), "removed orphaned exports staging dir");
                }
                None => {
                    if !name.starts_with('.')
                        && !name.ends_with(PENPOT_DIR_SUFFIX)
                        && !name.ends_with(EXPORTS_DIR_SUFFIX)
                    {
                        stack.push(path);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Handle to a spawned board-export service. Call [`stop`](Self::stop) for
/// an orderly shutdown (an in-flight render finishes first).
pub struct BoardExportHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    status_rx: watch::Receiver<ExportStatusSnapshot>,
}

impl BoardExportHandle {
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }

    /// The service's live status stream (tray "Exports:" line).
    pub fn status(&self) -> watch::Receiver<ExportStatusSnapshot> {
        self.status_rx.clone()
    }
}

/// Spawn the board-export service as a background task. `rpc` must be
/// authenticated (access token) against the backend base URL — it is used
/// for `get-file` reads only.
pub fn spawn(rpc: PenpotClient, config: ExportConfig) -> BoardExportHandle {
    let (shutdown, rx) = watch::channel(false);
    let (status_tx, status_rx) = watch::channel(ExportStatusSnapshot::default());
    let task = tokio::spawn(run(rpc, config, rx, status_tx));
    BoardExportHandle { shutdown, task, status_rx }
}

struct Service {
    rpc: PenpotClient,
    exporter: ExporterClient,
    cfg: ExportConfig,
    /// Cached session cookie for the exporter (dropped after any failure).
    session: Option<String>,
    /// Status publisher (tray line). Only notifies on actual change.
    status_tx: watch::Sender<ExportStatusSnapshot>,
}

async fn run(
    rpc: PenpotClient,
    cfg: ExportConfig,
    mut shutdown: watch::Receiver<bool>,
    status_tx: watch::Sender<ExportStatusSnapshot>,
) {
    sweep_exports_orphans(&cfg.sync_root);
    let mut service = Service {
        rpc,
        exporter: ExporterClient::new(cfg.exporter_base.clone()),
        cfg: cfg.clone(),
        session: None,
        status_tx,
    };
    let mut queue = RenderQueue::default();
    let mut interval = tokio::time::interval(cfg.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        root = %cfg.sync_root.display(),
        exporter = %cfg.exporter_base,
        formats = ?cfg.formats,
        "board-export service started"
    );
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("board-export service stopping");
                    return;
                }
            }
        }
        service.poll_once(&mut queue).await;
    }
}

impl Service {
    /// Publish a status mutation, notifying watchers only on actual change
    /// (idle poll cycles must not wake the tray).
    fn publish_status(&self, f: impl FnOnce(&mut ExportStatusSnapshot)) {
        self.status_tx.send_if_modified(|s| {
            let before = s.clone();
            f(s);
            *s != before
        });
    }

    /// One poll cycle: diff manifest hashes against exports state, arm/clear
    /// the debounce queue, render whatever is due.
    async fn poll_once(&mut self, queue: &mut RenderQueue) {
        let manifest = match Manifest::load(&self.cfg.sync_root) {
            Ok(Some(m)) => m,
            Ok(None) => return, // fresh root, nothing synced yet
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the sync manifest; skipping this cycle");
                return;
            }
        };
        let now = Instant::now();
        let known: std::collections::HashSet<&str> =
            manifest.files.keys().map(String::as_str).collect();
        // Entries that vanished from the manifest: forget any pending render.
        // (The stale exports dir is deliberately left in place — this service
        // never deletes user-visible outputs; document + let the user prune.)
        let stale: Vec<String> = queue
            .pending
            .keys()
            .filter(|id| !known.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale {
            queue.clear(&id);
        }
        let mut up_to_date = 0usize;
        for (file_id, entry) in &manifest.files {
            let exports_dir = self.cfg.sync_root.join(exports_rel_path(&entry.path));
            let state = ExportsState::load(&exports_dir);
            if needs_render(&entry.last_synced_hash, state.as_ref()) {
                queue.observe(
                    file_id,
                    &entry.last_synced_hash,
                    &entry.path,
                    now + self.cfg.debounce,
                );
            } else {
                up_to_date += 1;
                queue.clear(file_id);
            }
        }
        let pending_count = queue.pending.len();
        self.publish_status(|s| {
            s.files_up_to_date = up_to_date;
            s.files_pending = pending_count;
        });
        for (file_id, pending) in queue.take_due(Instant::now()) {
            self.publish_status(|s| s.rendering = Some(pending.rel_path.clone()));
            match self.render_file(&file_id, &pending).await {
                Ok(n_boards) => {
                    tracing::info!(
                        file = %pending.rel_path,
                        boards = n_boards,
                        hash = %pending.hash,
                        "exports updated"
                    );
                    self.publish_status(|s| {
                        s.rendering = None;
                        s.last_render_at = Some(sync_core::manifest::now_rfc3339());
                        s.last_error = None;
                    });
                }
                Err(e) => {
                    tracing::error!(
                        file = %pending.rel_path,
                        error = format!("{e:#}"),
                        retry_in = ?self.cfg.retry_cooldown,
                        "board export failed"
                    );
                    self.publish_status(|s| {
                        s.rendering = None;
                        s.last_error = Some(format!("{}: {e:#}", pending.rel_path));
                    });
                    // Session may have expired / exporter restarted: re-mint
                    // next time, and cool down before re-attempting.
                    self.session = None;
                    queue.observe(
                        &file_id,
                        &pending.hash,
                        &pending.rel_path,
                        Instant::now() + self.cfg.retry_cooldown,
                    );
                }
            }
        }
    }

    /// Mint (or reuse) the exporter session cookie.
    async fn ensure_session(&mut self) -> anyhow::Result<String> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let login_client = PenpotClient::new(&self.cfg.backend_base);
        let email = self.cfg.email.clone();
        let password = self.cfg.password.clone();
        let outcome = retry::with_retry("login-with-password", retry::rpc_is_transient, || {
            login_client.login_with_password(&email, &password)
        })
        .await?;
        self.session = Some(outcome.auth_token.clone());
        Ok(outcome.auth_token)
    }

    /// Render every board of one file (all formats) into a staging dir and
    /// atomically swap it into `<name>.exports/`.
    async fn render_file(&mut self, file_id: &str, pending: &PendingRender) -> anyhow::Result<usize> {
        let started = std::time::Instant::now();
        let cookie = self.ensure_session().await?;
        let rpc = self.rpc.clone();
        let file = retry::with_retry("get-file", retry::rpc_is_transient, || {
            rpc.get_file(file_id)
        })
        .await?;
        let boards = list_boards(&file);
        let stems = unique_stems(&boards.iter().map(|b| b.name.clone()).collect::<Vec<_>>());

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for (board, stem) in boards.iter().zip(&stems) {
            for format in &self.cfg.formats {
                let render_started = std::time::Instant::now();
                let payload = export_payload(
                    &self.cfg.profile_id,
                    file_id,
                    &board.page_id,
                    &board.object_id,
                    &board.name,
                    *format,
                );
                let bytes = retry::with_retry(
                    "render-board",
                    RenderError::is_transient,
                    || self.exporter.render(&cookie, &payload),
                )
                .await
                .map_err(|e| anyhow::anyhow!("board {:?} ({}): {e}", board.name, format.extension()))?;
                tracing::debug!(
                    board = %board.name,
                    format = format.extension(),
                    bytes = bytes.len(),
                    ms = render_started.elapsed().as_millis() as u64,
                    "board rendered"
                );
                files.push((format!("{stem}.{}", format.extension()), bytes));
            }
        }

        let exports_dir = self.cfg.sync_root.join(exports_rel_path(&pending.rel_path));
        let state = ExportsState {
            schema_version: state::STATE_SCHEMA_VERSION,
            file_id: file_id.to_string(),
            rendered_from_hash: pending.hash.clone(),
            rendered_at: sync_core::manifest::now_rfc3339(),
            boards: boards
                .iter()
                .zip(&stems)
                .map(|(b, stem)| BoardRecord {
                    object_id: b.object_id.clone(),
                    page_id: b.page_id.clone(),
                    name: b.name.clone(),
                    file_stem: stem.clone(),
                })
                .collect(),
        };
        let staging = stage_path_for(&exports_dir);
        if let Err(e) = write_staging(&staging, &files, &state) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(anyhow::anyhow!("writing exports staging: {e}"));
        }
        if let Err(e) = commit_dir_swap(&staging, &exports_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(anyhow::anyhow!("swapping exports dir: {e}"));
        }
        tracing::info!(
            file = %pending.rel_path,
            boards = boards.len(),
            files = files.len(),
            total_ms = started.elapsed().as_millis() as u64,
            "render batch complete"
        );
        Ok(boards.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_rel_path_replaces_the_suffix() {
        assert_eq!(exports_rel_path("client-x/homepage.penpot"), "client-x/homepage.exports");
        assert_eq!(exports_rel_path("root.penpot"), "root.exports");
        // Defensive: a path without the suffix still gets a sibling name.
        assert_eq!(exports_rel_path("odd"), "odd.exports");
    }

    // ---------------- RenderQueue (debounce) ----------------

    #[tokio::test(start_paused = true)]
    async fn queue_fires_after_quiescence_and_once() {
        let mut q = RenderQueue::default();
        let d = Duration::from_secs(3);
        q.observe("f1", "h1", "a/x.penpot", Instant::now() + d);
        tokio::time::advance(Duration::from_millis(2999)).await;
        assert!(q.take_due(Instant::now()).is_empty());
        tokio::time::advance(Duration::from_millis(2)).await;
        let due = q.take_due(Instant::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "f1");
        assert_eq!(due[0].1.hash, "h1");
        assert!(q.take_due(Instant::now()).is_empty(), "drained");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_rearms_on_hash_movement_but_not_on_same_hash() {
        let mut q = RenderQueue::default();
        let d = Duration::from_secs(3);
        q.observe("f1", "h1", "a/x.penpot", Instant::now() + d);
        tokio::time::advance(Duration::from_secs(2)).await;
        // Same hash re-observed (every poll cycle does this): deadline KEPT.
        q.observe("f1", "h1", "a/x.penpot", Instant::now() + d);
        tokio::time::advance(Duration::from_millis(1001)).await;
        assert_eq!(q.take_due(Instant::now()).len(), 1, "fires 3 s after first observation");

        // Moving hash re-arms.
        q.observe("f1", "h1", "a/x.penpot", Instant::now() + d);
        tokio::time::advance(Duration::from_secs(2)).await;
        q.observe("f1", "h2", "a/x.penpot", Instant::now() + d);
        tokio::time::advance(Duration::from_millis(1001)).await;
        assert!(q.take_due(Instant::now()).is_empty(), "re-armed by h2");
        tokio::time::advance(Duration::from_secs(2)).await;
        let due = q.take_due(Instant::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.hash, "h2", "renders the latest hash");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_clear_cancels() {
        let mut q = RenderQueue::default();
        q.observe("f1", "h1", "a/x.penpot", Instant::now());
        q.clear("f1");
        assert!(q.take_due(Instant::now()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn queue_due_is_sorted_by_path() {
        let mut q = RenderQueue::default();
        let now = Instant::now();
        q.observe("f2", "h", "b/y.penpot", now);
        q.observe("f1", "h", "a/x.penpot", now);
        let due = q.take_due(now);
        assert_eq!(
            due.iter().map(|(_, p)| p.rel_path.as_str()).collect::<Vec<_>>(),
            vec!["a/x.penpot", "b/y.penpot"]
        );
    }

    // ---------------- staging + swap (atomic write) ----------------

    fn dummy_state(hash: &str) -> ExportsState {
        ExportsState {
            schema_version: state::STATE_SCHEMA_VERSION,
            file_id: "f1".into(),
            rendered_from_hash: hash.into(),
            rendered_at: "2026-07-13T00:00:00Z".into(),
            boards: vec![],
        }
    }

    #[test]
    fn staging_plus_swap_replaces_the_exports_dir_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let exports = tmp.path().join("home.exports");

        // First render: two boards.
        let staging = stage_path_for(&exports);
        write_staging(
            &staging,
            &[("A.svg".into(), b"svg-a".to_vec()), ("A.png".into(), b"png-a".to_vec())],
            &dummy_state("h1"),
        )
        .unwrap();
        commit_dir_swap(&staging, &exports).unwrap();
        assert_eq!(std::fs::read(exports.join("A.svg")).unwrap(), b"svg-a");
        assert_eq!(
            ExportsState::load(&exports).unwrap().rendered_from_hash,
            "h1"
        );

        // Second render: board renamed — the old files must be GONE (whole
        // dir replaced, no stale outputs).
        let staging = stage_path_for(&exports);
        write_staging(&staging, &[("B.svg".into(), b"svg-b".to_vec())], &dummy_state("h2"))
            .unwrap();
        commit_dir_swap(&staging, &exports).unwrap();
        assert!(!exports.join("A.svg").exists());
        assert!(!exports.join("A.png").exists());
        assert_eq!(std::fs::read(exports.join("B.svg")).unwrap(), b"svg-b");
        assert_eq!(ExportsState::load(&exports).unwrap().rendered_from_hash, "h2");
        // No staging/old leftovers.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-")
                || e.file_name().to_string_lossy().contains(".old-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // ---------------- status channel (tray integration) ----------------

    fn offline_service(root: &Path, tx: watch::Sender<ExportStatusSnapshot>) -> Service {
        let cfg = ExportConfig::new(
            root,
            "http://127.0.0.1:9",
            "http://127.0.0.1:9",
            "x@local",
            "pw",
            "profile",
        );
        Service {
            rpc: PenpotClient::new("http://127.0.0.1:9"),
            exporter: ExporterClient::new(cfg.exporter_base.clone()),
            cfg,
            session: None,
            status_tx: tx,
        }
    }

    fn manifest_with_entry(root: &Path, file_id: &str, rel_path: &str, hash: &str) {
        let mut manifest = Manifest::default();
        manifest.files.insert(
            file_id.to_string(),
            sync_core::manifest::ManifestEntry {
                path: rel_path.to_string(),
                project_id: "proj".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: hash.to_string(),
                last_synced_at: "2026-07-13T00:00:00Z".into(),
            },
        );
        manifest.save(root).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn status_reflects_pending_and_up_to_date_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (tx, rx) = watch::channel(ExportStatusSnapshot::default());
        let mut service = offline_service(root, tx);
        let mut queue = RenderQueue::default();

        // A manifest entry with no exports state: pending (debounce armed,
        // not yet due — poll_once must not try to render offline).
        manifest_with_entry(root, "f1", "proj/home.penpot", "h1");
        service.poll_once(&mut queue).await;
        let snap = rx.borrow().clone();
        assert_eq!((snap.files_pending, snap.files_up_to_date), (1, 0));
        assert_eq!(snap.rendering, None);

        // Exports state catches up with the hash: up to date, queue cleared.
        let exports = root.join("proj/home.exports");
        std::fs::create_dir_all(&exports).unwrap();
        std::fs::write(
            exports.join(STATE_FILE_NAME),
            ExportsState {
                schema_version: state::STATE_SCHEMA_VERSION,
                file_id: "f1".into(),
                rendered_from_hash: "h1".into(),
                rendered_at: "2026-07-13T00:00:00Z".into(),
                boards: vec![],
            }
            .to_bytes(),
        )
        .unwrap();
        service.poll_once(&mut queue).await;
        let snap = rx.borrow().clone();
        assert_eq!((snap.files_pending, snap.files_up_to_date), (0, 1));
        assert_eq!(snap.last_error, None);
    }

    #[tokio::test(start_paused = true)]
    async fn status_records_render_failure_and_idle_cycles_stay_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (tx, mut rx) = watch::channel(ExportStatusSnapshot::default());
        let mut service = offline_service(root, tx);
        // Fast retries so the offline render fails quickly under paused time.
        service.cfg.debounce = Duration::from_millis(1);
        let mut queue = RenderQueue::default();
        manifest_with_entry(root, "f1", "proj/home.penpot", "h1");

        // Arm, then let the debounce elapse: the render runs and fails
        // (nothing listens on 127.0.0.1:9), which must surface in the status.
        service.poll_once(&mut queue).await;
        tokio::time::advance(Duration::from_millis(2)).await;
        service.poll_once(&mut queue).await;
        let snap = rx.borrow_and_update().clone();
        assert!(snap.last_error.is_some(), "offline render failure must be recorded");
        assert!(snap.last_error.as_deref().unwrap().contains("proj/home.penpot"));
        assert_eq!(snap.rendering, None, "rendering flag cleared after the attempt");

        // Idle cycle with an unchanged world: no new notification.
        // (The file sits in its retry cooldown → counts do not change.)
        service.poll_once(&mut queue).await;
        assert!(!rx.has_changed().unwrap(), "idle poll must not publish");
    }

    // ---------------- orphan sweep ----------------

    #[test]
    fn orphan_name_parsing() {
        assert_eq!(
            parse_exports_orphan("home.exports.tmp-0123456789ab"),
            Some(("home.exports", "tmp"))
        );
        assert_eq!(
            parse_exports_orphan("home.exports.old-abcdef012345"),
            Some(("home.exports", "old"))
        );
        assert_eq!(parse_exports_orphan("home.exports"), None);
        assert_eq!(parse_exports_orphan("home.penpot.tmp-0123456789ab"), None);
        assert_eq!(parse_exports_orphan("home.exports.tmp-SHOUTY12345A"), None);
        assert_eq!(parse_exports_orphan("home.exports.tmp-123"), None);
    }

    #[test]
    fn sweep_removes_tmp_restores_old_and_leaves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Live exports dir + orphaned tmp next to it → tmp removed.
        std::fs::create_dir_all(root.join("c/home.exports")).unwrap();
        std::fs::create_dir_all(root.join("c/home.exports.tmp-0123456789ab")).unwrap();
        // old with MISSING target → restored.
        std::fs::create_dir_all(root.join("c/gone.exports.old-0123456789ab")).unwrap();
        std::fs::write(root.join("c/gone.exports.old-0123456789ab/x.svg"), b"x").unwrap();
        // old with intact target → removed.
        std::fs::create_dir_all(root.join("c/done.exports")).unwrap();
        std::fs::create_dir_all(root.join("c/done.exports.old-0123456789ab")).unwrap();
        // Unrelated dirs untouched; .penpot payload not descended into.
        std::fs::create_dir_all(root.join("c/file.penpot/files")).unwrap();

        sweep_exports_orphans(root);

        assert!(!root.join("c/home.exports.tmp-0123456789ab").exists());
        assert!(root.join("c/home.exports").is_dir());
        assert!(root.join("c/gone.exports").join("x.svg").is_file(), "old restored");
        assert!(!root.join("c/gone.exports.old-0123456789ab").exists());
        assert!(!root.join("c/done.exports.old-0123456789ab").exists());
        assert!(root.join("c/done.exports").is_dir());
        assert!(root.join("c/file.penpot/files").is_dir());
    }
}
