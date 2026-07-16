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
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use penpot_rpc::{Auth, PenpotClient};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sync_core::{LockEntry, Lockfile};
use tokio::sync::watch;

/// Router state: where packages live + how to reach the backend. Rebuilt per
/// boot (and per vault switch) so `vault_root`/`team_id`/`token` stay fresh.
pub struct PackagesState {
    /// `<vault>/.penpot-packages` — the git-repo package home.
    pub packages_dir: PathBuf,
    /// `<vault>` — the lockfile (`lock.json`) lives at its root.
    pub vault_root: PathBuf,
    /// The app DATA dir (sibling of `postgres/`, OUTSIDE the vault). The E7
    /// local consent ledger (`plugin-consent.json`) lives at its root — it must
    /// NOT travel with a cloned vault, so it is anchored here, not in the vault.
    pub data_dir: PathBuf,
    /// The local proxy origins (both `localhost` and `127.0.0.1` spellings) a
    /// plugin pointer's `host` must equal to be treated as vault-local (E7
    /// finding 5: classify local by HOST, not by the code-path prefix).
    pub local_origins: Vec<String>,
    /// Backend RPC base URL (loopback).
    pub backend_base: String,
    /// Provisioned access token (None → install/fetch unavailable; listing OK).
    pub token: Option<String>,
    /// The single team's id (deep-link `team-id` + import target team).
    pub team_id: String,
    /// E4b: the latest surface-don't-apply update model, published on a `watch`
    /// channel by [`spawn_updates_poller`] (debounced via `send_if_modified`).
    /// The `/__api/packages/updates` poll endpoint just borrows the current value.
    pub updates_rx: watch::Receiver<PackageUpdatesModel>,
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
        // E4b — surface-don't-apply update channel + drift conflict copy.
        .route("/__api/packages/updates", get(updates))
        .route("/__api/packages/preserve-drift", post(preserve_drift))
        // E7 — plugin packages are static assets served AT THE LOCAL PROXY
        // ORIGIN (`/__packages/<pkg>/manifest.json`, `plugin.js`, icon...).
        // Carried-and-pointed-at, never imported into the design DB. The
        // exact `/__packages` path (no subpath) stays the E4 gallery page.
        .route("/__packages/{pkg}/{*path}", get(serve_plugin_asset))
        // E7 — the discovered-plugin surface the gallery renders: each plugin
        // package's local manifest URL + install state. Surface-don't-apply:
        // installing happens ONLY through Penpot's own native Plugin Manager.
        .route("/__api/packages/plugins", get(list_plugins))
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
    Ok(contract_hash_of_lib(&vault_index::extract_contracts(&files)))
}

/// The file-id-excluded canonical sha256 of an already-extracted contract — the
/// shared core of [`contract_hash_of_tree`]. Factored out so the E4b update
/// channel can derive a tree's contract AND its pin hash from a single read.
fn contract_hash_of_lib(lib: &vault_index::LibraryContract) -> String {
    let mut j = lib.to_json();
    if let Some(obj) = j.as_object_mut() {
        obj.remove("fileId");
    }
    sync_core::sha256_hex(sync_core::dumps(&j).as_bytes())
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
// E7 — plugin packages (PLAN3 ch. 3, activation GO / CSP-GO)
// ---------------------------------------------------------------------------
//
// A plugin package is a git repo of STATIC assets (a Penpot plugin
// `manifest.json` + `plugin.js` + icon) under `.penpot-packages/<pkg>/`,
// served at the LOCAL PROXY ORIGIN under `/__packages/<pkg>/*` and installed
// ONLY through Penpot's own native Plugin Manager URL boundary (the user
// pastes the local manifest URL, Penpot shows its own consent dialog) —
// never imported into the design DB, never auto-registered by us.
//
// The registry pointer Penpot writes on Install+Allow lives in profile props
// (`props.plugins.data.<pluginId>`, via the public `update-profile-props`
// RPC — verified live in the E7 activation spike). That is DB-only derived
// state. Two SEPARATE files govern it (adversarial-review finding 1):
//
//   - `lock.json` (vault root, git-versioned, E6-portable) pins each captured
//     pointer under `LockEntry.plugin_props` for PORTABILITY + gallery
//     visibility. It is NO LONGER sufficient to auto-register a plugin.
//   - `plugin-consent.json` (`<data_dir>`, NOT in the vault, NOT git-versioned,
//     survives a DB wipe) is the AUTHORITY for boot re-apply. It records only
//     genuine native-manager consent observed on THIS machine, pinning the
//     content hash that was consented.
//
// So this module:
//   - CAPTURES the pointers the USER created through the native manager into
//     BOTH `lock.json` (portability pin) AND the consent ledger (re-apply
//     authority) — recording consent already given, never granting it;
//   - RE-APPLIES a pinned pointer at boot after a DB wipe ONLY when the ledger
//     authorizes it AND the consented content hash still matches the live
//     served code AND the pointer host is local (insert-only merge through
//     `update-profile-props`), mirroring the E3 re-link reconcile — invariant
//     1: delete the DB and the consented plugin registry is rebuilt.
//
// Opening a CLONED/pulled vault therefore seeds NOTHING (lock pin present,
// ledger absent → `availableNeedsConsent`); a package whose served code drifted
// since consent is NOT re-registered (`driftedNeedsReconsent`). Both are
// surfaced on `/__api/packages/plugins`, never silently applied.
//
// The exact shipped promise (docs/ecosystem-spikes/plugin-supply-chain.md):
// content-pinned + offline + consent-gated + CSP(default-src+connect-src)
// egress-containment (the whitelist is cosmetic — disclaimer-only). NOT
// data-isolation: a `content:write` plugin still reads/rewrites the open file.

/// The lockfile `kind` a captured plugin package is pinned under.
pub const PLUGIN_KIND: &str = "plugin";

/// A parsed Penpot plugin `manifest.json` (the file the user pastes into the
/// native Plugin Manager). The presence of a string `code` field is what
/// marks a package directory as a plugin package. All other fields optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub code: String,
    pub icon: Option<String>,
    pub permissions: Vec<String>,
}

/// Parse a package dir's Penpot plugin `manifest.json`. `None` when the file
/// is absent/malformed or carries no string `code` field (i.e. the package is
/// not a plugin package). NOTE the E7 design finding: `code`/`icon` must be
/// ORIGIN-ABSOLUTE paths (`/__packages/<pkg>/plugin.js`) — Penpot v1
/// manifests resolve `code` against the ORIGIN, not the manifest directory.
pub fn read_plugin_manifest(pkg_dir: &Path) -> Option<PluginManifest> {
    let raw = std::fs::read(pkg_dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let code = v.get("code")?.as_str()?.to_string();
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(PluginManifest {
        name: get("name"),
        description: get("description"),
        code,
        icon: get("icon"),
        permissions: v
            .get("permissions")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Relative asset-path safety for `/__packages/{pkg}/{*path}`: every `/`
/// segment must be a plain component — never empty, `.`/`..`, a dotfile
/// (blocks `.git/config` & co.), a backslash, or a NUL.
pub fn is_safe_asset_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.starts_with('.'))
}

/// Deterministic content hash of a plugin package's SERVED surface: sha256
/// over sorted `(relpath, sha256(bytes))` pairs of every non-dot file under
/// the package dir (the same tree-hash discipline as the normalization spec;
/// dot entries are skipped exactly like the serve route refuses them). This
/// is the lockfile content pin — drift between the pin and the live assets is
/// SURFACED (gallery `drifted` flag), never silently re-pinned.
pub fn plugin_content_hash(pkg_dir: &Path) -> anyhow::Result<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) -> anyhow::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue; // dot entries (.git, dotfiles) are never served
            }
            let path = entry.path();
            let ftype = entry.file_type()?;
            if ftype.is_dir() {
                walk(&path, root, out)?;
            } else if ftype.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .expect("walk stays under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                let bytes = std::fs::read(&path)?;
                out.push((rel, sync_core::sha256_hex(&bytes)));
            }
            // symlinks are skipped: the serve route refuses to follow them
            // outside the package home, so they are not served surface.
        }
        Ok(())
    }
    let mut pairs = Vec::new();
    walk(pkg_dir, pkg_dir, &mut pairs)?;
    pairs.sort();
    let mut acc = String::new();
    for (rel, h) in &pairs {
        acc.push_str(rel);
        acc.push('\n');
        acc.push_str(h);
        acc.push('\n');
    }
    Ok(sync_core::sha256_hex(acc.as_bytes()))
}

// ---------------------------------------------------------------------------
// GET /__packages/{pkg}/{*path} — E7 plugin-package static assets
// ---------------------------------------------------------------------------

