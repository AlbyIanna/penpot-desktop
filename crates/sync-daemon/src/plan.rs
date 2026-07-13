//! Pure reconciliation decision table: disk × manifest × DB → action.
//!
//! Identity joining happens in the caller: manifest entries join disk by
//! `path` and the DB by fileId; a disk dir without a manifest entry can never
//! be correlated with a DB file (there is no id on disk), so it is always an
//! import-as-new; a DB file without a manifest entry is always a first
//! export. This function is total over all 8 presence combinations plus the
//! hash sub-cases, and exhaustively unit-tested.
//!
//! M3 semantics (two-way): when only the disk moved since `lastSyncedHash`,
//! the disk is imported (Direction B); when BOTH sides moved, the conflict
//! rule applies — the DB version is preserved as a
//! `<name>.conflict-<ts>.penpot/` copy next to the file, then the disk
//! version (the source of truth) is imported in place. Neither side is ever
//! silently overwritten.

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
    /// move since we last looked" hint, never as a conflict signal on its own.
    pub revn: i64,
    /// DB `modifiedAt` recorded at last sync; `""` = unknown (M2-era entry)
    /// → the DB-moved hint falls back to revn alone.
    pub db_modified_at: &'a str,
}

/// Facts from the DB poll surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFacts<'a> {
    pub revn: i64,
    pub modified_at: &'a str,
}

/// Did the DB move since this manifest entry was written? `(revn,
/// modifiedAt)` vs the manifest pair; an M2-era entry (empty
/// `db_modified_at`) compares revn alone. Advisory — used to pick between
/// import / export / conflict, while the semantic hash remains the truth for
/// "did the DISK change".
pub fn db_moved(manifest: &ManifestFacts<'_>, db: &DbFacts<'_>) -> bool {
    if db.revn != manifest.revn {
        return true;
    }
    !manifest.db_modified_at.is_empty() && db.modified_at != manifest.db_modified_at
}

/// What reconciliation must do for one file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run the export pipeline (DB → disk). Internally no-op-safe: if the
    /// freshly exported semantic hash equals `lastSyncedHash` the target dir
    /// is left untouched.
    Export,
    /// Both sides changed since `lastSyncedHash` → conflict rule: preserve
    /// the DB version as a `.conflict-<ts>.penpot/` copy next to the file,
    /// then import the disk version in place. Never silently overwrite
    /// either side.
    Conflict,
    /// Disk changed, DB did not (or the file is missing from the DB while
    /// the manifest knows it — the core-invariant resurrect path) →
    /// in-place import with the manifest's fileId. Import-as-new is the
    /// runtime fallback.
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
    db: Option<&DbFacts<'_>>,
) -> Option<Decision> {
    Some(match (disk, manifest, db) {
        (None, None, None) => return None,
        // Disk without a manifest entry: nothing to correlate — import as new
        // (the `db: Some` variant of this row cannot be joined by the caller
        // and is treated identically).
        (Some(_), None, _) => Decision::ImportAsNew,
        // New DB file (or manifest-known file whose disk dir vanished):
        // export re-creates the tree. A deleted disk dir is deliberately NOT
        // a DB deletion (M3 policy): the DB version is restored to disk.
        (None, None, Some(_)) => Decision::Export,
        (None, Some(_), Some(_)) => Decision::Export,
        // Gone from both sides: only the manifest remembers it.
        (None, Some(_), None) => Decision::ForgetManifestEntry,
        // The core-invariant path (DB wiped / file lost): resurrect under the
        // same id, whether or not the disk changed meanwhile (the DB side has
        // nothing to conflict with).
        (Some(_), Some(_), None) => Decision::ImportInPlace,
        (Some(d), Some(m), Some(db)) => {
            let disk_moved = d.semantic_hash != m.last_synced_hash;
            match (disk_moved, db_moved(m, db)) {
                (false, false) => Decision::Noop,
                // Disk clean, DB moved → export (no-op-safe if the DB change
                // was volatile).
                (false, true) => Decision::Export,
                // Disk moved, DB clean → Direction B import.
                (true, false) => Decision::ImportInPlace,
                // Both moved → the conflict rule.
                (true, true) => Decision::Conflict,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H_SYNCED: &str = "hash-synced";
    const H_OTHER: &str = "hash-other";
    const T_SYNCED: &str = "2026-07-13T09:00:00.000Z";
    const T_OTHER: &str = "2026-07-13T10:30:00.000Z";

    fn disk(h: &str) -> DiskFacts<'_> {
        DiskFacts { semantic_hash: h }
    }
    fn man(h: &str, revn: i64) -> ManifestFacts<'_> {
        ManifestFacts {
            last_synced_hash: h,
            revn,
            db_modified_at: T_SYNCED,
        }
    }
    fn db(revn: i64, modified_at: &str) -> DbFacts<'_> {
        DbFacts { revn, modified_at }
    }

    #[test]
    fn exhaustive_presence_combinations() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 3);
        let b = db(3, T_SYNCED);
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
    fn disk_dirty_db_missing_is_still_the_resurrect_path() {
        // The disk changed while the DB lost the file entirely (wipe): the
        // disk (source of truth) is imported under the manifest's id — there
        // is no DB version to preserve.
        let d = disk(H_OTHER);
        let m = man(H_SYNCED, 3);
        assert_eq!(decide(Some(&d), Some(&m), None), Some(Decision::ImportInPlace));
    }

    #[test]
    fn all_present_disk_clean_db_moved_exports() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 3);
        // revn moved forward
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(4, T_SYNCED))),
            Some(Decision::Export)
        );
        // revn moved BACKWARD (in-place import resets it — still a change)
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(1, T_SYNCED))),
            Some(Decision::Export)
        );
        // revn identical but modifiedAt moved — still a change.
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(3, T_OTHER))),
            Some(Decision::Export)
        );
    }

    #[test]
    fn all_present_disk_dirty_db_clean_imports_in_place() {
        // THE Direction B arm: an external edit with an unmoved DB.
        let d = disk(H_OTHER);
        let m = man(H_SYNCED, 3);
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(3, T_SYNCED))),
            Some(Decision::ImportInPlace)
        );
    }

    #[test]
    fn both_moved_is_a_conflict_in_every_variant() {
        let d = disk(H_OTHER);
        let m = man(H_SYNCED, 3);
        for facts in [db(9, T_OTHER), db(9, T_SYNCED), db(3, T_OTHER)] {
            assert_eq!(
                decide(Some(&d), Some(&m), Some(&facts)),
                Some(Decision::Conflict),
                "facts: {facts:?}"
            );
        }
    }

    #[test]
    fn m2_era_entry_without_db_modified_at_falls_back_to_revn() {
        let m = ManifestFacts {
            last_synced_hash: H_SYNCED,
            revn: 3,
            db_modified_at: "",
        };
        // Unknown stored modifiedAt: same revn counts as unmoved…
        assert!(!db_moved(&m, &db(3, T_OTHER)));
        // …and a moved revn as moved.
        assert!(db_moved(&m, &db(4, T_OTHER)));
        let d = disk(H_OTHER);
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(3, T_OTHER))),
            Some(Decision::ImportInPlace)
        );
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(4, T_OTHER))),
            Some(Decision::Conflict)
        );
    }

    #[test]
    fn all_present_clean_and_unmoved_is_noop() {
        let d = disk(H_SYNCED);
        let m = man(H_SYNCED, 0);
        assert_eq!(
            decide(Some(&d), Some(&m), Some(&db(0, T_SYNCED))),
            Some(Decision::Noop)
        );
    }
}
