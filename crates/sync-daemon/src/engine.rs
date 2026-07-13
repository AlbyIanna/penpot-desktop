//! The daemon engine: startup reconciliation, poll loop, export/import
//! pipelines. Pure decision logic lives in [`crate::plan`]; debounce state in
//! [`crate::tracker`]; this module does the I/O.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use penpot_rpc::{PenpotClient, ProjectInfo};
use sync_core::{
    cleanup_orphans, commit_dir_swap, manifest::now_rfc3339, normalize_tree, semantic_tree_hash,
    stage_path_for, unzip_to, zip_dir, Manifest, ManifestEntry,
};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::paths;
use crate::plan::{decide, DbFacts, Decision, DiskFacts, ManifestFacts};
use crate::retry::with_retry;
use crate::tracker::ChangeTracker;
use crate::{DbFileState, SyncConfig};

/// Outcome of one export-pipeline run.
#[derive(Debug, PartialEq, Eq)]
enum ExportOutcome {
    /// Semantic hash unchanged: staged tree discarded, target untouched.
    NoOp,
    /// Staged tree swapped into place.
    Updated,
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

/// Resolve/create DB projects during reconciliation imports, with a cache so
/// several files of one on-disk project share the (re)created DB project.
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
        tracing::info!(project = %created.id, name = %created.name, "created project in DB (reconciliation import)");
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
    Export {
        file_id: String,
        conflict: bool,
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
}

/// Daemon entry point (spawned by [`crate::spawn`]).
pub(crate) async fn run(client: PenpotClient, cfg: SyncConfig, mut shutdown: watch::Receiver<bool>) {
    let poll_interval = cfg.poll_interval;
    let mut engine = match Engine::new(client, cfg) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "sync daemon failed to initialize (manifest unreadable?); NOT resetting anything — fix the manifest and restart");
            return;
        }
    };

    // Startup reconciliation: retried forever (the backend may still be
    // settling); must complete before the poll loop starts.
    loop {
        if *shutdown.borrow() {
            return;
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

    tracing::info!(root = %engine.cfg.sync_root.display(), "sync daemon: reconciliation complete; polling every {poll_interval:?}");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("sync daemon stopping");
                return;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
        engine.poll_cycle().await;
    }
}