/// Serve a static asset out of `<vault>/.penpot-packages/<pkg>/<path>` at the
/// local proxy origin. This is how a plugin package's `manifest.json` /
/// `plugin.js` / icon become installable through Penpot's OWN native Plugin
/// Manager URL boundary — the assets never enter the design DB. Hardened:
/// the package id must be a safe id (no traversal, no dotfile), every path
/// segment a plain non-dot component, and the RESOLVED file (symlinks
/// followed) must still live under the package home — a hostile package
/// containing `evil -> /Users/me/.ssh/id_ed25519` gets a 404, never the file.
async fn serve_plugin_asset(
    State(state): State<Arc<PackagesState>>,
    axum::extract::Path((pkg, path)): axum::extract::Path<(String, String)>,
) -> Response {
    if !is_safe_id(&pkg) || !is_safe_asset_path(&path) {
        return (StatusCode::BAD_REQUEST, "invalid package asset path").into_response();
    }
    let root = state.packages_dir.join(&pkg);
    let full = root.join(&path);
    // Symlink-escape guard: canonicalize BOTH sides and require containment.
    let (canon_root, canon) = match (
        tokio::fs::canonicalize(&root).await,
        tokio::fs::canonicalize(&full).await,
    ) {
        (Ok(r), Ok(f)) => (r, f),
        _ => return (StatusCode::NOT_FOUND, "no such package asset").into_response(),
    };
    if !canon.starts_with(&canon_root) {
        return (StatusCode::NOT_FOUND, "no such package asset").into_response();
    }
    let bytes = match tokio::fs::read(&canon).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "no such package asset").into_response(),
    };
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("json") => "application/json",
        Some("js") | Some("mjs") => "application/javascript",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("txt") | Some("md") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    ([(http::header::CONTENT_TYPE, ctype)], bytes).into_response()
}

// ---------------------------------------------------------------------------
// E7 — plugin registry pointer: lock.json pin + boot-time re-apply
// ---------------------------------------------------------------------------

/// Parse a plugin pointer's `code` path into the local package id it is
/// served from: `/__packages/<pkg>/…` → `pkg`. `None` for a code that is not a
/// `/__packages/<pkg>/<asset>` path. NOTE (E7 finding 5): parsing the pkg from
/// the code path is how we KEY the lock/ledger, but it is NOT how we decide a
/// pointer is local — Penpot resolves code as `new URL(code, host)`, so a
/// REMOTE plugin (`host = https://evil.example`) could carry a `code` of
/// `/__packages/…` yet resolve off-origin. Local-ness is decided by
/// [`is_local_host`] on the pointer's `host`; this helper only maps a
/// confirmed-local pointer's code to its package directory.
pub fn plugin_pkg_from_code(code: &str) -> Option<String> {
    let rest = code.strip_prefix("/__packages/")?;
    let (pkg, _asset) = rest.split_once('/')?;
    is_safe_id(pkg).then(|| pkg.to_string())
}

/// The local proxy origins a plugin pointer's `host` must equal to be treated
/// as vault-local — both `http://localhost:<port>` and `http://127.0.0.1:<port>`
/// spellings (Penpot stores the manifest ORIGIN as `host`).
pub fn local_proxy_origins(proxy_port: u16) -> Vec<String> {
    vec![
        format!("http://localhost:{proxy_port}"),
        format!("http://127.0.0.1:{proxy_port}"),
    ]
}

/// Whether a pointer's `host` (the manifest ORIGIN) is one of our local proxy
/// origins (E7 finding 5). Trailing-slash tolerant. Only host-local pointers
/// are ever pinned to `lock.json` / the consent ledger / re-applied — a remote
/// plugin the user installed from elsewhere is the user's own business and is
/// never resurrected by us, regardless of its `code` path.
pub fn is_local_host(host: &str, local_origins: &[String]) -> bool {
    let h = host.trim_end_matches('/');
    local_origins
        .iter()
        .any(|o| o.trim_end_matches('/') == h)
}

/// The per-plugin consent state the boot re-apply and the `/__api/packages/plugins`
/// listing both derive from three authorities: the vault `lock.json` pin
/// (portability), the per-machine consent ledger (re-apply authority), and the
/// live served-code content hash. Pure/deterministic — the security decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginConsentState {
    /// Discovered on disk with no local lock pin and no ledger entry — never
    /// installed on this machine.
    Available,
    /// A local lock pin is present (carried in `lock.json`) but there is NO
    /// ledger entry on THIS machine — a cloned/pulled vault. NOT re-applied
    /// (no native Install/Allow ever happened here).
    AvailableNeedsConsent,
    /// A ledger entry exists but its `consentedContentHash` != the live content
    /// hash — the served code changed since consent. NOT re-applied (consent
    /// was for the old code).
    DriftedNeedsReconsent,
    /// Genuine, current consent on this machine (ledger present, hash matches).
    /// The ONLY state that authorizes a boot re-apply.
    Installed,
}

impl PluginConsentState {
    /// The camelCase state string surfaced in the plugins listing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::AvailableNeedsConsent => "availableNeedsConsent",
            Self::DriftedNeedsReconsent => "driftedNeedsReconsent",
            Self::Installed => "installed",
        }
    }
    /// Only a genuine current consent (ledger authority) is re-appliable.
    pub fn is_reappliable(&self) -> bool {
        matches!(self, Self::Installed)
    }
    /// Priority for merging per-pluginId states into a per-package state
    /// (higher wins): a package with any Installed pointer reads Installed.
    fn priority(&self) -> u8 {
        match self {
            Self::Available => 0,
            Self::AvailableNeedsConsent => 1,
            Self::DriftedNeedsReconsent => 2,
            Self::Installed => 3,
        }
    }
    fn merged_with(self, other: Self) -> Self {
        if other.priority() > self.priority() {
            other
        } else {
            self
        }
    }
}

/// Classify one plugin pointer's consent state (pure — the re-apply gate). The
/// caller has already confirmed the lock pin's `host` is local (finding 5) and
/// passes that as `has_local_lock_pin`; `ledger_entry` is the per-machine
/// consent record (if any); `live_content_hash` is a fresh hash over the served
/// assets. Re-apply happens ONLY for [`PluginConsentState::Installed`].
pub fn classify_plugin_consent(
    has_local_lock_pin: bool,
    ledger_entry: Option<&sync_core::ConsentRecord>,
    live_content_hash: &str,
) -> PluginConsentState {
    match ledger_entry {
        Some(rec) if rec.consented_content_hash == live_content_hash => {
            PluginConsentState::Installed
        }
        Some(_) => PluginConsentState::DriftedNeedsReconsent,
        None if has_local_lock_pin => PluginConsentState::AvailableNeedsConsent,
        None => PluginConsentState::Available,
    }
}

/// Extract `(host, code)` from a canonical pointer-JSON string.
fn pointer_host_code(pointer_json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(pointer_json).ok()?;
    let host = v.get("host").and_then(|h| h.as_str())?.to_string();
    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("").to_string();
    Some((host, code))
}

/// Canonical single-line JSON for a pointer value — the `plugin_props` pin
/// format. serde_json without `preserve_order` keeps map keys sorted, so this
/// is deterministic and git-diffable inside `lock.json`.
fn canonical_pointer_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Extract every LOCAL plugin pointer from profile props: the entries under
/// `props.plugins.data` whose `host` is one of our local proxy origins (E7
/// finding 5 — local-ness is decided by `host`, NOT the `code` path) and whose
/// `code` maps to a `/__packages/<pkg>/` package directory. Returns pkg id →
/// (pluginId → canonical pointer JSON). Pure — unit-tested against the exact
/// props shape captured live in the E7 spike.
pub fn local_plugin_pointers(
    props: &serde_json::Value,
    local_origins: &[String],
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    let mut out: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
        std::collections::BTreeMap::new();
    let Some(data) = props
        .get("plugins")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.as_object())
    else {
        return out;
    };
    for (plugin_id, value) in data {
        // Finding 5: gate on the HOST first — a remote plugin whose code happens
        // to start `/__packages/` is NOT local and is never pinned/resurrected.
        let host = value.get("host").and_then(|h| h.as_str()).unwrap_or("");
        if !is_local_host(host, local_origins) {
            continue;
        }
        let Some(code) = value.get("code").and_then(|c| c.as_str()) else {
            continue;
        };
        let Some(pkg) = plugin_pkg_from_code(code) else {
            continue;
        };
        out.entry(pkg)
            .or_default()
            .insert(plugin_id.clone(), canonical_pointer_json(value));
    }
    out
}

/// Compute the live content hash of every package that currently carries a
/// plugin pin in `lock.json` (pkg id → fresh `plugin_content_hash`). The
/// re-apply / listing drift gate compares the consented hash against these.
fn live_plugin_content_hashes(
    packages_dir: &Path,
    lock: &Lockfile,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (pkg, entry) in &lock.packages {
        if entry.plugin_props.is_empty() {
            continue;
        }
        let h = plugin_content_hash(&packages_dir.join(pkg)).unwrap_or_default();
        out.insert(pkg.clone(), h);
    }
    out
}

