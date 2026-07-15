//! `lock.json` — the per-vault package lockfile (PLAN3 chapter 3 / E2).
//!
//! A git-diffable record, at the vault root, pinning each installed package to
//! the provenance a consumer needs to reason about it and the DB-only pointers
//! it must re-derive after a database wipe. It is a near-clone of
//! [`crate::manifest`]'s discipline:
//!
//! - Versioned schema (`schemaVersion`); loading a newer/unknown version is a
//!   hard error ([`Error::LockSchema`]), never a silent reinterpretation.
//! - Unknown fields are rejected (`deny_unknown_fields`) so a forward-schema
//!   file can never be half-read.
//! - Atomic save: write `lock.json.tmp-<rand>`, fsync, rename — via the same
//!   [`crate::util::atomic_write`] the manifest uses.
//! - Byte-diffable serialization: sorted keys, 2-space indent, LF, trailing
//!   newline (the shared [`crate::normalize::dumps`]).
//!
//! ## What an entry pins
//!
//! Keyed by package **id** (the `.penpot-packages/<id>` directory name):
//!
//! - `version` — the package's declared version (`package.json`, else `0.0.0`).
//! - `kind` — informational package type (`component-library`, `template`, …).
//! - `contentHash` — [`crate::semantic_tree_hash`] of the package's `.penpot`
//!   source tree under `.penpot-packages/<id>`. Drives run-twice idempotency.
//! - `contractHash` — a hash of E1's `extract_contracts` over the package,
//!   file-id excluded, so it is stable across the import id churn (E3 hangs the
//!   update channel off it).
//! - `sourceGitUrl` — where `git clone`/`fetch` pulled the repo from (empty for
//!   a dropped-in package).
//! - `fileId` — the vault-local Penpot id the package materialized as. Because
//!   install lands the package as an ORDINARY vault `.penpot` file, a DB wipe
//!   rebuilds it by the proven M2 resurrect-by-id reconcile; this pins which
//!   id belongs to which package so the re-apply is verifiable.
//! - The DB-only pointers a design-data package cannot carry in its tree:
//!   `libraryShared` (E3 `set-file-shared`) and `pluginProps` (E7 registry
//!   pointer). Empty for an E2 design-data install — the file existence itself
//!   is re-derived by M2 — but modelled now so the lockfile is the stable spine
//!   every later package type hangs off.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// File name of the lockfile, at the root of the user's vault.
pub const LOCK_FILE_NAME: &str = "lock.json";

/// Current schema version written by this crate.
pub const LOCK_SCHEMA_VERSION: u32 = 1;

/// One library this consumer file links (E3). The consumer references the
/// library's components by the library's **vault-local file-id**
/// (`library_file_id`); the DB-side `file_library_rel` is derived/disposable and
/// re-established from this record after a DB wipe via `link-file-to-library`.
///
/// Added WITHOUT a schema bump: it is a `#[serde(default)]` field on
/// [`LockEntry`], so an E2 lockfile with no `links` loads forward-compatibly as
/// an empty list (like `library_shared`/`plugin_props`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LibraryLink {
    /// The library's vault-local Penpot file id (the `componentFile` value the
    /// consumer's instances carry, and the `library-id` for
    /// `link-file-to-library`). The durable pointer, stable across a DB-wipe
    /// rebuild (M2 resurrect-by-id).
    pub library_file_id: String,
    /// The package id (`.penpot-packages/<id>`) the library was installed from —
    /// the update channel (E1 contract diff) is keyed off this package's entry.
    pub library_package_id: String,
    /// The library package version pinned at link time (`library@version`).
    pub version: String,
    /// E1 `extract_contracts` hash of the library at link time. Drift is surfaced
    /// as a CONTRACT diff (E1 patch/minor/major), never `revn`.
    pub contract_hash: String,
}

