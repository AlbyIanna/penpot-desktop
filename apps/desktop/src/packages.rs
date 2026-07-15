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
        .route("/__api/packages/publish", post(publish_package))
        .route("/__api/packages/link", post(link_package))
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
    /// E3: whether this package's file is published as a shared library
    /// (`set-file-shared`). Rides the binfile + re-asserted on boot.
    library_shared: bool,
    /// E3: the libraries this package's file links (references components from).
    /// The linked-state witness the gate asserts; empty for a plain package.
    links: Vec<LinkView>,
}

/// One link surfaced in the `GET /__api/packages` witness (E3).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkView {
    library_file_id: String,
    library_package_id: String,
    version: String,
    contract_hash: String,
}

impl From<&sync_core::LibraryLink> for LinkView {
    fn from(l: &sync_core::LibraryLink) -> Self {
        LinkView {
            library_file_id: l.library_file_id.clone(),
            library_package_id: l.library_package_id.clone(),
            version: l.version.clone(),
            contract_hash: l.contract_hash.clone(),
        }
    }
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
            library_shared: e.library_shared,
            links: e.links.iter().map(LinkView::from).collect(),
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

/// Whether the vault's sync manifest still pins `file_id` — i.e. its `.penpot`
/// tree is on disk and M2 will resurrect it by id. Used to tell "momentarily
/// absent from the DB while resurrecting after a wipe" (keep the pinned id) from
/// "genuinely gone" (re-materialize). A missing/unreadable manifest reads as
/// "not pinned" (safe: nothing to resurrect).
fn manifest_pins(vault_root: &Path, file_id: &str) -> bool {
    sync_core::Manifest::load(vault_root)
        .ok()
        .flatten()
        .is_some_and(|m| m.files.contains_key(file_id))
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

    // Idempotency: same contentHash already pinned → a second install is a
    // no-op. The pinned file may be momentarily ABSENT from the DB while M2
    // resurrects it by id after a database wipe; in that window its `.penpot`
    // tree is still on disk (the sync manifest pins it). Re-importing then would
    // mint a DUPLICATE id and orphan the file every link/instance references
    // (invariant 1). So no-op when the file is live OR still on disk
    // (resurrecting); only re-materialize when it is genuinely gone from both.
    let mut lock = Lockfile::load_or_default(&state.vault_root)?;
    if let Some(existing) = lock.packages.get(id) {
        if existing.content_hash == content_hash {
            let live = client.get_file(&existing.file_id).await.is_ok();
            let resurrecting = !live && manifest_pins(&state.vault_root, &existing.file_id);
            if live || resurrecting {
                let page_id = crate::installer::first_page_id(client, &existing.file_id).await;
                tracing::info!(
                    package = %id, file = %existing.file_id, live, resurrecting,
                    "install no-op (already pinned; live or resurrecting from disk)"
                );
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

    // Preserve the DB-only pointers a content re-import must NOT erase: whether
    // the file is published shared (E3) and the libraries it links (E3) — these
    // are re-derived from the lockfile after a DB wipe, so dropping them on a
    // content change would silently break the boot re-link.
    let (library_shared, links, plugin_props) = lock
        .packages
        .get(id)
        .map(|e| (e.library_shared, e.links.clone(), e.plugin_props.clone()))
        .unwrap_or_default();

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
            library_shared,
            plugin_props,
            links,
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

// ---------------------------------------------------------------------------
// POST /__api/packages/publish — install (if needed) + set-file-shared (E3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishReq {
    id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishResp {
    ok: bool,
    id: String,
    file_id: String,
    library_shared: bool,
    already_installed: bool,
    deep_link: String,
}

async fn publish_package(
    State(state): State<Arc<PackagesState>>,
    Json(req): Json<PublishReq>,
) -> Response {
    let id = req.id.trim().to_string();
    if !is_safe_id(&id) {
        return bad_request(format!("unsafe package id {id:?}"));
    }
    let Some(client) = state.client() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "no access token provisioned; cannot publish"})),
        )
            .into_response();
    };
    match do_publish(&state, &client, &id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => {
            tracing::error!(package = %id, error = format!("{e:#}"), "package publish failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"ok": false, "error": format!("publish failed: {e:#}")})),
            )
                .into_response()
        }
    }
}

/// Publish a package's materialized file as a shared library: ensure it is
/// installed/live (the idempotent E2 install path), flip `set-file-shared`, and
/// record `library_shared=true` in its lock entry. Publishing is sticky (E3
/// never auto-unpublishes). Returns the file id + `library_shared` witness.
async fn do_publish(
    state: &PackagesState,
    client: &PenpotClient,
    id: &str,
) -> anyhow::Result<PublishResp> {
    let installed = do_install(state, client, id, None).await?;
    let file_id = installed.file_id.clone();

    client
        .set_file_shared(&file_id, true)
        .await
        .map_err(|e| anyhow::anyhow!("set-file-shared for {file_id}: {e}"))?;

    let mut lock = Lockfile::load_or_default(&state.vault_root)?;
    if let Some(e) = lock.packages.get_mut(id) {
        e.library_shared = true;
    }
    lock.save(&state.vault_root)?;

    let page_id = crate::installer::first_page_id(client, &file_id).await;
    Ok(PublishResp {
        ok: true,
        id: id.to_string(),
        file_id: file_id.clone(),
        library_shared: true,
        already_installed: installed.already_installed,
        deep_link: vault_index::workspace_deep_link(&state.team_id, &file_id, page_id.as_deref()),
    })
}

// ---------------------------------------------------------------------------
// POST /__api/packages/link — link a consumer package to a library package (E3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkReq {
    consumer_id: String,
    library_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkResp {
    ok: bool,
    consumer_id: String,
    consumer_file_id: String,
    library_id: String,
    library_file_id: String,
    /// Recursive libraries the linked library itself depends on (0 for a leaf).
    library_recursive_deps: usize,
}

async fn link_package(State(state): State<Arc<PackagesState>>, Json(req): Json<LinkReq>) -> Response {
    let consumer_id = req.consumer_id.trim().to_string();
    let library_id = req.library_id.trim().to_string();
    if !is_safe_id(&consumer_id) {
        return bad_request(format!("unsafe consumer package id {consumer_id:?}"));
    }
    if !is_safe_id(&library_id) {
        return bad_request(format!("unsafe library package id {library_id:?}"));
    }
    let Some(client) = state.client() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "no access token provisioned; cannot link"})),
        )
            .into_response();
    };
    match do_link(&state, &client, &consumer_id, &library_id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => {
            tracing::error!(
                consumer = %consumer_id, library = %library_id,
                error = format!("{e:#}"), "package link failed"
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"ok": false, "error": format!("link failed: {e:#}")})),
            )
                .into_response()
        }
    }
}

