//! The daemon engine: startup reconciliation, poll loop (Direction A),
//! filesystem watcher loop (Direction B), export/import pipelines and the
//! conflict rule. Pure decision logic lives in [`crate::plan`]; DB debounce
//! state in [`crate::tracker`]; FS debounce + event mapping in
//! [`crate::watcher`]; tree validation in [`crate::validate`]. This module
//! does the I/O.
//!
//! **The conflict rule** (CLAUDE.md, non-negotiable): when both the DB and
//! the on-disk tree changed since `lastSyncedHash`, the DB version is
//! exported to `<name>.conflict-<ts>.penpot/` NEXT TO the file (never over
//! it), then the disk version — the source of truth — is imported in place.
//! Nothing is ever silently overwritten; conflict copies are never watched,
//! never synced, never auto-deleted.
//!
//! **Loop prevention**: Direction A records the new `lastSyncedHash` in the
//! manifest BEFORE its dir swap lands, so when the watcher sees the swap and
//! the FS debounce fires, the tree's hash is already in the ledger and the
//! event is skipped silently. Symmetrically, Direction B reads back the
//! post-import `(revn, modifiedAt)` and seeds both the manifest and the poll
//! tracker before the next poll cycle can run, so its own import never looks
//! like a DB change.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use penpot_rpc::{PenpotClient, ProjectInfo};
use sync_core::{
    cleanup_orphans, commit_dir_swap, manifest::now_rfc3339, normalize_tree, semantic_tree_hash,
    stage_path_for, unzip_to, zip_dir, Manifest, ManifestEntry,
};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use crate::paths;
use crate::plan::{
    db_moved, decide, folder_of, plan_rekeys, DbFacts, Decision, DiskFacts, ManifestFacts,
    MissingEntry, RekeyOp, SurvivingEntry, UnclaimedDir,
};
use crate::retry::with_retry;
use crate::status::{FileState, StatusHub};
use crate::tracker::ChangeTracker;
use crate::validate::validate_tree;
use crate::watcher::{self, FsDebounce};
use crate::{DbFileState, SyncConfig};

/// Outcome of one export-pipeline run.
#[derive(Debug, PartialEq, Eq)]
enum ExportOutcome {
    /// Semantic hash unchanged (or both sides converged on the same
    /// content): staged tree discarded, target untouched.
    NoOp,
    /// Staged tree swapped into place.
    Updated,
    /// Both sides had changed: the staged DB tree became a conflict copy and
    /// the disk version was imported in place.
    Conflict { copy_path: String },
}

/// One poll of the whole team: live projects + all their files.
struct DbSnapshot {
    projects: Vec<ProjectInfo>,
    files: HashMap<String, DbFileState>,
}

async fn fetch_snapshot(client: &PenpotClient, team_id: &str) -> penpot_rpc::Result<DbSnapshot> {
    let projects = client.get_projects(team_id).await?;
    let mut files = HashMap::new();
    for p in projects.iter().filter(|p| p.deleted_at.is_none()) {
        for f in client.get_project_files(&p.id).await? {
            if f.deleted_at.is_some() {
                continue;
            }
            files.insert(
                f.id.clone(),
                DbFileState {
                    id: f.id,
                    name: f.name,
                    project_id: p.id.clone(),
                    project_name: p.name.clone(),
                    revn: f.revn,
                    modified_at: f.modified_at,
                },
            );
        }
    }
    Ok(DbSnapshot { projects, files })
}

/// Fresh DB facts about one file, fetched for Direction B decisions.
#[derive(Debug, Clone)]
struct DbFileFacts {
    revn: i64,
    modified_at: String,
    project_id: String,
    name: String,
}

/// Is this RPC error "the object does not exist" (as opposed to a transport
/// or server failure)? Used to distinguish "file absent from the DB" from
/// "cannot tell right now" — only the former may trigger the resurrect path.
fn is_not_found(err: &penpot_rpc::Error) -> bool {
    match err {
        penpot_rpc::Error::Rpc {
            status,
            code,
            error_type,
            ..
        } => {
            *status == 404
                || code.as_deref() == Some("object-not-found")
                || error_type.as_deref() == Some("not-found")
        }
        _ => false,
    }
}

/// Resolve/create DB projects during imports, with a cache so several files
/// of one on-disk project share the (re)created DB project.
struct ProjectResolver {
    projects: Vec<ProjectInfo>,
}

impl ProjectResolver {
    fn new(projects: Vec<ProjectInfo>) -> Self {
        ProjectResolver { projects }
    }

    /// Preferred id if it still exists → a live project matching `name`
    /// (exactly, or via its sanitized form — folder names are sanitized) →
    /// create a new project named `name`.
    async fn ensure(
        &mut self,
        client: &PenpotClient,
        team_id: &str,
        preferred_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<(String, String)> {
        if let Some(pid) = preferred_id {
            if let Some(p) = self
                .projects
                .iter()
                .find(|p| p.id == pid && p.deleted_at.is_none())
            {
                return Ok((p.id.clone(), p.name.clone()));
            }
        }
        if let Some(p) = self.projects.iter().find(|p| {
            p.deleted_at.is_none()
                && (p.name == name || paths::sanitize_component(&p.name) == name)
        }) {
            return Ok((p.id.clone(), p.name.clone()));
        }
        let created = with_retry("create-project", || {
            let c = client.clone();
            let t = team_id.to_string();
            let n = name.to_string();
            async move { c.create_project(&t, &n).await }
        })
        .await
        .with_context(|| format!("create-project {name:?}"))?;
        tracing::info!(project = %created.id, name = %created.name, "created project in DB (import)");
        let out = (created.id.clone(), created.name.clone());
        self.projects.push(created);
        Ok(out)
    }
}

/// Reconciliation work items (owned, so the manifest can be mutated while
/// executing them).
#[derive(Debug)]
enum ReconcileAction {
    Forget {
        file_id: String,
    },
    ImportInPlace {
        file_id: String,
        rel: String,
        disk_hash: String,
    },
    ImportAsNew {
        rel: String,
        disk_hash: String,
    },
    /// Runs the export pipeline; `conflict_expected` only marks the log —
    /// the pipeline itself detects a dirty disk and applies the conflict
    /// rule (staged DB tree → conflict copy, disk imported in place).
    Export {
        file_id: String,
        conflict_expected: bool,
    },
    Noop {
        file_id: String,
    },
}

pub(crate) struct Engine {
    client: PenpotClient,
    cfg: SyncConfig,
    manifest: Manifest,
    tracker: ChangeTracker,
    fs_debounce: FsDebounce,
    status: StatusHub,
    /// Debounced deadline for a *structural sweep* (M5): project-folder
    /// renames/moves fire watcher events only for the folder itself (FSEvents
    /// never notifies for children), so those events arm this instead of a
    /// per-file-dir timer. On fire: re-key pass + route leftovers through the
    /// normal per-dir handling.
    sweep_deadline: Option<Instant>,
}

/// Daemon entry point (spawned by [`crate::spawn`]).
pub(crate) async fn run(
    client: PenpotClient,
    cfg: SyncConfig,
    mut shutdown: watch::Receiver<bool>,
    status: StatusHub,
    mut pause_rx: watch::Receiver<bool>,
) {
    let poll_interval = cfg.poll_interval;
    let mut engine = match Engine::new(client, cfg, status) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "sync daemon failed to initialize (manifest unreadable?); NOT resetting anything — fix the manifest and restart");
            return;
        }
    };

    // Watch the sync root from the very start so external edits made during
    // reconciliation are not lost (they queue in the channel and are drained
    // below). A watcher failure downgrades to poll-only + reconciliation.
    let (fs_tx, mut fs_rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let _watcher = match watcher::start(&engine.cfg.sync_root, fs_tx) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::error!(error = %e, "fs watcher failed to start; Direction B degraded to startup reconciliation only");
            engine.status.set_last_error(format!("fs watcher failed: {e}"));
            None
        }
    };

    // Startup reconciliation: retried forever (the backend may still be
    // settling); must complete before the sync loop starts. A paused daemon
    // waits — it must not touch disk or DB.
    loop {
        if *shutdown.borrow() {
            return;
        }
        if *pause_rx.borrow() {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = pause_rx.changed() => {}
            }
            continue;
        }
        match engine.reconcile().await {
            Ok(()) => break,
            Err(e) => {
                tracing::error!(
                    error = format!("{e:#}"),
                    "startup reconciliation failed; retrying in 5s (reconciliation is idempotent)"
                );
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }

    // Drain events that arrived during reconciliation (mostly our own
    // exports; the hash ledger silences those).
    while let Ok(path) = fs_rx.try_recv() {
        engine.note_fs_event(&path);
    }

    tracing::info!(root = %engine.cfg.sync_root.display(), "sync daemon: reconciliation complete; polling every {poll_interval:?}, watching the filesystem");

    let mut poll_tick = tokio::time::interval(poll_interval);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Fine-grained tick so FS debounce deadlines fire promptly.
    let mut fs_tick = tokio::time::interval(Duration::from_millis(250));
    fs_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("sync daemon stopping");
                return;
            }
            _ = pause_rx.changed() => {
                let paused = *pause_rx.borrow();
                if paused {
                    // Pending work is dropped, never half-applied; resume
                    // recovers it with a full rescan.
                    engine.fs_debounce.clear();
                    engine.sweep_deadline = None;
                    tracing::info!("sync paused");
                } else {
                    tracing::info!("sync resumed; rescanning the sync root");
                    engine.rescan_disk();
                }
            }
            Some(path) = fs_rx.recv() => {
                if !*pause_rx.borrow() {
                    engine.note_fs_event(&path);
                }
            }
            _ = fs_tick.tick() => {
                if !*pause_rx.borrow() {
                    engine.process_due_fs_changes().await;
                }
            }
            _ = poll_tick.tick() => {
                if !*pause_rx.borrow() {
                    engine.poll_cycle().await;
                }
            }
        }
    }
}