/// One installed package's pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockEntry {
    /// The package's declared version (`package.json`), else `0.0.0`.
    pub version: String,
    /// Informational package type (`component-library`, `template`, …).
    pub kind: String,
    /// [`crate::semantic_tree_hash`] of the package's `.penpot` source tree.
    pub content_hash: String,
    /// Hash of E1's `extract_contracts` over the package (file-id excluded, so
    /// stable across import churn).
    pub contract_hash: String,
    /// Where the repo was cloned/fetched from (empty for a dropped-in package).
    pub source_git_url: String,
    /// The vault-local Penpot file id the package materialized as (the durable
    /// pointer M2 preserves across a DB-wipe rebuild).
    pub file_id: String,
    /// Display name the package was imported under.
    #[serde(default)]
    pub name: String,
    /// RFC 3339 UTC timestamp of the install.
    pub installed_at: String,
    /// DB-only pointer: whether the materialized file is published shared
    /// (E3 `set-file-shared`). `false` for an E2 design-data install.
    #[serde(default)]
    pub library_shared: bool,
    /// DB-only pointer: the plugin registry profile-props (E7). Empty for E2.
    #[serde(default)]
    pub plugin_props: BTreeMap<String, String>,
    /// E3: the libraries this file links (references components from). Empty for
    /// a plain design-data or library package. The DB-side `file_library_rel`
    /// each entry maps to is derived/disposable — re-established from these
    /// records after a DB wipe via `link-file-to-library` (see [`plan_relink`]).
    #[serde(default)]
    pub links: Vec<LibraryLink>,
}

/// The lockfile document. Keys of `packages` are package ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lockfile {
    pub schema_version: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LockEntry>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            schema_version: LOCK_SCHEMA_VERSION,
            packages: BTreeMap::new(),
        }
    }
}

impl Lockfile {
    /// Absolute path of the lockfile inside `vault_root`.
    pub fn path_in(vault_root: &Path) -> PathBuf {
        vault_root.join(LOCK_FILE_NAME)
    }

    /// Load the lockfile from `vault_root`. `Ok(None)` if it does not exist (no
    /// package installed yet). Errors on unreadable/invalid JSON or an
    /// unsupported schema version.
    pub fn load(vault_root: &Path) -> Result<Option<Lockfile>> {
        let path = Self::path_in(vault_root);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(&path, e)),
        };
        // Peek the schema version first so a future schema fails with the
        // dedicated error, not a serde "unknown field" message.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct VersionProbe {
            schema_version: u32,
        }
        let probe: VersionProbe = serde_json::from_slice(&raw).map_err(|e| Error::Json {
            path: path.clone(),
            source: e,
        })?;
        if probe.schema_version != LOCK_SCHEMA_VERSION {
            return Err(Error::LockSchema {
                found: probe.schema_version,
                expected: LOCK_SCHEMA_VERSION,
            });
        }
        let lock: Lockfile = serde_json::from_slice(&raw).map_err(|e| Error::Json {
            path: path.clone(),
            source: e,
        })?;
        Ok(Some(lock))
    }

    /// Load the lockfile, or a fresh empty one if none exists yet.
    pub fn load_or_default(vault_root: &Path) -> Result<Lockfile> {
        Ok(Self::load(vault_root)?.unwrap_or_default())
    }

    /// Atomically save to `<vault_root>/lock.json` (tmp + fsync + rename). The
    /// serialized form uses the same normalization rules as everything else we
    /// write (sorted keys, 2-space indent, LF, trailing newline) so the
    /// lockfile is git-diffable.
    pub fn save(&self, vault_root: &Path) -> Result<()> {
        let path = Self::path_in(vault_root);
        let value = serde_json::to_value(self).expect("lockfile serializes");
        let mut s = crate::normalize::dumps(&value);
        s.push('\n');
        crate::util::atomic_write(&path, s.as_bytes())
    }

    /// Insert or replace a package's pin.
    pub fn upsert(&mut self, id: impl Into<String>, entry: LockEntry) {
        self.packages.insert(id.into(), entry);
    }
}

