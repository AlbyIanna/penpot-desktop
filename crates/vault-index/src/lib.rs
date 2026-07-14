//! vault-index — Milestone N1: offline full-content search over the vault.
//!
//! A self-contained sidecar cloned from the board-export recipe
//! (`crates/board-export/src/{lib,state}.rs`): it **consumes the sync
//! daemon's outputs and never talks to it** — polls the `.penpot-sync.json`
//! manifest (read-only) and reindexes a file exactly when that file's
//! `lastSyncedHash` moved past the hash recorded in the index db
//! ([`needs_reindex`]).
//!
//! The corpus is the normalized JSON already on disk: text layers, board
//! names, component names, color names/values and typography names
//! ([`extract`]). The index is a bundled-SQLite FTS5 database living OUTSIDE
//! the vault (in the app data dir) — disposable by chapter-2 invariant 1:
//! delete it and it is rebuilt from disk alone, and it is never an input to
//! sync.
//!
//! **Sync-race rule (PLAN2.md risk 6):** the daemon saves the manifest
//! *before* the directory swap lands, so a reader keying off the manifest
//! hash could catch the old tree. This service therefore records the
//! semantic tree hash **of the bytes it actually read**
//! (`sync_core::semantic_view` + `tree_hash`) — if it raced the swap it
//! recorded the old hash, the next poll sees manifest ≠ recorded and
//! reindexes again. Staleness self-heals; it can never stick.
//!
//! No-churn guarantees (mirroring board-export): idle poll cycles only
//! *read* (manifest + one small SELECT); nothing is written unless a hash
//! moved, an entry vanished, or a path was re-keyed.

pub mod db;
pub mod extract;
pub mod query;

mod http;

pub use db::{Hit, IndexDb, SearchError, SearchHandle, SCHEMA_VERSION};
pub use extract::{extract_docs, DocKind, DocRow, ROOT_FRAME_ID};
pub use http::router;
pub use query::{build_match_query, workspace_deep_link};

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sync_core::Manifest;
use tokio::sync::watch;

/// THE reindex decision: reindex iff nothing is recorded for the file or the
/// manifest hash moved past the recorded one. Pure and total (the
/// board-export `needs_render` pattern).
pub fn needs_reindex(last_synced_hash: &str, recorded_hash: Option<&str>) -> bool {
    match recorded_hash {
        Some(h) => h != last_synced_hash,
        None => true,
    }
}

/// Vault-index service configuration.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// The user's designs root (the sync daemon's `sync_root`).
    pub vault_root: PathBuf,
    /// The SQLite index db path — MUST be outside the vault (data dir).
    pub db_path: PathBuf,
    /// Manifest poll interval (default 1 s — reindexing is cheap, unlike
    /// renders, so no extra debounce on top).
    pub poll_interval: Duration,
}

