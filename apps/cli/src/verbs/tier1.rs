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

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use app_core::AppConfig;
use vault_ops::vault::{self, StartupMode, VaultRegistry};

/// "The active vault", resolved the exact same way the real boot path does
/// (`vault_ops::vault::decide_startup`), plus whether that answer is a
/// normal registry read or the forward-recovery of an interrupted switch.
///
/// Every verb that needs "the active vault" — INCLUDING `vault_list`'s
/// "which one is marked active" — must go through [`resolve_active_vault`],
/// never `registry.active` directly: per `VaultRunner::switch_to`
/// (`crates/app-core/src/control.rs`), `registry.active` is repointed only
/// PARTWAY through a switch. A `SIGKILL` early in one — which the project's
/// own N5 gate performs on purpose — leaves `registry.active` still naming
/// the vault being LEFT, while the switch marker (and `decide_startup`) name
/// the vault boot will recover FORWARD to. A verb that reads
/// `registry.active` raw would report a different "active" vault than one
/// that goes through the marker-aware precedence, so a `checkpoint` moments
/// after an interrupted switch could commit into a vault `vault list` never
/// told the caller about.
struct ResolvedVault {
    root: PathBuf,
    /// `Some` iff a vault switch was interrupted (the marker was present);
    /// carries the switch marker itself so a caller can tell "A is active"
    /// from "A is active because a switch to B was interrupted" instead of
    /// the two looking identical.
    recovering: Option<vault::SwitchMarker>,
}

/// Resolve the vault a verb should act on. Takes raw paths (not `&AppConfig`)
/// so it is unit-testable without constructing a full boot configuration.
fn resolve_active_vault(data_dir: &Path, designs_dir: &Path) -> Result<ResolvedVault> {
    let marker = vault::read_switch_marker(data_dir)?;
    let registry = VaultRegistry::load(data_dir)?;
    let env_set = std::env::var_os("PENPOT_LOCAL_DESIGNS_DIR").is_some();
    let mode = vault::decide_startup(marker.as_ref(), designs_dir, env_set, registry.active.as_deref());
    Ok(match mode {
        StartupMode::Normal { vault } => ResolvedVault { root: vault, recovering: None },
        // A switch was interrupted (SIGKILL mid-switch). The target is
        // where boot would recover FORWARD to, so it is the best static
        // answer a read-only verb can give — but `recovering` still carries
        // the marker so the caller isn't left thinking this was a normal,
        // settled "active vault" answer.
        StartupMode::RecoverForward { target, .. } => ResolvedVault { root: target, recovering: marker },
    })
}

/// JSON for [`ResolvedVault::recovering`]: `null` when no switch is in
/// progress, else the switch marker's own camelCase shape
/// (`target`/`targetId`/`previous`/`previousId`/`startedAt`) — it already
/// derives `Serialize`, so this is not a second copy of its fields.
fn recovering_json(recovering: &Option<vault::SwitchMarker>) -> Result<Value> {
    Ok(match recovering {
        Some(marker) => serde_json::to_value(marker)?,
        None => Value::Null,
    })
}

fn vault_ref_json(v: &vault::VaultRef) -> Value {
    json!({ "id": v.id, "path": v.path })
}

// ---------------------------------------------------------------------------
// vault list
// ---------------------------------------------------------------------------

/// `vault list` — every known vault (`vaults.json`), with the active one
/// marked. "Active" is resolved through [`resolve_active_vault`] — the SAME
/// precedence `checkpoint`/`open`/`pkg_list`/`sync_status` use — never raw
/// `registry.active`, so this can never disagree with what those verbs act
/// on (see [`ResolvedVault`]'s doc for why that gap had teeth).
pub fn vault_list(config: &AppConfig) -> Result<Value> {
    let registry = VaultRegistry::load(&config.data_dir)?;
    let resolved = resolve_active_vault(&config.data_dir, &config.designs_dir)?;
    let active_path = resolved.root.to_string_lossy().into_owned();
    let vaults: Vec<Value> = registry
        .vaults
        .iter()
        .map(|v| {
            let mut entry = vault_ref_json(v);
            entry["active"] = json!(v.path == active_path);
            entry
        })
        .collect();
    Ok(json!({
        "active": active_path,
        "recovering": recovering_json(&resolved.recovering)?,
        "vaults": vaults,
    }))
}

// ---------------------------------------------------------------------------
// checkpoint
// ---------------------------------------------------------------------------

