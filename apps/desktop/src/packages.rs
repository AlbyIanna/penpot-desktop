//! E2 — the package home, lockfile, and generalized installer (PLAN3 ch. 3).
//!
//! A **package is just a folder / git repo** under `<vault>/.penpot-packages/`
//! (`sync_core::PACKAGES_DIR_NAME`). That dir is blind to BOTH sync directions
//! (the daemon never enumerates, hashes, conflict-copies, or imports anything
//! under it — proven in `sync-daemon`), so nothing a package contains ever
//! enters your files by surprise. **Install is an explicit verb.**
//!
//! Three verbs, served same-origin through the proxy's extra router (like N6's
//! `/__templates`), so a headless gate or the GUI can drive them over loopback:
//!
//! - `GET  /__api/packages` — every locked package + whether its materialized
//!   file is currently live in the DB (the DB-wipe re-apply witness).
//! - `POST /__api/packages/fetch   {url, id?}` — `git clone` a package repo into
//!   `.penpot-packages/<id>` (offline for a `file://` / already-cloned repo).
//! - `POST /__api/packages/install {id, name?}` — import the package's `.penpot`
//!   source tree as an ORDINARY vault file (generalized N6 installer: import-as-
//!   new + settle-to-fixpoint) and write its `lock.json` entry.
//!
//! ## Why install materializes an ordinary vault file
//!
//! A design-data package installs by importing its `.penpot` tree as a NEW
//! vault file (N6 already lands new-from-template in Drafts on disk). It then
//! becomes an ordinary sync-tracked file, so a DB wipe rebuilds it by the
//! PROVEN M2 resurrect-by-id reconcile (in-place import preserves the file-id,
//! invariant 1). The lockfile records the `{id, version, contentHash,
//! contractHash, sourceGitUrl, fileId}` pin so the re-apply is verifiable and
//! so later package types (E3 library-rel, E7 plugin props) have a stable place
//! to re-derive the DB-only pointers a plain tree cannot carry.
//!
//! ## Idempotency (run-twice = no-op)
//!
//! Install is keyed on the package's `contentHash` (the semantic tree hash of
//! its `.penpot` source). If the lockfile already pins `id` at the same
//! `contentHash` AND that `fileId` is still live in the DB, install is a no-op —
//! no re-import, no phantom diff.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use penpot_rpc::{Auth, PenpotClient};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sync_core::{LockEntry, Lockfile};

/// Router state: where packages live + how to reach the backend. Rebuilt per
/// boot (and per vault switch) so `vault_root`/`team_id`/`token` stay fresh.
pub struct PackagesState {
    /// `<vault>/.penpot-packages` — the git-repo package home.
    pub packages_dir: PathBuf,
    /// `<vault>` — the lockfile (`lock.json`) lives at its root.
    pub vault_root: PathBuf,
    /// Backend RPC base URL (loopback).
    pub backend_base: String,
    /// Provisioned access token (None → install/fetch unavailable; listing OK).
    pub token: Option<String>,
    /// The single team's id (deep-link `team-id` + import target team).
    pub team_id: String,
}

impl PackagesState {
    fn client(&self) -> Option<PenpotClient> {
        self.token
            .clone()
            .map(|t| PenpotClient::new(&self.backend_base).with_auth(Auth::Token(t)))
    }
}

/// Build the E2 package routes for the proxy's extra router.
pub fn router(state: Arc<PackagesState>) -> Router {
    Router::new()
        .route("/__api/packages", get(list_packages))
        .route("/__api/packages/fetch", post(fetch_package))
        .route("/__api/packages/install", post(install_package))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// A parsed `package.json` (all fields optional; totality like `extract.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageManifest {
    pub id: Option<String>,
    pub version: Option<String>,
    pub kind: Option<String>,
    pub name: Option<String>,
}