impl IndexConfig {
    pub fn new(vault_root: impl Into<PathBuf>, db_path: impl Into<PathBuf>) -> Self {
        IndexConfig {
            vault_root: vault_root.into(),
            db_path: db_path.into(),
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// Live status snapshot published by the service. Published only on actual
/// change (equality-guarded), so idle polls stay silent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStatusSnapshot {
    /// Manifest files whose recorded hash equals `lastSyncedHash`.
    pub files_indexed: usize,
    /// Manifest files still needing (re)indexing.
    pub files_pending: usize,
    /// Total docs currently in the index.
    pub docs_total: usize,
    /// Write mutations performed since this process started (reindexes,
    /// removals, path re-keys). Stable counter == hash-gated no-op cycles.
    pub mutations: u64,
    /// RFC 3339 UTC time of the last successful reindex.
    pub last_index_at: Option<String>,
    /// Last indexing failure (cleared by the next success).
    pub last_error: Option<String>,
}

/// The indexer's synchronous core — one poll = one manifest diff. Public so
/// tests (and future tooling) can drive it without the service thread.
pub struct Indexer {
    cfg: IndexConfig,
    db: IndexDb,
    status_tx: watch::Sender<IndexStatusSnapshot>,
    mutations: u64,
}

impl Indexer {
    pub fn new(
        cfg: IndexConfig,
        status_tx: watch::Sender<IndexStatusSnapshot>,
    ) -> anyhow::Result<Indexer> {
        let db = IndexDb::open(&cfg.db_path)?;
        Ok(Indexer { cfg, db, status_tx, mutations: 0 })
    }

    fn publish(&self, f: impl FnOnce(&mut IndexStatusSnapshot)) {
        self.status_tx.send_if_modified(|s| {
            let before = s.clone();
            f(s);
            *s != before
        });
    }

    /// One poll cycle: diff manifest hashes against the recorded hashes,
    /// reindex what moved, drop what vanished, re-key what renamed.
    pub fn poll_once(&mut self) {
        let manifest = match Manifest::load(&self.cfg.vault_root) {
            Ok(Some(m)) => m,
            Ok(None) => {
                // Fresh vault: nothing synced yet. Anything already indexed
                // is stale (e.g. the vault was emptied) — drop it.
                self.drop_all_but(&Default::default());
                self.refresh_counts(0, 0);
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "vault-index: cannot read the sync manifest; skipping cycle");
                return;
            }
        };
        let recorded = match self.db.indexed_files() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "vault-index: cannot read index state");
                return;
            }
        };

        let mut indexed = 0usize;
        let mut pending = 0usize;
        for (file_id, entry) in &manifest.files {
            let rec = recorded.get(file_id);
            if !needs_reindex(&entry.last_synced_hash, rec.map(|(_, h)| h.as_str())) {
                // Hash-gated no-op — except a pure path re-key (OS rename).
                if let Some((old_path, _)) = rec {
                    if old_path != &entry.path {
                        match self.db.update_rel_path(file_id, &entry.path) {
                            Ok(()) => {
                                self.mutations += 1;
                                tracing::info!(file = %file_id, path = %entry.path,
                                    "vault-index: re-keyed path (no content change)");
                            }
                            Err(e) => tracing::error!(error = format!("{e:#}"),
                                "vault-index: path re-key failed"),
                        }
                    }
                }
                indexed += 1;
                continue;
            }
            match self.reindex_file(file_id, &entry.path) {
                Ok(reached_manifest_hash) => {
                    self.mutations += 1;
                    if reached_manifest_hash == entry.last_synced_hash {
                        indexed += 1;
                    } else {
                        // Raced a swap (risk 6): recorded what we read; the
                        // next poll will reindex again and converge.
                        pending += 1;
                        tracing::debug!(file = %file_id,
                            "vault-index: tree hash behind the manifest (mid-swap); will re-poll");
                    }
                    self.publish(|s| {
                        s.last_index_at = Some(sync_core::manifest::now_rfc3339());
                        s.last_error = None;
                    });
                }
                Err(e) => {
                    pending += 1;
                    let msg = format!("{}: {e:#}", entry.path);
                    tracing::warn!("vault-index: reindex failed (will retry next poll): {msg}");
                    self.publish(|s| s.last_error = Some(msg));
                }
            }
        }

        self.drop_all_but(&manifest.files.keys().cloned().collect());
        self.refresh_counts(indexed, pending);
    }

    /// Read the tree, extract docs, record THE HASH OF WHAT WAS READ (see
    /// module docs). Returns that hash.
    fn reindex_file(&mut self, file_id: &str, rel_path: &str) -> anyhow::Result<String> {
        let dir = self.cfg.vault_root.join(rel_path);
        let raw = sync_core::read_tree(&dir)?;
        let sem = sync_core::semantic_view(&raw)?;
        let actual_hash = sync_core::tree_hash(&sem);
        let docs = extract::extract_docs(&sem);
        let n = docs.len();
        self.db.replace_file(file_id, rel_path, &actual_hash, &docs)?;
        tracing::info!(path = %rel_path, docs = n, hash = %actual_hash, "vault-index: indexed");
        Ok(actual_hash)
    }

    /// Remove every indexed file whose id is not in `keep`.
    fn drop_all_but(&mut self, keep: &std::collections::BTreeSet<String>) {
        let Ok(recorded) = self.db.indexed_files() else { return };
        for file_id in recorded.keys() {
            if !keep.contains(file_id) {
                match self.db.remove_file(file_id) {
                    Ok(()) => {
                        self.mutations += 1;
                        tracing::info!(file = %file_id, "vault-index: removed (gone from manifest)");
                    }
                    Err(e) => tracing::error!(error = format!("{e:#}"), "vault-index: remove failed"),
                }
            }
        }
    }

    fn refresh_counts(&self, indexed: usize, pending: usize) {
        let docs_total = self.db.docs_total().unwrap_or(0);
        let mutations = self.mutations;
        self.publish(|s| {
            s.files_indexed = indexed;
            s.files_pending = pending;
            s.docs_total = docs_total;
            s.mutations = mutations;
        });
    }
}