/// The re-apply decision for one locked package after a DB wipe. Install lands
/// a design-data package as an ordinary vault `.penpot` file, so the M2 startup
/// reconcile resurrects it by id from the sync manifest; this decides, per
/// entry, whether that already happened or the file still needs restoring from
/// the package source (`.penpot-packages/<id>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapplyAction {
    /// The package's `fileId` is live in the DB — M2 resurrected it by id
    /// (invariant 1); nothing to re-import.
    AlreadyResurrected { id: String, file_id: String },
    /// The package's `fileId` is absent (its vault file was removed) — it would
    /// be re-imported in place from the package source under the SAME id.
    NeedsReimport { id: String, file_id: String },
}

impl ReapplyAction {
    pub fn id(&self) -> &str {
        match self {
            ReapplyAction::AlreadyResurrected { id, .. }
            | ReapplyAction::NeedsReimport { id, .. } => id,
        }
    }
}

/// Decide, per locked package, whether the DB-wipe rebuild already restored it
/// (its `fileId` is among `present_file_ids`, resurrected by M2) or it still
/// needs re-importing from the package source. Pure and deterministic (sorted
/// by package id) so it is unit-testable without a live stack.
pub fn plan_reapply(lock: &Lockfile, present_file_ids: &BTreeSet<String>) -> Vec<ReapplyAction> {
    lock.packages
        .iter()
        .map(|(id, e)| {
            if present_file_ids.contains(&e.file_id) {
                ReapplyAction::AlreadyResurrected {
                    id: id.clone(),
                    file_id: e.file_id.clone(),
                }
            } else {
                ReapplyAction::NeedsReimport {
                    id: id.clone(),
                    file_id: e.file_id.clone(),
                }
            }
        })
        .collect()
}

/// One library link to re-establish after a DB wipe (E3). Carries the ids the
/// boot-time re-link reconcile feeds to `link-file-to-library {fileId:
/// consumer_file_id, libraryId: library_file_id}` (idempotent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelinkOp {
    /// The consumer file whose instances reference the library.
    pub consumer_file_id: String,
    /// The library file to re-link the consumer to.
    pub library_file_id: String,
    /// The library's package id (for logging / update-channel correlation).
    pub library_package_id: String,
}

/// The re-link decision for one lockfile link after a DB wipe. The DB-side
/// `file_library_rel` is disposable (it does NOT ride the binfile), so on
/// rebuild every link is re-derived by re-running the idempotent
/// `link-file-to-library`. This splits, per link, whether both endpoints are
/// live yet (M2 resurrected them by id → ready to re-link) or an endpoint is
/// still absent (blocked — its file must be resurrected first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelinkAction {
    /// Both consumer and library files are live in the DB — re-run
    /// `link-file-to-library` now (idempotent).
    Ready(RelinkOp),
    /// One endpoint (consumer or library) is not yet present — cannot re-link
    /// until it is resurrected.
    Blocked(RelinkOp),
}

impl RelinkAction {
    pub fn op(&self) -> &RelinkOp {
        match self {
            RelinkAction::Ready(op) | RelinkAction::Blocked(op) => op,
        }
    }
}

/// Decide, per lockfile link, whether it can be re-established now (both the
/// consumer and library file-ids are among `present_file_ids`, resurrected by
/// M2) or is blocked on a still-absent endpoint. Pure and deterministic
/// (packages sorted by id, links in entry order) so it is unit-testable without
/// a live stack. Mirrors [`plan_reapply`].
pub fn plan_relink(lock: &Lockfile, present_file_ids: &BTreeSet<String>) -> Vec<RelinkAction> {
    let mut out = Vec::new();
    for entry in lock.packages.values() {
        for link in &entry.links {
            let op = RelinkOp {
                consumer_file_id: entry.file_id.clone(),
                library_file_id: link.library_file_id.clone(),
                library_package_id: link.library_package_id.clone(),
            };
            if present_file_ids.contains(&entry.file_id)
                && present_file_ids.contains(&link.library_file_id)
            {
                out.push(RelinkAction::Ready(op));
            } else {
                out.push(RelinkAction::Blocked(op));
            }
        }
    }
    out
}

