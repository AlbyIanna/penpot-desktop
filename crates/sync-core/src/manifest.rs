//! `.penpot-sync.json` — the manifest at the sync root mapping
//! fileId ↔ relative path ↔ project ↔ revn ↔ lastSyncedHash ↔ lastSyncedAt.
//!
//! - Versioned schema (`schemaVersion`); loading a newer/unknown version is a
//!   hard error, never a silent reinterpretation.
//! - Atomic save: write `.penpot-sync.json.tmp-<rand>`, fsync, rename.
//!   (Stale tmp files are swept by [`crate::swap::cleanup_orphans`].)
//! - `revn` is stored for diagnostics only — it is advisory (M0: in-place
//!   import resets it; stale revn is accepted by the server). Conflict
//!   detection must use `lastSyncedHash`, never revn.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// File name of the manifest, at the root of the user's sync folder.
pub const MANIFEST_FILE_NAME: &str = ".penpot-sync.json";

/// Current schema version written by this crate.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// One synced Penpot file (an unzipped binfile-v3 directory on disk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestEntry {
    /// Path of the `<name>.penpot/` directory, relative to the sync root,
    /// always with `/` separators (e.g. `client-x/homepage.penpot`).
    pub path: String,
    /// Penpot project UUID this file belongs to.
    pub project_id: String,
    /// Project name (folder name on disk mirrors it).
    pub project_name: String,
    /// Last `revn` observed in the DB when this entry was synced. Advisory
    /// only — never a conflict signal (see module docs).
    pub revn: i64,
    /// DB `modifiedAt` observed at last sync. Advisory like `revn` — the
    /// pair `(revn, dbModifiedAt)` is the cheap "did the DB move since we
    /// last synced" hint Direction B checks before an in-place import; the
    /// conflict *decision* still keys off `lastSyncedHash`. Empty string =
    /// unknown (entry written by an M2 daemon) → fall back to revn alone.
    #[serde(default)]
    pub db_modified_at: String,
    /// Semantic tree hash (see [`crate::hash::semantic_tree_hash`]) of the
    /// on-disk directory at last successful sync. The no-op detector and the
    /// conflict rule both key off this.
    pub last_synced_hash: String,
    /// RFC 3339 UTC timestamp of the last successful sync.
    pub last_synced_at: String,
}

/// The manifest document. Keys of `files` are Penpot file UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            files: BTreeMap::new(),
        }
    }
}

impl Manifest {
    /// Absolute path of the manifest inside `sync_root`.
    pub fn path_in(sync_root: &Path) -> PathBuf {
        sync_root.join(MANIFEST_FILE_NAME)
    }

    /// Load the manifest from `sync_root`. `Ok(None)` if the file does not
    /// exist (fresh root). Errors on unreadable/invalid JSON or an
    /// unsupported schema version.
    pub fn load(sync_root: &Path) -> Result<Option<Manifest>> {
        let path = Self::path_in(sync_root);
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
        if probe.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(Error::ManifestSchema {
                found: probe.schema_version,
                expected: MANIFEST_SCHEMA_VERSION,
            });
        }
        let manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| Error::Json {
            path: path.clone(),
            source: e,
        })?;
        Ok(Some(manifest))
    }

    /// Atomically save to `<sync_root>/.penpot-sync.json` (tmp + fsync +
    /// rename). The serialized form uses the same normalization rules as
    /// everything else we write (sorted keys, 2-space indent, LF, trailing
    /// newline) so the manifest itself is git-diffable.
    pub fn save(&self, sync_root: &Path) -> Result<()> {
        let path = Self::path_in(sync_root);
        let value = serde_json::to_value(self).expect("manifest serializes");
        let mut s = crate::normalize::dumps(&value);
        s.push('\n');
        crate::util::atomic_write(&path, s.as_bytes())
    }

    /// Find the entry (fileId + entry) whose `path` equals `rel_path`.
    pub fn entry_by_path(&self, rel_path: &str) -> Option<(&str, &ManifestEntry)> {
        self.files
            .iter()
            .find(|(_, e)| e.path == rel_path)
            .map(|(id, e)| (id.as_str(), e))
    }
}

/// Current time as an RFC 3339 UTC string with second precision, e.g.
/// `2026-07-13T09:04:42Z`. Provided so callers don't need a time dependency.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(secs as i64)
}

/// Civil-from-days algorithm (Howard Hinnant) — no external time crate.
pub(crate) fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        // date -u -r 1784125482 +"%Y-%m-%dT%H:%M:%SZ" == 2026-07-15T14:24:42Z
        assert_eq!(rfc3339_from_unix(1_784_125_482), "2026-07-15T14:24:42Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
    }
}
