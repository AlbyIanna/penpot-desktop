# D2 — The Home Becomes the Front Door: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/__home` stops being a read-only lighttable and becomes what Penpot's dashboard was for a single user — create project, create file, rename, move, duplicate, delete, open — after which Penpot's dashboard is closed off for good.

**Architecture:** A new `apps/desktop/src/manage.rs` module mounts `POST /__api/vault/manage/*` routes that call the Penpot RPCs `crates/penpot-rpc` already ships, and lets the sync daemon carry the results to disk on its normal 2 s poll. Delete is the exception: it must also remove the file from the vault, because folder-is-truth would otherwise resurrect it. `home.html` grows the matching controls in its existing vanilla-JS, keyed-diff idiom. Finally `navwatch` closes `#/dashboard` by default and the escape hatch goes away.

**Tech Stack:** Rust (axum 0.8, tokio), `crates/penpot-rpc`, `crates/sync-core`, `crates/sync-daemon`, vanilla JS/CSS (no framework, no build step), bash + python3 + bundled Playwright for the gate.

## Global Constraints

- **Core invariant (P0):** delete the entire database, restart, and every project/file is rebuilt from the folder tree with no data loss. The user's folder tree is the source of truth; the Penpot DB is a disposable cache.
- **Invariant 3:** the SPA stays byte-untouched — no serve-time patching of upstream JS/CSS, no injected scripts, nothing under `runtime/frontend/` modified. Only configuration and URLs reach the canvas.
- **Conflict rule:** never silently overwrite either side. If both DB and filesystem changed since `lastSyncedHash`, export the DB version as a `.conflict-<timestamp>.penpot/` copy and surface it.
- **Delete semantics (product-owner decision, D2):** deleting moves the `.penpot` directory into a vault trash directory and drops its manifest entry. Never an unrecoverable `rm -rf` of user work.
- **D2 dedicated ports:** proxy 9048, backend 6510, postgres 5583, valkey 6526. (D1 uses 9046/6508/5581/6524 — do not collide.)
- Every commit message ends with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Never write a bare `#<number>` in commit text or PR text — GitHub autolinks it to an issue/PR. Write "milestone D2" / "open question 2".
- `just d2` must be chained into `just e2e`; the gate must be green twice.

## File Structure

| File | Responsibility |
|---|---|
| `crates/sync-core/src/trash.rs` | **New.** The trash primitive: move a `.penpot` dir under `<vault>/.trash/` and drop its manifest entry, atomically enough that no half-state survives. |
| `crates/sync-core/src/lib.rs` | Modify: `pub mod trash;` |
| `apps/desktop/src/manage.rs` | **New.** All D2 mutation routes + `ManageState`. One module because these verbs share state, validation and the pause/resume discipline. |
| `apps/desktop/src/lib.rs` | Modify: build `ManageState`, merge `manage::router(...)` into `extra`. |
| `apps/desktop/src/home.html` | Modify: new-project/new-file controls, per-card actions, remove the escape hatch. |
| `apps/desktop/src/navwatch.rs` | Modify: `#/dashboard` cancelled by default; `#/settings` unchanged. |
| `scripts/d2-home.sh` | **New.** The gate. |
| `scripts/d2_home_helper.py` | **New.** RPC/HTTP-level lifecycle driver + on-disk assertions. |
| `scripts/d2_home_nav.cjs` | **New.** Browser leg: drive the real UI, assert the dashboard is never loaded. |
| `scripts/routes_gate_nav.cjs` | Modify: its escape-hatch assertion dies with the hatch. |
| `justfile` | Modify: `d2` recipe + chain into `e2e`. |
| `docs/milestones/d2/README.md` + `img/` | **New.** The milestone doc. |

---

### Task 1: The trash primitive

**Files:**
- Create: `crates/sync-core/src/trash.rs`
- Modify: `crates/sync-core/src/lib.rs`

**Interfaces:**
- Consumes: `crate::manifest::{Manifest, ManifestEntry}` — `Manifest::load(sync_root) -> Result<Option<Manifest>>`, `Manifest::save(&self, sync_root) -> Result<()>`, field `files: BTreeMap<String, ManifestEntry>` keyed by file UUID, `ManifestEntry::path: String` (vault-relative, `/` separators).
- Produces:
  - `pub const TRASH_DIR_NAME: &str = ".trash";`
  - `pub fn trash_dir(vault_root: &Path) -> PathBuf`
  - `pub struct TrashOutcome { pub trashed_path: PathBuf, pub former_rel_path: String }`
  - `pub fn trash_file(vault_root: &Path, file_id: &str, stamp: &str) -> anyhow::Result<TrashOutcome>`

**Why a dot-directory:** `.trash` is invisible to every scanner for free — the daemon's tree walk (`crates/sync-daemon/src/engine.rs:1969`), the FS watcher (`crates/sync-daemon/src/watcher.rs:49,88`) and the vault index (`crates/vault-index/src/lib.rs:86`) all skip names starting with `.`. Do not invent an exclusion list; rely on this and assert it in Task 7.

- [ ] **Step 1: Write the failing tests**