/// Parse a package dir's optional `package.json`. Missing/malformed → defaults
/// (never an error): the id falls back to the directory name, version to
/// `0.0.0`, kind to `design-data`.
pub fn read_manifest(pkg_dir: &Path) -> PackageManifest {
    let raw = match std::fs::read(pkg_dir.join("package.json")) {
        Ok(b) => b,
        Err(_) => return PackageManifest::default(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return PackageManifest::default(),
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    PackageManifest {
        id: get("id"),
        version: get("version"),
        kind: get("kind"),
        name: get("name"),
    }
}

/// A package id must be a single safe path component (no traversal, no
/// separators, not a dotfile). Guards every filesystem join against the request.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.starts_with('.')
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

/// Derive a package id from a git URL: the last path segment minus a trailing
/// `.git`. Returns `None` if the result is not a safe id.
pub fn id_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    let id = last.strip_suffix(".git").unwrap_or(last).to_string();
    is_safe_id(&id).then_some(id)
}

/// Find the package's single `.penpot` source tree: the first (sorted) direct
/// child directory whose name ends in `.penpot` and is not a dotfile. `None` if
/// the package carries no design-data tree.
pub fn discover_penpot_tree(pkg_dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(pkg_dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !n.starts_with('.') && n.ends_with(".penpot"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// The package's contract hash: E1's `extract_contracts` over the `.penpot`
/// tree, **file-id excluded**, canonicalized (sorted-key JSON), sha256'd. File-
/// id is excluded so the hash is stable across the import id churn (E1 proved
/// the contract body is uuid-invariant), which is exactly what makes it usable
/// as a version pin the consumer diffs against.
pub fn contract_hash_of_tree(tree_dir: &Path) -> anyhow::Result<String> {
    let files = sync_core::read_tree(tree_dir)?;
    let lib = vault_index::extract_contracts(&files);
    let mut j = lib.to_json();
    if let Some(obj) = j.as_object_mut() {
        obj.remove("fileId");
    }
    let canonical = sync_core::dumps(&j);
    Ok(sync_core::sha256_hex(canonical.as_bytes()))
}

/// Best-effort `git remote get-url origin` for a cloned package (empty string
/// for a dropped-in, non-git package).
fn origin_url(pkg_dir: &Path) -> String {
    if !pkg_dir.join(".git").exists() {
        return String::new();
    }
    std::process::Command::new("git")
        .arg("-C")
        .arg(pkg_dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A human display name for a package id (`button-library` → `Button Library`).
fn display_name_from_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// GET /__api/packages — list locked packages + live status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageStatus {
    id: String,
    version: String,
    kind: String,
    file_id: String,
    content_hash: String,
    contract_hash: String,
    source_git_url: String,
    /// Whether the materialized vault file is currently live in the DB. After a
    /// delete-DB + reboot this is the re-apply witness: M2 resurrected it by id.
    live: bool,
}

async fn list_packages(State(state): State<Arc<PackagesState>>) -> Response {
    let lock = match Lockfile::load_or_default(&state.vault_root) {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("reading lock.json: {e}")})),
            )
                .into_response()
        }
    };
    let client = state.client();
    let mut out: Vec<PackageStatus> = Vec::new();
    for (id, e) in &lock.packages {
        let live = match &client {
            Some(c) => c.get_file(&e.file_id).await.is_ok(),
            None => false,
        };
        out.push(PackageStatus {
            id: id.clone(),
            version: e.version.clone(),
            kind: e.kind.clone(),
            file_id: e.file_id.clone(),
            content_hash: e.content_hash.clone(),
            contract_hash: e.contract_hash.clone(),
            source_git_url: e.source_git_url.clone(),
            live,
        });
    }
    Json(json!({ "ok": true, "count": out.len(), "packages": out })).into_response()
}

