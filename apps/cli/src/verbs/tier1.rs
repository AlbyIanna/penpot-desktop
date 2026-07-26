//! Tier 1 — the spike's "pure offline" verbs: read the vault tree and the
//! app data dir with no app and no backend running (`docs/spikes/cli-verbs.md`).
//!
//! Every function here is a `pub fn(&AppConfig, ...) -> anyhow::Result<Value>`
//! so `main.rs` can print the JSON on stdout uniformly and put human text on
//! stderr — the Task 12 gate asserts on stdout without parsing prose.
//!
//! Two verbs (`pkg_list`, `sync_status`) have real but INCOMPLETE offline
//! answers: each emits `"partial": true` plus a `"missing"` array naming the
//! fields only the running daemon/backend can supply. An offline answer that
//! looks complete is worse than no answer — the caller could not otherwise
//! tell it was reading a subset.
//!
//! Nothing here constructs `packages::packages::PackagesState` or otherwise
//! runs any reconciler in-process: the spike (RC-7) proved that a second
//! process doing that beside the running app is an undetected concurrent
//! writer to `lock.json` and the consent ledger. These functions only ever
//! read.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

use app_core::AppConfig;
use vault_ops::vault::{self, StartupMode, VaultRegistry};

/// Resolve the vault a verb should act on, using the exact same precedence
/// the real boot path uses (`vault_ops::vault::decide_startup`), so an
/// offline answer never disagrees with what a live boot would pick. Reusing
/// the existing pure decision table rather than re-deriving "which vault is
/// active" here is exactly the kind of duplication chapter C exists to
/// remove — see the task brief's instruction not to duplicate a rule about
/// the user's vault.
fn active_vault_root(config: &AppConfig) -> Result<PathBuf> {
    let marker = vault::read_switch_marker(&config.data_dir)?;
    let registry = VaultRegistry::load(&config.data_dir)?;
    let env_set = std::env::var_os("PENPOT_LOCAL_DESIGNS_DIR").is_some();
    let mode = vault::decide_startup(
        marker.as_ref(),
        &config.designs_dir,
        env_set,
        registry.active.as_deref(),
    );
    Ok(match mode {
        StartupMode::Normal { vault } => vault,
        // A switch was interrupted (SIGKILL mid-switch, e.g. the project's
        // own N5 gate). The target is where boot would recover FORWARD to,
        // so it is the best static answer a read-only verb can give.
        StartupMode::RecoverForward { target, .. } => target,
    })
}

fn vault_ref_json(v: &vault::VaultRef) -> Value {
    json!({ "id": v.id, "path": v.path })
}

// ---------------------------------------------------------------------------
// vault list
// ---------------------------------------------------------------------------

/// `vault list` — every known vault (`vaults.json`), with the active one
/// marked.
pub fn vault_list(config: &AppConfig) -> Result<Value> {
    let registry = VaultRegistry::load(&config.data_dir)?;
    let vaults: Vec<Value> = registry
        .vaults
        .iter()
        .map(|v| {
            let mut entry = vault_ref_json(v);
            entry["active"] = json!(registry.active.as_deref() == Some(v.path.as_str()));
            entry
        })
        .collect();
    Ok(json!({
        "active": registry.active,
        "vaults": vaults,
    }))
}

// ---------------------------------------------------------------------------
// checkpoint
// ---------------------------------------------------------------------------

/// `checkpoint` — commit the active vault's current state (N4b), the same
/// decision table the `/__api/vault/checkpoint` route runs online.
pub fn checkpoint(config: &AppConfig) -> Result<Value> {
    let vault_root = active_vault_root(config)?;
    let label = vault_ops::checkpoint::default_label();
    let outcome = vault_ops::checkpoint::checkpoint(&vault_root, &label)?;
    let refused = matches!(outcome.decision, vault_ops::checkpoint::Decision::Refuse(_));
    Ok(json!({
        "vault": vault_root.to_string_lossy(),
        "ok": !refused,
        "decision": outcome.decision.as_str(),
        "commit": outcome.commit,
        "message": outcome.message,
    }))
}

// ---------------------------------------------------------------------------
// open <path> — resolution only
// ---------------------------------------------------------------------------

/// `open <path>` — resolve what the path would open, without opening,
/// importing, or writing anything.
pub fn open(config: &AppConfig, path: &str) -> Result<Value> {
    let vault_root = active_vault_root(config)?;
    let manifest = sync_core::Manifest::load(&vault_root)?.unwrap_or_default();
    let raw_path = PathBuf::from(path);
    let resolved = vault_ops::docopen::resolve(&raw_path, &vault_root, &manifest);
    let resolved_json = match resolved {
        vault_ops::docopen::Resolved::InVault { file_id, title } => {
            json!({ "kind": "in_vault", "fileId": file_id, "title": title })
        }
        vault_ops::docopen::Resolved::PendingImport { rel_path } => {
            json!({ "kind": "pending_import", "relPath": rel_path })
        }
        vault_ops::docopen::Resolved::External { path } => {
            json!({ "kind": "external", "path": path.to_string_lossy() })
        }
        vault_ops::docopen::Resolved::ImportableBinfile { path, title } => {
            json!({
                "kind": "importable_binfile",
                "path": path.to_string_lossy(),
                "title": title,
            })
        }
        vault_ops::docopen::Resolved::NotAPenpot { reason } => {
            json!({ "kind": "not_a_penpot", "reason": reason })
        }
    };
    Ok(json!({
        "vault": vault_root.to_string_lossy(),
        "resolved": resolved_json,
    }))
}