Create `crates/sync-core/src/trash.rs` with only the tests plus `use` lines at first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};

    fn seed(root: &Path, file_id: &str, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
        std::fs::write(root.join(rel).join("file.json"), b"{}").unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            file_id.to_string(),
            ManifestEntry {
                path: rel.to_string(),
                project_id: "p1".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: "h".into(),
                last_synced_at: "2026-07-19T00:00:00Z".into(),
            },
        );
        m.save(root).unwrap();
    }

    #[test]
    fn trash_moves_the_directory_and_drops_the_manifest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");

        let out = trash_file(root, "f1", "20260719-120000").unwrap();

        // The original is gone from the live tree...
        assert!(!root.join("Proj/hello.penpot").exists());
        // ...and present under the trash dir, contents intact.
        assert!(out.trashed_path.starts_with(trash_dir(root)));
        assert!(out.trashed_path.join("file.json").exists());
        assert_eq!(out.former_rel_path, "Proj/hello.penpot");

        // The manifest no longer knows about it — this is what stops the
        // startup reconciliation from resurrecting it.
        let m = Manifest::load(root).unwrap().unwrap();
        assert!(!m.files.contains_key("f1"), "manifest entry survived: {:?}", m.files);
    }

    #[test]
    fn trashed_file_is_invisible_to_the_dot_directory_skip_rule() {
        // The whole design rests on scanners skipping dot-dirs. Pin the shape
        // of the path we produce so a rename of TRASH_DIR_NAME to something
        // undotted fails loudly here rather than silently resurrecting files.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        let out = trash_file(root, "f1", "20260719-120000").unwrap();
        let rel = out.trashed_path.strip_prefix(root).unwrap();
        let first = rel.components().next().unwrap().as_os_str().to_string_lossy().to_string();
        assert!(first.starts_with('.'), "trash root must be a dot-dir, got {first}");
    }

    #[test]
    fn trashing_twice_does_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        let a = trash_file(root, "f1", "20260719-120000").unwrap();
        seed(root, "f2", "Proj/hello.penpot");
        let b = trash_file(root, "f2", "20260719-120000").unwrap();
        assert_ne!(a.trashed_path, b.trashed_path, "same-stamp trashes collided");
        assert!(a.trashed_path.join("file.json").exists(), "first trash was clobbered");
    }

    #[test]
    fn unknown_file_id_is_an_error_not_a_silent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        assert!(trash_file(root, "nope", "20260719-120000").is_err());
        assert!(root.join("Proj/hello.penpot").exists(), "unrelated file was touched");
    }

    #[test]
    fn missing_directory_still_drops_the_manifest_entry() {
        // Disk already gone (user deleted it in Finder) but the manifest still
        // lists it: dropping the entry is exactly what must happen, otherwise
        // the entry lingers forever.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        std::fs::remove_dir_all(root.join("Proj/hello.penpot")).unwrap();
        trash_file(root, "f1", "20260719-120000").unwrap();
        let m = Manifest::load(root).unwrap().unwrap();
        assert!(!m.files.contains_key("f1"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sync-core trash::`
Expected: FAIL — `cannot find function trash_file in this scope`.

- [ ] **Step 3: Implement**

Prepend to `crates/sync-core/src/trash.rs`:

```rust
//! D2 delete: move a file's `.penpot` directory out of the live vault tree
//! instead of removing it.
//!
//! Why this exists at all: the core invariant resurrects anything that is on
//! disk but missing from the DB (`crates/sync-daemon/src/plan.rs`,
//! `(Some, Some, None) => ImportInPlace`) — that is how a wiped database
//! rebuilds itself from the folder tree. "The user deleted this file" reaches
//! the daemon as that exact same state, so an RPC-only delete comes back at
//! the next startup reconciliation. Deleting therefore has to leave the live
//! tree AND the manifest, together.
//!
//! It moves rather than removes because the folder tree is the user's own
//! work and the source of truth. `.trash/` is a dot-directory, which every
//! scanner already skips (daemon walk, FS watcher, vault index), so trashed
//! files are inert without any new exclusion logic.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::manifest::Manifest;

/// Dot-prefixed on purpose — see the module docs.
pub const TRASH_DIR_NAME: &str = ".trash";

/// Where trashed files live for a given vault.
pub fn trash_dir(vault_root: &Path) -> PathBuf {
    vault_root.join(TRASH_DIR_NAME)
}

/// What a successful trash did, for logging and for the API response.
#[derive(Debug, Clone)]
pub struct TrashOutcome {
    pub trashed_path: PathBuf,
    pub former_rel_path: String,
}

/// Move `file_id`'s directory into the vault trash and drop its manifest entry.
///
/// `stamp` is caller-supplied (an RFC3339-ish compact timestamp) so this stays
/// deterministic and unit-testable. Order matters: the directory moves first,
/// and the manifest is only rewritten once the move succeeded — a crash
/// between the two leaves a manifest entry pointing at a missing directory,
/// which the daemon already tolerates, whereas the reverse would leave a live
/// directory with no manifest entry and re-import it as a brand new file.
pub fn trash_file(vault_root: &Path, file_id: &str, stamp: &str) -> Result<TrashOutcome> {
    let mut manifest = Manifest::load(vault_root)
        .context("loading manifest")?
        .ok_or_else(|| anyhow!("no manifest in {}", vault_root.display()))?;

    let entry = manifest
        .files
        .get(file_id)
        .ok_or_else(|| anyhow!("file id {file_id} is not in the manifest"))?;
    let rel = entry.path.clone();

    let src = vault_root.join(&rel);
    let dest = unique_dest(vault_root, &rel, stamp)?;

    if src.exists() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).context("creating trash dir")?;
        }
        std::fs::rename(&src, &dest)
            .with_context(|| format!("moving {} to {}", src.display(), dest.display()))?;
    }

    manifest.files.remove(file_id);
    manifest.save(vault_root).context("saving manifest")?;

    Ok(TrashOutcome { trashed_path: dest, former_rel_path: rel })
}