// ---------------------------------------------------------------------------
// The service (dedicated thread — rusqlite is synchronous; the async world
// talks to it only through the watch channel and the read-only SearchHandle)
// ---------------------------------------------------------------------------

/// Handle to a spawned vault-index service. Call [`stop`](Self::stop) for an
/// orderly shutdown.
pub struct VaultIndexHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    status_rx: watch::Receiver<IndexStatusSnapshot>,
    db_path: PathBuf,
}

impl VaultIndexHandle {
    /// Live status stream.
    pub fn status(&self) -> watch::Receiver<IndexStatusSnapshot> {
        self.status_rx.clone()
    }

    /// Cheap cloneable read handle for queries.
    pub fn searcher(&self) -> SearchHandle {
        SearchHandle::new(&self.db_path)
    }

    /// Orderly shutdown: finish the in-flight poll, then exit.
    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
    }
}

/// Spawn the vault-index service on its own thread.
pub fn spawn(cfg: IndexConfig) -> VaultIndexHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let (status_tx, status_rx) = watch::channel(IndexStatusSnapshot::default());
    let db_path = cfg.db_path.clone();
    let stop_flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("vault-index".into())
        .spawn(move || run(cfg, status_tx, stop_flag))
        .expect("vault-index thread spawns");
    VaultIndexHandle { stop, thread: Some(thread), status_rx, db_path }
}