// ---------------------------------------------------------------------------
// tpl list
// ---------------------------------------------------------------------------

/// `tpl list` — the offline builtin-template catalog (N6), enumerated from
/// the runtime bundle's `backend/builtin-templates/`.
pub fn tpl_list(config: &AppConfig) -> Result<Value> {
    let builtin_dir = config.runtime_dir.join(packages::templates::BUILTIN_TEMPLATES_REL);
    let catalog = packages::templates::enumerate_catalog(&builtin_dir);
    let dropped: Vec<Value> = catalog
        .dropped
        .iter()
        .map(|d| json!({ "id": d.id, "reason": d.reason }))
        .collect();
    Ok(json!({
        "count": catalog.entries.len(),
        "templates": catalog.entries,
        "dropped": dropped,
    }))
}

// ---------------------------------------------------------------------------
// prefs get
// ---------------------------------------------------------------------------

/// `prefs get` — the persisted Preferences file (D4). `prefs::load` never
/// fails: a missing or corrupt file degrades to defaults, same posture here.
pub fn prefs_get(config: &AppConfig) -> Result<Value> {
    let prefs = app_state::prefs::load(&config.data_dir);
    Ok(serde_json::to_value(prefs)?)
}

// ---------------------------------------------------------------------------
// pkg list — partial (no `live`)
// ---------------------------------------------------------------------------

/// `pkg list` — every package `lock.json` pins, enriched with each package's
/// CURRENT on-disk `package.json` (`packages::packages::read_manifest`) so
/// drift since install (e.g. a `git pull` on the package source with no
/// reinstall) is visible offline. Cannot answer `live` — whether the pinned
/// `fileId` is still present in the DB, the DB-wipe re-apply witness — since
/// that needs a live RPC to the backend; flagged explicitly rather than
/// omitted silently.
pub fn pkg_list(config: &AppConfig) -> Result<Value> {
    let vault_root = active_vault_root(config)?;
    let packages_dir = vault_root.join(sync_core::PACKAGES_DIR_NAME);
    let lock = sync_core::Lockfile::load_or_default(&vault_root)?;
    let packages: Vec<Value> = lock
        .packages
        .iter()
        .map(|(id, e)| {
            let on_disk = packages::packages::read_manifest(&packages_dir.join(id));
            json!({
                "id": id,
                "version": e.version,
                "kind": e.kind,
                "name": e.name,
                "fileId": e.file_id,
                "contentHash": e.content_hash,
                "contractHash": e.contract_hash,
                "sourceGitUrl": e.source_git_url,
                "libraryShared": e.library_shared,
                "links": e.links.iter().map(|l| json!({
                    "libraryFileId": l.library_file_id,
                    "libraryPackageId": l.library_package_id,
                    "version": l.version,
                    "contractHash": l.contract_hash,
                })).collect::<Vec<_>>(),
                "currentOnDisk": {
                    "name": on_disk.name,
                    "version": on_disk.version,
                    "kind": on_disk.kind,
                },
            })
        })
        .collect();
    Ok(json!({
        "vault": vault_root.to_string_lossy(),
        "count": packages.len(),
        "packages": packages,
        "partial": true,
        "missing": ["live"],
    }))
}

// ---------------------------------------------------------------------------
// sync status — partial (no paused/lastError/per-file state|conflict)
// ---------------------------------------------------------------------------

/// `sync status` — the manifest's per-file `lastSyncedHash`/`lastSyncedAt`
/// (`.penpot-sync.json`, `sync_core::manifest`). Cannot answer `paused`,
/// `lastError`, or the per-file `state`/`conflict` classification
/// (`sync_daemon::SyncStatusSnapshot`/`FileState`) — those live only in the
/// running daemon's in-memory snapshot; flagged explicitly.
pub fn sync_status(config: &AppConfig) -> Result<Value> {
    let vault_root = active_vault_root(config)?;
    let manifest = sync_core::Manifest::load(&vault_root)?.unwrap_or_default();
    let files: Vec<Value> = manifest
        .files
        .iter()
        .map(|(file_id, e)| {
            json!({
                "fileId": file_id,
                "path": e.path,
                "projectId": e.project_id,
                "projectName": e.project_name,
                "lastSyncedHash": e.last_synced_hash,
                "lastSyncedAt": e.last_synced_at,
            })
        })
        .collect();
    Ok(json!({
        "vault": vault_root.to_string_lossy(),
        "count": files.len(),
        "files": files,
        "partial": true,
        "missing": ["paused", "lastError", "state", "conflict"],
    }))
}