impl Engine {
    fn new(client: PenpotClient, cfg: SyncConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.sync_root)
            .with_context(|| format!("cannot create sync root {}", cfg.sync_root.display()))?;
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
        })
    }

    // ------------------------------------------------------------------
    // Poll loop
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
            }
            Err(e) => {
                // NEVER treat a failed poll as deletions — skip the cycle.
                tracing::warn!(error = %e, "poll failed; skipping this cycle");
                return;
            }
        }
        for state in self.tracker.take_due(Instant::now()) {
            match self.export_file(&state, false).await {
                Ok(outcome) => {
                    self.tracker.mark_synced(&state);
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
                    self.tracker
                        .reschedule(state, Instant::now() + self.cfg.debounce);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Export pipeline (DB → disk), shared by poll loop and reconciliation
    // ------------------------------------------------------------------

    async fn export_file(
        &mut self,
        db: &DbFileState,
        conflict: bool,
    ) -> anyhow::Result<ExportOutcome> {
        if conflict {
            tracing::error!(
                file = %db.id,
                name = %db.name,
                "CONFLICT: both the DB and the on-disk tree changed since lastSyncedHash. M2 sync is one-way, so the DB version now overwrites the disk version. TODO(M3 conflict rule): export a .conflict-<timestamp>.penpot/ copy next to the file and never overwrite either side."
            );
        }
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

        // export-binfile + authenticated download, retried as one unit (the
        // artifact URI may not outlive a backend restart).
        let client = self.client.clone();
        let file_id = db.id.clone();
        let zip = with_retry("export-binfile", || {
            let c = client.clone();
            let id = file_id.clone();
            async move {
                let exported = c.export_binfile(&id, false, true).await?;
                c.download_exported_binfile(&exported.uri).await
            }
        })
        .await
        .with_context(|| format!("export-binfile for file {}", db.id))?;

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

        let unchanged = self
            .manifest
            .files
            .get(&db.id)
            .is_some_and(|e| e.last_synced_hash == hash)
            && target.is_dir();
        if unchanged {
            // No-op save: MUST NOT touch the target dir's mtimes.
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("discarding no-op stage {}", stage.display()))?;
            let entry = self.manifest.files.get_mut(&db.id).expect("checked above");
            entry.revn = db.revn;
            entry.project_id = db.project_id.clone();
            entry.project_name = db.project_name.clone();
            entry.last_synced_at = now_rfc3339();
            self.manifest.save(&self.cfg.sync_root)?;
            tracing::debug!(file = %db.id, path = %rel, "export was a semantic no-op; disk untouched");
            Ok(ExportOutcome::NoOp)
        } else {
            // Record the hash BEFORE the swap lands (PLAN.md step 6) so M3's
            // watcher can recognize our own write and ignore it.
            self.manifest.files.insert(
                db.id.clone(),
                ManifestEntry {
                    path: rel.clone(),
                    project_id: db.project_id.clone(),
                    project_name: db.project_name.clone(),
                    revn: db.revn,
                    last_synced_hash: hash,
                    last_synced_at: now_rfc3339(),
                },
            );
            self.manifest.save(&self.cfg.sync_root)?;
            commit_dir_swap(&stage, &target)?;
            tracing::info!(file = %db.id, path = %rel, revn = db.revn, "exported DB → disk");
            Ok(ExportOutcome::Updated)
        }
    }

    // ------------------------------------------------------------------
    // Import pipelines (disk → DB), reconciliation only in M2
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
                tracing::info!(file = %id, path = %rel, "reconciliation: imported disk → DB under the SAME file id (core-invariant path)");
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
                tracing::warn!(file = %id, path = %rel, "reconciliation: imported disk → DB as a NEW file id (fallback — links/refs into the old id break)");
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
        tracing::info!(file = %file_id, path = %rel, "reconciliation: imported unknown disk dir → DB as new file");
        self.manifest.files.insert(
            file_id.clone(),
            ManifestEntry {
                path: rel.to_string(),
                project_id,
                project_name,
                revn: 0, // corrected from the post-import snapshot below
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

        // 2. DB snapshot (retried; the backend may still be settling).
        let client = self.client.clone();
        let team = self.cfg.team_id.clone();
        let snap = with_retry("fetch db snapshot", || {
            let c = client.clone();
            let t = team.clone();
            async move { fetch_snapshot(&c, &t).await }
        })
        .await
        .context("listing projects/files for reconciliation")?;

        // 3. Disk walk + semantic hashes.
        let mut disk: BTreeMap<String, String> = BTreeMap::new();
        for rel in walk_penpot_dirs(&self.cfg.sync_root)? {
            let hash = semantic_tree_hash(&self.cfg.sync_root.join(&rel))
                .with_context(|| format!("hashing on-disk tree {rel}"))?;
            disk.insert(rel, hash);
        }

        // 4. Join: one decision per identity.
        let mut actions: Vec<ReconcileAction> = Vec::new();
        for (file_id, entry) in &self.manifest.files {
            let disk_hash = disk.get(&entry.path);
            let disk_facts = disk_hash.map(|h| DiskFacts { semantic_hash: h });
            let man_facts = ManifestFacts {
                last_synced_hash: &entry.last_synced_hash,
                revn: entry.revn,
            };
            let db_facts = snap.files.get(file_id).map(|f| DbFacts { revn: f.revn });
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
                    conflict: false,
                },
                Decision::ExportDbWinsConflict => ReconcileAction::Export {
                    file_id: file_id.clone(),
                    conflict: true,
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
                    conflict: false,
                });
            }
        }

        // 5. Execute: forgets, then imports (so the DB reflects the disk
        //    before any exports), then exports, then no-op seeding.
        // Per-file failures are logged loudly and skipped (never fatal): a
        // single permanently-broken file must not wedge the whole daemon in
        // the reconcile-retry loop. Unseeded files are re-detected by the
        // poll loop; un-imported dirs are retried at the next startup.
        let mut resolver = ProjectResolver::new(snap.projects.clone());
        let mut manifest_dirty = false;
        let mut imported_ids: Vec<String> = Vec::new();
        let (mut n_forget, mut n_import, mut n_export, mut n_noop) = (0u32, 0u32, 0u32, 0u32);
        let mut n_failed = 0u32;

        for action in &actions {
            if let ReconcileAction::Forget { file_id } = action {
                let entry = self.manifest.files.remove(file_id);
                tracing::warn!(
                    file = %file_id,
                    path = ?entry.map(|e| e.path),
                    "manifest entry has neither a disk dir nor a DB file; forgetting it"
                );
                manifest_dirty = true;
                n_forget += 1;
            }
        }
        for action in &actions {
            let result = match action {
                ReconcileAction::ImportInPlace {
                    file_id,
                    rel,
                    disk_hash,
                } => self
                    .import_in_place(&mut resolver, file_id, rel, disk_hash)
                    .await
                    .with_context(|| format!("in-place import of {rel}")),
                ReconcileAction::ImportAsNew { rel, disk_hash } => self
                    .import_as_new(&mut resolver, rel, disk_hash)
                    .await
                    .with_context(|| format!("import of {rel}")),
                _ => continue,
            };
            match result {
                Ok(id) => {
                    imported_ids.push(id);
                    manifest_dirty = true;
                    n_import += 1;
                }
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "reconciliation import failed; skipping (will be retried at next startup)");
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
            if let ReconcileAction::Export { file_id, conflict } = action {
                let db = snap.files.get(file_id).expect("export decided ⇒ in DB").clone();
                match self
                    .export_file(&db, *conflict)
                    .await
                    .with_context(|| format!("export of file {file_id}"))
                {
                    Ok(_) => {
                        self.tracker.mark_synced(&db);
                        n_export += 1;
                    }
                    Err(e) => {
                        // Not seeded → the poll loop re-detects and retries.
                        tracing::error!(error = format!("{e:#}"), "reconciliation export failed; the poll loop will retry");
                        n_failed += 1;
                    }
                }
            }
        }
        for action in &actions {
            if let ReconcileAction::Noop { file_id } = action {
                let db = snap.files.get(file_id).expect("noop decided ⇒ in DB");
                self.tracker.mark_synced(db);
                n_noop += 1;
            }
        }

        // 6. Seed the tracker (and correct advisory revn) for what we just
        //    imported: the import reset revn to the binfile's value.
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
                            if entry.revn != f.revn {
                                entry.revn = f.revn;
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
            failed = n_failed,
            "startup reconciliation done"
        );
        Ok(())
    }
}

/// All `*.penpot` directories under `root`, as sorted `/`-separated relative
/// paths. Dot-dirs are skipped; the walk does not descend into `.penpot` dirs
/// themselves (their contents are payload).
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
            if name.starts_with('.') {
                continue;
            }
            if name.ends_with(paths::PENPOT_DIR_SUFFIX) {
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
    fn walk_missing_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let got = walk_penpot_dirs(&tmp.path().join("nope")).unwrap();
        assert!(got.is_empty());
    }
}