/// Plan the boot-time re-apply (THE security decision, pure/unit-tested): the
/// merged `plugins` props value with every AUTHORIZED lock-pinned pointer that
/// is MISSING from the live props inserted, plus how many were inserted (0 →
/// nothing to apply). A pointer is re-applied ONLY when ALL hold:
///
///   (a) a `lock.json` pin exists (iterated here);
///   (b) the pointer `host` is one of our local proxy origins (finding 5);
///   (c) a consent-ledger entry exists for that pluginId (finding 1 — the
///       per-machine authority; a cloned vault has none → nothing re-applied);
///   (d) the ledger's `consentedContentHash` == the CURRENT package content
///       hash (finding 3 — drift → not re-applied, consent was for old code).
///
/// **Insert-only**: an entry already present in the DB is never overwritten —
/// the DB carries the user's latest consent (install/permission changes made
/// through the native manager win over the pin). Pure and deterministic.
///
/// Wire shape (2.16.2, malli-validated server-side — probed live):
/// `props.plugins = {ids: [string], data: {<pluginId> → pointer}}` where each
/// pointer requires `pluginId`/`name`/`host`/`code`/`permissions`. The merge
/// keeps `ids` in sync with `data` (existing order preserved, new ids
/// appended sorted) or the RPC rejects the whole write.
pub fn plan_plugin_reapply(
    lock: &Lockfile,
    ledger: &sync_core::ConsentLedger,
    live_content_hashes: &std::collections::BTreeMap<String, String>,
    props: &serde_json::Value,
    local_origins: &[String],
) -> (serde_json::Value, usize) {
    let mut plugins = props
        .get("plugins")
        .cloned()
        .unwrap_or_else(|| json!({ "ids": [], "data": {} }));
    if !plugins.is_object() {
        plugins = json!({ "ids": [], "data": {} });
    }
    if !plugins.get("data").map(|d| d.is_object()).unwrap_or(false) {
        plugins["data"] = json!({});
    }
    let mut inserted = 0usize;
    for (pkg, entry) in &lock.packages {
        let live_hash = live_content_hashes.get(pkg).map(String::as_str).unwrap_or("");
        for (plugin_id, pointer_json) in &entry.plugin_props {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(pointer_json) else {
                tracing::warn!(plugin = %plugin_id, "lock.json plugin_props value is not JSON; skipping");
                continue;
            };
            // (b) finding 5: only host-local pins are ever re-applied.
            let host = value.get("host").and_then(|h| h.as_str()).unwrap_or("");
            let has_local_lock_pin = is_local_host(host, local_origins);
            if !has_local_lock_pin {
                continue;
            }
            // (c)+(d): the ledger is the authority, gated on content freshness.
            let state = classify_plugin_consent(
                has_local_lock_pin,
                ledger.plugins.get(plugin_id),
                live_hash,
            );
            if !state.is_reappliable() {
                continue;
            }
            // Insert-only: never overwrite a pointer the user already has.
            if plugins["data"].get(plugin_id).is_some() {
                continue;
            }
            plugins["data"][plugin_id] = value;
            inserted += 1;
        }
    }
    // Re-sync `ids` with the data keys: keep the existing order for ids still
    // present, then append any data key not yet listed (sorted — data is a
    // sorted map). The server schema requires ids to be present.
    let data_keys: Vec<String> = plugins["data"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut ids: Vec<String> = plugins
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .filter(|id| data_keys.contains(id))
                .collect()
        })
        .unwrap_or_default();
    for key in &data_keys {
        if !ids.contains(key) {
            ids.push(key.clone());
        }
    }
    plugins["ids"] = json!(ids);
    (plugins, inserted)
}

/// One boot re-apply pass: re-insert every lock-pinned plugin pointer missing
/// from profile props via the public `update-profile-props` RPC (the same
/// call class as `import-binfile`). Returns how many pointers were applied
/// (0 = already complete / nothing pinned). Pure DB-pointer re-derivation —
/// it never writes the lockfile or any file tree.
pub async fn reapply_plugin_props(state: &PackagesState) -> anyhow::Result<usize> {
    let Some(client) = state.client() else {
        return Ok(0);
    };
    let lock = Lockfile::load_or_default(&state.vault_root)?;
    if lock.packages.values().all(|e| e.plugin_props.is_empty()) {
        return Ok(0);
    }
    // The consent ledger is the re-apply AUTHORITY (finding 1); the live content
    // hashes gate on freshness (finding 3). Both are read off the DB.
    let ledger = sync_core::ConsentLedger::load_or_default(&state.data_dir)?;
    let live_hashes = live_plugin_content_hashes(&state.packages_dir, &lock);
    let props = client.get_profile_props().await?;
    let (merged, inserted) =
        plan_plugin_reapply(&lock, &ledger, &live_hashes, &props, &state.local_origins);
    if inserted == 0 {
        return Ok(0);
    }
    client
        .update_profile_props(&json!({ "plugins": merged }))
        .await?;
    tracing::info!(
        applied = inserted,
        "E7 boot re-apply: plugin registry pointer(s) re-derived from lock.json via update-profile-props"
    );
    Ok(inserted)
}

/// One capture pass: RECORD the local-origin plugin pointers the USER created
/// through Penpot's native Plugin Manager into BOTH `lock.json` (portability
/// pin) AND the per-machine consent ledger (re-apply authority, finding 1) —
/// consent already given, we only record it so a DB wipe can re-derive it;
/// surface-don't-apply is intact because this never writes profile props.
///
/// Recording into the ledger is SOUND because boot re-apply never seeds a
/// pointer without ledger authority: so any local-origin pointer live in the DB
/// is either a genuine native consent on this machine OR one WE re-applied from
/// an already-valid ledger entry — recording the current content hash for it is
/// correct in both cases.
///
/// A `seen` set (session-scoped, starts empty each boot) tracks pluginIds
/// observed present. A pluginId we SAW present and that then vanishes is a
/// genuine native-manager UNINSTALL → unpin `lock.json` AND prune the ledger
/// (no resurrection against intent). Crucially, absence we NEVER saw present
/// (a cloned-vault pin, or a drifted pin re-apply declined to seed) is left
/// untouched — it is not an uninstall, so the `availableNeedsConsent` /
/// `driftedNeedsReconsent` surface survives. Returns whether anything changed.
pub async fn capture_plugin_props(
    state: &PackagesState,
    seen: &mut std::collections::BTreeSet<String>,
) -> anyhow::Result<bool> {
    let Some(client) = state.client() else {
        return Ok(false);
    };
    let props = client.get_profile_props().await?;
    let captured = local_plugin_pointers(&props, &state.local_origins);
    let mut lock = Lockfile::load_or_default(&state.vault_root)?;
    let mut ledger = sync_core::ConsentLedger::load_or_default(&state.data_dir)?;
    let mut lock_changed = false;
    let mut ledger_changed = false;

    let live_plugin_ids: std::collections::BTreeSet<String> = captured
        .values()
        .flat_map(|m| m.keys().cloned())
        .collect();

    // Pin / refresh every package that currently has a live local pointer, and
    // record the consent ledger entry (consentedContentHash = CURRENT hash).
    for (pkg, pins) in &captured {
        let pkg_dir = state.packages_dir.join(pkg);
        let content_hash = plugin_content_hash(&pkg_dir).unwrap_or_default();
        match lock.packages.get_mut(pkg) {
            Some(entry) => {
                if entry.plugin_props != *pins {
                    entry.plugin_props = pins.clone();
                    lock_changed = true;
                }
                // The lock content_hash is the portability pin recorded at
                // first capture; drift is judged against the LEDGER's
                // consentedContentHash, not this, so it is not refreshed here.
            }
            None => {
                let manifest = read_manifest(&pkg_dir);
                let plugin_manifest = read_plugin_manifest(&pkg_dir);
                let name = manifest
                    .name
                    .clone()
                    .or_else(|| plugin_manifest.as_ref().and_then(|m| m.name.clone()))
                    .unwrap_or_else(|| display_name_from_id(pkg));
                lock.upsert(
                    pkg.clone(),
                    LockEntry {
                        version: manifest.version.clone().unwrap_or_else(|| "0.0.0".into()),
                        kind: manifest.kind.clone().unwrap_or_else(|| PLUGIN_KIND.into()),
                        // The content pin: hash of the SERVED asset surface.
                        content_hash: content_hash.clone(),
                        contract_hash: String::new(),
                        source_git_url: origin_url(&pkg_dir),
                        // A plugin package materializes NO vault file — it is
                        // carried-and-pointed-at, never imported into the DB.
                        file_id: String::new(),
                        name,
                        installed_at: sync_core::lock::now_rfc3339(),
                        library_shared: false,
                        plugin_props: pins.clone(),
                        links: Vec::new(),
                    },
                );
                lock_changed = true;
                tracing::info!(
                    package = %pkg,
                    "E7 capture: user-installed plugin pointer pinned in lock.json"
                );
            }
        }
        // Record / refresh the per-machine consent ledger — the re-apply
        // authority. Preserve the original `consentedAt`; refresh the content
        // hash to the currently-served surface.
        for (plugin_id, pointer_json) in pins {
            let (host, code) = pointer_host_code(pointer_json)
                .unwrap_or_else(|| (String::new(), String::new()));
            let consented_at = ledger
                .plugins
                .get(plugin_id)
                .map(|r| r.consented_at.clone())
                .unwrap_or_else(sync_core::lock::now_rfc3339);
            let rec = sync_core::ConsentRecord {
                consented_content_hash: content_hash.clone(),
                host,
                code,
                consented_at,
            };
            if ledger.plugins.get(plugin_id) != Some(&rec) {
                let first = !ledger.plugins.contains_key(plugin_id);
                ledger.plugins.insert(plugin_id.clone(), rec);
                ledger_changed = true;
                if first {
                    tracing::info!(
                        plugin = %plugin_id,
                        "E7 capture: native-manager consent recorded in the local consent ledger"
                    );
                }
            }
            seen.insert(plugin_id.clone());
        }
    }

    // Genuine uninstall: a pluginId we SAW present this session that is now gone
    // → unpin lock.json AND prune the ledger. Never triggered for a cloned pin
    // or a drift-declined pin (those were never observed present this session).
    let uninstalled: Vec<String> = seen
        .iter()
        .filter(|id| !live_plugin_ids.contains(*id))
        .cloned()
        .collect();
    for plugin_id in uninstalled {
        if ledger.plugins.remove(&plugin_id).is_some() {
            ledger_changed = true;
        }
        let pkgs: Vec<String> = lock
            .packages
            .iter()
            .filter(|(_, e)| e.plugin_props.contains_key(&plugin_id))
            .map(|(k, _)| k.clone())
            .collect();
        for pkg in pkgs {
            if let Some(entry) = lock.packages.get_mut(&pkg) {
                entry.plugin_props.remove(&plugin_id);
                if entry.plugin_props.is_empty()
                    && entry.kind == PLUGIN_KIND
                    && entry.file_id.is_empty()
                {
                    lock.packages.remove(&pkg);
                }
                lock_changed = true;
            }
        }
        seen.remove(&plugin_id);
        tracing::info!(
            plugin = %plugin_id,
            "E7 capture: plugin uninstalled through the native manager — unpinned from lock.json + pruned from consent ledger"
        );
    }

    if lock_changed {
        lock.save(&state.vault_root)?;
    }
    if ledger_changed {
        ledger.save(&state.data_dir)?;
    }
    Ok(lock_changed || ledger_changed)
}

