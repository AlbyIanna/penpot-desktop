//! N5 — Vaults, plural. The persistent bookkeeping around a switchable design
//! vault, plus the pure decision logic the switch state machine and its
//! crash-recovery path key off. The runtime orchestration (teardown → wipe →
//! reconcile) lives in [`crate::control`]; everything here is data + pure
//! functions so the recovery decision table is unit-testable without a stack.
//!
//! Three files carry the state:
//! - **Vault dotfolder** `<vault-root>/.penpot-vault/vault.json` — a stable
//!   vault id + vault-local settings, living INSIDE the vault so it travels
//!   with a git clone. The disk walker skips any `.`-prefixed dir
//!   (`sync-daemon` `walk_penpot_dirs`), so it is never mistaken for a project
//!   and never synced. Chapter-2 invariant 1 (derived state is disposable)
//!   does NOT apply — this is the vault's identity, not derived state.
//! - **Registry** `<data-dir>/vaults.json` — which vault is currently open
//!   (`active`) and the list of known vaults. Lives in the app data dir
//!   (OUTSIDE any vault).
//! - **Switch marker** `<data-dir>/vault-switch.json` — written BEFORE a
//!   switch wipes the DB, cleared AFTER the target reconciles. Its presence on
//!   boot means a switch was interrupted (SIGKILL mid-swap): the recovery path
//!   completes the switch forward to the target so we never come up in a
//!   half-switched hybrid (invariant 2, P0).
//!
//! The switch itself IS the core invariant (PLAN.md): stop the daemon → wipe
//! the disposable Penpot DB → repoint at the new root → reconcile from the new
//! tree. [`reset_disposable_state`] is that wipe, scoped to the DB cluster, the
//! objects-storage blobs (`<data>/assets`) and the (also disposable) vault
//! index — never the postgres install binaries, so a switch stays offline and
//! fast.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Dotfolder at the vault root holding the identity marker + settings.
pub const VAULT_DOTFOLDER: &str = ".penpot-vault";
/// Identity marker file inside [`VAULT_DOTFOLDER`].
pub const VAULT_MARKER_FILE: &str = "vault.json";
/// Registry file in the app data dir.
pub const REGISTRY_FILE: &str = "vaults.json";
/// Switch-in-progress marker file in the app data dir.
pub const SWITCH_MARKER_FILE: &str = "vault-switch.json";

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A fresh vault id: a UUID-shaped 32-hex string (internal identity, not an
/// RFC-4122 UUID — it only has to be stable + unique across a user's vaults).
pub fn new_vault_id() -> String {
    let mut rng = rand::thread_rng();
    let a: u32 = rng.gen();
    let b: u16 = rng.gen();
    let c: u16 = rng.gen();
    let d: u16 = rng.gen();
    let e: u64 = rng.gen::<u64>() & 0xffff_ffff_ffff;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

/// Absolutize a user-pointed path without requiring it to exist yet (a vault
/// root may be a directory the user is about to create). Falls back to
/// joining the cwd; never fails.
pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Atomic JSON write: serialize → temp sibling → fsync → rename. Same shape as
/// the manifest writer so a crash never leaves a half-written file.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create dir {}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension(format!("tmp-{}", rand::thread_rng().gen::<u32>()));
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// VaultRef — a known vault (stable id + absolute root)
// ---------------------------------------------------------------------------

/// A known vault: its stable id and its absolute root path (string form, so
/// it round-trips cleanly through JSON and matches the manifest's path style).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRef {
    pub id: String,
    pub path: String,
}

impl VaultRef {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }
}

// ---------------------------------------------------------------------------
// Vault dotfolder marker (.penpot-vault/vault.json)
// ---------------------------------------------------------------------------

/// The vault-root identity marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultMarker {
    /// Stable vault id, minted once when the vault is first opened.
    pub id: String,
    /// When the marker was first written.
    pub created_at: String,
    /// Vault-local settings (opaque map — reserved for future per-vault
    /// preferences; travels with the vault).
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(VAULT_DOTFOLDER).join(VAULT_MARKER_FILE)
}

/// Read the identity marker, if the vault has one.
pub fn read_marker(root: &Path) -> Result<Option<VaultMarker>> {
    let path = marker_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let marker: VaultMarker = serde_json::from_str(&text)
        .with_context(|| format!("corrupt vault marker {}", path.display()))?;
    Ok(Some(marker))
}