/// Link a consumer package to a library package within one vault (E3). Ensures
/// the library is installed + published shared, ensures the consumer is
/// installed, runs the idempotent `link-file-to-library {consumerFileId,
/// libraryFileId}`, and records a [`sync_core::LibraryLink`] in the consumer's
/// lock entry (dedup by library file id, sorted for git-diffability).
/// Resolution is by vault-local id — no id remap (that is E6).
async fn do_link(
    state: &PackagesState,
    client: &PenpotClient,
    consumer_id: &str,
    library_id: &str,
) -> anyhow::Result<LinkResp> {
    if consumer_id == library_id {
        anyhow::bail!("a package cannot link itself as its own library");
    }
    // Library first: installed + shared (publish is idempotent).
    let library = do_publish(state, client, library_id).await?;
    let library_file_id = library.file_id.clone();
    // Consumer: installed/live.
    let consumer = do_install(state, client, consumer_id, None).await?;
    let consumer_file_id = consumer.file_id.clone();
    if consumer_file_id == library_file_id {
        anyhow::bail!(
            "consumer and library resolved to the same vault file id {consumer_file_id}"
        );
    }

    // Derive the disposable file_library_rel (idempotent — safe to re-run).
    let link_resp = client
        .link_file_to_library(&consumer_file_id, &library_file_id)
        .await
        .map_err(|e| anyhow::anyhow!("link-file-to-library: {e}"))?;
    let library_recursive_deps = link_resp.as_array().map(|a| a.len()).unwrap_or(0);

    // Pin the link in the consumer's lock entry (source of truth for rebuild).
    let mut lock = Lockfile::load_or_default(&state.vault_root)?;
    let (lib_version, lib_contract_hash) = lock
        .packages
        .get(library_id)
        .map(|e| (e.version.clone(), e.contract_hash.clone()))
        .unwrap_or_default();
    let consumer_entry = lock.packages.get_mut(consumer_id).ok_or_else(|| {
        anyhow::anyhow!("consumer {consumer_id:?} has no lock entry after install")
    })?;
    consumer_entry
        .links
        .retain(|l| l.library_file_id != library_file_id);
    consumer_entry.links.push(sync_core::LibraryLink {
        library_file_id: library_file_id.clone(),
        library_package_id: library_id.to_string(),
        version: lib_version,
        contract_hash: lib_contract_hash,
    });
    consumer_entry
        .links
        .sort_by(|a, b| a.library_file_id.cmp(&b.library_file_id));
    lock.save(&state.vault_root)?;

    Ok(LinkResp {
        ok: true,
        consumer_id: consumer_id.to_string(),
        consumer_file_id,
        library_id: library_id.to_string(),
        library_file_id,
        library_recursive_deps,
    })
}