/// Bounded window for the boot re-apply retry (mirrors the E3 re-link
/// constants): the backend is already provisioned when this spawns, so the
/// first pass usually completes; retries cover transient RPC hiccups.
const PLUGIN_REAPPLY_BOOT_ATTEMPTS: usize = 60;
const PLUGIN_REAPPLY_BOOT_INTERVAL: Duration = Duration::from_secs(1);

/// How often the capture loop re-reads profile props to pin/unpin what the
/// user did through the native Plugin Manager. Loopback `get-profile` — cheap.
pub const PLUGIN_CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the E7 plugin reconcile (mirrors [`spawn_relink_reconcile`]):
///
/// 1. **Boot re-apply phase** (bounded): after a DB wipe the registry pointer
///    is gone (DB-only derived state) — re-insert every lock-pinned pointer
///    via `update-profile-props`. Insert-only; runs once to completion.
/// 2. **Capture loop**: keep `lock.json` recording the pointers the USER
///    installs/uninstalls through the native manager, so the pin is always
///    current when the next wipe happens. Never writes profile props.
pub fn spawn_plugin_reconcile(state: Arc<PackagesState>) {
    tokio::spawn(async move {
        for _ in 0..PLUGIN_REAPPLY_BOOT_ATTEMPTS {
            match reapply_plugin_props(&state).await {
                Ok(applied) => {
                    if applied > 0 {
                        tracing::info!(applied, "E7 boot plugin re-apply complete");
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "E7 boot plugin re-apply pass errored; retrying"
                    );
                }
            }
            tokio::time::sleep(PLUGIN_REAPPLY_BOOT_INTERVAL).await;
        }
        // Session-scoped set of pluginIds observed present. Starts empty each
        // boot so the capture prune only fires on a within-session uninstall
        // transition (was present → now gone), never on a wipe/clone/drift
        // absence we never saw present.
        let mut seen = std::collections::BTreeSet::new();
        loop {
            if let Err(e) = capture_plugin_props(&state, &mut seen).await {
                tracing::debug!(
                    error = format!("{e:#}"),
                    "E7 plugin capture pass errored; will retry"
                );
            }
            tokio::time::sleep(PLUGIN_CAPTURE_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------
// GET /__api/packages/plugins — the discovered-plugin surface (E7)
// ---------------------------------------------------------------------------

/// One discovered plugin package, as the gallery renders it. The install
/// affordance is the LOCAL MANIFEST URL — the user pastes it into Penpot's
/// own native Plugin Manager (surface-don't-apply: presenting is ours,
/// installing is the user's, consent is Penpot's).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginView {
    id: String,
    name: String,
    description: String,
    version: String,
    kind: String,
    /// `/__packages/<id>/manifest.json` — what the user pastes into the
    /// native Plugin Manager.
    manifest_url: String,
    icon_url: Option<String>,
    permissions: Vec<String>,
    /// True ONLY for a genuine current consent on this machine (ledger entry
    /// present AND its consented content hash == the live hash). A cloned-vault
    /// pin (no ledger) or a drifted pin reads `installed=false` — a `lock.json`
    /// pin alone is NOT "installed" (finding 1).
    installed: bool,
    /// The pointer is currently present in profile props (live in the DB).
    live: bool,
    /// The per-machine consent state (finding 1): `available` |
    /// `availableNeedsConsent` (cloned vault) | `driftedNeedsReconsent`
    /// (served code changed since consent) | `installed`.
    state: String,
    /// The lockfile content pin over the served assets ("" if never pinned).
    pinned_content_hash: String,
    /// The content hash recorded in the local consent ledger at consent time
    /// ("" if never consented on this machine).
    consented_content_hash: String,
    /// Fresh hash over the current on-disk assets.
    live_content_hash: String,
    /// `state == driftedNeedsReconsent` — the served assets moved since
    /// consent. Surfaced, never auto-re-registered (surface-don't-apply).
    drifted: bool,
}

async fn list_plugins(State(state): State<Arc<PackagesState>>) -> Response {
    let packages_dir = state.packages_dir.clone();
    let vault_root = state.vault_root.clone();
    let data_dir = state.data_dir.clone();

    // Disk scan + lockfile + consent-ledger read off the async worker.
    let scanned = tokio::task::spawn_blocking(move || {
        let lock = Lockfile::load_or_default(&vault_root).unwrap_or_default();
        let ledger = sync_core::ConsentLedger::load_or_default(&data_dir).unwrap_or_default();
        let mut dirs: Vec<String> = std::fs::read_dir(&packages_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| is_safe_id(n))
            .collect();
        dirs.sort();
        let mut out: Vec<(String, PluginManifest, String)> = Vec::new();
        for id in dirs {
            let pkg_dir = packages_dir.join(&id);
            let Some(pm) = read_plugin_manifest(&pkg_dir) else {
                continue; // not a plugin package (no manifest.json code field)
            };
            let live_hash = plugin_content_hash(&pkg_dir).unwrap_or_default();
            out.push((id, pm, live_hash));
        }
        (lock, ledger, out)
    })
    .await;
    let (lock, ledger, discovered) = match scanned {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("plugin scan join: {e}")})),
            )
                .into_response()
        }
    };

    // Live witness: which local pointers are currently in profile props.
    let live_pkgs: std::collections::BTreeSet<String> = match state.client() {
        Some(client) => match client.get_profile_props().await {
            Ok(props) => local_plugin_pointers(&props, &state.local_origins)
                .into_keys()
                .collect(),
            Err(_) => Default::default(),
        },
        None => Default::default(),
    };

    let plugins: Vec<PluginView> = discovered
        .into_iter()
        .map(|(id, pm, live_hash)| {
            let manifest = read_manifest(&state.packages_dir.join(&id));
            let entry = lock.packages.get(&id);
            let pinned_hash = entry.map(|e| e.content_hash.clone()).unwrap_or_default();

            // Per-package consent state (finding 1): merge across the package's
            // host-local lock pins, gated on the per-machine ledger + drift.
            let mut consent_state = PluginConsentState::Available;
            let mut consented_content_hash = String::new();
            if let Some(entry) = entry {
                for (plugin_id, pointer_json) in &entry.plugin_props {
                    let host_local = pointer_host_code(pointer_json)
                        .map(|(h, _)| is_local_host(&h, &state.local_origins))
                        .unwrap_or(false);
                    if !host_local {
                        continue;
                    }
                    let led = ledger.plugins.get(plugin_id);
                    if let Some(rec) = led {
                        consented_content_hash = rec.consented_content_hash.clone();
                    }
                    let s = classify_plugin_consent(true, led, &live_hash);
                    consent_state = consent_state.merged_with(s);
                }
            }

            PluginView {
                name: pm
                    .name
                    .clone()
                    .or_else(|| manifest.name.clone())
                    .unwrap_or_else(|| display_name_from_id(&id)),
                description: pm.description.clone().unwrap_or_default(),
                version: manifest.version.unwrap_or_else(|| "0.0.0".into()),
                kind: manifest.kind.unwrap_or_else(|| PLUGIN_KIND.into()),
                manifest_url: format!("/__packages/{id}/manifest.json"),
                icon_url: pm.icon.clone(),
                permissions: pm.permissions.clone(),
                installed: consent_state == PluginConsentState::Installed,
                live: live_pkgs.contains(&id),
                state: consent_state.as_str().to_string(),
                drifted: consent_state == PluginConsentState::DriftedNeedsReconsent,
                pinned_content_hash: pinned_hash,
                consented_content_hash,
                live_content_hash: live_hash,
                id,
            }
        })
        .collect();

    Json(json!({ "ok": true, "count": plugins.len(), "plugins": plugins })).into_response()
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

// ---------------------------------------------------------------------------
// E4b — surface-don't-apply update channel + drift conflict copy
// ---------------------------------------------------------------------------
//
// THE CORE VALUE: a consumer's materialized `.penpot` file on disk stays
// BYTE-UNCHANGED. We SURFACE a package's update (its `.penpot-packages/<id>`
// source moved since install) and classify the bump; we NEVER rewrite the
// installed file to apply it. Drift (a managed package whose incoming source
// diverges) is preserved as a `.conflict-<ts>.penpot` copy that overwrites
// neither side — the exact M3 conflict rule, reused verbatim.