/// Ensure `root` is a vault: reuse its existing id if the dotfolder marker is
/// present, otherwise mint one and write the marker. Returns the [`VaultRef`]
/// (absolute path + id). The vault root is created if missing (a vault root
/// may be an empty directory the user just pointed at).
pub fn ensure_vault(root: &Path) -> Result<VaultRef> {
    let root = absolutize(root);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("cannot create vault root {}", root.display()))?;
    let path_str = root.to_string_lossy().into_owned();
    if let Some(existing) = read_marker(&root)? {
        return Ok(VaultRef { id: existing.id, path: path_str });
    }
    let marker = VaultMarker {
        id: new_vault_id(),
        created_at: now_rfc3339(),
        settings: serde_json::Map::new(),
    };
    write_json_atomic(&marker_path(&root), &marker)
        .with_context(|| format!("cannot write vault marker under {}", root.display()))?;
    tracing::info!(root = %root.display(), id = %marker.id, "vault: identity marker created");
    Ok(VaultRef { id: marker.id, path: path_str })
}

// ---------------------------------------------------------------------------
// Registry (vaults.json)
// ---------------------------------------------------------------------------

/// The app-data-dir registry: the active vault + the known list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultRegistry {
    /// Absolute path of the currently-open vault (`None` on a fresh install).
    #[serde(default)]
    pub active: Option<String>,
    /// Every vault the user has opened, most-recently-added last.
    #[serde(default)]
    pub vaults: Vec<VaultRef>,
}

fn registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join(REGISTRY_FILE)
}

impl VaultRegistry {
    /// Load the registry, or a default (empty) one if it does not exist yet.
    pub fn load(data_dir: &Path) -> Result<VaultRegistry> {
        let path = registry_path(data_dir);
        if !path.is_file() {
            return Ok(VaultRegistry::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let reg: VaultRegistry = serde_json::from_str(&text)
            .with_context(|| format!("corrupt vault registry {}", path.display()))?;
        Ok(reg)
    }

    /// Atomically persist the registry.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        write_json_atomic(&registry_path(data_dir), self)
    }

    /// Add-or-update a vault by id (path may have changed) and by path (id may
    /// have been minted). Keeps the list deduplicated on both keys.
    pub fn upsert(&mut self, vault: &VaultRef) {
        self.vaults
            .retain(|v| v.id != vault.id && v.path != vault.path);
        self.vaults.push(vault.clone());
    }

    /// Set the active vault (does not add it — call [`upsert`](Self::upsert)
    /// too for a brand-new vault).
    pub fn set_active(&mut self, path: &str) {
        self.active = Some(path.to_string());
    }

    /// The active vault's registry entry, if the active path is known.
    pub fn active_ref(&self) -> Option<&VaultRef> {
        let active = self.active.as_deref()?;
        self.vaults.iter().find(|v| v.path == active)
    }
}

// ---------------------------------------------------------------------------
// Switch marker (vault-switch.json) — crash safety
// ---------------------------------------------------------------------------

/// Written BEFORE a switch wipes the DB, cleared AFTER the target reconciles.
/// Its presence on boot is the sole signal that a switch was interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchMarker {
    /// Absolute path of the vault the switch was moving TO.
    pub target: String,
    /// The target vault's id.
    pub target_id: String,
    /// Absolute path of the vault we were leaving (diagnostics only).
    #[serde(default)]
    pub previous: Option<String>,
    /// The previous vault's id (diagnostics only).
    #[serde(default)]
    pub previous_id: Option<String>,
    /// When the switch began.
    pub started_at: String,
}

fn switch_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SWITCH_MARKER_FILE)
}