// ---------------------------------------------------------------------------
// POST /__api/packages/fetch — git clone into .penpot-packages/<id>
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchReq {
    url: String,
    #[serde(default)]
    id: Option<String>,
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

async fn fetch_package(
    State(state): State<Arc<PackagesState>>,
    Json(req): Json<FetchReq>,
) -> Response {
    let url = req.url.trim().to_string();
    if url.is_empty() {
        return bad_request("empty url");
    }
    let id = match req.id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(id) => id,
        None => match id_from_url(&url) {
            Some(id) => id,
            None => return bad_request(format!("cannot derive a safe package id from url {url:?}")),
        },
    };
    if !is_safe_id(&id) {
        return bad_request(format!("unsafe package id {id:?}"));
    }
    let packages_dir = state.packages_dir.clone();
    let dest = packages_dir.join(&id);

    let already = dest.is_dir();
    let result = tokio::task::spawn_blocking(move || {
        if already {
            // Already cloned → idempotent no-op (run-twice safety). A real
            // update would `git -C <dest> fetch`; E2 keeps fetch clone-once.
            return Ok::<bool, String>(false);
        }
        std::fs::create_dir_all(&packages_dir)
            .map_err(|e| format!("creating packages dir: {e}"))?;
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", &url])
            .arg(&dest)
            .output()
            .map_err(|e| format!("spawning git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(true)
    })
    .await;

    match result {
        Ok(Ok(cloned)) => Json(json!({
            "ok": true, "id": id, "cloned": cloned,
            "alreadyPresent": !cloned,
        }))
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("git task join: {e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /__api/packages/install — import + settle + lock
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallReq {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallResp {
    ok: bool,
    id: String,
    file_id: String,
    version: String,
    kind: String,
    content_hash: String,
    contract_hash: String,
    source_git_url: String,
    /// True when the install was a no-op (already pinned at this contentHash and
    /// still live) — the run-twice idempotency signal.
    already_installed: bool,
    /// In-place re-import cycles run to settle to a fixpoint (0 when no-op).
    settle_cycles: usize,
    deep_link: String,
}

async fn install_package(
    State(state): State<Arc<PackagesState>>,
    Json(req): Json<InstallReq>,
) -> Response {
    let id = req.id.trim().to_string();
    if !is_safe_id(&id) {
        return bad_request(format!("unsafe package id {id:?}"));
    }
    let Some(client) = state.client() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "no access token provisioned; cannot install"})),
        )
            .into_response();
    };
    match do_install(&state, &client, &id, req.name.as_deref()).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => {
            tracing::error!(package = %id, error = format!("{e:#}"), "package install failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"ok": false, "error": format!("install failed: {e:#}")})),
            )
                .into_response()
        }
    }
}

async fn do_install(
    state: &PackagesState,
    client: &PenpotClient,
    id: &str,
    name_override: Option<&str>,
) -> anyhow::Result<InstallResp> {
    let pkg_dir = state.packages_dir.join(id);
    if !pkg_dir.is_dir() {
        anyhow::bail!("package {id:?} not found under .penpot-packages (fetch it first)");
    }
    let tree_dir = discover_penpot_tree(&pkg_dir)
        .ok_or_else(|| anyhow::anyhow!("package {id:?} carries no .penpot source tree"))?;

    // Provenance + hashes (pure, off the DB).
    let manifest = read_manifest(&pkg_dir);
    let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".to_string());
    let kind = manifest.kind.clone().unwrap_or_else(|| "design-data".to_string());
    let source_git_url = origin_url(&pkg_dir);
    let content_hash = sync_core::semantic_tree_hash(&tree_dir)?;
    let contract_hash = contract_hash_of_tree(&tree_dir)?;
    let display = name_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| manifest.name.clone())
        .unwrap_or_else(|| display_name_from_id(id));

    // Idempotency: same contentHash pinned AND still live → no-op (no phantom
    // diff on a second install).
    let mut lock = Lockfile::load_or_default(&state.vault_root)?;
    if let Some(existing) = lock.packages.get(id) {
        if existing.content_hash == content_hash && client.get_file(&existing.file_id).await.is_ok()
        {
            let page_id = crate::installer::first_page_id(client, &existing.file_id).await;
            tracing::info!(package = %id, file = %existing.file_id, "install no-op (already pinned + live)");
            return Ok(InstallResp {
                ok: true,
                id: id.to_string(),
                file_id: existing.file_id.clone(),
                version: existing.version.clone(),
                kind: existing.kind.clone(),
                content_hash: existing.content_hash.clone(),
                contract_hash: existing.contract_hash.clone(),
                source_git_url: existing.source_git_url.clone(),
                already_installed: true,
                settle_cycles: 0,
                deep_link: vault_index::workspace_deep_link(
                    &state.team_id,
                    &existing.file_id,
                    page_id.as_deref(),
                ),
            });
        }
    }

    // import-as-new + settle to a round-trip fixpoint (shared installer).
    let zip = sync_core::zip_dir(&tree_dir)?;
    let project_id = crate::installer::default_project_id(client, &state.team_id).await?;
    let (file_id, settle_cycles) =
        crate::installer::import_binfile_and_settle(client, &project_id, &display, zip, None)
            .await?;
    tracing::info!(
        package = %id, file = %file_id, cycles = settle_cycles,
        "imported + settled package to a round-trip fixpoint"
    );

    // Record the pin (git-diffable lock.json at the vault root).
    lock.upsert(
        id.to_string(),
        LockEntry {
            version: version.clone(),
            kind: kind.clone(),
            content_hash: content_hash.clone(),
            contract_hash: contract_hash.clone(),
            source_git_url: source_git_url.clone(),
            file_id: file_id.clone(),
            name: display.clone(),
            installed_at: sync_core::lock::now_rfc3339(),
            library_shared: false,
            plugin_props: Default::default(),
        },
    );
    lock.save(&state.vault_root)?;

    let page_id = crate::installer::first_page_id(client, &file_id).await;
    let deep_link =
        vault_index::workspace_deep_link(&state.team_id, &file_id, page_id.as_deref());

    Ok(InstallResp {
        ok: true,
        id: id.to_string(),
        file_id,
        version,
        kind,
        content_hash,
        contract_hash,
        source_git_url,
        already_installed: false,
        settle_cycles,
        deep_link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_safe_id_rejects_traversal_and_dotfiles() {
        assert!(is_safe_id("button-library"));
        assert!(is_safe_id("icons_v2"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("."));
        assert!(!is_safe_id(".."));
        assert!(!is_safe_id(".hidden"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("a\\b"));
        assert!(!is_safe_id("../escape"));
    }

    #[test]
    fn id_from_url_strips_git_suffix_and_path() {
        assert_eq!(id_from_url("file:///tmp/repos/buttons.git").as_deref(), Some("buttons"));
        assert_eq!(id_from_url("https://example.com/u/icons").as_deref(), Some("icons"));
        assert_eq!(id_from_url("git@github.com:org/design-kit.git").as_deref(), Some("design-kit"));
        assert_eq!(id_from_url("https://example.com/repos/").as_deref(), Some("repos"));
        // A URL that would derive a dotfile id is rejected.
        assert_eq!(id_from_url("file:///tmp/.git"), None);
    }

    #[test]
    fn read_manifest_defaults_when_absent_or_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        // absent
        assert_eq!(read_manifest(tmp.path()), PackageManifest::default());
        // malformed
        std::fs::write(tmp.path().join("package.json"), b"not json").unwrap();
        assert_eq!(read_manifest(tmp.path()), PackageManifest::default());
        // present
        std::fs::write(
            tmp.path().join("package.json"),
            serde_json::to_vec(&json!({
                "id": "buttons", "version": "2.1.0", "kind": "component-library",
                "name": "Button Kit"
            }))
            .unwrap(),
        )
        .unwrap();
        let m = read_manifest(tmp.path());
        assert_eq!(m.version.as_deref(), Some("2.1.0"));
        assert_eq!(m.kind.as_deref(), Some("component-library"));
        assert_eq!(m.name.as_deref(), Some("Button Kit"));
    }

    #[test]
    fn discover_penpot_tree_finds_the_single_tree_skipping_dotdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path();
        std::fs::create_dir_all(pkg.join("button-library.penpot/files")).unwrap();
        std::fs::create_dir_all(pkg.join(".git")).unwrap();
        std::fs::write(pkg.join("package.json"), b"{}").unwrap();
        let tree = discover_penpot_tree(pkg).unwrap();
        assert_eq!(tree.file_name().unwrap().to_str().unwrap(), "button-library.penpot");
        // Empty package → None.
        let empty = tempfile::tempdir().unwrap();
        assert!(discover_penpot_tree(empty.path()).is_none());
    }

    /// contractHash is stable across the uuid churn import-as-new performs — the
    /// whole point of hashing the file-id-excluded contract (E1 caveat 2). This
    /// is the E2 companion to the E1 `contract_is_uuid_invariant` test.
    #[test]
    fn contract_hash_is_stable_across_uuid_churn() {
        let fid = "3a4be581-6d37-8010-8008-51f0c6eb307f";
        let pid = "3a4be581-6d37-8010-8008-51f0c6eb3080";
        let a = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, v: &serde_json::Value| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, serde_json::to_vec(v).unwrap()).unwrap();
        };
        write(a.path(), "manifest.json", &json!({"files": [{"id": fid}]}));
        write(
            a.path(),
            &format!("files/{fid}/components/c1.json"),
            &json!({"id": "c1", "name": "Default", "path": "Controls / Button",
                    "variantId": "v-1",
                    "variantProperties": [{"name": "Size", "value": "S"}],
                    "mainInstancePage": pid, "mainInstanceId": "mi1"}),
        );
        write(
            a.path(),
            &format!("files/{fid}/pages/{pid}/mi1.json"),
            &json!({"id": "mi1", "type": "frame", "mainInstance": true,
                    "appliedTokens": {"fill": "layerBase.text"}}),
        );
        let hash_a = contract_hash_of_tree(a.path()).unwrap();

        // Churn every uuid consistently (simulates import-as-new's per-DB remap).
        let b = tempfile::tempdir().unwrap();
        let remap = |s: &str| {
            s.replace(fid, "9999ffff-0000-0000-0000-000000000000")
                .replace(pid, "8888eeee-0000-0000-0000-000000000000")
                .replace("v-1", "zzzz")
                .replace("mi1", "newmain")
                .replace("c1", "newcomp")
        };
        for (rel, bytes) in sync_core::read_tree(a.path()).unwrap() {
            let text = String::from_utf8(bytes).unwrap();
            let p = b.path().join(remap(&rel));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, remap(&text)).unwrap();
        }
        let hash_b = contract_hash_of_tree(b.path()).unwrap();
        assert_eq!(hash_a, hash_b, "contractHash must be uuid-invariant");
        // And it is a real hash (64 hex chars), not empty.
        assert_eq!(hash_a.len(), 64);
    }

    #[test]
    fn display_name_title_cases_the_id() {
        assert_eq!(display_name_from_id("button-library"), "Button Library");
        assert_eq!(display_name_from_id("icons_v2"), "Icons V2");
    }
}