/// Current time as an RFC 3339 UTC string (delegates to the manifest helper).
pub fn now_rfc3339() -> String {
    crate::manifest::now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file_id: &str, content: &str) -> LockEntry {
        LockEntry {
            version: "1.0.0".into(),
            kind: "component-library".into(),
            content_hash: content.into(),
            contract_hash: "ch".into(),
            source_git_url: "file:///tmp/repo".into(),
            file_id: file_id.into(),
            name: "Button Library".into(),
            installed_at: "2026-07-15T00:00:00Z".into(),
            library_shared: false,
            plugin_props: BTreeMap::new(),
            links: Vec::new(),
        }
    }

    fn link(library_file_id: &str, package_id: &str) -> LibraryLink {
        LibraryLink {
            library_file_id: library_file_id.into(),
            library_package_id: package_id.into(),
            version: "1.0.0".into(),
            contract_hash: "lch".into(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lockfile::default();
        lock.upsert("buttons", entry("file-a", "hash-a"));
        lock.upsert("icons", entry("file-b", "hash-b"));
        lock.save(tmp.path()).unwrap();

        let loaded = Lockfile::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, lock);
        assert_eq!(loaded.schema_version, LOCK_SCHEMA_VERSION);
        assert_eq!(loaded.packages.len(), 2);
    }

    #[test]
    fn missing_lockfile_loads_none_and_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Lockfile::load(tmp.path()).unwrap().is_none());
        let d = Lockfile::load_or_default(tmp.path()).unwrap();
        assert!(d.packages.is_empty());
        assert_eq!(d.schema_version, LOCK_SCHEMA_VERSION);
    }

    #[test]
    fn serialization_is_sorted_and_lf_terminated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lockfile::default();
        // Insert out of order; BTreeMap + sorted dumps must byte-normalize it.
        lock.upsert("zeta", entry("fz", "hz"));
        lock.upsert("alpha", entry("fa", "ha"));
        lock.save(tmp.path()).unwrap();
        let raw = std::fs::read_to_string(Lockfile::path_in(tmp.path())).unwrap();
        assert!(raw.ends_with("\n"), "trailing newline");
        assert!(
            raw.find("alpha").unwrap() < raw.find("zeta").unwrap(),
            "keys sorted for git-diffability"
        );
        // 2-space indent (normalize::dumps).
        assert!(raw.contains("\n  \"packages\""), "2-space indent");
    }

    #[test]
    fn unknown_schema_version_is_a_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            Lockfile::path_in(tmp.path()),
            br#"{"schemaVersion": 999, "packages": {}}"#,
        )
        .unwrap();
        match Lockfile::load(tmp.path()) {
            Err(Error::LockSchema { found: 999, expected }) => {
                assert_eq!(expected, LOCK_SCHEMA_VERSION);
            }
            other => panic!("expected LockSchema error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            Lockfile::path_in(tmp.path()),
            br#"{"schemaVersion": 1, "packages": {}, "mystery": true}"#,
        )
        .unwrap();
        assert!(
            matches!(Lockfile::load(tmp.path()), Err(Error::Json { .. })),
            "deny_unknown_fields must reject a forward-schema file"
        );
    }

    #[test]
    fn plan_reapply_splits_resurrected_from_missing() {
        let mut lock = Lockfile::default();
        lock.upsert("buttons", entry("file-a", "ha"));
        lock.upsert("icons", entry("file-b", "hb"));
        // Only file-a survived the wipe rebuild (M2 resurrected it by id).
        let present: BTreeSet<String> = ["file-a".to_string()].into_iter().collect();
        let plan = plan_reapply(&lock, &present);
        assert_eq!(
            plan,
            vec![
                ReapplyAction::AlreadyResurrected {
                    id: "buttons".into(),
                    file_id: "file-a".into()
                },
                ReapplyAction::NeedsReimport {
                    id: "icons".into(),
                    file_id: "file-b".into()
                },
            ]
        );
    }

    #[test]
    fn plan_reapply_all_present_is_all_resurrected() {
        let mut lock = Lockfile::default();
        lock.upsert("buttons", entry("file-a", "ha"));
        let present: BTreeSet<String> = ["file-a".to_string()].into_iter().collect();
        let plan = plan_reapply(&lock, &present);
        assert!(matches!(plan[0], ReapplyAction::AlreadyResurrected { .. }));
    }

    /// An E3 lockfile with `links` round-trips through disk unchanged.
    #[test]
    fn links_round_trip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lockfile::default();
        let mut consumer = entry("consumer-file", "hc");
        consumer.links.push(link("library-file", "button-library"));
        lock.upsert("app-screens", consumer);
        lock.upsert("button-library", entry("library-file", "hl"));
        lock.save(tmp.path()).unwrap();

        let loaded = Lockfile::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, lock);
        assert_eq!(loaded.packages["app-screens"].links.len(), 1);
        assert_eq!(
            loaded.packages["app-screens"].links[0].library_file_id,
            "library-file"
        );
    }

    /// An E2 lockfile written BEFORE E3 (no `links` field anywhere) still loads —
    /// `links` is `#[serde(default)]`, so forward-compat load yields an empty
    /// list, never an "unknown field" or "missing field" error.
    #[test]
    fn e2_lockfile_without_links_loads_forward_compatibly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            Lockfile::path_in(tmp.path()),
            br#"{
              "schemaVersion": 1,
              "packages": {
                "buttons": {
                  "version": "1.0.0",
                  "kind": "component-library",
                  "contentHash": "h",
                  "contractHash": "ch",
                  "sourceGitUrl": "",
                  "fileId": "file-a",
                  "name": "Buttons",
                  "installedAt": "2026-07-15T00:00:00Z"
                }
              }
            }"#,
        )
        .unwrap();
        let loaded = Lockfile::load(tmp.path()).unwrap().unwrap();
        assert!(loaded.packages["buttons"].links.is_empty());
        assert!(!loaded.packages["buttons"].library_shared);
    }

    #[test]
    fn plan_relink_splits_ready_from_blocked() {
        let mut lock = Lockfile::default();
        // Consumer links a library; library also has an entry of its own.
        let mut consumer = entry("consumer-file", "hc");
        consumer.links.push(link("library-file", "button-library"));
        lock.upsert("app-screens", consumer);
        lock.upsert("button-library", entry("library-file", "hl"));
        // A second consumer whose library is NOT resurrected yet.
        let mut other = entry("other-consumer", "ho");
        other.links.push(link("absent-library", "icons"));
        lock.upsert("other-screens", other);

        // Only the first consumer + its library are live.
        let present: BTreeSet<String> = ["consumer-file", "library-file", "other-consumer"]
            .into_iter()
            .map(String::from)
            .collect();
        let plan = plan_relink(&lock, &present);
        // Packages sorted by id: app-screens, button-library, other-screens.
        // button-library has no links → contributes nothing.
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            RelinkAction::Ready(RelinkOp {
                consumer_file_id: "consumer-file".into(),
                library_file_id: "library-file".into(),
                library_package_id: "button-library".into(),
            })
        );
        assert_eq!(
            plan[1],
            RelinkAction::Blocked(RelinkOp {
                consumer_file_id: "other-consumer".into(),
                library_file_id: "absent-library".into(),
                library_package_id: "icons".into(),
            })
        );
    }

    #[test]
    fn plan_relink_empty_when_no_links() {
        let mut lock = Lockfile::default();
        lock.upsert("buttons", entry("file-a", "ha"));
        let present: BTreeSet<String> = ["file-a".to_string()].into_iter().collect();
        assert!(plan_relink(&lock, &present).is_empty());
    }
}