/// Read the switch marker, if a switch is in progress / was interrupted.
pub fn read_switch_marker(data_dir: &Path) -> Result<Option<SwitchMarker>> {
    let path = switch_marker_path(data_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let marker: SwitchMarker = serde_json::from_str(&text)
        .with_context(|| format!("corrupt switch marker {}", path.display()))?;
    Ok(Some(marker))
}

/// Write the switch marker (atomic).
pub fn write_switch_marker(data_dir: &Path, marker: &SwitchMarker) -> Result<()> {
    write_json_atomic(&switch_marker_path(data_dir), marker)
}

/// Clear the switch marker (idempotent — a missing file is success).
pub fn clear_switch_marker(data_dir: &Path) -> Result<()> {
    let path = switch_marker_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot clear switch marker {}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Startup mode / recovery decision (pure)
// ---------------------------------------------------------------------------

/// What the boot path should do about vault selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMode {
    /// Normal boot at this vault root — no DB wipe (the existing DB, if any,
    /// belongs to this same vault; reconciliation handles it, exactly as
    /// pre-N5).
    Normal { vault: PathBuf },
    /// A switch was interrupted (marker present). Complete it FORWARD to the
    /// target: wipe the disposable DB + index first, boot the target, clear
    /// the marker after. Forward-completion is deterministic regardless of how
    /// far the interrupted switch got (the wipe always precedes the reconcile,
    /// so the previous vault leaves zero residue).
    RecoverForward { target: PathBuf, target_id: String },
}

/// Decide the startup mode. Pure — the full recovery decision table.
///
/// Precedence:
/// 1. **switch marker present** → recover forward to the marker's target
///    (beats env + registry: an interrupted switch must be completed, never a
///    half-switched hybrid).
/// 2. **`PENPOT_LOCAL_DESIGNS_DIR` explicitly set** → that path (this keeps
///    every pre-N5 script's behavior byte-identical: they always set the env,
///    so they always take this arm and never consult the registry).
/// 3. **registry has an active vault** → that path (the app remembers the last
///    vault across clean restarts).
/// 4. otherwise the resolved default (`<data-dir>/designs`).
pub fn decide_startup(
    marker: Option<&SwitchMarker>,
    config_designs_dir: &Path,
    env_designs_dir_was_set: bool,
    registry_active: Option<&str>,
) -> StartupMode {
    if let Some(m) = marker {
        return StartupMode::RecoverForward {
            target: PathBuf::from(&m.target),
            target_id: m.target_id.clone(),
        };
    }
    if env_designs_dir_was_set {
        return StartupMode::Normal { vault: config_designs_dir.to_path_buf() };
    }
    if let Some(active) = registry_active {
        return StartupMode::Normal { vault: PathBuf::from(active) };
    }
    StartupMode::Normal { vault: config_designs_dir.to_path_buf() }
}

// ---------------------------------------------------------------------------
// Disposable-state reset (the M2 DB-wipe, index reset)
// ---------------------------------------------------------------------------

/// Wipe the disposable Penpot DB cluster, the objects-storage blobs AND the
/// vault index — the M2 "delete the DB, rebuild from disk" reset, as used by a
/// vault switch and its crash recovery. Keeps `<data>/postgres/install` (the
/// binaries) so the reboot is offline and fast; only the cluster
/// (`<data>/postgres/data`), the objects storage (`<data>/assets`) and the
/// index db (`<data>/vault-index/`) go. Idempotent (missing paths are
/// success), so recovery can call it unconditionally.
///
/// `<data>/assets` is Penpot's `fs` objects-storage backend
/// (`PENPOT_OBJECTS_STORAGE_FS_DIRECTORY`, wired in [`crate::AppConfig::storage_dir`]),
/// holding every uploaded media blob. Wiping it is safe because a reconcile
/// re-materializes each referenced blob from the `.penpot` on disk: the switch
/// re-imports every file under its original id via `import-binfile`, and with
/// the DB cluster gone there is no dedup row to suppress the write, so the fs
/// backend re-writes the blob and `GET /assets/by-file-media-id/<id>` serves
/// the exact bytes again (verified empirically for N5, see
/// `docs/milestones/n5.md`). Not wiping it would leave the previous vault's
/// blobs orphaned in the shared cache — never surfaced (their DB rows are gone)
/// but an unbounded-growth / local-first-hygiene leak across vaults.
pub fn reset_disposable_state(data_dir: &Path) -> Result<()> {
    let pg_cluster = data_dir.join("postgres").join("data");
    if pg_cluster.exists() {
        std::fs::remove_dir_all(&pg_cluster)
            .with_context(|| format!("cannot wipe postgres cluster {}", pg_cluster.display()))?;
        tracing::info!(dir = %pg_cluster.display(), "vault switch: wiped Penpot DB cluster");
    }
    // `.pgpass` is regenerated by the supervisor on the next boot; drop the
    // stale one too so a fresh initdb never sees a mismatched credential file.
    let _ = std::fs::remove_file(data_dir.join("postgres").join(".pgpass"));

    // Objects storage (uploaded media blobs). Re-materialized from the .penpot
    // on the next reconcile, so wiping it is safe AND stops the previous
    // vault's blobs lingering in the shared cache.
    let assets = data_dir.join("assets");
    if assets.exists() {
        std::fs::remove_dir_all(&assets)
            .with_context(|| format!("cannot wipe objects storage {}", assets.display()))?;
        tracing::info!(dir = %assets.display(), "vault switch: wiped objects storage (media blobs)");
    }

    let index = data_dir.join("vault-index");
    if index.exists() {
        std::fs::remove_dir_all(&index)
            .with_context(|| format!("cannot wipe vault index {}", index.display()))?;
        tracing::info!(dir = %index.display(), "vault switch: reset the vault index");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_ids_are_unique_and_uuid_shaped() {
        let a = new_vault_id();
        let b = new_vault_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn ensure_vault_mints_then_reuses_the_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("VaultA");
        let first = ensure_vault(&root).unwrap();
        assert!(root.join(VAULT_DOTFOLDER).join(VAULT_MARKER_FILE).is_file());
        // Absolute path recorded.
        assert!(Path::new(&first.path).is_absolute());
        // Re-opening the same root reuses the id (stable identity).
        let second = ensure_vault(&root).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.path, second.path);
        // Two different roots get different ids.
        let other = ensure_vault(&tmp.path().join("VaultB")).unwrap();
        assert_ne!(first.id, other.id);
    }

    #[test]
    fn registry_roundtrips_and_upserts_dedupe() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        assert_eq!(VaultRegistry::load(data).unwrap(), VaultRegistry::default());

        let a = VaultRef { id: "id-a".into(), path: "/vaults/a".into() };
        let b = VaultRef { id: "id-b".into(), path: "/vaults/b".into() };
        let mut reg = VaultRegistry::default();
        reg.upsert(&a);
        reg.upsert(&b);
        reg.set_active(&b.path);
        reg.save(data).unwrap();

        let loaded = VaultRegistry::load(data).unwrap();
        assert_eq!(loaded.vaults.len(), 2);
        assert_eq!(loaded.active.as_deref(), Some("/vaults/b"));
        assert_eq!(loaded.active_ref(), Some(&b));

        // Re-upserting the same id (new path) replaces, does not duplicate.
        let mut reg = loaded;
        reg.upsert(&VaultRef { id: "id-a".into(), path: "/vaults/a-moved".into() });
        assert_eq!(reg.vaults.len(), 2);
        assert!(reg.vaults.iter().any(|v| v.path == "/vaults/a-moved"));
        assert!(!reg.vaults.iter().any(|v| v.path == "/vaults/a"));
    }

    #[test]
    fn switch_marker_write_read_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        assert!(read_switch_marker(data).unwrap().is_none());
        let m = SwitchMarker {
            target: "/vaults/b".into(),
            target_id: "id-b".into(),
            previous: Some("/vaults/a".into()),
            previous_id: Some("id-a".into()),
            started_at: now_rfc3339(),
        };
        write_switch_marker(data, &m).unwrap();
        assert_eq!(read_switch_marker(data).unwrap().as_ref(), Some(&m));
        clear_switch_marker(data).unwrap();
        assert!(read_switch_marker(data).unwrap().is_none());
        // Clearing again is a no-op (idempotent).
        clear_switch_marker(data).unwrap();
    }

    #[test]
    fn startup_decision_table() {
        let cfg = Path::new("/data/designs");
        let marker = SwitchMarker {
            target: "/vaults/b".into(),
            target_id: "id-b".into(),
            previous: None,
            previous_id: None,
            started_at: "t".into(),
        };

        // 1. marker present → recover forward to the target, beating env+registry.
        assert_eq!(
            decide_startup(Some(&marker), cfg, true, Some("/vaults/a")),
            StartupMode::RecoverForward {
                target: PathBuf::from("/vaults/b"),
                target_id: "id-b".into()
            }
        );

        // 2. no marker, env set → env path (registry ignored — pre-N5 behavior).
        assert_eq!(
            decide_startup(None, cfg, true, Some("/vaults/a")),
            StartupMode::Normal { vault: cfg.to_path_buf() }
        );

        // 3. no marker, env unset, registry active → the registry's vault.
        assert_eq!(
            decide_startup(None, cfg, false, Some("/vaults/a")),
            StartupMode::Normal { vault: PathBuf::from("/vaults/a") }
        );

        // 4. no marker, env unset, no registry → the default.
        assert_eq!(
            decide_startup(None, cfg, false, None),
            StartupMode::Normal { vault: cfg.to_path_buf() }
        );
    }

    #[test]
    fn reset_disposable_state_wipes_cluster_assets_and_index_keeps_install() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        std::fs::create_dir_all(data.join("postgres/data/base")).unwrap();
        std::fs::create_dir_all(data.join("postgres/install/bin")).unwrap();
        std::fs::write(data.join("postgres/install/bin/initdb"), b"x").unwrap();
        std::fs::write(data.join("postgres/.pgpass"), b"secret").unwrap();
        // Objects storage: a media blob sharded the way the fs backend lays it out.
        std::fs::create_dir_all(data.join("assets/9b/6d")).unwrap();
        std::fs::write(data.join("assets/9b/6d/4690443a40fc88d6941beb488226"), b"png").unwrap();
        std::fs::create_dir_all(data.join("vault-index")).unwrap();
        std::fs::write(data.join("vault-index/index.sqlite3"), b"db").unwrap();

        reset_disposable_state(data).unwrap();

        assert!(!data.join("postgres/data").exists(), "cluster wiped");
        assert!(!data.join("postgres/.pgpass").exists(), ".pgpass wiped");
        assert!(!data.join("assets").exists(), "objects storage wiped");
        assert!(!data.join("vault-index").exists(), "index wiped");
        // Install binaries survive → the reboot stays offline.
        assert!(data.join("postgres/install/bin/initdb").is_file(), "install kept");

        // Idempotent on already-clean state.
        reset_disposable_state(data).unwrap();
    }
}