impl Engine {
    fn new(client: PenpotClient, mut cfg: SyncConfig, status: StatusHub) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.sync_root)
            .with_context(|| format!("cannot create sync root {}", cfg.sync_root.display()))?;
        // Canonicalize so watcher event paths (which the OS reports resolved,
        // e.g. /private/tmp vs /tmp on macOS) strip cleanly against the root.
        cfg.sync_root = cfg
            .sync_root
            .canonicalize()
            .with_context(|| format!("cannot canonicalize {}", cfg.sync_root.display()))?;
        // A corrupt/newer-schema manifest is a hard error — never silently
        // reset (that would turn every disk dir into an import-as-new).
        let manifest = Manifest::load(&cfg.sync_root)
            .context("loading .penpot-sync.json")?
            .unwrap_or_default();
        Ok(Engine {
            client,
            cfg,
            manifest,
            tracker: ChangeTracker::new(),
            fs_debounce: FsDebounce::new(),
            status,
            sweep_deadline: None,
        })
    }

    // ------------------------------------------------------------------
    // Direction B: filesystem events
    // ------------------------------------------------------------------

    /// Ingest one raw watcher event path: map it to its owning `.penpot` dir
    /// and (re)arm that dir's debounce timer. Paths outside any `.penpot` dir
    /// (project folders — the only watcher trace of a folder rename/move on
    /// macOS) arm the structural-sweep timer instead.
    fn note_fs_event(&mut self, path: &Path) {
        if let Some(rel) = watcher::map_event_path(&self.cfg.sync_root, path) {
            self.fs_debounce
                .arm(rel.clone(), Instant::now(), self.cfg.fs_debounce);
            self.status.set_file(&rel, FileState::Pending);
        } else if watcher::is_structural_event(&self.cfg.sync_root, path) {
            self.sweep_deadline = Some(Instant::now() + self.cfg.fs_debounce);
        }
    }

    /// Resume-time rescan: re-arm every on-disk `.penpot` dir (deadline now).
    /// The hash ledger turns unchanged dirs into silent no-ops; the poll loop
    /// picks up DB-side changes on its own.
    fn rescan_disk(&mut self) {
        match walk_penpot_dirs(&self.cfg.sync_root) {
            Ok(dirs) => {
                tracing::info!(count = dirs.len(), "rescanning every on-disk file dir");
                let now = Instant::now();
                for rel in dirs {
                    self.fs_debounce.arm(rel, now, Duration::ZERO);
                }
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "resume rescan failed to walk the sync root");
                self.status.set_last_error(format!("resume rescan failed: {e:#}"));
            }
        }
    }

    async fn process_due_fs_changes(&mut self) {
        if self.sweep_deadline.is_some_and(|dl| dl <= Instant::now()) {
            self.sweep_deadline = None;
            self.structural_sweep().await;
        }
        for rel in self.fs_debounce.take_due(Instant::now()) {
            self.handle_fs_change(&rel).await;
        }
    }

    // ------------------------------------------------------------------
    // M5: OS-side rename/move — re-key instead of delete + reimport
    // ------------------------------------------------------------------

    /// Pair manifest entries whose dir vanished with unclaimed on-disk dirs
    /// of identical semantic hash ([`plan_rekeys`]) and execute the ops:
    /// re-key the manifest path (SAME fileId, NO reimport) and mirror the
    /// change into the DB (`move-files` / `rename-file` / `rename-project`).
    ///
    /// DB mirroring is best-effort: a failure is surfaced loudly but never
    /// blocks the re-key — preserving the file's identity is the invariant
    /// (the DB is a disposable cache; an unmirrored rename is cosmetic drift
    /// that later imports/reconciliations resolve under the same id).
    /// Ambiguous or unpaired changes are left for the M3-era handling
    /// (vanish = loud log + DB kept; appear = import-as-new).
    ///
    /// Returns the number of re-keyed files.
    async fn rekey_pass(&mut self) -> anyhow::Result<usize> {
        let disk_dirs = walk_penpot_dirs(&self.cfg.sync_root)?;
        let disk_set: BTreeSet<&str> = disk_dirs.iter().map(String::as_str).collect();
        let missing: Vec<(String, ManifestEntry)> = self
            .manifest
            .files
            .iter()
            .filter(|(_, e)| !disk_set.contains(e.path.as_str()))
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let claimed: BTreeSet<&str> = self
            .manifest
            .files
            .values()
            .map(|e| e.path.as_str())
            .collect();
        let mut unclaimed: Vec<(String, String)> = Vec::new(); // (rel, hash)
        for rel in &disk_dirs {
            if claimed.contains(rel.as_str()) {
                continue;
            }
            match semantic_tree_hash(&self.cfg.sync_root.join(rel)) {
                Ok(h) => unclaimed.push((rel.clone(), h)),
                Err(e) => tracing::debug!(
                    path = %rel,
                    error = %e,
                    "unhashable unclaimed dir; not a re-key candidate"
                ),
            }
        }
        if unclaimed.is_empty() {
            return Ok(0);
        }
        let surviving: Vec<(&str, &str)> = self
            .manifest
            .files
            .values()
            .filter(|e| disk_set.contains(e.path.as_str()))
            .map(|e| (e.path.as_str(), e.project_id.as_str()))
            .collect();
        let mut existing_folders: BTreeSet<String> = BTreeSet::new();
        for (_, entry) in &missing {
            let folder = folder_of(&entry.path);
            if !folder.is_empty() && self.cfg.sync_root.join(folder).is_dir() {
                existing_folders.insert(folder.to_string());
            }
        }
        let ops = plan_rekeys(
            &missing
                .iter()
                .map(|(id, e)| MissingEntry {
                    file_id: id,
                    old_rel: &e.path,
                    last_synced_hash: &e.last_synced_hash,
                    project_id: &e.project_id,
                })
                .collect::<Vec<_>>(),
            &unclaimed
                .iter()
                .map(|(rel, h)| UnclaimedDir {
                    rel,
                    semantic_hash: h,
                })
                .collect::<Vec<_>>(),
            &surviving
                .iter()
                .map(|(rel, pid)| SurvivingEntry {
                    rel,
                    project_id: pid,
                })
                .collect::<Vec<_>>(),
            &existing_folders,
        );
        if ops.is_empty() {
            return Ok(0);
        }
        let mut rekeyed = 0usize;
        for op in ops {
            rekeyed += self.apply_rekey_op(op).await;
        }
        self.manifest.save(&self.cfg.sync_root)?;
        Ok(rekeyed)
    }

    /// Execute one [`RekeyOp`] (see [`Self::rekey_pass`] for the policy).
    /// Returns the number of files re-keyed.
    async fn apply_rekey_op(&mut self, op: RekeyOp) -> usize {
        match op {
            RekeyOp::Relocate {
                file_id,
                old_rel,
                new_rel,
            } => {
                let Some(entry) = self.manifest.files.get(&file_id).cloned() else {
                    return 0;
                };
                let old_folder = folder_of(&old_rel).to_string();
                let new_folder = folder_of(&new_rel).to_string();
                let (mut project_id, mut project_name) =
                    (entry.project_id.clone(), entry.project_name.clone());
                if old_folder != new_folder {
                    match self.resolve_project_for_folder(&new_folder).await {
                        Ok((pid, pname)) => {
                            project_id = pid;
                            project_name = pname;
                        }
                        Err(e) => {
                            tracing::error!(
                                file = %file_id,
                                folder = %new_folder,
                                error = format!("{e:#}"),
                                "cannot resolve the target project for a moved dir; re-keying the path but keeping the old project (drift resolves at the next import/reconciliation, same file id)"
                            );
                            self.status.set_last_error(format!(
                                "move {old_rel} → {new_rel}: target project unresolved: {e:#}"
                            ));
                        }
                    }
                    if project_id != entry.project_id {
                        let client = self.client.clone();
                        let (fid, pid) = (file_id.clone(), project_id.clone());
                        let moved = with_retry("move-files", || {
                            let c = client.clone();
                            let (f, p) = (fid.clone(), pid.clone());
                            async move { c.move_files(&[f.as_str()], &p).await }
                        })
                        .await;
                        match moved {
                            Ok(()) => tracing::info!(
                                file = %file_id,
                                from = %old_rel,
                                to = %new_rel,
                                "OS-side move mirrored to the DB (move-files, same file id)"
                            ),
                            Err(e) => {
                                tracing::error!(
                                    file = %file_id,
                                    error = %e,
                                    "move-files failed; manifest re-keyed anyway (identity preserved; the DB project membership will drift until the next import/reconciliation)"
                                );
                                self.status.set_last_error(format!(
                                    "move-files for {new_rel} failed: {e}"
                                ));
                            }
                        }
                    }
                }
                let old_stem = paths::file_stem_of(&old_rel);
                let new_stem = paths::file_stem_of(&new_rel);
                if old_stem != new_stem {
                    let client = self.client.clone();
                    let (fid, name) = (file_id.clone(), new_stem.clone());
                    let renamed = with_retry("rename-file", || {
                        let c = client.clone();
                        let (f, n) = (fid.clone(), name.clone());
                        async move { c.rename_file(&f, &n).await }
                    })
                    .await;
                    match renamed {
                        // Deliberately NOT recording the bumped modifiedAt in
                        // the manifest/tracker: the next poll must see the
                        // rename as a DB change so the export pipeline
                        // refreshes the name embedded in the on-disk JSON
                        // (otherwise a later disk-side import would revert
                        // the rename).
                        Ok(r) => tracing::info!(
                            file = %file_id,
                            name = %r.name,
                            "OS-side rename mirrored to the DB (rename-file, same file id)"
                        ),
                        Err(e) => {
                            tracing::error!(
                                file = %file_id,
                                error = %e,
                                "rename-file failed; manifest re-keyed anyway (identity preserved; the DB name will drift until the next import/reconciliation)"
                            );
                            self.status
                                .set_last_error(format!("rename-file for {new_rel} failed: {e}"));
                        }
                    }
                }
                self.rekey_manifest_entry(&file_id, &old_rel, &new_rel, &project_id, &project_name);
                1
            }
            RekeyOp::RenameProject {
                project_id,
                old_folder,
                new_folder,
                pairs,
            } => {
                let client = self.client.clone();
                let (pid, name) = (project_id.clone(), new_folder.clone());
                let renamed = with_retry("rename-project", || {
                    let c = client.clone();
                    let (p, n) = (pid.clone(), name.clone());
                    async move { c.rename_project(&p, &n).await }
                })
                .await;
                match renamed {
                    Ok(()) => tracing::info!(
                        project = %project_id,
                        from = %old_folder,
                        to = %new_folder,
                        files = pairs.len(),
                        "OS-side project folder rename mirrored to the DB (rename-project)"
                    ),
                    Err(e) => {
                        tracing::error!(
                            project = %project_id,
                            error = %e,
                            "rename-project failed; manifest re-keyed anyway (identity preserved; the DB project name will drift)"
                        );
                        self.status
                            .set_last_error(format!("rename-project → {new_folder:?} failed: {e}"));
                    }
                }
                let mut n = 0usize;
                for pair in pairs {
                    if self.manifest.files.contains_key(&pair.file_id) {
                        self.rekey_manifest_entry(
                            &pair.file_id,
                            &pair.old_rel,
                            &pair.new_rel,
                            &project_id,
                            &new_folder,
                        );
                        n += 1;
                    }
                }
                n
            }
        }
    }

    /// Re-key one manifest entry to its new path (same fileId — the whole
    /// point). `lastSyncedHash`, `revn` and `dbModifiedAt` are deliberately
    /// untouched: the content did not change, and a rename-file's bumped
    /// modifiedAt must keep looking like a DB change to the poll loop.
    fn rekey_manifest_entry(
        &mut self,
        file_id: &str,
        old_rel: &str,
        new_rel: &str,
        project_id: &str,
        project_name: &str,
    ) {
        if let Some(e) = self.manifest.files.get_mut(file_id) {
            e.path = new_rel.to_string();
            e.project_id = project_id.to_string();
            e.project_name = project_name.to_string();
            e.last_synced_at = now_rfc3339();
        }
        self.status.remove_file(old_rel);
        self.status.set_file(new_rel, FileState::Synced);
        self.status.record_success();
        tracing::info!(
            file = %file_id,
            from = %old_rel,
            to = %new_rel,
            "manifest re-keyed (same file id, no reimport)"
        );
    }

    /// Project for a folder name: an existing manifest entry under that
    /// folder wins (the folder ↔ project mapping is manifest-authoritative);
    /// otherwise find-or-create by name. Root-level dirs use the "imported"
    /// catch-all, mirroring [`Self::import_as_new`].
    async fn resolve_project_for_folder(&self, folder: &str) -> anyhow::Result<(String, String)> {
        if !folder.is_empty() {
            if let Some(e) = self
                .manifest
                .files
                .values()
                .find(|e| folder_of(&e.path) == folder)
            {
                return Ok((e.project_id.clone(), e.project_name.clone()));
            }
        }
        let name = if folder.is_empty() { "imported" } else { folder };
        let projects = self.fetch_projects().await?;
        let mut resolver = ProjectResolver::new(projects);
        resolver
            .ensure(&self.client, &self.cfg.team_id, None, name)
            .await
    }

    /// Fired by the debounced structural timer: a project folder was
    /// renamed/moved/created/deleted (macOS FSEvents fires only for the
    /// folder itself — its children never get events). Run the re-key pass,
    /// then route leftovers through the normal per-dir handling: unclaimed
    /// dirs are armed for import-as-new; still-missing entries are logged
    /// loudly (the DB copy is kept — M3 policy — and re-exported at the next
    /// startup reconciliation).
    async fn structural_sweep(&mut self) {
        match self.rekey_pass().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "structural sweep re-keyed moved/renamed dirs"),
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "structural sweep re-key pass failed");
                self.status
                    .set_last_error(format!("structural sweep failed: {e:#}"));
                return;
            }
        }
        match walk_penpot_dirs(&self.cfg.sync_root) {
            Ok(dirs) => {
                let now = Instant::now();
                let disk: BTreeSet<&str> = dirs.iter().map(String::as_str).collect();
                for rel in &dirs {
                    if self.manifest.entry_by_path(rel).is_none() {
                        // e.g. a whole project folder moved INTO the root:
                        // its file dirs never produced their own events.
                        self.fs_debounce.arm(rel.clone(), now, Duration::ZERO);
                    }
                }
                for entry in self.manifest.files.values() {
                    if !disk.contains(entry.path.as_str()) {
                        tracing::warn!(
                            path = %entry.path,
                            "dir missing after a structural change (folder moved out or deleted?); the DB copy is deliberately kept and will be re-exported at the next startup reconciliation"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "structural sweep walk failed");
                self.status
                    .set_last_error(format!("structural sweep walk failed: {e:#}"));
            }
        }
    }

    fn fail_file(&self, rel: &str, message: String) {
        tracing::error!(
            path = %rel,
            error = %message,
            "FS→DB sync failed; nothing was overwritten (retried when the tree changes again)"
        );
        self.status.set_last_error(format!("{rel}: {message}"));
        self.status.set_file(rel, FileState::Error { message });
    }

    /// The debounce fired for one file dir: THE Direction B entry point.
    async fn handle_fs_change(&mut self, rel: &str) {
        let target = self.cfg.sync_root.join(rel);
        if !target.is_dir() {
            if self.manifest.entry_by_path(rel).is_some() {
                // M5: a vanished dir may be the OLD path of a rename/move —
                // try to pair it with an unclaimed dir of identical content
                // before declaring it deleted.
                if let Err(e) = self.rekey_pass().await {
                    tracing::warn!(error = format!("{e:#}"), "re-key pass failed for a vanished dir; falling back to the deletion policy");
                }
            }
            if let Some((file_id, _)) = self.manifest.entry_by_path(rel) {
                // M3 policy: a deletion on disk NEVER deletes DB-side. The
                // next startup reconciliation re-exports the DB version.
                tracing::error!(
                    file = %file_id,
                    path = %rel,
                    "file dir deleted on disk (no rename/move pair found); the DB copy is deliberately NOT deleted — restart the app to re-export it, or recreate the directory"
                );
                self.status.set_file(
                    rel,
                    FileState::Error {
                        message: "directory deleted on disk; DB copy retained (re-exported at next startup)"
                            .to_string(),
                    },
                );
            } else {
                self.status.remove_file(rel);
            }
            return;
        }
        let disk_hash = match semantic_tree_hash(&target) {
            Ok(h) => h,
            Err(e) => {
                self.fail_file(rel, format!("cannot hash the tree (broken JSON?): {e}"));
                return;
            }
        };
        let mut known = self
            .manifest
            .entry_by_path(rel)
            .map(|(id, e)| (id.to_string(), e.clone()));
        if known.is_none() {
            // M5: an unknown dir may be the NEW path of a rename/move — try
            // to pair it before treating it as a brand-new file. A re-keyed
            // dir then hits the hash-skip arm below (same content).
            if let Err(e) = self.rekey_pass().await {
                tracing::warn!(error = format!("{e:#}"), "re-key pass failed for an unknown dir; falling back to import-as-new");
            }
            known = self
                .manifest
                .entry_by_path(rel)
                .map(|(id, e)| (id.to_string(), e.clone()));
        }
        let result: anyhow::Result<Option<String>> = match known {
            Some((file_id, entry)) => {
                if entry.last_synced_hash == disk_hash {
                    // THE loop-prevention check: Direction A saved this hash
                    // before its swap landed, so our own writes (and any
                    // semantically empty change) end here, silently.
                    tracing::debug!(path = %rel, "fs event tree matches lastSyncedHash (own write or no semantic change); skipping");
                    self.status.set_file(rel, FileState::Synced);
                    return;
                }
                if let Err(msg) = validate_tree(&target, Some(&file_id)) {
                    self.fail_file(rel, format!("validation failed: {msg}"));
                    return;
                }
                self.status.set_file(rel, FileState::Importing);
                self.import_changed_tree(&file_id, &entry, rel, &disk_hash).await
            }
            None => {
                if let Err(msg) = validate_tree(&target, None) {
                    self.fail_file(rel, format!("validation failed: {msg}"));
                    return;
                }
                self.status.set_file(rel, FileState::Importing);
                self.import_new_tree(rel, &disk_hash).await.map(|_| None)
            }
        };
        match result {
            Ok(None) => {
                self.status.set_file(rel, FileState::Synced);
                self.status.record_success();
                tracing::info!(path = %rel, "sync FS→DB complete");
            }
            Ok(Some(copy_path)) => {
                self.status.set_file(
                    rel,
                    FileState::Conflict {
                        copy_path: copy_path.clone(),
                    },
                );
                self.status.record_success();
                tracing::error!(
                    path = %rel,
                    copy = %copy_path,
                    "CONFLICT resolved: the DB version was preserved as a conflict copy and the disk version was imported (folder tree is the source of truth)"
                );
            }
            Err(e) => self.fail_file(rel, format!("{e:#}")),
        }
    }

    /// Disk changed since `lastSyncedHash` for a manifest-known file: decide
    /// import vs conflict against fresh DB facts, execute, update manifest +
    /// poll tracker. Returns `Some(conflict_copy_rel)` iff the conflict rule
    /// fired.
    async fn import_changed_tree(
        &mut self,
        file_id: &str,
        entry: &ManifestEntry,
        rel: &str,
        disk_hash: &str,
    ) -> anyhow::Result<Option<String>> {
        match self.fetch_db_state(file_id, &entry.project_id).await? {
            None => {
                // Absent from the DB (wipe / deletion): the resurrect recipe;
                // there is no DB version to conflict with.
                let projects = self.fetch_projects().await?;
                let mut resolver = ProjectResolver::new(projects);
                let final_id = self
                    .import_in_place(&mut resolver, file_id, rel, disk_hash)
                    .await?;
                self.finalize_import(&final_id, disk_hash).await?;
                Ok(None)
            }
            Some(db) => {
                let conflict = db_moved(
                    &ManifestFacts {
                        last_synced_hash: &entry.last_synced_hash,
                        revn: entry.revn,
                        db_modified_at: &entry.db_modified_at,
                    },
                    &DbFacts {
                        revn: db.revn,
                        modified_at: &db.modified_at,
                    },
                );
                // Conflict rule step (a): preserve the DB version FIRST —
                // only then may the disk version overwrite it in the DB.
                let copy = if conflict {
                    Some(self.write_conflict_copy(file_id, rel).await?)
                } else {
                    None
                };
                self.import_existing_in_place(file_id, &db.project_id, rel, disk_hash)
                    .await?;
                Ok(copy)
            }
        }
    }

    /// Direction B for a dir the manifest has never seen: import-as-new.
    async fn import_new_tree(&mut self, rel: &str, disk_hash: &str) -> anyhow::Result<String> {
        let projects = self.fetch_projects().await?;
        let mut resolver = ProjectResolver::new(projects);
        let file_id = self.import_as_new(&mut resolver, rel, disk_hash).await?;
        self.finalize_import(&file_id, disk_hash).await?;
        Ok(file_id)
    }

    /// In-place import of an on-disk tree onto a file that currently exists
    /// in the DB, then manifest/tracker bookkeeping.
    async fn import_existing_in_place(
        &mut self,
        file_id: &str,
        project_id: &str,
        rel: &str,
        disk_hash: &str,
    ) -> anyhow::Result<()> {
        let target = self.cfg.sync_root.join(rel);
        let zip = zip_dir(&target)?;
        let name = paths::file_stem_of(rel);
        let client = self.client.clone();
        with_retry("import-binfile (in-place)", || {
            let c = client.clone();
            let (n, p, f, z) = (
                name.clone(),
                project_id.to_string(),
                file_id.to_string(),
                zip.clone(),
            );
            async move { c.import_binfile(&n, &p, Some(&f), z).await }
        })
        .await
        .with_context(|| format!("in-place import of {rel}"))?;
        tracing::info!(file = %file_id, path = %rel, "imported disk → DB in place");
        self.finalize_import(file_id, disk_hash).await
    }

    /// Read back the post-import `(revn, modifiedAt)` and store them in the
    /// manifest AND the poll tracker, so Direction A's poller sees no
    /// phantom change from our own import.
    async fn finalize_import(&mut self, file_id: &str, disk_hash: &str) -> anyhow::Result<()> {
        let hint = self
            .manifest
            .files
            .get(file_id)
            .map(|e| e.project_id.clone())
            .unwrap_or_default();
        let fresh = self.fetch_db_state(file_id, &hint).await?;
        let Some(fresh) = fresh else {
            // Extremely unlikely (imported a moment ago); the poll loop will
            // sort out whatever happened.
            tracing::warn!(file = %file_id, "file missing from the DB right after import");
            self.manifest.save(&self.cfg.sync_root)?;
            return Ok(());
        };
        let project_name = self
            .manifest
            .files
            .get(file_id)
            .map(|e| e.project_name.clone())
            .unwrap_or_default();
        if let Some(entry) = self.manifest.files.get_mut(file_id) {
            entry.revn = fresh.revn;
            entry.db_modified_at = fresh.modified_at.clone();
            entry.project_id = fresh.project_id.clone();
            entry.last_synced_hash = disk_hash.to_string();
            entry.last_synced_at = now_rfc3339();
        }
        self.manifest.save(&self.cfg.sync_root)?;
        self.tracker.mark_synced(&DbFileState {
            id: file_id.to_string(),
            name: fresh.name,
            project_id: fresh.project_id,
            project_name,
            revn: fresh.revn,
            modified_at: fresh.modified_at,
        });
        Ok(())
    }

    /// Fresh `(revn, modifiedAt, …)` for one file. `Ok(None)` = the file
    /// does not exist in the DB (a definitive not-found — transport/server
    /// failures error out instead, they must never masquerade as absence).
    /// Prefers the listing surface, falls back to `get-file` when the
    /// listing is stale or the project itself is gone.
    async fn fetch_db_state(
        &self,
        file_id: &str,
        project_id_hint: &str,
    ) -> anyhow::Result<Option<DbFileFacts>> {
        if !project_id_hint.is_empty() {
            let client = self.client.clone();
            let pid = project_id_hint.to_string();
            let listing = with_retry("get-project-files", || {
                let c = client.clone();
                let p = pid.clone();
                async move { c.get_project_files(&p).await }
            })
            .await;
            match listing {
                Ok(files) => {
                    if let Some(f) = files.into_iter().find(|f| f.id == file_id) {
                        if f.deleted_at.is_none() {
                            return Ok(Some(DbFileFacts {
                                revn: f.revn,
                                modified_at: f.modified_at,
                                project_id: f.project_id,
                                name: f.name,
                            }));
                        }
                    }
                    // Not listed → maybe a stale listing; get-file decides.
                }
                Err(e) if is_not_found(&e) => {} // project gone; get-file decides
                Err(e) => return Err(e).context("get-project-files"),
            }
        }
        let client = self.client.clone();
        let fid = file_id.to_string();
        let got = with_retry("get-file", || {
            let c = client.clone();
            let f = fid.clone();
            async move { c.get_file(&f).await }
        })
        .await;
        match got {
            Ok(v) => {
                let revn = v
                    .get("revn")
                    .and_then(|x| x.as_i64())
                    .context("get-file response has no revn")?;
                let modified_at = v
                    .get("modifiedAt")
                    .and_then(|x| x.as_str())
                    .context("get-file response has no modifiedAt")?
                    .to_string();
                let project_id = v
                    .get("projectId")
                    .and_then(|x| x.as_str())
                    .context("get-file response has no projectId")?
                    .to_string();
                let name = v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(Some(DbFileFacts {
                    revn,
                    modified_at,
                    project_id,
                    name,
                }))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e).context("get-file"),
        }
    }

    async fn fetch_projects(&self) -> anyhow::Result<Vec<ProjectInfo>> {
        let client = self.client.clone();
        let team = self.cfg.team_id.clone();
        with_retry("get-projects", || {
            let c = client.clone();
            let t = team.clone();
            async move { c.get_projects(&t).await }
        })
        .await
        .context("get-projects")
    }

    // ------------------------------------------------------------------
    // Conflict copies
    // ------------------------------------------------------------------

    /// Move an already-staged (normalized) DB tree into a fresh
    /// `<name>.conflict-<ts>.penpot/` sibling of `rel`. Never overwrites
    /// anything: the name is timestamped and uniquified.
    fn stage_to_conflict_copy(&self, stage: &Path, rel: &str) -> anyhow::Result<String> {
        let mut conflict_rel = paths::conflict_path_for(rel, &now_rfc3339());
        let mut counter = 1u32;
        while self.cfg.sync_root.join(&conflict_rel).symlink_metadata().is_ok() {
            counter += 1;
            conflict_rel =
                paths::conflict_path_for(rel, &format!("{}-{counter}", now_rfc3339()));
        }
        let conflict_target = self.cfg.sync_root.join(&conflict_rel);
        std::fs::rename(stage, &conflict_target).with_context(|| {
            format!(
                "moving staged conflict copy into place at {}",
                conflict_target.display()
            )
        })?;
        Ok(conflict_rel)
    }

    /// Conflict rule step (a) for the import path: export the CURRENT DB
    /// version of `file_id` into a conflict copy next to `rel`.
    async fn write_conflict_copy(&self, file_id: &str, rel: &str) -> anyhow::Result<String> {
        let zip = self.download_export(file_id).await?;
        let target = self.cfg.sync_root.join(rel);
        let stage = stage_path_for(&target);
        let staged: anyhow::Result<String> = (|| {
            unzip_to(&zip, &stage)?;
            normalize_tree(&stage)?;
            self.stage_to_conflict_copy(&stage, rel)
        })();
        match staged {
            Ok(copy) => {
                tracing::error!(
                    file = %file_id,
                    copy = %copy,
                    "CONFLICT: both sides changed since lastSyncedHash — DB version preserved at the conflict copy before importing the disk version"
                );
                Ok(copy)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                Err(e.context(format!("writing conflict copy for {rel}")))
            }
        }
    }

    /// `export-binfile` + authenticated download, retried as one unit (the
    /// artifact URI may not outlive a backend restart).
    async fn download_export(&self, file_id: &str) -> anyhow::Result<Vec<u8>> {
        let client = self.client.clone();
        let fid = file_id.to_string();
        with_retry("export-binfile", || {
            let c = client.clone();
            let id = fid.clone();
            async move {
                let exported = c.export_binfile(&id, false, true).await?;
                c.download_exported_binfile(&exported.uri).await
            }
        })
        .await
        .with_context(|| format!("export-binfile for file {file_id}"))
    }

    // ------------------------------------------------------------------
    // Poll loop (Direction A)
    // ------------------------------------------------------------------

    async fn poll_cycle(&mut self) {
        match fetch_snapshot(&self.client, &self.cfg.team_id).await {
            Ok(snap) => {
                let vanished =
                    self.tracker
                        .observe(Instant::now(), self.cfg.debounce, &snap.files);
                for id in vanished {
                    let path = self.manifest.files.get(&id).map(|e| e.path.clone());
                    tracing::warn!(
                        file = %id,
                        path = ?path,
                        "file disappeared from the DB listing; on-disk copy left untouched (disk is the source of truth — it will be re-imported at the next startup reconciliation)"
                    );
                }
                // Surface debouncing DB changes as Pending.
                for id in self.tracker.pending_ids() {
                    if let Some(entry) = self.manifest.files.get(&id) {
                        self.status.set_file(&entry.path, FileState::Pending);
                    }
                }
            }
            Err(e) => {
                // NEVER treat a failed poll as deletions — skip the cycle.
                tracing::warn!(error = %e, "poll failed; skipping this cycle");
                return;
            }
        }
        for state in self.tracker.take_due(Instant::now()) {
            match self.export_file(&state).await {
                Ok(outcome) => {
                    if !matches!(outcome, ExportOutcome::Conflict { .. }) {
                        // The conflict arm already re-seeded the tracker with
                        // the post-import DB state.
                        self.tracker.mark_synced(&state);
                    }
                    tracing::info!(
                        file = %state.id,
                        name = %state.name,
                        revn = state.revn,
                        outcome = ?outcome,
                        "sync DB→FS complete"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        file = %state.id,
                        error = format!("{e:#}"),
                        "export failed; will retry after the debounce interval"
                    );
                    if let Some(entry) = self.manifest.files.get(&state.id) {
                        self.status.set_file(
                            &entry.path,
                            FileState::Error {
                                message: format!("export failed: {e:#}"),
                            },
                        );
                    }
                    self.status
                        .set_last_error(format!("export of {} failed: {e:#}", state.id));
                    self.tracker
                        .reschedule(state, Instant::now() + self.cfg.debounce);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Export pipeline (DB → disk), shared by poll loop and reconciliation
    // ------------------------------------------------------------------

    async fn export_file(&mut self, db: &DbFileState) -> anyhow::Result<ExportOutcome> {
        let project_dir = paths::project_dir_name(&self.manifest, &db.project_id, &db.project_name);
        let rel = paths::allocate_file_path(
            &self.manifest,
            &self.cfg.sync_root,
            &project_dir,
            &db.id,
            &db.name,
        );
        let target = self.cfg.sync_root.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        self.status.set_file(&rel, FileState::Exporting);

        let zip = self.download_export(&db.id).await?;
        let stage = stage_path_for(&target);
        let staged: anyhow::Result<String> = (|| {
            unzip_to(&zip, &stage)?;
            normalize_tree(&stage)?;
            Ok(semantic_tree_hash(&stage)?)
        })();
        let hash = match staged {
            Ok(h) => h,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(e.context(format!("staging export of {}", db.id)));
            }
        };

        // Conflict guard: NEVER swap over a disk tree that itself changed
        // since lastSyncedHash (the poll may fire while an external edit is
        // still inside its FS debounce, or the watcher may have missed it).
        let entry = self.manifest.files.get(&db.id).cloned();
        if let Some(entry) = &entry {
            if target.is_dir() {
                let disk_hash = match semantic_tree_hash(&target) {
                    Ok(h) => h,
                    Err(e) => {
                        // A tree we wrote always hashes; failure = the user
                        // broke it. Block the export, overwrite nothing.
                        let _ = std::fs::remove_dir_all(&stage);
                        return Err(anyhow::anyhow!(e).context(format!(
                            "disk tree {rel} is unreadable/invalid; export blocked (nothing overwritten)"
                        )));
                    }
                };
                if disk_hash != entry.last_synced_hash {
                    if hash == disk_hash {
                        // Both sides converged on the same content (e.g. our
                        // own import racing the poll): just fast-forward the
                        // ledger, touch nothing on disk.
                        std::fs::remove_dir_all(&stage)
                            .with_context(|| format!("discarding stage {}", stage.display()))?;
                        let e = self.manifest.files.get_mut(&db.id).expect("entry cloned above");
                        e.revn = db.revn;
                        e.db_modified_at = db.modified_at.clone();
                        e.project_id = db.project_id.clone();
                        e.project_name = db.project_name.clone();
                        e.last_synced_hash = disk_hash;
                        e.last_synced_at = now_rfc3339();
                        self.manifest.save(&self.cfg.sync_root)?;
                        self.status.set_file(&rel, FileState::Synced);
                        self.status.record_success();
                        tracing::debug!(file = %db.id, path = %rel, "DB and disk converged on the same content; ledger fast-forwarded");
                        return Ok(ExportOutcome::NoOp);
                    }
                    // Both changed → the conflict rule. Validate the disk
                    // tree BEFORE writing the copy: an unimportable tree
                    // must not produce copy after copy on every retry.
                    if let Err(msg) = validate_tree(&target, Some(&db.id)) {
                        let _ = std::fs::remove_dir_all(&stage);
                        anyhow::bail!(
                            "conflict detected on {rel} but the disk tree fails validation ({msg}); nothing overwritten — fix the tree (export stays blocked meanwhile)"
                        );
                    }
                    let copy_path = self.stage_to_conflict_copy(&stage, &rel)?;
                    tracing::error!(
                        file = %db.id,
                        path = %rel,
                        copy = %copy_path,
                        "CONFLICT: both the DB and the disk changed since lastSyncedHash — DB version preserved as a conflict copy; importing the disk version (source of truth)"
                    );
                    self.import_existing_in_place(&db.id, &db.project_id, &rel, &disk_hash)
                        .await?;
                    self.status.set_file(
                        &rel,
                        FileState::Conflict {
                            copy_path: copy_path.clone(),
                        },
                    );
                    self.status.record_success();
                    return Ok(ExportOutcome::Conflict { copy_path });
                }
            }
        }

        let unchanged = entry.as_ref().is_some_and(|e| e.last_synced_hash == hash)
            && target.is_dir();
        if unchanged {
            // No-op save: MUST NOT touch the target dir's mtimes.
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("discarding no-op stage {}", stage.display()))?;
            let entry = self.manifest.files.get_mut(&db.id).expect("checked above");
            entry.revn = db.revn;
            entry.db_modified_at = db.modified_at.clone();
            entry.project_id = db.project_id.clone();
            entry.project_name = db.project_name.clone();
            entry.last_synced_at = now_rfc3339();
            self.manifest.save(&self.cfg.sync_root)?;
            self.status.set_file(&rel, FileState::Synced);
            self.status.record_success();
            tracing::debug!(file = %db.id, path = %rel, "export was a semantic no-op; disk untouched");
            Ok(ExportOutcome::NoOp)
        } else {
            // Record the hash BEFORE the swap lands (PLAN.md step 6) so the
            // watcher recognizes our own write and ignores it.
            self.manifest.files.insert(
                db.id.clone(),
                ManifestEntry {
                    path: rel.clone(),
                    project_id: db.project_id.clone(),
                    project_name: db.project_name.clone(),
                    revn: db.revn,
                    db_modified_at: db.modified_at.clone(),
                    last_synced_hash: hash,
                    last_synced_at: now_rfc3339(),
                },
            );
            self.manifest.save(&self.cfg.sync_root)?;
            commit_dir_swap(&stage, &target)?;
            self.status.set_file(&rel, FileState::Synced);
            self.status.record_success();
            tracing::info!(file = %db.id, path = %rel, revn = db.revn, "exported DB → disk");
            Ok(ExportOutcome::Updated)
        }
    }

    // ------------------------------------------------------------------
    // Import pipelines (disk → DB)
    // ------------------------------------------------------------------

    /// Resurrect a file the manifest knows under its ORIGINAL id (the
    /// core-invariant path after a DB wipe). Verified live on 2.16.2:
    /// `import-binfile` with a `file-id` that does not currently exist fails
    /// with an SSE `error` event (`object-not-found`) — it only *replaces*
    /// existing files — so the recipe is: `create-file` with the client-chosen
    /// old id, then in-place import onto it. A direct in-place import is
    /// attempted first (covers the file actually existing, e.g. a stale
    /// listing). Falls back to import-as-new (re-keying the manifest entry;
    /// links/library refs into the file break) only if everything else fails
    /// — e.g. the old id is still held by a soft-deleted file (`delete-file`
    /// keeps the row ~7 days and `create-file` then 500s). Returns the final
    /// fileId.
    async fn import_in_place(
        &mut self,
        resolver: &mut ProjectResolver,
        file_id: &str,
        rel: &str,
        disk_hash: &str,
    ) -> anyhow::Result<String> {
        let entry = self
            .manifest
            .files
            .get(file_id)
            .expect("caller joined on manifest")
            .clone();
        let target = self.cfg.sync_root.join(rel);
        let zip = zip_dir(&target)?;
        let (project_id, project_name) = resolver
            .ensure(
                &self.client,
                &self.cfg.team_id,
                Some(&entry.project_id),
                &entry.project_name,
            )
            .await?;
        let name = paths::file_stem_of(rel);

        let client = self.client.clone();
        let import_in_place = || {
            let c = client.clone();
            let (n, p, f, z) = (name.clone(), project_id.clone(), file_id.to_string(), zip.clone());
            async move {
                with_retry("import-binfile (in-place)", || {
                    let c = c.clone();
                    let (n, p, f, z) = (n.clone(), p.clone(), f.clone(), z.clone());
                    async move { c.import_binfile(&n, &p, Some(&f), z).await }
                })
                .await
            }
        };

        // 1. Direct in-place import (succeeds iff the file currently exists).
        let mut resurrected = match import_in_place().await {
            Ok(ids) => Some(ids),
            Err(e) => {
                tracing::debug!(file = %file_id, error = %e, "direct in-place import failed (file absent from DB, expected after a wipe); trying the create-then-import resurrect recipe");
                None
            }
        };

        // 2. Resurrect recipe: create-file with the old id, import onto it.
        if resurrected.is_none() {
            let create = with_retry("create-file (with old id)", || {
                let c = client.clone();
                let (n, p, f) = (name.clone(), project_id.clone(), file_id.to_string());
                async move { c.create_file_with_id(&p, &n, &f).await }
            })
            .await;
            match create {
                Ok(created) => {
                    debug_assert_eq!(created.id, file_id);
                    match import_in_place().await {
                        Ok(ids) => resurrected = Some(ids),
                        Err(e) => {
                            tracing::warn!(file = %file_id, error = %e, "in-place import onto the re-created file failed; deleting the empty shell and falling back to import-as-new");
                            // Best effort: don't leave an empty file behind.
                            let _ = self.client.delete_file(file_id).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        file = %file_id,
                        error = %e,
                        "create-file with the old id failed (id taken by a soft-deleted file?); falling back to import-as-new"
                    );
                }
            }
        }

        let final_id = match resurrected {
            Some(ids) => {
                let id = ids
                    .first()
                    .cloned()
                    .unwrap_or_else(|| file_id.to_string());
                if id != file_id {
                    tracing::warn!(expected = %file_id, got = %id, "in-place import returned a different file id");
                }
                tracing::info!(file = %id, path = %rel, "imported disk → DB under the SAME file id (core-invariant path)");
                id
            }
            None => {
                let ids = with_retry("import-binfile (as-new fallback)", || {
                    let c = client.clone();
                    let (n, p, z) = (name.clone(), project_id.clone(), zip.clone());
                    async move { c.import_binfile(&n, &p, None, z).await }
                })
                .await
                .with_context(|| format!("import-binfile fallback for {rel}"))?;
                let id = ids
                    .first()
                    .cloned()
                    .context("import-binfile returned no file id")?;
                tracing::warn!(file = %id, path = %rel, "imported disk → DB as a NEW file id (fallback — links/refs into the old id break)");
                id
            }
        };

        let mut entry = self
            .manifest
            .files
            .remove(file_id)
            .expect("still present; nothing removed it");
        entry.project_id = project_id;
        entry.project_name = project_name;
        entry.last_synced_hash = disk_hash.to_string();
        entry.last_synced_at = now_rfc3339();
        self.manifest.files.insert(final_id.clone(), entry);
        Ok(final_id)
    }

    /// Import a disk dir the manifest has never seen. Returns the new fileId.
    async fn import_as_new(
        &mut self,
        resolver: &mut ProjectResolver,
        rel: &str,
        disk_hash: &str,
    ) -> anyhow::Result<String> {
        let target = self.cfg.sync_root.join(rel);
        let zip = zip_dir(&target)?;
        // Project = the folder the dir lives in (a root-level .penpot dir
        // gets a catch-all project).
        let folder = match rel.rsplit_once('/') {
            Some((parent, _)) => parent.split('/').next().unwrap_or(parent).to_string(),
            None => "imported".to_string(),
        };
        let (project_id, project_name) = resolver
            .ensure(&self.client, &self.cfg.team_id, None, &folder)
            .await?;
        let name = paths::file_stem_of(rel);

        let client = self.client.clone();
        let ids = with_retry("import-binfile (as-new)", || {
            let c = client.clone();
            let (n, p, z) = (name.clone(), project_id.clone(), zip.clone());
            async move { c.import_binfile(&n, &p, None, z).await }
        })
        .await
        .with_context(|| format!("import-binfile for {rel}"))?;
        let file_id = ids
            .first()
            .cloned()
            .context("import-binfile returned no file id")?;
        tracing::info!(file = %file_id, path = %rel, "imported unknown disk dir → DB as new file");
        self.manifest.files.insert(
            file_id.clone(),
            ManifestEntry {
                path: rel.to_string(),
                project_id,
                project_name,
                revn: 0,                      // corrected by finalize/post-import snapshot
                db_modified_at: String::new(), // ditto
                last_synced_hash: disk_hash.to_string(),
                last_synced_at: now_rfc3339(),
            },
        );
        Ok(file_id)
    }

    // ------------------------------------------------------------------
    // Startup reconciliation
    // ------------------------------------------------------------------

    pub(crate) async fn reconcile(&mut self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.cfg.sync_root)?;

        // 1. Sweep interrupted-swap leftovers.
        let sweep = cleanup_orphans(&self.cfg.sync_root)?;
        if !sweep.removed_tmp.is_empty()
            || !sweep.removed_old.is_empty()
            || !sweep.restored.is_empty()
        {
            tracing::info!(
                removed_tmp = sweep.removed_tmp.len(),
                removed_old = sweep.removed_old.len(),
                restored = sweep.restored.len(),
                "swept interrupted-swap leftovers"
            );
        }

        // 2. M5: re-key dirs that were moved/renamed across the shutdown
        //    (same lastSyncedHash under a different path than the manifest
        //    records) BEFORE the join — otherwise the old path would be
        //    re-exported as an orphan and the new path imported as a NEW
        //    file. Runs before the snapshot so any project it creates or
        //    renames is visible to the decisions below.
        let rekeyed = self
            .rekey_pass()
            .await
            .context("re-key pass (dirs moved/renamed across the shutdown)")?;
        if rekeyed > 0 {
            tracing::info!(count = rekeyed, "startup reconciliation re-keyed moved/renamed dirs (same file ids, no reimport)");
        }

        // 3. DB snapshot (retried; the backend may still be settling).
        let client = self.client.clone();
        let team = self.cfg.team_id.clone();
        let snap = with_retry("fetch db snapshot", || {
            let c = client.clone();
            let t = team.clone();
            async move { fetch_snapshot(&c, &t).await }
        })
        .await
        .context("listing projects/files for reconciliation")?;

        // 4. Disk walk + semantic hashes. A tree that cannot be hashed
        //    (broken JSON) is surfaced and skipped — never fatal, never
        //    destructive.
        let mut disk: BTreeMap<String, String> = BTreeMap::new();
        let mut broken: Vec<String> = Vec::new();
        for rel in walk_penpot_dirs(&self.cfg.sync_root)? {
            match semantic_tree_hash(&self.cfg.sync_root.join(&rel)) {
                Ok(hash) => {
                    disk.insert(rel, hash);
                }
                Err(e) => {
                    self.fail_file(&rel, format!("cannot hash the tree (broken JSON?): {e}"));
                    broken.push(rel);
                }
            }
        }

        // 5. Join: one decision per identity.
        let mut actions: Vec<ReconcileAction> = Vec::new();
        for (file_id, entry) in &self.manifest.files {
            if broken.contains(&entry.path) {
                continue; // surfaced above; left alone until fixed
            }
            let disk_hash = disk.get(&entry.path);
            let disk_facts = disk_hash.map(|h| DiskFacts { semantic_hash: h });
            let man_facts = ManifestFacts {
                last_synced_hash: &entry.last_synced_hash,
                revn: entry.revn,
                db_modified_at: &entry.db_modified_at,
            };
            let db_facts = snap.files.get(file_id).map(|f| DbFacts {
                revn: f.revn,
                modified_at: &f.modified_at,
            });
            let decision = decide(disk_facts.as_ref(), Some(&man_facts), db_facts.as_ref())
                .expect("manifest present → never the vacuous case");
            actions.push(match decision {
                Decision::ForgetManifestEntry => ReconcileAction::Forget {
                    file_id: file_id.clone(),
                },
                Decision::ImportInPlace => ReconcileAction::ImportInPlace {
                    file_id: file_id.clone(),
                    rel: entry.path.clone(),
                    disk_hash: disk_hash.expect("disk present").clone(),
                },
                Decision::Export => ReconcileAction::Export {
                    file_id: file_id.clone(),
                    conflict_expected: false,
                },
                Decision::Conflict => ReconcileAction::Export {
                    file_id: file_id.clone(),
                    conflict_expected: true,
                },
                Decision::Noop => ReconcileAction::Noop {
                    file_id: file_id.clone(),
                },
                Decision::ImportAsNew => unreachable!("manifest facts were provided"),
            });
        }
        // Disk dirs no manifest entry claims → import-as-new.
        let claimed: Vec<String> = self.manifest.files.values().map(|e| e.path.clone()).collect();
        for (rel, disk_hash) in &disk {
            if !claimed.contains(rel) {
                actions.push(ReconcileAction::ImportAsNew {
                    rel: rel.clone(),
                    disk_hash: disk_hash.clone(),
                });
            }
        }
        // DB files the manifest has never seen → first export.
        for file_id in snap.files.keys() {
            if !self.manifest.files.contains_key(file_id) {
                actions.push(ReconcileAction::Export {
                    file_id: file_id.clone(),
                    conflict_expected: false,
                });
            }
        }

        // 6. Execute: forgets, then imports (so the DB reflects the disk
        //    before any exports), then exports, then no-op seeding.
        // Per-file failures are logged loudly and skipped (never fatal): a
        // single permanently-broken file must not wedge the whole daemon in
        // the reconcile-retry loop. Unseeded files are re-detected by the
        // poll loop; un-imported dirs are retried at the next startup.
        let mut resolver = ProjectResolver::new(snap.projects.clone());
        let mut manifest_dirty = false;
        let mut imported_ids: Vec<String> = Vec::new();
        let (mut n_forget, mut n_import, mut n_export, mut n_noop, mut n_conflict) =
            (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut n_failed = broken.len() as u32;

        for action in &actions {
            if let ReconcileAction::Forget { file_id } = action {
                let entry = self.manifest.files.remove(file_id);
                tracing::warn!(
                    file = %file_id,
                    path = ?entry.as_ref().map(|e| &e.path),
                    "manifest entry has neither a disk dir nor a DB file; forgetting it"
                );
                if let Some(entry) = entry {
                    self.status.remove_file(&entry.path);
                }
                manifest_dirty = true;
                n_forget += 1;
            }
        }
        for action in &actions {
            let (rel, result) = match action {
                ReconcileAction::ImportInPlace {
                    file_id,
                    rel,
                    disk_hash,
                } => {
                    if let Err(msg) = validate_tree(&self.cfg.sync_root.join(rel), Some(file_id)) {
                        self.fail_file(rel, format!("validation failed: {msg}"));
                        n_failed += 1;
                        continue;
                    }
                    self.status.set_file(rel, FileState::Importing);
                    (
                        rel.clone(),
                        self.import_in_place(&mut resolver, file_id, rel, disk_hash)
                            .await
                            .with_context(|| format!("in-place import of {rel}")),
                    )
                }
                ReconcileAction::ImportAsNew { rel, disk_hash } => {
                    if let Err(msg) = validate_tree(&self.cfg.sync_root.join(rel), None) {
                        self.fail_file(rel, format!("validation failed: {msg}"));
                        n_failed += 1;
                        continue;
                    }
                    self.status.set_file(rel, FileState::Importing);
                    (
                        rel.clone(),
                        self.import_as_new(&mut resolver, rel, disk_hash)
                            .await
                            .with_context(|| format!("import of {rel}")),
                    )
                }
                _ => continue,
            };
            match result {
                Ok(id) => {
                    imported_ids.push(id);
                    manifest_dirty = true;
                    n_import += 1;
                    self.status.set_file(&rel, FileState::Synced);
                    self.status.record_success();
                }
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "reconciliation import failed; skipping (will be retried at next startup)");
                    self.status.set_file(
                        &rel,
                        FileState::Error {
                            message: format!("import failed: {e:#}"),
                        },
                    );
                    n_failed += 1;
                }
            }
        }
        // Persist import results before the (slower) exports.
        if manifest_dirty {
            self.manifest.save(&self.cfg.sync_root)?;
            manifest_dirty = false;
        }
        for action in &actions {
            if let ReconcileAction::Export {
                file_id,
                conflict_expected,
            } = action
            {
                let db = snap.files.get(file_id).expect("export decided ⇒ in DB").clone();
                if *conflict_expected {
                    tracing::warn!(file = %file_id, "reconciliation: both sides changed since lastSyncedHash; applying the conflict rule");
                }
                match self
                    .export_file(&db)
                    .await
                    .with_context(|| format!("export of file {file_id}"))
                {
                    Ok(outcome) => {
                        if matches!(outcome, ExportOutcome::Conflict { .. }) {
                            n_conflict += 1;
                        } else {
                            self.tracker.mark_synced(&db);
                            n_export += 1;
                        }
                    }
                    Err(e) => {
                        // Not seeded → the poll loop re-detects and retries.
                        tracing::error!(error = format!("{e:#}"), "reconciliation export failed; the poll loop will retry");
                        if let Some(entry) = self.manifest.files.get(file_id) {
                            self.status.set_file(
                                &entry.path,
                                FileState::Error {
                                    message: format!("export failed: {e:#}"),
                                },
                            );
                        }
                        n_failed += 1;
                    }
                }
            }
        }
        for action in &actions {
            if let ReconcileAction::Noop { file_id } = action {
                let db = snap.files.get(file_id).expect("noop decided ⇒ in DB");
                self.tracker.mark_synced(db);
                if let Some(entry) = self.manifest.files.get(file_id) {
                    self.status.set_file(&entry.path, FileState::Synced);
                }
                n_noop += 1;
            }
        }

        // 7. Seed the tracker (and correct advisory revn/modifiedAt) for what
        //    we just imported: the import reset revn to the binfile's value.
        if !imported_ids.is_empty() {
            let client = self.client.clone();
            let team = self.cfg.team_id.clone();
            let fresh = with_retry("post-import snapshot", || {
                let c = client.clone();
                let t = team.clone();
                async move { fetch_snapshot(&c, &t).await }
            })
            .await
            .context("post-import snapshot")?;
            for id in &imported_ids {
                match fresh.files.get(id) {
                    Some(f) => {
                        self.tracker.mark_synced(f);
                        if let Some(entry) = self.manifest.files.get_mut(id) {
                            if entry.revn != f.revn || entry.db_modified_at != f.modified_at {
                                entry.revn = f.revn;
                                entry.db_modified_at = f.modified_at.clone();
                                manifest_dirty = true;
                            }
                        }
                    }
                    None => tracing::warn!(file = %id, "just-imported file missing from the post-import snapshot"),
                }
            }
        }
        if manifest_dirty {
            self.manifest.save(&self.cfg.sync_root)?;
        }

        tracing::info!(
            imports = n_import,
            exports = n_export,
            noops = n_noop,
            forgotten = n_forget,
            conflicts = n_conflict,
            failed = n_failed,
            "startup reconciliation done"
        );
        Ok(())
    }
}

/// All `*.penpot` directories under `root`, as sorted `/`-separated relative
/// paths. Dot-dirs are skipped; conflict copies (`*.conflict-<ts>.penpot`)
/// are never synced and are skipped entirely; the walk does not descend into
/// `.penpot` dirs themselves (their contents are payload).
fn walk_penpot_dirs(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // The in-vault package home is blind to sync (PLAN3 E2 invariant 1):
            // the reconcile walk never descends into it, so a package `.penpot`
            // tree is never hashed, imported, or conflict-copied. Dot-prefixed,
            // so the next arm already skips it; explicit + named as
            // defense-in-depth against any future relaxation of the dot rule.
            if name == sync_core::PACKAGES_DIR_NAME {
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            if name.ends_with(paths::PENPOT_DIR_SUFFIX) {
                if paths::is_conflict_dir_name(&name) {
                    continue; // conflict copies: never watched, never synced
                }
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(&entry.path())
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push(rel);
            } else {
                stack.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_finds_penpot_dirs_at_any_depth_skipping_dot_and_payload_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("Client A/home.penpot/files")).unwrap();
        std::fs::create_dir_all(root.join("Client A/nested/deep.penpot")).unwrap();
        std::fs::create_dir_all(root.join("rootfile.penpot")).unwrap();
        std::fs::create_dir_all(root.join(".git/x.penpot")).unwrap(); // dot dir skipped
        std::fs::create_dir_all(root.join("Client A/home.penpot/inner.penpot")).unwrap(); // payload
        std::fs::write(root.join("Client A/readme.txt"), "x").unwrap(); // file ignored
        let got = walk_penpot_dirs(root).unwrap();
        assert_eq!(
            got,
            vec![
                "Client A/home.penpot".to_string(),
                "Client A/nested/deep.penpot".to_string(),
                "rootfile.penpot".to_string(),
            ]
        );
    }

    #[test]
    fn walk_is_blind_to_the_penpot_packages_home() {
        // PLAN3 E2 invariant 1: the full reconcile walk never descends into
        // `.penpot-packages/`, so a package `.penpot` tree (even a valid one) is
        // never enumerated, hashed, or imported by the daemon.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("Client/home.penpot/files")).unwrap();
        std::fs::create_dir_all(
            root.join(".penpot-packages/buttons/button-library.penpot/files"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".penpot-packages/icons/icons.penpot")).unwrap();
        let got = walk_penpot_dirs(root).unwrap();
        assert_eq!(got, vec!["Client/home.penpot".to_string()]);
    }

    #[test]
    fn walk_skips_conflict_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("C/home.penpot")).unwrap();
        std::fs::create_dir_all(
            root.join("C/home.conflict-2026-07-13T09-04-42Z.penpot/files"),
        )
        .unwrap();
        let got = walk_penpot_dirs(root).unwrap();
        assert_eq!(got, vec!["C/home.penpot".to_string()]);
    }

    #[test]
    fn walk_missing_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let got = walk_penpot_dirs(&tmp.path().join("nope")).unwrap();
        assert!(got.is_empty());
    }
}