fn run(cfg: IndexConfig, status_tx: watch::Sender<IndexStatusSnapshot>, stop: Arc<AtomicBool>) {
    let poll_interval = cfg.poll_interval;
    tracing::info!(
        root = %cfg.vault_root.display(),
        db = %cfg.db_path.display(),
        "vault-index service started"
    );
    let mut indexer = match Indexer::new(cfg, status_tx) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "vault-index: cannot open the index db; service disabled");
            return;
        }
    };
    while !stop.load(Ordering::SeqCst) {
        indexer.poll_once();
        // Sleep in small slices so stop() returns promptly.
        let deadline = std::time::Instant::now() + poll_interval;
        while std::time::Instant::now() < deadline {
            if stop.load(Ordering::SeqCst) {
                tracing::info!("vault-index service stopping");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    tracing::info!("vault-index service stopping");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use sync_core::ManifestEntry;

    #[test]
    fn needs_reindex_decision_table() {
        // Nothing recorded → reindex.
        assert!(needs_reindex("h1", None));
        // Recorded the same hash → up to date, DO NOT reindex (no churn).
        assert!(!needs_reindex("h1", Some("h1")));
        // Hash moved → reindex.
        assert!(needs_reindex("h2", Some("h1")));
    }

    // ---------------- integration: Indexer against a fake vault ----------

    const FID: &str = "3a4be581-6d37-8010-8008-51f0c6eb307f";
    const PID: &str = "3a4be581-6d37-8010-8008-51f0c6eb3080";

    fn write_penpot_dir(root: &Path, rel: &str, board_name: &str, text: &str) {
        let dir = root.join(rel);
        let pages = dir.join(format!("files/{FID}/pages/{PID}"));
        std::fs::create_dir_all(&pages).unwrap();
        let write = |p: &Path, v: serde_json::Value| {
            let mut s = sync_core::dumps(&v);
            s.push('\n');
            std::fs::write(p, s).unwrap();
        };
        write(&dir.join("manifest.json"), json!({"files": [{"id": FID, "name": "f"}]}));
        write(&dir.join(format!("files/{FID}.json")), json!({"id": FID, "name": "f"}));
        write(&dir.join(format!("files/{FID}/pages/{PID}.json")), json!({"id": PID, "name": "Page 1"}));
        write(
            &pages.join("b0000000-0000-0000-0000-000000000001.json"),
            json!({"id": "b0000000-0000-0000-0000-000000000001", "type": "frame", "name": board_name}),
        );
        write(
            &pages.join("t0000000-0000-0000-0000-000000000002.json"),
            json!({"id": "t0000000-0000-0000-0000-000000000002", "type": "text", "name": "txt",
                   "frameId": "b0000000-0000-0000-0000-000000000001",
                   "content": {"type": "root", "children": [{"type": "paragraph-set",
                       "children": [{"type": "paragraph", "children": [{"text": text}]}]}]}}),
        );
    }

    fn manifest_with(root: &Path, entries: &[(&str, &str)]) {
        let mut m = Manifest::default();
        for (fid, rel) in entries {
            let hash = sync_core::semantic_tree_hash(&root.join(rel)).unwrap();
            m.files.insert(
                fid.to_string(),
                ManifestEntry {
                    path: rel.to_string(),
                    project_id: "proj".into(),
                    project_name: "Proj".into(),
                    revn: 1,
                    db_modified_at: String::new(),
                    last_synced_hash: hash,
                    last_synced_at: "2026-07-14T00:00:00Z".into(),
                },
            );
        }
        m.save(root).unwrap();
    }

    fn indexer_for(root: &Path, db: &Path) -> (Indexer, watch::Receiver<IndexStatusSnapshot>) {
        let (tx, rx) = watch::channel(IndexStatusSnapshot::default());
        (Indexer::new(IndexConfig::new(root, db), tx).unwrap(), rx)
    }

    #[test]
    fn full_cycle_index_noop_edit_rename_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        let db_path = tmp.path().join("data/idx.sqlite3");
        std::fs::create_dir_all(&root).unwrap();
        write_penpot_dir(&root, "Proj/home.penpot", "Checkout Flow", "the needle-xyz text");
        manifest_with(&root, &[(FID, "Proj/home.penpot")]);

        let (mut idx, rx) = indexer_for(&root, &db_path);
        idx.poll_once();
        let snap = rx.borrow().clone();
        assert_eq!((snap.files_indexed, snap.files_pending), (1, 0));
        assert_eq!(snap.mutations, 1);
        assert_eq!(snap.docs_total, 2);

        let search = SearchHandle::new(&db_path);
        let expr = build_match_query("needle-xyz").unwrap();
        let hits = search.search(&expr, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_id, FID);
        assert_eq!(hits[0].page_id, PID);
        assert_eq!(hits[0].board_id, "b0000000-0000-0000-0000-000000000001");

        // Idle polls: hash-gated no-op, mutation counter frozen.
        idx.poll_once();
        idx.poll_once();
        assert_eq!(rx.borrow().mutations, 1, "idle cycles must not write");

        // Content edit (hash moves) → reindex; old needle hit GONE.
        write_penpot_dir(&root, "Proj/home.penpot", "Checkout Flow", "renamed to nail-abc");
        manifest_with(&root, &[(FID, "Proj/home.penpot")]);
        idx.poll_once();
        assert_eq!(rx.borrow().mutations, 2);
        assert!(search.search(&expr, None, 10).unwrap().is_empty(), "stale hit must be gone");
        let expr2 = build_match_query("nail-abc").unwrap();
        assert_eq!(search.search(&expr2, None, 10).unwrap().len(), 1);

        // Pure path re-key (rename on disk, same content hash).
        std::fs::rename(root.join("Proj/home.penpot"), root.join("Proj/landing.penpot")).unwrap();
        manifest_with(&root, &[(FID, "Proj/landing.penpot")]);
        idx.poll_once();
        assert_eq!(rx.borrow().mutations, 3);
        let hits = search.search(&expr2, None, 10).unwrap();
        assert_eq!(hits[0].rel_path, "Proj/landing.penpot");

        // Entry vanishes from the manifest → rows dropped.
        manifest_with(&root, &[]);
        idx.poll_once();
        assert_eq!(rx.borrow().docs_total, 0);
        assert!(search.search(&expr2, None, 10).unwrap().is_empty());
    }

    /// Invariant 1: delete the db → next poll rebuilds identical results
    /// from disk alone.
    #[test]
    fn deleting_the_db_rebuilds_identical_results() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        let db_path = tmp.path().join("data/idx.sqlite3");
        std::fs::create_dir_all(&root).unwrap();
        write_penpot_dir(&root, "P/a.penpot", "Board Alpha", "alpha text one");
        manifest_with(&root, &[(FID, "P/a.penpot")]);

        let expr = build_match_query("alpha").unwrap();
        let (mut idx, _rx) = indexer_for(&root, &db_path);
        idx.poll_once();
        let before = SearchHandle::new(&db_path).search(&expr, None, 50).unwrap();
        assert!(!before.is_empty());
        drop(idx);

        // Delete the whole db (incl. WAL sidecars) and rebuild.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(tmp.path().join(format!("data/idx.sqlite3{suffix}")));
        }
        let (mut idx, rx) = indexer_for(&root, &db_path);
        idx.poll_once();
        assert_eq!(rx.borrow().files_indexed, 1);
        let after = SearchHandle::new(&db_path).search(&expr, None, 50).unwrap();
        assert_eq!(before, after, "rebuild from disk must yield identical results");
    }

    /// Risk 6: the manifest may briefly claim a hash the tree doesn't have
    /// yet (manifest saved before the swap). The indexer records what it
    /// READ and converges on the next poll.
    #[test]
    fn mid_swap_read_self_heals() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        let db_path = tmp.path().join("data/idx.sqlite3");
        std::fs::create_dir_all(&root).unwrap();
        write_penpot_dir(&root, "P/a.penpot", "B", "old content");

        // Manifest lies: it already has the hash of the FUTURE tree.
        let future = tmp.path().join("future");
        write_penpot_dir(&future, "P/a.penpot", "B", "new content");
        let future_hash = sync_core::semantic_tree_hash(&future.join("P/a.penpot")).unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            FID.to_string(),
            ManifestEntry {
                path: "P/a.penpot".into(),
                project_id: "proj".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: future_hash,
                last_synced_at: "2026-07-14T00:00:00Z".into(),
            },
        );
        m.save(&root).unwrap();

        let (mut idx, rx) = indexer_for(&root, &db_path);
        idx.poll_once();
        // Indexed the OLD tree but knows it's behind: stays pending.
        assert_eq!(rx.borrow().files_pending, 1);
        assert_eq!(rx.borrow().files_indexed, 0);

        // The swap lands; the next poll converges.
        write_penpot_dir(&root, "P/a.penpot", "B", "new content");
        idx.poll_once();
        assert_eq!(rx.borrow().files_indexed, 1);
        assert_eq!(rx.borrow().files_pending, 0);
        let expr = build_match_query("new").unwrap();
        assert_eq!(SearchHandle::new(&db_path).search(&expr, None, 10).unwrap().len(), 1);
        // And is a no-op afterwards.
        let mutations = rx.borrow().mutations;
        idx.poll_once();
        assert_eq!(rx.borrow().mutations, mutations);
    }
}
