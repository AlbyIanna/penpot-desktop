//! `plugin-consent.json` — the per-DATA-DIR local consent ledger (PLAN3 ch. 3 /
//! E7, adversarial-review finding 1).
//!
//! This file is the **authority for boot re-apply** of an E7 plugin registry
//! pointer. It is deliberately SEPARATE from `lock.json`:
//!
//! - `lock.json` lives at the **vault root**, is git-versioned, and travels with
//!   the vault (E6 portability). It pins a plugin's `plugin_props` for
//!   portability + gallery visibility, but a pin is NO LONGER sufficient to
//!   auto-register a plugin — otherwise opening a cloned/pulled vault would seed
//!   a consented-looking registration with no native Install/Allow ever having
//!   happened on THIS machine (one Open → arbitrary `content:write` JS).
//! - `plugin-consent.json` lives at `<data_dir>/plugin-consent.json` — a sibling
//!   of `postgres/`, OUTSIDE the vault and NOT git-versioned. It survives a DB
//!   wipe (so same-machine wipe-recovery still re-applies) but does NOT travel
//!   with the vault (so a cloned vault has NO ledger → nothing auto-registers).
//!
//! An entry is recorded ONLY when the capture loop observes a genuine
//! native-manager consent on this machine (a local-origin pointer live in the
//! DB that we did not just re-apply). It pins the content hash that was
//! consented, so a later boot re-applies ONLY if the served code still hashes to
//! that value (drift → `driftedNeedsReconsent`, not silent re-registration).
//!
//! Same discipline as [`crate::lock`] / [`crate::manifest`]: versioned schema
//! (unknown/newer version is a hard error), `deny_unknown_fields`, atomic save
//! (tmp + fsync + rename), byte-diffable serialization.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// File name of the consent ledger, at the root of the app's DATA dir (NOT the
/// vault — it must not travel with a cloned vault).
pub const CONSENT_FILE_NAME: &str = "plugin-consent.json";

/// Current schema version written by this crate.
pub const CONSENT_SCHEMA_VERSION: u32 = 1;

/// One plugin's recorded local consent. Keyed by the Penpot-generated
/// `pluginId` (the same key `props.plugins.data` uses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentRecord {
    /// The [`crate`]-side `plugin_content_hash` of the served asset surface at
    /// the moment consent was observed. Boot re-apply re-applies the pointer
    /// ONLY when this equals the CURRENT content hash — otherwise the served
    /// code changed since consent and re-registration would grant consent the
    /// user never gave for the new code (`driftedNeedsReconsent`).
    pub consented_content_hash: String,
    /// The pointer's `host` (the manifest ORIGIN) — recorded so the ledger is
    /// self-describing; only local-origin pointers are ever recorded.
    pub host: String,
    /// The pointer's `code` path (`/__packages/<pkg>/plugin.js`).
    pub code: String,
    /// RFC 3339 UTC timestamp of the first observed consent (preserved across
    /// content-hash refreshes).
    pub consented_at: String,
}

/// The consent-ledger document. Keys of `plugins` are Penpot `pluginId`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentLedger {
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, ConsentRecord>,
}

impl Default for ConsentLedger {
    fn default() -> Self {
        ConsentLedger {
            schema_version: CONSENT_SCHEMA_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

impl ConsentLedger {
    /// Absolute path of the ledger inside `data_dir`.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(CONSENT_FILE_NAME)
    }

    /// Load the ledger from `data_dir`. `Ok(None)` if it does not exist (nothing
    /// consented yet). Errors on unreadable/invalid JSON or an unsupported
    /// schema version.
    pub fn load(data_dir: &Path) -> Result<Option<ConsentLedger>> {
        let path = Self::path_in(data_dir);
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
        if probe.schema_version != CONSENT_SCHEMA_VERSION {
            return Err(Error::ConsentSchema {
                found: probe.schema_version,
                expected: CONSENT_SCHEMA_VERSION,
            });
        }
        let ledger: ConsentLedger = serde_json::from_slice(&raw).map_err(|e| Error::Json {
            path: path.clone(),
            source: e,
        })?;
        Ok(Some(ledger))
    }

    /// Load the ledger, or a fresh empty one if none exists yet.
    pub fn load_or_default(data_dir: &Path) -> Result<ConsentLedger> {
        Ok(Self::load(data_dir)?.unwrap_or_default())
    }

    /// Atomically save to `<data_dir>/plugin-consent.json` (tmp + fsync +
    /// rename), byte-diffable (sorted keys, 2-space indent, LF, trailing
    /// newline) — the same discipline as the lockfile/manifest.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path_in(data_dir);
        // Ensure the data dir exists (the vault may be created before it on a
        // fresh install); a missing parent would fail the atomic rename.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let value = serde_json::to_value(self).expect("consent ledger serializes");
        let mut s = crate::normalize::dumps(&value);
        s.push('\n');
        crate::util::atomic_write(&path, s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(hash: &str) -> ConsentRecord {
        ConsentRecord {
            consented_content_hash: hash.into(),
            host: "http://localhost:9022".into(),
            code: "/__packages/kit/plugin.js".into(),
            consented_at: "2026-07-16T00:00:00Z".into(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = ConsentLedger::default();
        ledger.plugins.insert("plugin-a".into(), record("h-a"));
        ledger.plugins.insert("plugin-b".into(), record("h-b"));
        ledger.save(tmp.path()).unwrap();

        let loaded = ConsentLedger::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, ledger);
        assert_eq!(loaded.schema_version, CONSENT_SCHEMA_VERSION);
        assert_eq!(loaded.plugins.len(), 2);
    }

    #[test]
    fn missing_ledger_loads_none_and_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ConsentLedger::load(tmp.path()).unwrap().is_none());
        let d = ConsentLedger::load_or_default(tmp.path()).unwrap();
        assert!(d.plugins.is_empty());
        assert_eq!(d.schema_version, CONSENT_SCHEMA_VERSION);
    }

    #[test]
    fn serialization_is_sorted_and_lf_terminated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = ConsentLedger::default();
        ledger.plugins.insert("zeta".into(), record("hz"));
        ledger.plugins.insert("alpha".into(), record("ha"));
        ledger.save(tmp.path()).unwrap();
        let raw = std::fs::read_to_string(ConsentLedger::path_in(tmp.path())).unwrap();
        assert!(raw.ends_with("\n"), "trailing newline");
        assert!(
            raw.find("alpha").unwrap() < raw.find("zeta").unwrap(),
            "keys sorted for git-diffability"
        );
    }

    #[test]
    fn save_creates_missing_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("not-created-yet");
        let mut ledger = ConsentLedger::default();
        ledger.plugins.insert("plugin-a".into(), record("h"));
        ledger.save(&data_dir).unwrap();
        assert!(ConsentLedger::load(&data_dir).unwrap().is_some());
    }

    #[test]
    fn unknown_schema_version_is_a_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            ConsentLedger::path_in(tmp.path()),
            br#"{"schemaVersion": 999, "plugins": {}}"#,
        )
        .unwrap();
        match ConsentLedger::load(tmp.path()) {
            Err(Error::ConsentSchema { found: 999, expected }) => {
                assert_eq!(expected, CONSENT_SCHEMA_VERSION);
            }
            other => panic!("expected ConsentSchema error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            ConsentLedger::path_in(tmp.path()),
            br#"{"schemaVersion": 1, "plugins": {}, "mystery": true}"#,
        )
        .unwrap();
        assert!(
            matches!(ConsentLedger::load(tmp.path()), Err(Error::Json { .. })),
            "deny_unknown_fields must reject a forward-schema file"
        );
    }
}