// ---------------------------------------------------------------------------
// Boot re-link reconcile (E3) — re-derive file_library_rel after a DB wipe
// ---------------------------------------------------------------------------

/// Bounded window for the boot re-link retry: files come alive as the sync
/// daemon's startup reconcile resurrects them by id, so we poll until every
/// lock link's endpoints are live (or the window closes).
const RELINK_BOOT_ATTEMPTS: usize = 60;
const RELINK_BOOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Outcome of one [`reapply_links`] pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RelinkSummary {
    /// Links re-established this pass (both endpoints live).
    pub applied: usize,
    /// Links whose consumer or library file was not yet live.
    pub blocked: usize,
}

/// Re-derive every lockfile link's disposable `file_library_rel` once its
/// endpoints are live: for each [`sync_core::RelinkAction::Ready`] link,
/// defensively `set-file-shared` the library and re-run the idempotent
/// `link-file-to-library`. Pure DB-pointer re-derivation — it never writes the
/// lockfile or any file tree. Returns a per-pass summary so the caller can
/// decide whether to retry (blocked links await resurrection).
pub async fn reapply_links(state: &PackagesState) -> anyhow::Result<RelinkSummary> {
    let Some(client) = state.client() else {
        return Ok(RelinkSummary::default());
    };
    let lock = Lockfile::load_or_default(&state.vault_root)?;

    // The file ids either side of any link — the liveness probe set.
    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in lock.packages.values() {
        for l in &e.links {
            candidates.insert(e.file_id.clone());
            candidates.insert(l.library_file_id.clone());
        }
    }
    if candidates.is_empty() {
        return Ok(RelinkSummary::default());
    }
    let mut present: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for id in &candidates {
        if client.get_file(id).await.is_ok() {
            present.insert(id.clone());
        }
    }

    let mut summary = RelinkSummary::default();
    for action in sync_core::plan_relink(&lock, &present) {
        match action {
            sync_core::RelinkAction::Ready(op) => {
                // isShared rides the binfile, but re-assert defensively (cheap).
                if let Err(e) = client.set_file_shared(&op.library_file_id, true).await {
                    tracing::warn!(
                        library = %op.library_file_id, error = %e,
                        "E3 re-link: defensive set-file-shared failed (continuing to link)"
                    );
                }
                if let Err(e) = client
                    .link_file_to_library(&op.consumer_file_id, &op.library_file_id)
                    .await
                {
                    // Isolate a persistently-failing link: log and count it as
                    // blocked, then continue so a single bad link can't starve
                    // every link sorted after it (each pass re-plans in the same
                    // deterministic order). Transient failures self-heal on the
                    // next retry pass; a persistent one no longer poisons the batch.
                    tracing::warn!(
                        consumer = %op.consumer_file_id, library = %op.library_file_id,
                        error = %e, "E3 re-link: link-file-to-library failed; skipping this link this pass"
                    );
                    summary.blocked += 1;
                    continue;
                }
                tracing::info!(
                    consumer = %op.consumer_file_id, library = %op.library_file_id,
                    package = %op.library_package_id, "E3 re-link: file_library_rel re-derived"
                );
                summary.applied += 1;
            }
            sync_core::RelinkAction::Blocked(op) => {
                tracing::debug!(
                    consumer = %op.consumer_file_id, library = %op.library_file_id,
                    "E3 re-link: endpoint not live yet; will retry"
                );
                summary.blocked += 1;
            }
        }
    }
    Ok(summary)
}

/// Spawn the E3 boot re-link reconcile as a bounded background task. Wired from
/// boot right after the sync daemon starts: the daemon resurrects vault files by
/// id, and this re-derives the DB-only `file_library_rel` each lock link maps to
/// once both endpoints are live. A vault with no links exits after one cheap
/// pass. Idempotent, so a partial run is always safe to repeat.
pub fn spawn_relink_reconcile(state: Arc<PackagesState>) {
    tokio::spawn(async move {
        for _ in 0..RELINK_BOOT_ATTEMPTS {
            match reapply_links(&state).await {
                Ok(s) if s.blocked == 0 => {
                    if s.applied > 0 {
                        tracing::info!(applied = s.applied, "E3 boot re-link reconcile complete");
                    }
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "E3 boot re-link pass errored; retrying")
                }
            }
            tokio::time::sleep(RELINK_BOOT_INTERVAL).await;
        }
        tracing::warn!("E3 boot re-link reconcile window closed with links still blocked");
    });
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