/// `<vault>/.trash/<stamp>-<basename>` , suffixed if that already exists so a
/// second delete of the same name in the same second cannot clobber the first.
fn unique_dest(vault_root: &Path, rel: &str, stamp: &str) -> Result<PathBuf> {
    let base = Path::new(rel)
        .file_name()
        .ok_or_else(|| anyhow!("manifest path has no file name: {rel}"))?
        .to_string_lossy()
        .to_string();
    let dir = trash_dir(vault_root);
    let first = dir.join(format!("{stamp}-{base}"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..1000 {
        let cand = dir.join(format!("{stamp}-{n}-{base}"));
        if !cand.exists() {
            return Ok(cand);
        }
    }
    Err(anyhow!("could not find a free trash name for {rel}"))
}
```

Add to `crates/sync-core/src/lib.rs` next to the other `pub mod` lines:

```rust
pub mod trash;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sync-core trash::`
Expected: PASS, 5 tests.

Then run the whole crate to be sure nothing else moved: `cargo test -p sync-core`

- [ ] **Step 5: Commit**

```bash
git add crates/sync-core/src/trash.rs crates/sync-core/src/lib.rs
git commit -m "D2: vault trash primitive — delete must leave the tree AND the manifest

An RPC-only delete is invisible to the folder tree, so the startup
reconciliation resurrects the file the user just deleted: (disk, manifest, no
DB) is exactly the wiped-database state the core invariant exists to repair.
Trashing moves the directory under a dot-prefixed .trash/ (already skipped by
the daemon walk, the FS watcher and the index) and drops the manifest entry.

Moves rather than removes: the folder tree is the user's own work.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: The manage module — create, rename, move

**Files:**
- Create: `apps/desktop/src/manage.rs`
- Modify: `apps/desktop/src/lib.rs`

**Interfaces:**
- Consumes: `penpot_rpc::PenpotClient` methods, verified present in `crates/penpot-rpc/src/lib.rs`:
  - `create_project(&self, team_id: &str, name: &str) -> Result<ProjectInfo>` (:302)
  - `create_file(&self, project_id: &str, name: &str) -> Result<CreatedFile>` (:310)
  - `rename_file(&self, file_id: &str, name: &str) -> Result<RenamedFile>` (:378)
  - `rename_project(&self, project_id: &str, name: &str) -> Result<()>` (:399)
  - `move_files(&self, file_ids: &[&str], target_project_id: &str) -> Result<()>` (:388)
  - `get_projects(&self, team_id: &str) -> Result<Vec<ProjectInfo>>` (:290)
  - Construction pattern from `apps/desktop/src/packages.rs:81-86`: `PenpotClient::new(&backend_base).with_auth(Auth::Token(t))`
- Produces:
  - `pub struct ManageState { pub backend_base: String, pub token: Option<String>, pub team_id: String, pub vault_root: PathBuf, pub sync: Option<sync_daemon::SyncControl> }`
  - `pub fn router(state: Arc<ManageState>) -> Router`
  - Routes: `POST /__api/vault/manage/project` `{name}` → `{projectId, name}`; `POST /__api/vault/manage/file` `{projectId, name}` → `{fileId, name}`; `POST /__api/vault/manage/rename` `{kind:"file"|"project", id, name}` → `{ok:true}`; `POST /__api/vault/manage/move` `{fileIds:[..], projectId}` → `{ok:true}`
  - `pub fn valid_name(raw: &str) -> Result<String, String>`

**Design note:** these four verbs are pure RPC passthroughs — the sync daemon carries the result to disk on its own 2 s poll, so none of them touch the vault. Only delete (Task 4) does, and only because it must.

- [ ] **Step 1: Write the failing test**

Create `apps/desktop/src/manage.rs` containing only this test module to start:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_trims_and_accepts_ordinary_names() {
        assert_eq!(valid_name("  Homepage  ").unwrap(), "Homepage");
        assert_eq!(valid_name("Client X — v2").unwrap(), "Client X — v2");
    }

    #[test]
    fn valid_name_rejects_empty_and_overlong() {
        assert!(valid_name("").is_err());
        assert!(valid_name("   ").is_err());
        assert!(valid_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn valid_name_rejects_path_separators_and_traversal() {
        // The name becomes a DIRECTORY NAME on disk once the daemon exports
        // it, so a separator or a traversal segment would write outside the
        // project folder.
        for bad in ["a/b", "a\\b", "..", ".", "../escape", "x/../y"] {
            assert!(valid_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn valid_name_rejects_control_characters_and_nul() {
        assert!(valid_name("a\nb").is_err());
        assert!(valid_name("a\0b").is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p penpot-desktop manage::`
Expected: FAIL — `cannot find function valid_name`.

- [ ] **Step 3: Implement**

Prepend to `apps/desktop/src/manage.rs`:

```rust
//! D2: the mutation verbs behind `/__home`. Penpot's dashboard is no longer
//! the way a single user manages their files — this module is.
//!
//! Everything here except delete is a straight RPC passthrough: create/rename/
//! move change the DB, and the sync daemon carries the change to the folder
//! tree on its normal poll. Delete is different and lives in the same module
//! because it shares this state — see `delete_file`.
//!
//! Route shape and registration follow `packages.rs` (the E2/E7 precedent):
//! a `pub fn router(Arc<State>)` merged into the proxy's extra router in
//! `lib.rs::boot`, JSON in / JSON out, blocking work in `spawn_blocking`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use http::StatusCode;
use penpot_rpc::{Auth, PenpotClient};
use serde::Deserialize;
use serde_json::json;

/// Longest name we accept. Penpot itself is more permissive; this is about
/// keeping the derived on-disk directory name sane across filesystems.
const MAX_NAME_LEN: usize = 200;

pub struct ManageState {
    pub backend_base: String,
    pub token: Option<String>,
    pub team_id: String,
    pub vault_root: PathBuf,
    /// Lets delete pause the daemon across its two-step operation.
    pub sync: Option<sync_daemon::SyncControl>,
}

impl ManageState {
    fn client(&self) -> Option<PenpotClient> {
        self.token
            .clone()
            .map(|t| PenpotClient::new(&self.backend_base).with_auth(Auth::Token(t)))
    }
}

/// Validate a user-supplied project/file name.
///
/// This name becomes a DIRECTORY NAME in the user's folder tree once the
/// daemon exports it, so separators and traversal segments are rejected here
/// rather than sanitised — silently rewriting what the user typed would make
/// the name on screen disagree with the name on disk.
pub fn valid_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("name must be at most {MAX_NAME_LEN} characters"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name must not contain path separators".into());
    }
    if name == "." || name == ".." || name.split('/').any(|s| s == "..") {
        return Err("name must not be a path traversal segment".into());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("name must not contain control characters".into());
    }
    Ok(name.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewProjectReq {
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewFileReq {
    pub project_id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameReq {
    /// "file" or "project"
    pub kind: String,
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveReq {
    pub file_ids: Vec<String>,
    pub project_id: String,
}

fn bad_request(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn upstream_error(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn no_token() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "no access token — the stack is still provisioning" })),
    )
        .into_response()
}

async fn new_project(State(st): State<Arc<ManageState>>, Json(req): Json<NewProjectReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    match client.create_project(&st.team_id, &name).await {
        Ok(p) => Json(json!({ "projectId": p.id, "name": name })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn new_file(State(st): State<Arc<ManageState>>, Json(req): Json<NewFileReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    match client.create_file(&req.project_id, &name).await {
        Ok(f) => Json(json!({ "fileId": f.id, "name": name })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn rename(State(st): State<Arc<ManageState>>, Json(req): Json<RenameReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    let res = match req.kind.as_str() {
        "file" => client.rename_file(&req.id, &name).await.map(|_| ()),
        "project" => client.rename_project(&req.id, &name).await,
        other => return bad_request(format!("unknown kind {other:?}")),
    };
    match res {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn move_files(State(st): State<Arc<ManageState>>, Json(req): Json<MoveReq>) -> Response {
    if req.file_ids.is_empty() {
        return bad_request("fileIds must not be empty");
    }
    let Some(client) = st.client() else { return no_token() };
    let ids: Vec<&str> = req.file_ids.iter().map(|s| s.as_str()).collect();
    match client.move_files(&ids, &req.project_id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => upstream_error(e),
    }
}

/// Build the D2 manage routes for the proxy's extra router.
pub fn router(state: Arc<ManageState>) -> Router {
    Router::new()
        .route("/__api/vault/manage/project", post(new_project))
        .route("/__api/vault/manage/file", post(new_file))
        .route("/__api/vault/manage/rename", post(rename))
        .route("/__api/vault/manage/move", post(move_files))
        .with_state(state)
}
```

Wire it in `apps/desktop/src/lib.rs`: add `mod manage;` beside the other module declarations, then build the state and merge the router where `extra` is assembled (around `lib.rs:934`), reusing the same `backend_base`, token and `team_id` values `PackagesState` is already given there:

```rust
    let manage_state = std::sync::Arc::new(manage::ManageState {
        backend_base: backend_base.clone(),
        token: token.clone(),
        team_id: team_id.clone(),
        vault_root: designs_dir.clone(),
        sync: sync_control.clone(),
    });
```

and add `.merge(manage::router(manage_state))` to the `extra` chain.

**Note for the implementer:** the exact local variable names for the backend base, token, team id, designs dir and sync control at that point in `boot` may differ — read the surrounding code (the `PackagesState { .. }` literal is right there and already has four of the five) and use whatever those bindings are actually called. If no `SyncControl` is in scope at that point, pass `None` and say so in your report; Task 4 needs it and will wire it properly.

- [ ] **Step 4: Run tests**

Run: `cargo test -p penpot-desktop manage::`
Expected: PASS, 4 tests.

Run: `cargo build -p penpot-desktop` — expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/manage.rs apps/desktop/src/lib.rs
git commit -m "D2: manage routes for create project/file, rename, move

Pure RPC passthroughs onto the commands penpot-rpc already ships; the sync
daemon carries each result to the folder tree on its normal poll, so none of
these verbs touch the vault directly.

Names are validated rather than sanitised: the name becomes a directory name
on disk once the daemon exports it, and silently rewriting what the user typed
would make the name on screen disagree with the name in their folder tree.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Duplicate a file

**Files:**
- Modify: `apps/desktop/src/manage.rs`

**Interfaces:**
- Consumes: `crate::installer::import_binfile_and_settle(client, project_id, display, bytes, version) -> anyhow::Result<(String, usize)>` (`apps/desktop/src/installer.rs:46`); `PenpotClient::export_binfile(file_id, include_libraries: bool, embed_assets: bool)` (`penpot-rpc/src/lib.rs:494`) and `download_exported_binfile(&uri)` (:519).
- Produces: `POST /__api/vault/manage/duplicate` `{fileId, name}` → `{fileId: <new id>, name}`.

**Why this shape:** there is no `duplicate-file` RPC in Penpot 2.16.2. The composition export → download → import-as-new is exactly what `templates.rs`'s "new from template" already does, so duplicate reuses a proven path rather than inventing one. Export flags must be `(include_libraries: false, embed_assets: true)` — `(true, true)` is rejected by the server (E3 finding).

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `apps/desktop/src/manage.rs`:

```rust
    #[test]
    fn duplicate_name_defaults_are_validated_like_any_other_name() {
        // The duplicate route takes a user-supplied name for the copy; it must
        // go through the same gate as create/rename, not a looser one.
        assert!(valid_name("Homepage copy").is_ok());
        assert!(valid_name("../evil").is_err());
    }

    #[test]
    fn duplicate_export_flags_are_the_server_accepted_pair() {
        // (include_libraries=true, embed_assets=true) is rejected by Penpot
        // 2.16.2 (E3). Pin the pair we send so a well-meaning edit to
        // "include everything" fails here instead of at runtime.
        assert_eq!(DUPLICATE_EXPORT_FLAGS, (false, true));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p penpot-desktop manage::`
Expected: FAIL — `cannot find value DUPLICATE_EXPORT_FLAGS`.

- [ ] **Step 3: Implement**

Add to `apps/desktop/src/manage.rs`:

```rust
/// `(include_libraries, embed_assets)` for the duplicate export.
///
/// `(true, true)` is server-rejected on Penpot 2.16.2 (E3). A duplicate wants
/// its own copy of the media but must not drag linked libraries in, so this
/// is the only correct pair.
const DUPLICATE_EXPORT_FLAGS: (bool, bool) = (false, true);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateReq {
    pub file_id: String,
    pub name: String,
    /// Target project; defaults to the source file's project when omitted.
    #[serde(default)]
    pub project_id: Option<String>,
}

async fn duplicate_file(State(st): State<Arc<ManageState>>, Json(req): Json<DuplicateReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };

    let Some(project_id) = req.project_id.clone() else {
        return bad_request("projectId is required");
    };

    let (include_libraries, embed_assets) = DUPLICATE_EXPORT_FLAGS;
    let uri = match client.export_binfile(&req.file_id, include_libraries, embed_assets).await {
        Ok(u) => u,
        Err(e) => return upstream_error(format!("export failed: {e}")),
    };
    let bytes = match client.download_exported_binfile(&uri).await {
        Ok(b) => b,
        Err(e) => return upstream_error(format!("download failed: {e}")),
    };
    match crate::installer::import_binfile_and_settle(&client, &project_id, &name, bytes, 3).await {
        Ok((new_id, _)) => Json(json!({ "fileId": new_id, "name": name })).into_response(),
        Err(e) => upstream_error(format!("import failed: {e}")),
    }
}
```

Add the route to `router`:

```rust
        .route("/__api/vault/manage/duplicate", post(duplicate_file))
```

**Note for the implementer:** check `import_binfile_and_settle`'s real signature at `apps/desktop/src/installer.rs:46` before wiring — the argument order, the borrow-vs-owned client, and the meaning of the last parameter (binfile version) must match what is actually there. If it differs from the call above, follow the real signature and note the difference in your report rather than changing `installer.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p penpot-desktop manage::` — expected PASS (6 tests).
Run: `cargo build -p penpot-desktop` — expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/manage.rs
git commit -m "D2: duplicate a file by export -> import-as-new

Penpot 2.16.2 has no duplicate-file RPC, so this composes the same
export/download/import-as-new path templates.rs already uses for new-from-
template, which mints a fresh id and settles to a round-trip fixpoint.

Export flags are pinned to (include_libraries=false, embed_assets=true) with a
test, because (true, true) is server-rejected and the failure would otherwise
only show up at runtime.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Delete a file — the invariant-safe path

**Files:**
- Modify: `apps/desktop/src/manage.rs`
- Modify: `apps/desktop/src/lib.rs` (pass a real `SyncControl` if Task 2 had to pass `None`)

**Interfaces:**
- Consumes: `sync_core::trash::{trash_file, TrashOutcome}` from Task 1; `sync_daemon::SyncControl` with `pause()` / `resume()` / `is_paused()` (`crates/sync-daemon/src/status.rs:118-136`); `PenpotClient::delete_file(&self, file_id: &str) -> Result<()>` (`penpot-rpc/src/lib.rs:406`).
- Produces: `POST /__api/vault/manage/delete` `{fileId}` → `{ok:true, trashedPath:"<vault-rel>"}`.

**The ordering problem — read before implementing.** The daemon polls every 2 s and decides from the triple `(disk, manifest, db)`:

- Trash first, delete second → the daemon can observe `(None, None, Some)` = "a new file in the DB" and export it straight back to disk.
- Delete first, trash second → the daemon can observe `(Some, Some, None)` = the wiped-database state, and re-import the file the user just deleted.

Both orders lose if the daemon looks in between. So the operation runs with the daemon **paused**, and the pause must be released on every exit path including errors.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `apps/desktop/src/manage.rs`:

```rust
    #[test]
    fn delete_stamp_is_filename_safe() {
        // The stamp lands in a directory name inside .trash/.
        let s = super::trash_stamp_from(1_753_000_000);
        assert!(!s.contains(':'), "colon in {s} breaks on some filesystems");
        assert!(!s.contains('/'), "separator in {s}");
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "unexpected char in {s}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p penpot-desktop manage::`
Expected: FAIL — `cannot find function trash_stamp_from`.

- [ ] **Step 3: Implement**

Add to `apps/desktop/src/manage.rs`:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReq {
    pub file_id: String,
}

/// Compact, filesystem-safe stamp (`YYYYMMDD-HHMMSS`) from a unix timestamp.
/// Split out from the handler so it is testable without a clock.
pub fn trash_stamp_from(unix_secs: i64) -> String {
    // Deliberately not RFC3339: colons are illegal in directory names on
    // Windows and awkward everywhere else.
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Avoids pulling in `chrono`
/// just to name a directory.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

async fn delete_file(State(st): State<Arc<ManageState>>, Json(req): Json<DeleteReq>) -> Response {
    let Some(client) = st.client() else { return no_token() };

    // Pause the daemon for the whole two-step operation. Either order of
    // (RPC delete, trash) exposes a state the daemon would "repair" — see the
    // module docs — so it must not observe the midpoint at all.
    if let Some(sync) = &st.sync {
        sync.pause();
    }
    let result = delete_inner(&st, &client, &req.file_id).await;
    if let Some(sync) = &st.sync {
        sync.resume();
    }

    match result {
        Ok(rel) => Json(json!({ "ok": true, "trashedPath": rel })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn delete_inner(
    st: &ManageState,
    client: &PenpotClient,
    file_id: &str,
) -> Result<String, String> {
    // DB first: if this fails, the vault is untouched and the user still has
    // their file. The reverse (trash first) would leave the DB authoritative
    // over an empty tree.
    client.delete_file(file_id).await.map_err(|e| format!("delete-file RPC failed: {e}"))?;

    let stamp = trash_stamp_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let vault = st.vault_root.clone();
    let id = file_id.to_string();
    let outcome = tokio::task::spawn_blocking(move || sync_core::trash::trash_file(&vault, &id, &stamp))
        .await
        .map_err(|e| format!("trash task panicked: {e}"))?
        .map_err(|e| format!("trashing the file failed: {e}"))?;

    Ok(outcome
        .trashed_path
        .strip_prefix(&st.vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| outcome.trashed_path.to_string_lossy().to_string()))
}
```

Add the route:

```rust
        .route("/__api/vault/manage/delete", post(delete_file))
```

Extend the module docs at the top of `manage.rs` with the ordering rationale:

```rust
//! Delete is the one verb that touches the vault. It must, because the core
//! invariant resurrects anything on disk but missing from the DB, and "the
//! user deleted this" is indistinguishable from "the DB was wiped". It runs
//! with the sync daemon PAUSED because both possible orderings expose a state
//! the daemon would otherwise repair: trash-then-delete looks like a new file
//! in the DB and gets re-exported, delete-then-trash looks like a wiped DB and
//! gets re-imported.
```

If Task 2 passed `sync: None`, wire the real `SyncControl` now — it comes from the sync daemon handle (`SyncDaemonHandle::control()`, `crates/sync-daemon/src/lib.rs:124-149`) created in `boot`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p penpot-desktop manage::` — expected PASS (7 tests).
Run: `cargo test -p penpot-desktop` — expected: all green.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/manage.rs apps/desktop/src/lib.rs
git commit -m "D2: delete a file without letting folder-is-truth undo it

Delete is the only verb that touches the vault, and it has an ordering trap:
trash-then-delete looks to the daemon like a new file in the DB and gets
re-exported, delete-then-trash looks like a wiped DB and gets re-imported.
Both orders lose if the daemon polls in between, so the operation runs with
the daemon paused and resumes on every exit path including errors.

The DB side goes first: if it fails the vault is untouched and the user still
has their file.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: The home UI grows the verbs

**Files:**
- Modify: `apps/desktop/src/home.html`

**Interfaces:**
- Consumes: the routes from Tasks 2-4.
- Produces: DOM contract the gate keys off — `button#new-project`, `button#new-file`, and on each card `button.card-action[data-action="rename"|"duplicate"|"move"|"delete"][data-file-id]`.

**Match the existing idiom — read `home.html` first.** It is vanilla JS in one inline `<script>`, no framework, no build step. Rendering is a **keyed diff/patch** over `cardMap` (`home.html:293-343`) that never does `innerHTML = ""`, so scroll position and keyboard focus survive the 5 s poll. New controls must not break that: add them inside the existing card template and re-use `button.bar` styling and the existing CSS custom properties. Do not introduce a framework, a bundler, or a new styling system.

- [ ] **Step 1: Add the create controls**

In the sticky `<header>` (`home.html:154-170`), beside the existing "Checkpoint now" button, add:

```html
      <button class="bar" id="new-project" type="button">New project</button>
      <button class="bar" id="new-file" type="button">New file</button>
```

- [ ] **Step 2: Add per-card actions**

In the card template used by the keyed renderer, append an actions row inside each card:

```html
<div class="card-actions">
  <button class="card-action" data-action="rename"    type="button" title="Rename">Rename</button>
  <button class="card-action" data-action="duplicate" type="button" title="Duplicate">Duplicate</button>
  <button class="card-action" data-action="move"      type="button" title="Move to project">Move</button>
  <button class="card-action" data-action="delete"    type="button" title="Move to trash">Delete</button>
</div>
```

Set `data-file-id` on each button from the board's file id when the card is created, and update it in the patch path so a recycled card never carries a stale id. Add CSS next to the existing card rules:

```css
    .card-actions { display: flex; gap: 6px; padding: 6px 8px 8px; flex-wrap: wrap; }
    .card-action {
      font: inherit; font-size: 12px; padding: 2px 8px; cursor: pointer;
      color: var(--fg); background: var(--panel);
      border: 1px solid var(--border); border-radius: 6px;
    }
    .card-action:hover { border-color: var(--accent); }
```

- [ ] **Step 3: Wire the calls**

Add one delegated listener rather than per-card handlers (cards are recycled by the patch path, so per-card listeners would leak):

```javascript
    async function post(path, body) {
      const res = await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const data = await res.json().catch(function () { return {}; });
      if (!res.ok) throw new Error(data.error || ("HTTP " + res.status));
      return data;
    }

    // One delegated listener: cards are recycled by the keyed patch path, so
    // per-card handlers would leak and could fire against a stale file id.
    document.getElementById("grid").addEventListener("click", async function (ev) {
      const btn = ev.target.closest(".card-action");
      if (!btn) return;
      ev.preventDefault();          // the card itself is an <a>; don't navigate
      ev.stopPropagation();
      const fileId = btn.getAttribute("data-file-id");
      const action = btn.getAttribute("data-action");
      if (!fileId) return;
      try {
        if (action === "rename") {
          const name = window.prompt("New name?");
          if (!name) return;
          await post("/__api/vault/manage/rename", { kind: "file", id: fileId, name: name });
        } else if (action === "duplicate") {
          const name = window.prompt("Name for the copy?");
          if (!name) return;
          await post("/__api/vault/manage/duplicate", {
            fileId: fileId, name: name, projectId: btn.getAttribute("data-project-id"),
          });
        } else if (action === "move") {
          const projectId = window.prompt("Target project id?");
          if (!projectId) return;
          await post("/__api/vault/manage/move", { fileIds: [fileId], projectId: projectId });
        } else if (action === "delete") {
          if (!window.confirm("Move this file to the vault trash?")) return;
          await post("/__api/vault/manage/delete", { fileId: fileId });
        }
        refresh();
      } catch (e) {
        setStripMessage("Action failed: " + e.message);
      }
    });

    document.getElementById("new-project").addEventListener("click", async function () {
      const name = window.prompt("Project name?");
      if (!name) return;
      try { await post("/__api/vault/manage/project", { name: name }); refresh(); }
      catch (e) { setStripMessage("Could not create the project: " + e.message); }
    });

    document.getElementById("new-file").addEventListener("click", async function () {
      const projectId = document.getElementById("project-filter").value;
      if (!projectId || projectId === "all") {
        setStripMessage("Pick a project first, then New file.");
        return;
      }
      const name = window.prompt("File name?");
      if (!name) return;
      try { await post("/__api/vault/manage/file", { projectId: projectId, name: name }); refresh(); }
      catch (e) { setStripMessage("Could not create the file: " + e.message); }
    });
```

**Note for the implementer:** `refresh()`, `setStripMessage()` and the project `<select>`'s real element id may be named differently in the current file — read `home.html` and call whatever the existing functions/ids actually are. Do not add new ones if an equivalent exists. If there is no existing "show a message in the strip" helper, add one small function rather than using `alert()`.

Because the vault index lags the manifest by one poll and the daemon itself polls every 2 s, a single `refresh()` will often still show stale data. That is expected and acceptable here: the existing 5 s poll converges. Do not add a spinner or optimistic row insertion — the gate asserts convergence, not immediacy.

- [ ] **Step 4: Verify by hand**

Run: `cargo build -p penpot-desktop --bin headless`, boot the D2 port block, open `/__home`, and confirm the buttons render, a new project appears within ~10 s, and rename/duplicate/delete work. Then confirm the keyed diff still holds: scroll down, wait through two polls, and check the scroll position does not jump.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/home.html
git commit -m "D2: the home page grows create, rename, duplicate, move and delete

Keeps the existing idiom deliberately: vanilla JS, no framework, no build step,
and one delegated listener rather than per-card handlers, because the keyed
diff/patch recycles cards and per-card handlers would leak and could fire
against a stale file id.

No optimistic rendering. The daemon polls every 2s and the index lags it by
one more, so the honest behaviour is to let the existing poll converge rather
than draw a row that may not exist yet.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Close the dashboard

**Files:**
- Modify: `apps/desktop/src/navwatch.rs`
- Modify: `apps/desktop/src/home.html`
- Modify: `scripts/routes_gate_nav.cjs`

**Interfaces:**
- Consumes: `navwatch::decide(url: &str, redirect_enabled: bool) -> Decision` and the existing constants.
- Produces: `#/dashboard` cancelled unconditionally, exactly like `#/auth`; `#/settings` behaviour UNCHANGED (still only under `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`).

**Why now and why not settings:** PLAN4's D2 says the `#/dashboard` → `/__home` redirect lands in this milestone, and it can only land now because Tasks 2-5 built the replacement. `#/settings` keeps its current behaviour: its replacement is D4's native Preferences, and closing a surface before its replacement exists is the mistake D1 explicitly avoided.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `apps/desktop/src/navwatch.rs`:

```rust
    #[test]
    fn dashboard_is_cancelled_even_with_redirect_disabled() {
        // D2: the replacement (/__home with create/rename/move/delete) now
        // exists, so the dashboard closes by default like the auth family.
        for url in [
            "http://localhost:9048/#/dashboard",
            "http://localhost:9048/#/dashboard/recent?team-id=abc",
            "http://localhost:9048/#/dashboard/fonts?team-id=abc",
        ] {
            match decide(url, false) {
                Decision::CancelAndRedirect(to) => assert!(to.ends_with(HOME_PATH), "{url} -> {to}"),
                other => panic!("{url} was not cancelled with redirect disabled: {other:?}"),
            }
        }
    }

    #[test]
    fn settings_is_unchanged_by_d2() {
        // Its replacement is D4's native Preferences. Closing a surface before
        // its replacement exists is exactly the mistake D1 avoided.
        assert!(matches!(decide("http://localhost:9048/#/settings/profile", false), Decision::Allow));
        assert!(matches!(
            decide("http://localhost:9048/#/settings/profile", true),
            Decision::CancelAndRedirect(_)
        ));
    }

    #[test]
    fn dashboard_prefix_boundary_still_holds() {
        // "#/dashboardx" must not be treated as the dashboard.
        assert!(matches!(decide("http://localhost:9048/#/dashboardx", false), Decision::Allow));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p penpot-desktop navwatch::`
Expected: FAIL on `dashboard_is_cancelled_even_with_redirect_disabled` — it currently returns `Allow` when `redirect_enabled` is false.

- [ ] **Step 3: Implement**

In `apps/desktop/src/navwatch.rs`, move `#/dashboard` from the env-gated class into the unconditional class alongside `#/auth`, leaving `#/settings` in the env-gated class. Update the module docs to say why: the dashboard's replacement shipped in D2, settings' replacement is D4.

- [ ] **Step 4: Remove the escape hatch**

In `apps/desktop/src/home.html`, delete the escape-hatch anchor (`home.html:169`, `<a id="escape-hatch" href="/#/dashboard/recent">`) and its now-unused `.escape` CSS rule. Leaving it would be a visible link that the navigation policy silently cancels — worse than no link.

- [ ] **Step 5: Update the routes gate**

`scripts/routes_gate_nav.cjs` asserts the escape hatch exists and navigates to `/#/dashboard/recent` (around :88-116). That assertion is now testing dead behaviour. Replace it with the inverse: assert `#escape-hatch` is **absent** from `/__home`. Do not simply delete the leg — an assertion that the hatch is gone is what stops it being reintroduced by accident.

- [ ] **Step 6: Run tests**

Run: `cargo test -p penpot-desktop navwatch::` — expected PASS.
Run: `node --check scripts/routes_gate_nav.cjs` — expected: no output.
Run: `cargo test -p penpot-desktop` — expected: all green.

- [ ] **Step 7: Commit**

```bash
git add apps/desktop/src/navwatch.rs apps/desktop/src/home.html scripts/routes_gate_nav.cjs
git commit -m "D2: close the dashboard now that the home page replaces it

The #/dashboard family is cancelled unconditionally, like #/auth. This could
only land now: the replacement verbs (create, rename, duplicate, move, delete)
shipped earlier in this milestone.

#/settings is deliberately unchanged -- its replacement is D4's native
Preferences, and closing a surface before its replacement exists is the mistake
D1 avoided.

The escape-hatch link is removed rather than left to be silently cancelled, and
the routes gate now asserts it is ABSENT so it cannot be reintroduced quietly.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: The gate

**Files:**
- Create: `scripts/d2-home.sh`, `scripts/d2_home_helper.py`, `scripts/d2_home_nav.cjs`
- Modify: `justfile`

**Interfaces:**
- Produces: `just d2`; chained into `just e2e`.
- `python3 scripts/d2_home_helper.py <cmd>` subcommands: `lifecycle` (drives create → rename → duplicate → move → delete through `/__api/vault/manage/*`), `assert-disk` (asserts the vault reflects a named state), `wait-present` (polls with a timeout, mirroring `scripts/n5_vaults_helper.py:378-403`).
- `BASE=<url> node scripts/d2_home_nav.cjs` → one JSON line `{"ok":bool,"escapeHatch":"gone|present","dashboardLoaded":bool,"actionsPresent":bool}`.

**Model it on `scripts/d1-offline.sh`** — same header block documenting the port set, same `pass`/`fail` helpers, same PID-scoped cleanup trap, same "ALL PASS" totals line, non-zero exit on any failure.

The gate must assert:

- [ ] **Step 1: The lifecycle, end to end through our own surfaces**

New project → new file in it → rename the file → duplicate it → move the duplicate to a second project → delete the original. Each step via `POST /__api/vault/manage/*`, never via Penpot's dashboard.

- [ ] **Step 2: The vault on disk reflects every operation**

After each step, poll (never assert on first read — the daemon polls every 2 s and the index lags one further) until the folder tree shows the expected state: the new `.penpot` directory exists under the project folder, the rename moved it, the duplicate exists as its own directory with a different file id, and the move relocated it.

- [ ] **Step 3: Delete lands, and STAYS deleted across a restart**

This is the milestone's most important assertion, because it is where the core invariant and the delete verb collide. After deleting: the directory is gone from the live tree, present under `.trash/`, and absent from the manifest. Then **restart the stack against the same data dir and vault**, let the startup reconciliation run, and assert the file did **not** come back — neither on disk nor in `get-project-files`. A delete that survives one boot but not two is the exact failure this design exists to prevent.

- [ ] **Step 4: `/dashboard` is never loaded in the whole session**

Boot with `PENPOT_LOCAL_NAVWATCH_LOG=<file>` (the D0 mechanism) and assert no logged navigation URL contains `#/dashboard`. Assert separately, via `d2_home_nav.cjs`, that `#escape-hatch` is absent from `/__home` and that the card action buttons are present — the same "prove you were looking" discipline D1 used, so an empty page cannot pass as "no dashboard".

- [ ] **Step 5: D0's deferred caveat — vault integrity with a REAL workspace open**

D0 asserted the vault survived a mid-session redirect, but measured a seeded canary with **no workspace open**, and explicitly deferred the real check to D2. Do it here: open a real file at `/#/workspace?team-id=…&file-id=…&page-id=…` in the bundled browser, let it fully render (use a route-identifying marker and a generous settle — a short settle silently lies, D1 finding), trigger a `#/dashboard` navigation so the policy cancels it, then assert the vault tree hash is unchanged and the file still opens. Reuse `scripts/roundtrip.py`'s hashing or `n5_vaults_helper.py`'s `cmd_tree_hash` rather than writing a third hasher.

- [ ] **Step 6: Wire into just**

Add a `d2` recipe mirroring `d1`, and add `d2` to the `e2e` chain.

- [ ] **Step 7: Verify and commit**

Run: `bash -n scripts/d2-home.sh`, `python3 -m py_compile scripts/d2_home_helper.py`, `node --check scripts/d2_home_nav.cjs`, `just --list`.

```bash
git add scripts/d2-home.sh scripts/d2_home_helper.py scripts/d2_home_nav.cjs justfile
git commit -m "D2: the front-door gate

Drives the full lifecycle through our own surfaces only, asserts the folder
tree reflects every operation, and asserts the dashboard is never loaded.

The load-bearing assertion is that a deleted file stays deleted ACROSS A
RESTART: delete is where the core invariant and the delete verb collide, and a
file that survives one boot but not two is exactly the failure the trash design
exists to prevent.

Also discharges D0's deferred caveat. D0 proved the vault survived a mid-session
redirect but measured a seeded canary with no workspace open; this opens a real
file first, then redirects.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: The milestone document

**Files:**
- Create: `docs/milestones/d2/README.md`, `docs/milestones/d2/img/*.png`

**Interfaces:**
- Consumes: `scripts/shots.sh` from D1 — `BASE=<url> OUT_DIR=docs/milestones/d2/img SETTLE_MS=15000 bash scripts/shots.sh <name=path>...`.

- [ ] **Step 1: Capture**

Capture `home-before`-equivalents from D1's committed baseline where useful, plus new shots: the home page with the action controls, a card mid-action, and the trash directory in a file manager (a plain screenshot is fine for that one). Remember the two gotchas D1 documented: `/__bootstrap` is one-shot per boot, and a short settle silently lies.

- [ ] **Step 2: Write it**

Follow `docs/milestones/d1/README.md`'s shape: what changed and why, before/after images, a diagram, then **known limits stated not buried**. Cover honestly:

- Delete moves to `.trash/` and nothing empties it yet — the vault grows until the user clears it by hand.
- Move and duplicate take raw project ids through `window.prompt` in this milestone; a real picker is native-dialog work (D4).
- `#/settings` is still reachable; its replacement is D4.
- Whether the daemon's rename handling leaves a stale directory behind, measured rather than assumed.

- [ ] **Step 3: Commit**

```bash
git add docs/milestones/d2
git commit -m "docs(d2): the milestone document — the front door, and what it still cannot do

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage.** PLAN4's D2 asks for: create file ✅ (Task 2), create project ✅ (2), rename ✅ (2), duplicate ✅ (3), move ✅ (2), delete ✅ (4), open in workspace ✅ (already works via the existing deep link; Task 7 asserts it), boot → `/__home` ✅ (already the case — `bootstrap_login` redirects to `/__home`, `lib.rs:500`; Task 7 asserts it), `#/dashboard` → `/__home` redirect ✅ (6), the gate's four assertions ✅ (7), green twice ✅ (7). D0's deferred workspace-open caveat ✅ (7, step 5).

**2. Placeholders.** No "TBD"/"handle errors appropriately". Three tasks carry explicit "read the real signature/name before wiring" notes — those are instructions to verify against real code, not deferred decisions, and each names the exact file and line to check.

**3. Type consistency.** `trash_file(vault_root, file_id, stamp) -> Result<TrashOutcome>` defined in Task 1 and consumed with that exact signature in Task 4. `ManageState` fields defined in Task 2 (`sync: Option<SyncControl>`) and used in Task 4. Route paths `/__api/vault/manage/{project,file,rename,move,duplicate,delete}` consistent between Tasks 2-5 and the gate in Task 7. `valid_name` defined once in Task 2 and reused by Tasks 3-4.

**Known risk carried into execution:** whether Penpot's `rename-file` / `move-files` cause the daemon to *relocate* the on-disk directory or to leave a stale copy beside the new one is not documented anywhere in the repo and was not verified while writing this plan. Task 7 step 2 asserts the correct behaviour; if the daemon turns out not to handle path changes, that assertion fails loudly and the fix belongs in `sync-daemon`, not in a workaround inside `manage.rs`.