/// How often the background poller recomputes the update model. The
/// `send_if_modified` debounce means an unchanged model is never republished, so
/// a short interval is cheap for the small set of installed packages.
pub const UPDATES_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One package's surface-don't-apply update status, serialized camelCase.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdateRow {
    pub id: String,
    pub name: String,
    pub version: String,
    /// The materialized vault file id (the durable M2 resurrect-by-id target).
    pub file_id: String,
    /// The contract hash pinned in `lock.json` at install time (the "before").
    pub pinned_contract_hash: String,
    /// Freshly computed contract hash over the current `.penpot-packages/<id>`
    /// source (the "after"). Empty when the package carries no readable source.
    pub live_contract_hash: String,
    /// True when `live != pinned` — the source moved since install.
    pub update_available: bool,
    /// Bump severity of the change: `patch` | `migration` | `minor` | `major`.
    /// `None` when there is no update, or the pinned "before" contract could not
    /// be reconstructed (surface the update without over-claiming a severity).
    pub bump: Option<String>,
    /// True for a contract-**major** bump (a breaking change surfaced).
    pub is_major: bool,
    /// Exact deep link to open the materialized file in the workspace.
    pub deep_link: String,
    /// A drift conflict copy already preserved next to the installed file
    /// (`<stem>.conflict-<ts>.penpot`), if one exists — read-only detection.
    pub conflict_copy_path: Option<String>,
}

/// The whole update model, serialized camelCase. Deterministic (packages are
/// iterated in lockfile key order), so `send_if_modified` only fires on a real
/// change.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdatesModel {
    /// Count of packages with `update_available`.
    pub updates: usize,
    /// Count of packages whose surfaced bump is `major`.
    pub majors: usize,
    pub rows: Vec<PackageUpdateRow>,
}

/// Pure classification core: given the pinned "before" contract, the live
/// "after" contract, and their file-id-excluded hashes, decide whether an update
/// is available and its bump. `diff_contracts` is uuid-invariant (E1), so the
/// "before" may be reconstructed from the materialized vault file even though it
/// carries different ids than the source. Read-only — it mutates nothing.
pub fn classify_update(
    pinned_hash: &str,
    live_hash: &str,
    before: Option<&vault_index::LibraryContract>,
    after: &vault_index::LibraryContract,
) -> (bool, Option<vault_index::Bump>) {
    if pinned_hash == live_hash {
        // Contract byte-identical (patch or no change) — nothing to surface.
        (false, None)
    } else {
        let bump = before.map(|b| vault_index::diff_contracts(b, after).overall);
        (true, bump)
    }
}

/// Extract the contract of the tree materialized on disk for `file_id`, located
/// via the sync manifest (file_id → vault-relative `.penpot` dir). This is the
/// honest "before": install round-trips the source through the DB, and the
/// contract is round-trip- and uuid-invariant, so the materialized file's
/// contract equals the source-at-install contract. `None` if no manifest, no
/// entry, or the tree is unreadable (→ update surfaced without a bump).
fn materialized_contract(vault_root: &Path, file_id: &str) -> Option<vault_index::LibraryContract> {
    let manifest = sync_core::Manifest::load(vault_root).ok().flatten()?;
    let rel = &manifest.files.get(file_id)?.path;
    let files = sync_core::read_tree(&vault_root.join(rel)).ok()?;
    Some(vault_index::extract_contracts(&files))
}

/// Read-only: existing drift conflict copies (`<stem>.conflict-<ts>.penpot`)
/// sitting next to the installed file at `installed_rel`. Sorted for
/// determinism.
fn existing_conflict_copies(vault_root: &Path, installed_rel: &str) -> Vec<String> {
    let stem = installed_rel
        .strip_suffix(sync_daemon::paths::PENPOT_DIR_SUFFIX)
        .unwrap_or(installed_rel);
    let (dir_rel, base) = match stem.rsplit_once('/') {
        Some((d, b)) => (d.to_string(), b.to_string()),
        None => (String::new(), stem.to_string()),
    };
    let prefix = format!("{base}{}", sync_daemon::paths::CONFLICT_MARKER);
    let scan_dir = if dir_rel.is_empty() {
        vault_root.to_path_buf()
    } else {
        vault_root.join(&dir_rel)
    };
    let mut out: Vec<String> = std::fs::read_dir(&scan_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with(&prefix) && sync_daemon::paths::is_conflict_dir_name(n))
        .map(|n| if dir_rel.is_empty() { n.clone() } else { format!("{dir_rel}/{n}") })
        .collect();
    out.sort();
    out
}

/// Compute the surface-don't-apply update model from disk. Read-only: it loads
/// `lock.json`, the sync manifest, and each package's `.penpot-packages/<id>`
/// source tree, and NEVER writes any file (the "surface, don't apply"
/// guarantee). Errors degrade to an empty/partial model rather than propagating.
pub fn compute_updates_model(
    vault_root: &Path,
    packages_dir: &Path,
    team_id: &str,
) -> PackageUpdatesModel {
    let lock = match Lockfile::load_or_default(vault_root) {
        Ok(l) => l,
        Err(_) => return PackageUpdatesModel::default(),
    };
    let mut rows: Vec<PackageUpdateRow> = Vec::new();
    let mut updates = 0usize;
    let mut majors = 0usize;

    for (id, entry) in &lock.packages {
        // The live "after": the current source tree's contract + its pin hash,
        // from a single read. Missing/unreadable source → nothing to surface.
        let after = discover_penpot_tree(&packages_dir.join(id))
            .and_then(|tree| sync_core::read_tree(&tree).ok())
            .map(|files| vault_index::extract_contracts(&files));

        let (update_available, bump, live_hash) = match &after {
            Some(after) => {
                let live_hash = contract_hash_of_lib(after);
                let before = if live_hash == entry.contract_hash {
                    None // no need to reconstruct — no update.
                } else {
                    // Reconstruct the "before" from the installed vault file, but
                    // only TRUST it as the baseline if it still hashes to the pin.
                    // lock.json stores only the pinned hash (not the contract
                    // body), and the materialized file is sync-tracked — a local
                    // edit to the installed package would drift its contract and
                    // skew the surfaced bump (e.g. a false MAJOR). The pinned hash
                    // is uuid-invariant (E1), so `contract_hash_of_lib(materialized)
                    // == entry.contract_hash` iff the install has NOT drifted at the
                    // contract level. When it has, surface the update WITHOUT a
                    // (potentially wrong) severity rather than a misleading one.
                    // Exact severity under a drifted install would need the pinned
                    // contract body in lock.json — a deliberate future refinement.
                    materialized_contract(vault_root, &entry.file_id)
                        .filter(|m| contract_hash_of_lib(m) == entry.contract_hash)
                };
                let (avail, bump) =
                    classify_update(&entry.contract_hash, &live_hash, before.as_ref(), after);
                (avail, bump, live_hash)
            }
            None => (false, None, String::new()),
        };

        if update_available {
            updates += 1;
        }
        let is_major = bump == Some(vault_index::Bump::Major);
        if is_major {
            majors += 1;
        }

        // Read-only: surface an already-preserved drift copy, if any.
        let conflict_copy_path = sync_core::Manifest::load(vault_root)
            .ok()
            .flatten()
            .and_then(|m| m.files.get(&entry.file_id).map(|e| e.path.clone()))
            .map(|rel| existing_conflict_copies(vault_root, &rel))
            .and_then(|mut v| v.pop());

        rows.push(PackageUpdateRow {
            id: id.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            file_id: entry.file_id.clone(),
            pinned_contract_hash: entry.contract_hash.clone(),
            live_contract_hash: live_hash,
            update_available,
            bump: bump.map(|b| b.as_str().to_string()),
            is_major,
            deep_link: vault_index::workspace_deep_link(team_id, &entry.file_id, None),
            conflict_copy_path,
        });
    }

    PackageUpdatesModel { updates, majors, rows }
}

/// Spawn the background update poller. It recomputes [`compute_updates_model`] on
/// [`UPDATES_POLL_INTERVAL`] in a `spawn_blocking` (off the async worker) and
/// publishes to a `watch` channel with `send_if_modified` — the debounce, so an
/// unchanged model never churns the channel. Returns the receiver the
/// `/__api/packages/updates` endpoint borrows.
pub fn spawn_updates_poller(
    vault_root: PathBuf,
    packages_dir: PathBuf,
    team_id: String,
) -> watch::Receiver<PackageUpdatesModel> {
    let (tx, rx) = watch::channel(PackageUpdatesModel::default());
    tokio::spawn(async move {
        loop {
            let (vr, pd, tid) = (vault_root.clone(), packages_dir.clone(), team_id.clone());
            let model = tokio::task::spawn_blocking(move || compute_updates_model(&vr, &pd, &tid))
                .await
                .unwrap_or_default();
            // send_if_modified = the debounce: publish ONLY on an actual change.
            tx.send_if_modified(move |cur| {
                if *cur != model {
                    *cur = model;
                    true
                } else {
                    false
                }
            });
            // Stop once no one is listening (the receiver in state was dropped).
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(UPDATES_POLL_INTERVAL).await;
        }
    });
    rx
}

async fn updates(State(state): State<Arc<PackagesState>>) -> Response {
    let model = state.updates_rx.borrow().clone();
    Json(model).into_response()
}