/// `checkpoint` — commit the active vault's current state (N4b), the same
/// decision table the `/__api/vault/checkpoint` route runs online.
pub fn checkpoint(config: &AppConfig) -> Result<Value> {
    let resolved = resolve_active_vault(&config.data_dir, &config.designs_dir)?;
    let label = vault_ops::checkpoint::default_label();
    let outcome = vault_ops::checkpoint::checkpoint(&resolved.root, &label)?;
    let refused = matches!(outcome.decision, vault_ops::checkpoint::Decision::Refuse(_));
    Ok(json!({
        "vault": resolved.root.to_string_lossy(),
        "recovering": recovering_json(&resolved.recovering)?,
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
    let resolved_vault = resolve_active_vault(&config.data_dir, &config.designs_dir)?;
    let vault_root = &resolved_vault.root;
    let manifest = sync_core::Manifest::load(vault_root)?.unwrap_or_default();
    let raw_path = PathBuf::from(path);
    let resolved = vault_ops::docopen::resolve(&raw_path, vault_root, &manifest);
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
        "recovering": recovering_json(&resolved_vault.recovering)?,
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
    let resolved = resolve_active_vault(&config.data_dir, &config.designs_dir)?;
    let vault_root = &resolved.root;
    let packages_dir = vault_root.join(sync_core::PACKAGES_DIR_NAME);
    let lock = sync_core::Lockfile::load_or_default(vault_root)?;
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
        "recovering": recovering_json(&resolved.recovering)?,
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
    let resolved = resolve_active_vault(&config.data_dir, &config.designs_dir)?;
    let vault_root = &resolved.root;
    let manifest = sync_core::Manifest::load(vault_root)?.unwrap_or_default();
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
        "recovering": recovering_json(&resolved.recovering)?,
        "count": files.len(),
        "files": files,
        "partial": true,
        "missing": ["paused", "lastError", "state", "conflict"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, valid `AppConfig` built by hand (same approach
    /// `app_core::boot`'s own tests use for `supervisor_config_maps_the_exporter_spec`)
    /// rather than `AppConfig::resolve()`, which reads real process env vars —
    /// unsafe to mutate per-test when `cargo test` runs many tests in parallel
    /// in one process.
    fn test_config(data_dir: &Path, designs_dir: &Path, runtime_dir: &Path) -> AppConfig {
        AppConfig {
            data_dir: data_dir.to_path_buf(),
            runtime_dir: runtime_dir.to_path_buf(),
            proxy_port: 0,
            ports: Default::default(),
            java_path: PathBuf::new(),
            valkey_path: PathBuf::new(),
            designs_dir: designs_dir.to_path_buf(),
            postgres_install_dir: None,
            watchdog_bin: None,
            child_path_prepend: Vec::new(),
            exporter: None,
            layout: app_core::layout::resolve_layout(&app_core::layout::EnvOverrides::default(), None),
        }
    }

    fn path_str(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    // -----------------------------------------------------------------
    // resolve_active_vault precedence
    // -----------------------------------------------------------------

    #[test]
    fn resolve_active_vault_follows_the_registrys_active_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let vault_a = path_str(&tmp.path().join("vault-a"));
        let default_dir = tmp.path().join("default-designs");

        let mut registry = VaultRegistry::default();
        registry.upsert(&vault::VaultRef { id: "a".into(), path: vault_a.clone() });
        registry.set_active(&vault_a);
        registry.save(&data_dir).unwrap();

        let resolved = resolve_active_vault(&data_dir, &default_dir).unwrap();
        assert_eq!(path_str(&resolved.root), vault_a);
        assert!(resolved.recovering.is_none());
    }

    #[test]
    fn resolve_active_vault_falls_back_to_the_default_designs_dir_with_no_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let default_dir = tmp.path().join("default-designs");

        // No vaults.json at all (fresh install).
        let resolved = resolve_active_vault(&data_dir, &default_dir).unwrap();
        assert_eq!(resolved.root, default_dir);
        assert!(resolved.recovering.is_none());
    }

    #[test]
    fn resolve_active_vault_recovers_forward_when_a_switch_was_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let vault_a = path_str(&tmp.path().join("vault-a"));
        let vault_b = path_str(&tmp.path().join("vault-b"));

        // Registry still names A — `switch_to` only repoints `active` PARTWAY
        // through the switch, so an interruption before that point leaves it
        // pointing at the vault being LEFT.
        let mut registry = VaultRegistry::default();
        registry.upsert(&vault::VaultRef { id: "a".into(), path: vault_a.clone() });
        registry.upsert(&vault::VaultRef { id: "b".into(), path: vault_b.clone() });
        registry.set_active(&vault_a);
        registry.save(&data_dir).unwrap();

        vault::write_switch_marker(
            &data_dir,
            &vault::SwitchMarker {
                target: vault_b.clone(),
                target_id: "b".into(),
                previous: Some(vault_a.clone()),
                previous_id: Some("a".into()),
                started_at: "2026-07-20T00:00:00Z".into(),
            },
        )
        .unwrap();

        let resolved = resolve_active_vault(&data_dir, Path::new(&vault_a)).unwrap();
        assert_eq!(path_str(&resolved.root), vault_b, "must recover FORWARD to the target, not stay on A");
        let marker = resolved.recovering.expect("an interrupted switch must be surfaced");
        assert_eq!(marker.target, vault_b);
        assert_eq!(marker.previous.as_deref(), Some(vault_a.as_str()));
    }

    // -----------------------------------------------------------------
    // vault_list: normal case + agreement with resolve_active_vault when a
    // switch was interrupted (the regression finding 1 called out)
    // -----------------------------------------------------------------

    #[test]
    fn vault_list_marks_the_registrys_active_entry_in_the_normal_case() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let vault_a = path_str(&tmp.path().join("vault-a"));
        let vault_b = path_str(&tmp.path().join("vault-b"));

        let mut registry = VaultRegistry::default();
        registry.upsert(&vault::VaultRef { id: "a".into(), path: vault_a.clone() });
        registry.upsert(&vault::VaultRef { id: "b".into(), path: vault_b.clone() });
        registry.set_active(&vault_a);
        registry.save(&data_dir).unwrap();

        let config = test_config(&data_dir, Path::new(&vault_a), tmp.path());
        let out = vault_list(&config).unwrap();
        assert_eq!(out["active"], json!(vault_a));
        assert_eq!(out["recovering"], Value::Null);
        let vaults = out["vaults"].as_array().unwrap();
        let a = vaults.iter().find(|v| v["id"] == "a").unwrap();
        let b = vaults.iter().find(|v| v["id"] == "b").unwrap();
        assert_eq!(a["active"], json!(true));
        assert_eq!(b["active"], json!(false));
    }

    /// Finding 1's regression test: construct the interrupted-switch state
    /// (registry still pointing at A, switch marker naming B) and assert
    /// `vault_list` and `resolve_active_vault` (the function `checkpoint`,
    /// `open`, `pkg_list` and `sync_status` all go through) agree on which
    /// vault is active — and that `vault_list` says so explicitly via
    /// `"recovering"` rather than looking like a normal, settled answer.
    #[test]
    fn vault_list_agrees_with_resolve_active_vault_when_a_switch_was_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let vault_a = path_str(&tmp.path().join("vault-a"));
        let vault_b = path_str(&tmp.path().join("vault-b"));

        let mut registry = VaultRegistry::default();
        registry.upsert(&vault::VaultRef { id: "a".into(), path: vault_a.clone() });
        registry.upsert(&vault::VaultRef { id: "b".into(), path: vault_b.clone() });
        registry.set_active(&vault_a); // still A — the switch never got this far
        registry.save(&data_dir).unwrap();

        vault::write_switch_marker(
            &data_dir,
            &vault::SwitchMarker {
                target: vault_b.clone(),
                target_id: "b".into(),
                previous: Some(vault_a.clone()),
                previous_id: Some("a".into()),
                started_at: "2026-07-20T00:00:00Z".into(),
            },
        )
        .unwrap();

        let config = test_config(&data_dir, Path::new(&vault_a), tmp.path());

        let list = vault_list(&config).unwrap();
        let resolved = resolve_active_vault(&data_dir, Path::new(&vault_a)).unwrap();

        // The two code paths must agree: both say B is active, not A.
        assert_eq!(list["active"], json!(vault_b));
        assert_eq!(list["active"].as_str().unwrap(), path_str(&resolved.root));

        // And the interruption must be explicit, not merely "agreed upon".
        assert!(list["recovering"].is_object(), "must surface the interrupted switch, got: {list}");
        assert_eq!(list["recovering"]["target"], json!(vault_b));
        assert_eq!(list["recovering"]["previous"], json!(vault_a));

        // The registry entries themselves: B (the target) is marked active,
        // A (what `registry.active` alone would have said) is not.
        let vaults = list["vaults"].as_array().unwrap();
        let a = vaults.iter().find(|v| v["id"] == "a").unwrap();
        let b = vaults.iter().find(|v| v["id"] == "b").unwrap();
        assert_eq!(a["active"], json!(false), "registry.active alone would wrongly say A");
        assert_eq!(b["active"], json!(true));
    }

    // -----------------------------------------------------------------
    // pkg_list / sync_status: exact partial + missing arrays
    // -----------------------------------------------------------------

    #[test]
    fn pkg_list_is_partial_and_names_exactly_live_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let designs_dir = tmp.path().join("designs");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&designs_dir).unwrap();
        // No lock.json at all — `pkg_list` must still answer (empty list),
        // not fail, and still flag itself partial.
        let config = test_config(&data_dir, &designs_dir, tmp.path());

        let out = pkg_list(&config).unwrap();
        assert_eq!(out["partial"], json!(true));
        assert_eq!(out["missing"], json!(["live"]));
        assert_eq!(out["packages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn sync_status_is_partial_and_names_exactly_the_daemon_only_fields_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let designs_dir = tmp.path().join("designs");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&designs_dir).unwrap();
        // No .penpot-sync.json either — same "must still answer" requirement.
        let config = test_config(&data_dir, &designs_dir, tmp.path());

        let out = sync_status(&config).unwrap();
        assert_eq!(out["partial"], json!(true));
        assert_eq!(out["missing"], json!(["paused", "lastError", "state", "conflict"]));
        assert_eq!(out["files"].as_array().unwrap().len(), 0);
    }
}
