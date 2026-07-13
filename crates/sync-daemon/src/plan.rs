//! Pure reconciliation decision table: disk × manifest × DB → action.
//!
//! Identity joining happens in the caller: manifest entries join disk by
//! `path` and the DB by fileId; a disk dir without a manifest entry can never
//! be correlated with a DB file (there is no id on disk), so it is always an
//! import-as-new; a DB file without a manifest entry is always a first
//! export. This function is total over all 8 presence combinations plus the
//! hash sub-cases, and exhaustively unit-tested.

/// Facts about the on-disk `.penpot` directory (already normalized trees;
/// `semantic_hash` = [`sync_core::semantic_tree_hash`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFacts<'a> {
    pub semantic_hash: &'a str,
}

/// Facts from the manifest entry for this fileId.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFacts<'a> {
    pub last_synced_hash: &'a str,
    /// Advisory revn recorded at last sync — used only as a cheap "did the DB
    /// move since we last looked" hint, never as a conflict signal.
    pub revn: i64,
}

/// Facts from the DB poll surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFacts {
    pub revn: i64,
}

/// What reconciliation must do for one file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the export pipeline (DB → disk). Internally no-op-safe: if the
    /// freshly exported semantic hash equals `lastSyncedHash` the target dir
    /// is left untouched.
    Export,
    /// Both sides changed since `lastSyncedHash`. M2 is one-way: same export
    /// pipeline, but logged loudly as a conflict.
    /// TODO(M3 conflict rule): export a `.conflict-<timestamp>.penpot/` copy
    /// next to the file instead of overwriting the disk version.
    ExportDbWinsConflict,
    /// On disk + in manifest, missing from the DB → in-place import with the
    /// manifest's fileId (resurrects the file under the SAME id after a DB
    /// wipe — the core-invariant path). Import-as-new is the runtime fallback.
    ImportInPlace,
    /// On disk, unknown to the manifest → import-as-new, record the new id.
    ImportAsNew,
    /// Disk, manifest and DB agree → nothing to do (seed the change tracker).
    Noop,
    /// Manifest entry with neither a disk dir nor a DB file → drop the entry.
    ForgetManifestEntry,
}

/// The decision table. Returns `None` only for the vacuous
/// (absent, absent, absent) combination.
pub fn decide(
    disk: Option<&DiskFacts<'_>>,
    manifest: Option<&ManifestFacts<'_>>,
    db: Option<&DbFacts>,
) -> Option<Decision> {
    Some(match (disk, manifest, db) {
        (None, None, None) => return None,
        // Disk without a manifest entry: nothing to correlate — import as new
        // (the `db: Some` variant of this row cannot be joined by the caller
        // and is treated identically).
        (Some(_), None, _) => Decision::ImportAsNew,
        // New DB file (or manifest-known file whose disk dir vanished):
        // export re-creates the tree.
        (None, None, Some(_)) => Decision::Export,
        (None, Some(_), Some(_)) => Decision::Export,
        // Gone from both sides: only the manifest remembers it.
        (None, Some(_), None) => Decision::ForgetManifestEntry,
        // The core-invariant path (DB wiped / file lost).
        (Some(_), Some(_), None) => Decision::ImportInPlace,
        (Some(d), Some(m), Some(db)) => {
            if d.semantic_hash == m.last_synced_hash {
                // Disk is clean. If the DB also looks unmoved, done;
                // otherwise export (no-op-safe if the change was volatile).
                if db.revn == m.revn {
                    Decision::Noop
                } else {
                    Decision::Export
                }
            } else {
                // Disk changed since last sync. M2 is one-way — even if the
                // DB looks unmoved we cannot import (Direction B is M3), and
                // revn is advisory anyway. DB wins, loudly.
                Decision::ExportDbWinsConflict
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H_SYNCED: &str = "hash-synced";
    const H_OTHER: &str = "hash-other";

    fn disk(h: &str) -> DiskFacts<'_> {
        DiskFacts { semantic_hash: h }
    }
    fn man(h: &str, revn: i64) -> ManifestFacts<'_> {
        ManifestFacts {
            last_synced_hash: h,
            revn,
        }
    }

    #[test]
    fn exhaustive_presence_combinations() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 3);
        let b = DbFacts { revn: 3 };
        // (disk, manifest, db)
        assert_eq!(decide(None, None, None), None);
        assert_eq!(decide(Some(&d), None, None), Some(Decision::ImportAsNew));
        assert_eq!(decide(None, Some(&m), None), Some(Decision::ForgetManifestEntry));
        assert_eq!(decide(None, None, Some(&b)), Some(Decision::Export));
        assert_eq!(decide(Some(&d), Some(&m), None), Some(Decision::ImportInPlace));
        assert_eq!(decide(Some(&d), None, Some(&b)), Some(Decision::ImportAsNew));
        assert_eq!(decide(None, Some(&m), Some(&b)), Some(Decision::Export));
        assert_eq!(decide(Some(&d), Some(&m), Some(&b)), Some(Decision::Noop));
    }

    #[test]
    fn all_present_disk_clean_db_moved_exports() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 3);
        // revn moved forward
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&DbFacts { revn: 4 })),
            Some(Decision::Export)
        );
        // revn moved BACKWARD (in-place import resets it — still a change)
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&DbFacts { revn: 1 })),
            Some(Decision::Export)
        );
    }

    #[test]
    fn all_present_disk_dirty_is_conflict_db_wins() {
        let d = disk(H_OTHER);
        let m = man(H_SYNCED, 3);
        // DB unmoved: still conflict (one-way M2; revn is advisory).
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&DbFacts { revn: 3 })),
            Some(Decision::ExportDbWinsConflict)
        );
        // DB moved too: the classic both-sides-changed conflict.
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&DbFacts { revn: 9 })),
            Some(Decision::ExportDbWinsConflict)
        );
    }

    #[test]
    fn all_present_clean_and_unmoved_is_noop() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 0);
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&DbFacts { revn: 0 })),
            Some(Decision::Noop)
        );
    }
}