/// The conflict rule for drift of a managed package (reuses
/// [`sync_daemon::paths::conflict_path_for`]): preserve the INCOMING source
/// version as a uniquified `<stem>.conflict-<ts>.penpot` sibling of the installed
/// file, NEVER overwriting the installed file OR the source. Returns the
/// vault-relative path of the copy. Surface-don't-apply: the installed file is
/// left byte-unchanged (we do not import the update over it).
pub fn preserve_package_drift(
    vault_root: &Path,
    installed_rel: &str,
    source_tree: &Path,
) -> anyhow::Result<String> {
    // Uniquify against anything already on disk — never overwrite (mirrors the
    // engine's `stage_to_conflict_copy`).
    let now = sync_core::lock::now_rfc3339();
    let mut conflict_rel = sync_daemon::paths::conflict_path_for(installed_rel, &now);
    let mut counter = 1u32;
    while vault_root.join(&conflict_rel).symlink_metadata().is_ok() {
        counter += 1;
        conflict_rel =
            sync_daemon::paths::conflict_path_for(installed_rel, &format!("{now}-{counter}"));
    }
    let target = vault_root.join(&conflict_rel);
    // Copy the incoming source tree file-by-file, then normalize it into the same
    // clean `.penpot` representation every managed file has.
    let files = sync_core::read_tree(source_tree)?;
    for (rel, bytes) in &files {
        let dest = target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
    }
    sync_core::normalize_tree(&target)?;
    tracing::warn!(
        installed = %installed_rel, copy = %conflict_rel,
        "PACKAGE DRIFT: incoming source preserved as a conflict copy — installed file left byte-unchanged (surface, not applied)"
    );
    Ok(conflict_rel)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriftReq {
    id: String,
}

async fn preserve_drift(
    State(state): State<Arc<PackagesState>>,
    Json(req): Json<DriftReq>,
) -> Response {
    let id = req.id.trim().to_string();
    if !is_safe_id(&id) {
        return bad_request(format!("unsafe package id {id:?}"));
    }
    let vault_root = state.vault_root.clone();
    let packages_dir = state.packages_dir.clone();
    let result =
        tokio::task::spawn_blocking(move || do_preserve_drift(&vault_root, &packages_dir, &id)).await;
    match result {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": format!("{e:#}")})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("drift task join: {e}")})),
        )
            .into_response(),
    }
}

fn do_preserve_drift(
    vault_root: &Path,
    packages_dir: &Path,
    id: &str,
) -> anyhow::Result<serde_json::Value> {
    let lock = Lockfile::load_or_default(vault_root)?;
    let entry = lock
        .packages
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("package {id:?} is not installed"))?;
    let source_tree = discover_penpot_tree(&packages_dir.join(id))
        .ok_or_else(|| anyhow::anyhow!("package {id:?} carries no .penpot source tree"))?;
    let manifest = sync_core::Manifest::load(vault_root)?
        .ok_or_else(|| anyhow::anyhow!("no sync manifest yet — nothing installed on disk"))?;
    let installed_rel = manifest
        .files
        .get(&entry.file_id)
        .map(|e| e.path.clone())
        .ok_or_else(|| anyhow::anyhow!("package {id:?} file {} not on disk", entry.file_id))?;
    // Drift gate: only preserve a copy when the source actually differs from the
    // pinned contract. Calling this with nothing drifted is a safe no-op — never
    // litter the vault with a spurious `.conflict-<ts>` copy. (When there IS
    // drift, each call snapshots the incoming source as a fresh uniquified copy,
    // overwriting neither side.) The pin hash is uuid-invariant (E1), so this
    // matches the update channel's own availability signal.
    let source_contract_hash = contract_hash_of_tree(&source_tree)?;
    if source_contract_hash == entry.contract_hash {
        return Ok(json!({
            "ok": true,
            "id": id,
            "installedRel": installed_rel,
            "conflictCopyPath": serde_json::Value::Null,
            "drifted": false,
        }));
    }
    let conflict_rel = preserve_package_drift(vault_root, &installed_rel, &source_tree)?;
    Ok(json!({
        "ok": true,
        "id": id,
        "installedRel": installed_rel,
        "conflictCopyPath": conflict_rel,
        "drifted": true,
    }))
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

    // -----------------------------------------------------------------------
    // E4b — surface-don't-apply update channel + drift conflict copy
    // -----------------------------------------------------------------------

    /// Write a minimal contract-bearing `.penpot` tree: a `manifest.json`
    /// pinning `file_id` plus one color doc per name (the library-level exported
    /// surface `diff_contracts` classifies). Distinct file ids exercise the
    /// uuid-invariance of the contract.
    fn write_color_tree(tree_dir: &Path, file_id: &str, colors: &[&str]) {
        let write = |rel: &str, v: &serde_json::Value| {
            let p = tree_dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, serde_json::to_vec(v).unwrap()).unwrap();
        };
        write("manifest.json", &json!({"files": [{"id": file_id}]}));
        for (i, name) in colors.iter().enumerate() {
            write(
                &format!("files/{file_id}/colors/col{i}.json"),
                &json!({"id": format!("col-{i}"), "name": name}),
            );
        }
    }

    const MAT_ID: &str = "aaaa1111-2222-3333-4444-555566667777";
    const SRC_ID: &str = "bbbb9999-8888-7777-6666-555544443333";

    /// Build a vault fixture: a package `kit` installed as `Drafts/kit.penpot`
    /// (the materialized file, id `MAT_ID`) pinned in `lock.json`, plus its
    /// source tree under `.penpot-packages/kit/kit.penpot` (id `SRC_ID`). Both
    /// start with the same colors, so the pinned contract hash matches the
    /// source. Returns the vault root and the packages dir.
    fn setup_vault(colors: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let vault_root = tmp.path().to_path_buf();
        let packages_dir = vault_root.join(sync_core::PACKAGES_DIR_NAME);

        // Materialized vault file (the "before" — byte-frozen at install).
        let installed = vault_root.join("Drafts/kit.penpot");
        write_color_tree(&installed, MAT_ID, colors);

        // Package source tree (the "after" — may drift/update).
        let source = packages_dir.join("kit/kit.penpot");
        write_color_tree(&source, SRC_ID, colors);

        // Sync manifest: file_id → on-disk path (the resurrect-by-id spine).
        let mut manifest = sync_core::Manifest::default();
        manifest.files.insert(
            MAT_ID.to_string(),
            sync_core::ManifestEntry {
                path: "Drafts/kit.penpot".to_string(),
                project_id: "proj".to_string(),
                project_name: "Drafts".to_string(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: "h".to_string(),
                last_synced_at: "2026-07-15T00:00:00Z".to_string(),
            },
        );
        manifest.save(&vault_root).unwrap();

        // Lockfile: pin kit at the source's contract hash (== materialized hash).
        let pinned = contract_hash_of_tree(&source).unwrap();
        let mut lock = Lockfile::default();
        lock.upsert(
            "kit",
            LockEntry {
                version: "1.0.0".into(),
                kind: "component-library".into(),
                content_hash: "content".into(),
                contract_hash: pinned,
                source_git_url: String::new(),
                file_id: MAT_ID.into(),
                name: "Kit".into(),
                installed_at: "2026-07-15T00:00:00Z".into(),
                library_shared: false,
                plugin_props: Default::default(),
                links: Vec::new(),
            },
        );
        lock.save(&vault_root).unwrap();

        (tmp, vault_root, packages_dir)
    }

    /// Snapshot a tree's bytes for a byte-unchanged assertion.
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        sync_core::read_tree(dir).unwrap()
    }

    #[test]
    fn classify_update_is_pure_and_hash_gated() {
        let before = vault_index::extract_contracts(
            &{
                let t = tempfile::tempdir().unwrap();
                write_color_tree(t.path(), MAT_ID, &["Brand"]);
                sync_core::read_tree(t.path()).unwrap()
            },
        );
        let after = vault_index::extract_contracts(
            &{
                let t = tempfile::tempdir().unwrap();
                write_color_tree(t.path(), SRC_ID, &["Brand", "Accent"]);
                sync_core::read_tree(t.path()).unwrap()
            },
        );
        // Equal hashes → no update, regardless of the contracts.
        let (avail, bump) = classify_update("H", "H", Some(&before), &after);
        assert!(!avail);
        assert_eq!(bump, None);
        // Different hashes → update; added color → minor.
        let (avail, bump) = classify_update("H1", "H2", Some(&before), &after);
        assert!(avail);
        assert_eq!(bump, Some(vault_index::Bump::Minor));
        // Different hashes but no "before" → update surfaced without a severity.
        let (avail, bump) = classify_update("H1", "H2", None, &after);
        assert!(avail);
        assert_eq!(bump, None);
    }

    #[test]
    fn updates_model_surfaces_no_update_when_source_matches() {
        let (_tmp, vault_root, packages_dir) = setup_vault(&["Brand"]);
        let before = snapshot(&vault_root.join("Drafts/kit.penpot"));
        let model = compute_updates_model(&vault_root, &packages_dir, "team-1");
        assert_eq!(model.rows.len(), 1);
        assert_eq!(model.updates, 0);
        assert_eq!(model.majors, 0);
        let row = &model.rows[0];
        assert_eq!(row.id, "kit");
        assert!(!row.update_available);
        assert_eq!(row.bump, None);
        assert_eq!(row.pinned_contract_hash, row.live_contract_hash);
        assert_eq!(
            row.deep_link,
            format!("/#/workspace?team-id=team-1&file-id={MAT_ID}")
        );
        // Surface, don't apply: the materialized file is byte-unchanged.
        assert_eq!(before, snapshot(&vault_root.join("Drafts/kit.penpot")));
    }

    #[test]
    fn updates_model_surfaces_minor_bump_and_leaves_file_byte_unchanged() {
        let (_tmp, vault_root, packages_dir) = setup_vault(&["Brand"]);
        // Edit the SOURCE only: add a color (a minor, additive change).
        write_color_tree(&packages_dir.join("kit/kit.penpot"), SRC_ID, &["Brand", "Accent"]);
        let before = snapshot(&vault_root.join("Drafts/kit.penpot"));

        let model = compute_updates_model(&vault_root, &packages_dir, "team-1");
        let row = &model.rows[0];
        assert!(row.update_available);
        assert_eq!(row.bump.as_deref(), Some("minor"));
        assert!(!row.is_major);
        assert_ne!(row.pinned_contract_hash, row.live_contract_hash);
        assert_eq!(model.updates, 1);
        assert_eq!(model.majors, 0);
        // THE CORE VALUE: the installed file was NOT rewritten to apply it.
        assert_eq!(before, snapshot(&vault_root.join("Drafts/kit.penpot")));
    }

    #[test]
    fn updates_model_surfaces_major_bump_on_removed_element() {
        let (_tmp, vault_root, packages_dir) = setup_vault(&["Brand"]);
        // Edit the SOURCE: drop "Brand", add "Accent" → a removed element = major.
        write_color_tree(&packages_dir.join("kit/kit.penpot"), SRC_ID, &["Accent"]);
        let before = snapshot(&vault_root.join("Drafts/kit.penpot"));

        let model = compute_updates_model(&vault_root, &packages_dir, "team-1");
        let row = &model.rows[0];
        assert!(row.update_available);
        assert_eq!(row.bump.as_deref(), Some("major"));
        assert!(row.is_major);
        assert_eq!(model.majors, 1);
        assert_eq!(before, snapshot(&vault_root.join("Drafts/kit.penpot")));
    }

    #[test]
    fn preserve_drift_writes_conflict_copy_overwriting_neither_side() {
        let (_tmp, vault_root, packages_dir) = setup_vault(&["Brand"]);
        // The source has drifted from the installed file.
        write_color_tree(&packages_dir.join("kit/kit.penpot"), SRC_ID, &["Brand", "Accent"]);
        let installed = vault_root.join("Drafts/kit.penpot");
        let source = packages_dir.join("kit/kit.penpot");
        let installed_before = snapshot(&installed);
        let source_before = snapshot(&source);

        let copy_rel = preserve_package_drift(&vault_root, "Drafts/kit.penpot", &source).unwrap();
        // A conflict-copy name next to the installed file.
        assert!(copy_rel.starts_with("Drafts/kit.conflict-"));
        assert!(sync_daemon::paths::is_conflict_dir_name(
            copy_rel.rsplit('/').next().unwrap()
        ));
        assert!(vault_root.join(&copy_rel).is_dir());
        // Neither side was overwritten.
        assert_eq!(installed_before, snapshot(&installed), "installed file untouched");
        assert_eq!(source_before, snapshot(&source), "package source untouched");
        // The copy holds the INCOMING (drifted) version: 2 colors.
        let copy_files = snapshot(&vault_root.join(&copy_rel));
        let colors = copy_files
            .keys()
            .filter(|k| k.contains("/colors/"))
            .count();
        assert_eq!(colors, 2);

        // A second preservation in the same second uniquifies — never overwrites.
        let copy_rel2 = preserve_package_drift(&vault_root, "Drafts/kit.penpot", &source).unwrap();
        assert_ne!(copy_rel, copy_rel2);
        assert!(vault_root.join(&copy_rel).is_dir());
        assert!(vault_root.join(&copy_rel2).is_dir());
    }

    #[test]
    fn model_surfaces_an_existing_drift_conflict_copy() {
        let (_tmp, vault_root, packages_dir) = setup_vault(&["Brand"]);
        write_color_tree(&packages_dir.join("kit/kit.penpot"), SRC_ID, &["Brand", "Accent"]);
        let source = packages_dir.join("kit/kit.penpot");
        let copy_rel = preserve_package_drift(&vault_root, "Drafts/kit.penpot", &source).unwrap();

        let model = compute_updates_model(&vault_root, &packages_dir, "team-1");
        assert_eq!(model.rows[0].conflict_copy_path.as_deref(), Some(copy_rel.as_str()));
    }

    #[test]
    fn empty_vault_yields_an_empty_updates_model() {
        let tmp = tempfile::tempdir().unwrap();
        let model = compute_updates_model(
            tmp.path(),
            &tmp.path().join(sync_core::PACKAGES_DIR_NAME),
            "team-1",
        );
        assert_eq!(model, PackageUpdatesModel::default());
    }

    // -----------------------------------------------------------------------
    // E7 — plugin packages: static-route safety + pointer pin/re-apply
    // -----------------------------------------------------------------------

    #[test]
    fn safe_asset_path_rejects_traversal_dotfiles_and_junk() {
        assert!(is_safe_asset_path("manifest.json"));
        assert!(is_safe_asset_path("assets/ui/panel.html"));
        assert!(!is_safe_asset_path(""));
        assert!(!is_safe_asset_path("/etc/passwd"));
        assert!(!is_safe_asset_path("../../secret"));
        assert!(!is_safe_asset_path("a/../b"));
        assert!(!is_safe_asset_path("a/./b"));
        assert!(!is_safe_asset_path(".git/config"));
        assert!(!is_safe_asset_path("assets/.hidden"));
        assert!(!is_safe_asset_path("a//b"));
        assert!(!is_safe_asset_path("a\\b"));
        assert!(!is_safe_asset_path("a\0b"));
    }

    #[test]
    fn plugin_pkg_from_code_parses_only_local_package_paths() {
        assert_eq!(
            plugin_pkg_from_code("/__packages/e7-fixture-plugin/plugin.js").as_deref(),
            Some("e7-fixture-plugin")
        );
        assert_eq!(
            plugin_pkg_from_code("/__packages/kit/sub/dir/code.js").as_deref(),
            Some("kit")
        );
        // Third-party / non-local pointers are never pinned.
        assert_eq!(plugin_pkg_from_code("https://plugins.example.com/x/plugin.js"), None);
        assert_eq!(plugin_pkg_from_code("/plugin.js"), None);
        assert_eq!(plugin_pkg_from_code("/__packages/"), None);
        assert_eq!(plugin_pkg_from_code("/__packages/only-pkg-no-asset"), None);
        assert_eq!(plugin_pkg_from_code("/__packages/../escape/x.js"), None);
        assert_eq!(plugin_pkg_from_code("/__packages/.dot/x.js"), None);
    }

    #[test]
    fn plugin_manifest_requires_a_code_field() {
        let tmp = tempfile::tempdir().unwrap();
        // No manifest at all → not a plugin package.
        assert_eq!(read_plugin_manifest(tmp.path()), None);
        // A manifest without `code` (e.g. some random JSON) → not a plugin.
        std::fs::write(tmp.path().join("manifest.json"), br#"{"files": []}"#).unwrap();
        assert_eq!(read_plugin_manifest(tmp.path()), None);
        // A real plugin manifest parses (the E7 fixture shape).
        std::fs::write(
            tmp.path().join("manifest.json"),
            serde_json::to_vec(&json!({
                "name": "E7 Fixture Plugin",
                "description": "fixture",
                "code": "/__packages/e7-fixture-plugin/plugin.js",
                "icon": "/__packages/e7-fixture-plugin/icon.png",
                "permissions": ["content:write"]
            }))
            .unwrap(),
        )
        .unwrap();
        let pm = read_plugin_manifest(tmp.path()).unwrap();
        assert_eq!(pm.name.as_deref(), Some("E7 Fixture Plugin"));
        assert_eq!(pm.code, "/__packages/e7-fixture-plugin/plugin.js");
        assert_eq!(pm.permissions, vec!["content:write".to_string()]);
    }

    #[test]
    fn plugin_content_hash_is_deterministic_and_skips_dot_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("manifest.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("plugin.js"), b"code").unwrap();
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("assets/icon.png"), b"png").unwrap();
        let h1 = plugin_content_hash(tmp.path()).unwrap();
        assert_eq!(h1.len(), 64);
        // Adding dot entries (a .git dir, a dotfile) does NOT move the hash —
        // they are never served, so they are not pinned surface.
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), b"secret").unwrap();
        std::fs::write(tmp.path().join(".hidden"), b"secret").unwrap();
        assert_eq!(plugin_content_hash(tmp.path()).unwrap(), h1);
        // Changing a served byte DOES move the hash.
        std::fs::write(tmp.path().join("plugin.js"), b"code2").unwrap();
        assert_ne!(plugin_content_hash(tmp.path()).unwrap(), h1);
    }

    /// The exact props shape captured/probed live on 2.16.2:
    /// `plugins = {ids: [string], data: {pluginId → pointer}}`, each pointer
    /// carrying `pluginId`/`name`/`host`/`code`/`permissions`.
    fn spike_props(with_local: bool, with_foreign: bool) -> serde_json::Value {
        let mut data = serde_json::Map::new();
        let mut ids: Vec<String> = Vec::new();
        if with_local {
            ids.push("plugin-a".into());
            data.insert(
                "plugin-a".into(),
                json!({
                    "pluginId": "plugin-a",
                    "code": "/__packages/e7-fixture-plugin/plugin.js",
                    "host": "http://localhost:9022",
                    "icon": "/__packages/e7-fixture-plugin/icon.png",
                    "name": "E7 Fixture Plugin",
                    "permissions": ["content:write"]
                }),
            );
        }
        if with_foreign {
            ids.push("plugin-b".into());
            data.insert(
                "plugin-b".into(),
                json!({
                    "pluginId": "plugin-b",
                    "code": "https://plugins.example.com/b/plugin.js",
                    "host": "https://plugins.example.com",
                    "name": "Foreign",
                    "permissions": []
                }),
            );
        }
        json!({ "plugins": { "ids": ids, "data": data } })
    }

    const PKG: &str = "e7-fixture-plugin";

    fn local_origins() -> Vec<String> {
        vec![
            "http://localhost:9022".to_string(),
            "http://127.0.0.1:9022".to_string(),
        ]
    }

    /// A consent ledger authorizing `plugin-a` at content hash `hash`.
    fn ledger_with(hash: &str) -> sync_core::ConsentLedger {
        let mut l = sync_core::ConsentLedger::default();
        l.plugins.insert(
            "plugin-a".to_string(),
            sync_core::ConsentRecord {
                consented_content_hash: hash.into(),
                host: "http://localhost:9022".into(),
                code: format!("/__packages/{PKG}/plugin.js"),
                consented_at: "2026-07-16T00:00:00Z".into(),
            },
        );
        l
    }

    /// pkg id → live content hash (the drift-gate input).
    fn live_hashes(hash: &str) -> std::collections::BTreeMap<String, String> {
        let mut m = std::collections::BTreeMap::new();
        m.insert(PKG.to_string(), hash.to_string());
        m
    }

    #[test]
    fn local_plugin_pointers_gated_on_local_host_not_code_path() {
        let origins = local_origins();
        let props = spike_props(true, true);
        let pins = local_plugin_pointers(&props, &origins);
        assert_eq!(pins.len(), 1, "only the local-host pointer is captured");
        let pkg = &pins[PKG];
        assert_eq!(pkg.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&pkg["plugin-a"]).unwrap();
        assert_eq!(value["code"].as_str().unwrap(), format!("/__packages/{PKG}/plugin.js"));

        // Finding 5: a REMOTE-host pointer whose `code` starts `/__packages/`
        // is NOT local — it must never be captured/pinned/resurrected.
        let spoof = json!({ "plugins": { "ids": ["evil"], "data": { "evil": {
            "pluginId": "evil",
            "code": "/__packages/e7-fixture-plugin/plugin.js",
            "host": "https://evil.example.com",
            "name": "Spoof",
            "permissions": ["content:write"]
        }}}});
        assert!(
            local_plugin_pointers(&spoof, &origins).is_empty(),
            "a remote host with a /__packages code path is not local"
        );

        // No props / no plugins subtree → empty.
        assert!(local_plugin_pointers(&json!({}), &origins).is_empty());
        assert!(local_plugin_pointers(&json!({"plugins": {}}), &origins).is_empty());
    }

    #[test]
    fn is_local_host_matches_both_spellings_only() {
        let origins = local_origins();
        assert!(is_local_host("http://localhost:9022", &origins));
        assert!(is_local_host("http://127.0.0.1:9022/", &origins)); // trailing slash
        assert!(!is_local_host("https://plugins.example.com", &origins));
        assert!(!is_local_host("http://localhost:9999", &origins)); // wrong port
        assert!(!is_local_host("", &origins));
    }

    #[test]
    fn classify_plugin_consent_is_the_reapply_gate() {
        // Genuine current consent → Installed (the only re-appliable state).
        let led = ledger_with("H");
        let s = classify_plugin_consent(true, led.plugins.get("plugin-a"), "H");
        assert_eq!(s, PluginConsentState::Installed);
        assert!(s.is_reappliable());
        // Ledger present but content drifted → DriftedNeedsReconsent.
        let s = classify_plugin_consent(true, led.plugins.get("plugin-a"), "H2");
        assert_eq!(s, PluginConsentState::DriftedNeedsReconsent);
        assert!(!s.is_reappliable());
        // Lock pin present, NO ledger → AvailableNeedsConsent (cloned vault).
        let s = classify_plugin_consent(true, None, "H");
        assert_eq!(s, PluginConsentState::AvailableNeedsConsent);
        assert!(!s.is_reappliable());
        // No pin, no ledger → Available (discovered only).
        let s = classify_plugin_consent(false, None, "H");
        assert_eq!(s, PluginConsentState::Available);
        assert!(!s.is_reappliable());
    }

    fn plugin_lock_entry(pointer: &serde_json::Value) -> LockEntry {
        let mut plugin_props = std::collections::BTreeMap::new();
        plugin_props.insert("plugin-a".to_string(), serde_json::to_string(pointer).unwrap());
        LockEntry {
            version: "0.0.1".into(),
            kind: PLUGIN_KIND.into(),
            content_hash: "H".into(),
            contract_hash: String::new(),
            source_git_url: String::new(),
            file_id: String::new(),
            name: "E7 Fixture Plugin".into(),
            installed_at: "2026-07-16T00:00:00Z".into(),
            library_shared: false,
            plugin_props,
            links: Vec::new(),
        }
    }

    fn local_pointer() -> serde_json::Value {
        json!({
            "pluginId": "plugin-a",
            "code": format!("/__packages/{PKG}/plugin.js"),
            "host": "http://localhost:9022",
            "name": "E7 Fixture Plugin",
            "permissions": ["content:write"]
        })
    }

    /// HAPPY PATH: lock pin + ledger authority + fresh content → re-applied.
    #[test]
    fn plan_plugin_reapply_applies_only_ledger_authorized_pins() {
        let origins = local_origins();
        let mut lock = Lockfile::default();
        lock.upsert(PKG, plugin_lock_entry(&local_pointer()));

        // Wiped DB (no props) + ledger authorizes at "H" == live "H" → inserted.
        let (merged, inserted) =
            plan_plugin_reapply(&lock, &ledger_with("H"), &live_hashes("H"), &json!({}), &origins);
        assert_eq!(inserted, 1, "ledger-authorized, content-fresh → re-applied");
        assert_eq!(merged["data"]["plugin-a"], local_pointer());
        assert_eq!(merged["ids"], json!(["plugin-a"]));

        // Insert-only: a pointer already in the DB is never overwritten.
        let mut live = spike_props(true, false);
        live["plugins"]["data"]["plugin-a"]["permissions"] = json!([]);
        let (merged, inserted) =
            plan_plugin_reapply(&lock, &ledger_with("H"), &live_hashes("H"), &live, &origins);
        assert_eq!(inserted, 0);
        assert_eq!(merged["data"]["plugin-a"]["permissions"], json!([]));
    }

    /// THE SECURITY REGRESSION: a CLONED vault carries the lock pin but has NO
    /// ledger on this machine → NOTHING is re-applied (no consent here).
    #[test]
    fn plan_plugin_reapply_seeds_nothing_from_a_cloned_vault_without_ledger() {
        let origins = local_origins();
        let mut lock = Lockfile::default();
        lock.upsert(PKG, plugin_lock_entry(&local_pointer()));
        // Empty ledger = a vault cloned/pulled onto a fresh machine.
        let empty_ledger = sync_core::ConsentLedger::default();
        let (merged, inserted) = plan_plugin_reapply(
            &lock,
            &empty_ledger,
            &live_hashes("H"),
            &json!({}),
            &origins,
        );
        assert_eq!(inserted, 0, "no ledger authority → nothing auto-registered");
        assert!(
            merged["data"].get("plugin-a").is_none(),
            "the cloned-vault pin must NOT be written to profile props"
        );
    }

    /// DRIFT: the ledger authorizes an OLD content hash but the served code
    /// changed → NOT re-applied (consent was for the old code).
    #[test]
    fn plan_plugin_reapply_declines_a_drifted_package() {
        let origins = local_origins();
        let mut lock = Lockfile::default();
        lock.upsert(PKG, plugin_lock_entry(&local_pointer()));
        // Ledger consented at "H1"; the live served code now hashes to "H2".
        let (_, inserted) = plan_plugin_reapply(
            &lock,
            &ledger_with("H1"),
            &live_hashes("H2"),
            &json!({}),
            &origins,
        );
        assert_eq!(inserted, 0, "drift since consent → not re-registered");
    }

    /// Finding 5 at the plan level: a REMOTE-host pin (even with a ledger entry)
    /// is never re-applied — only host-local pointers are ours to resurrect.
    #[test]
    fn plan_plugin_reapply_ignores_a_non_local_host_pin() {
        let origins = local_origins();
        let remote = json!({
            "pluginId": "plugin-a",
            "code": format!("/__packages/{PKG}/plugin.js"),
            "host": "https://evil.example.com",
            "name": "Spoof",
            "permissions": ["content:write"]
        });
        let mut lock = Lockfile::default();
        lock.upsert(PKG, plugin_lock_entry(&remote));
        let (_, inserted) = plan_plugin_reapply(
            &lock,
            &ledger_with("H"),
            &live_hashes("H"),
            &json!({}),
            &origins,
        );
        assert_eq!(inserted, 0, "non-local host pin is never re-applied");
    }

    /// The wipe → reboot round trip at the planning level: a captured pin, once
    /// ledger-authorized, re-applies over empty props to the exact pointer, and
    /// a second plan is a no-op (idempotent, run-twice discipline).
    #[test]
    fn plugin_reapply_is_idempotent_over_the_wipe_round_trip() {
        let origins = local_origins();
        let props = spike_props(true, false);
        let pins = local_plugin_pointers(&props, &origins);
        let mut lock = Lockfile::default();
        let pointer: serde_json::Value =
            serde_json::from_str(&pins[PKG]["plugin-a"]).unwrap();
        lock.upsert(PKG, plugin_lock_entry(&pointer));

        let (merged, inserted) =
            plan_plugin_reapply(&lock, &ledger_with("H"), &live_hashes("H"), &json!({}), &origins);
        assert_eq!(inserted, 1);
        let restored = json!({ "plugins": merged });
        assert_eq!(
            local_plugin_pointers(&restored, &origins),
            pins,
            "pin round-trips exactly"
        );
        let (_, inserted_again) =
            plan_plugin_reapply(&lock, &ledger_with("H"), &live_hashes("H"), &restored, &origins);
        assert_eq!(inserted_again, 0, "second pass is a no-op");
    }
}
