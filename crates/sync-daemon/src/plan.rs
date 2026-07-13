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

// ---------------------------------------------------------------------
// M5: OS-side rename/move — pure pairing + classification
//
// "Path is identity" is relaxed here: a manifest entry whose directory
// vanished is paired with an unclaimed on-disk directory carrying the SAME
// semantic tree hash as the entry's `lastSyncedHash`. A pair means the dir
// was renamed/moved on the OS side — the engine re-keys the manifest path
// (same fileId, NO reimport) and mirrors the change into the DB
// (rename-file / move-files / rename-project).
//
// Safety rules (non-negotiable):
// - pairing requires a hash match that is UNIQUE on both sides — ambiguity
//   (two identical missing entries, or two identical unclaimed dirs)
//   degrades to the M3 behavior (vanish = loud log + DB kept, appear =
//   import-as-new). Never guess.
// - a group of pairs is a *project rename* only under strict conditions
//   (below); anything else is per-file relocation.
// ---------------------------------------------------------------------

use std::collections::{BTreeMap, BTreeSet};

/// First path component of a manifest-relative path — the project folder.
/// `""` for a root-level `.penpot` dir (those live in the catch-all
/// "imported" project).
pub fn folder_of(rel: &str) -> &str {
    rel.split_once('/').map(|(f, _)| f).unwrap_or("")
}

/// Path remainder after the project folder (the whole path for root-level).
fn rest_of(rel: &str) -> &str {
    rel.split_once('/').map(|(_, r)| r).unwrap_or(rel)
}

/// A manifest entry whose recorded path no longer exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEntry<'a> {
    pub file_id: &'a str,
    pub old_rel: &'a str,
    pub last_synced_hash: &'a str,
    pub project_id: &'a str,
}

/// An on-disk `.penpot` dir no manifest entry claims (already hashed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnclaimedDir<'a> {
    pub rel: &'a str,
    pub semantic_hash: &'a str,
}

/// A manifest entry whose dir is still on disk (vetoes project renames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurvivingEntry<'a> {
    pub rel: &'a str,
    pub project_id: &'a str,
}

/// One re-key: `file_id` keeps its identity, its manifest path changes from
/// `old_rel` to `new_rel`. Never triggers a reimport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RekeyPair {
    pub file_id: String,
    pub old_rel: String,
    pub new_rel: String,
}

/// What the engine must do for a detected OS-side rename/move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RekeyOp {
    /// One file dir renamed and/or moved. The engine re-keys the manifest
    /// path, calls `move-files` iff the resolved target project differs from
    /// the entry's, and `rename-file` iff the dir stem changed.
    Relocate {
        file_id: String,
        old_rel: String,
        new_rel: String,
    },
    /// A whole project folder was renamed: same project (by manifest
    /// projectId mapping), every missing entry under `old_folder` reappears
    /// under `new_folder` with an identical sub-path. The engine calls
    /// `rename-project` once and re-keys every pair.
    RenameProject {
        project_id: String,
        old_folder: String,
        new_folder: String,
        pairs: Vec<RekeyPair>,
    },
}

/// Pair missing manifest entries with unclaimed disk dirs by semantic hash
/// (unique on both sides), then classify into per-file relocations and
/// whole-project renames. Pure and deterministic (inputs are order-insensitive;
/// output is sorted).
///
/// `existing_folders` = first-level folder names that still exist on disk
/// (as directories), used to veto project renames: if the old folder is
/// still there, files were moved OUT of it, the project was not renamed.
pub fn plan_rekeys(
    missing: &[MissingEntry<'_>],
    unclaimed: &[UnclaimedDir<'_>],
    surviving: &[SurvivingEntry<'_>],
    existing_folders: &BTreeSet<String>,
) -> Vec<RekeyOp> {
    // 1. Unique-hash pairing. Ambiguity on either side → no pair (safe).
    let mut missing_by_hash: BTreeMap<&str, Vec<&MissingEntry<'_>>> = BTreeMap::new();
    for m in missing {
        missing_by_hash.entry(m.last_synced_hash).or_default().push(m);
    }
    let mut unclaimed_by_hash: BTreeMap<&str, Vec<&UnclaimedDir<'_>>> = BTreeMap::new();
    for u in unclaimed {
        unclaimed_by_hash.entry(u.semantic_hash).or_default().push(u);
    }
    let mut pairs: Vec<(&MissingEntry<'_>, &UnclaimedDir<'_>)> = Vec::new();
    for (hash, ms) in &missing_by_hash {
        if ms.len() != 1 {
            continue; // two vanished entries with identical content: ambiguous
        }
        let Some(us) = unclaimed_by_hash.get(hash) else {
            continue; // nothing on disk matches: a real deletion (or an edit+move)
        };
        if us.len() != 1 {
            continue; // two identical unclaimed dirs: ambiguous
        }
        pairs.push((ms[0], us[0]));
    }
    pairs.sort_by(|a, b| a.0.old_rel.cmp(b.0.old_rel).then(a.0.file_id.cmp(b.0.file_id)));

    // 2. Group cross-folder pairs by (old_folder, new_folder) to detect
    //    whole-project renames.
    let missing_count_per_folder = |folder: &str| -> usize {
        missing.iter().filter(|m| folder_of(m.old_rel) == folder).count()
    };
    let mut ops: Vec<RekeyOp> = Vec::new();
    let mut grouped: BTreeMap<(String, String), Vec<&(&MissingEntry<'_>, &UnclaimedDir<'_>)>> =
        BTreeMap::new();
    for pair in &pairs {
        let (m, u) = pair;
        grouped
            .entry((folder_of(m.old_rel).to_string(), folder_of(u.rel).to_string()))
            .or_default()
            .push(pair);
    }
    for ((old_folder, new_folder), group) in &grouped {
        let is_project_rename = !old_folder.is_empty()
            && !new_folder.is_empty()
            && old_folder != new_folder
            // Same project for every pair (project identity via the manifest
            // projectId mapping), and a known one.
            && !group[0].0.project_id.is_empty()
            && group.iter().all(|(m, _)| m.project_id == group[0].0.project_id)
            // A pure folder rename: identical sub-path on both sides.
            && group.iter().all(|(m, u)| rest_of(m.old_rel) == rest_of(u.rel))
            // The old folder is gone from disk…
            && !existing_folders.contains(old_folder)
            // …no live entry still lives under it…
            && !surviving.iter().any(|s| folder_of(s.rel) == old_folder)
            // …and EVERY vanished entry under it is in this group (an
            // unpaired or elsewhere-paired sibling means it was not a clean
            // folder rename).
            && missing_count_per_folder(old_folder) == group.len()
            // The new folder must not already belong to a different project.
            && !surviving
                .iter()
                .any(|s| folder_of(s.rel) == new_folder && s.project_id != group[0].0.project_id);
        if is_project_rename {
            ops.push(RekeyOp::RenameProject {
                project_id: group[0].0.project_id.to_string(),
                old_folder: old_folder.clone(),
                new_folder: new_folder.clone(),
                pairs: group
                    .iter()
                    .map(|(m, u)| RekeyPair {
                        file_id: m.file_id.to_string(),
                        old_rel: m.old_rel.to_string(),
                        new_rel: u.rel.to_string(),
                    })
                    .collect(),
            });
        } else {
            for (m, u) in group {
                ops.push(RekeyOp::Relocate {
                    file_id: m.file_id.to_string(),
                    old_rel: m.old_rel.to_string(),
                    new_rel: u.rel.to_string(),
                });
            }
        }
    }
    ops
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

    // -----------------------------------------------------------------
    // M5 rekey planning (plan_rekeys) — exhaustive decision table
    // -----------------------------------------------------------------

    fn miss<'a>(
        file_id: &'a str,
        old_rel: &'a str,
        hash: &'a str,
        project_id: &'a str,
    ) -> MissingEntry<'a> {
        MissingEntry {
            file_id,
            old_rel,
            last_synced_hash: hash,
            project_id,
        }
    }
    fn unc<'a>(rel: &'a str, hash: &'a str) -> UnclaimedDir<'a> {
        UnclaimedDir {
            rel,
            semantic_hash: hash,
        }
    }
    fn surv<'a>(rel: &'a str, project_id: &'a str) -> SurvivingEntry<'a> {
        SurvivingEntry { rel, project_id }
    }
    fn folders(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    fn relocate(file_id: &str, old_rel: &str, new_rel: &str) -> RekeyOp {
        RekeyOp::Relocate {
            file_id: file_id.into(),
            old_rel: old_rel.into(),
            new_rel: new_rel.into(),
        }
    }

    #[test]
    fn folder_helpers() {
        assert_eq!(folder_of("A/x.penpot"), "A");
        assert_eq!(folder_of("A/nested/x.penpot"), "A");
        assert_eq!(folder_of("x.penpot"), "");
        assert_eq!(rest_of("A/x.penpot"), "x.penpot");
        assert_eq!(rest_of("A/nested/x.penpot"), "nested/x.penpot");
        assert_eq!(rest_of("x.penpot"), "x.penpot");
    }

    #[test]
    fn rename_within_a_folder_is_a_relocate() {
        let ops = plan_rekeys(
            &[miss("f1", "A/old.penpot", "h1", "p1")],
            &[unc("A/new.penpot", "h1")],
            &[surv("A/other.penpot", "p1")],
            &folders(&["A"]),
        );
        assert_eq!(ops, vec![relocate("f1", "A/old.penpot", "A/new.penpot")]);
    }

    #[test]
    fn move_across_folders_is_a_relocate_when_old_folder_survives() {
        // A/x.penpot → B/x.penpot while A still exists (other content).
        let ops = plan_rekeys(
            &[miss("f1", "A/x.penpot", "h1", "p1")],
            &[unc("B/x.penpot", "h1")],
            &[],
            &folders(&["A", "B"]),
        );
        assert_eq!(ops, vec![relocate("f1", "A/x.penpot", "B/x.penpot")]);
    }

    #[test]
    fn move_into_a_surviving_sibling_project_is_a_relocate() {
        // The old folder is GONE but another entry still lives under it? No —
        // here the new folder belongs to a DIFFERENT project: never a rename.
        let ops = plan_rekeys(
            &[miss("f1", "A/x.penpot", "h1", "p1")],
            &[unc("B/x.penpot", "h1")],
            &[surv("B/theirs.penpot", "p2")],
            &folders(&["B"]),
        );
        assert_eq!(ops, vec![relocate("f1", "A/x.penpot", "B/x.penpot")]);
    }

    #[test]
    fn hash_mismatch_never_pairs() {
        let ops = plan_rekeys(
            &[miss("f1", "A/x.penpot", "h1", "p1")],
            &[unc("A/y.penpot", "h2")],
            &[],
            &folders(&["A"]),
        );
        assert!(ops.is_empty(), "edited-and-moved dirs must degrade safely");
    }

    #[test]
    fn ambiguous_missing_side_never_pairs() {
        // Two vanished entries with identical content: cannot tell which one
        // became the unclaimed dir.
        let ops = plan_rekeys(
            &[
                miss("f1", "A/x.penpot", "h1", "p1"),
                miss("f2", "A/y.penpot", "h1", "p1"),
            ],
            &[unc("A/z.penpot", "h1")],
            &[],
            &folders(&["A"]),
        );
        assert!(ops.is_empty());
    }

    #[test]
    fn ambiguous_unclaimed_side_never_pairs() {
        // One vanished entry, two identical unclaimed dirs.
        let ops = plan_rekeys(
            &[miss("f1", "A/x.penpot", "h1", "p1")],
            &[unc("A/y.penpot", "h1"), unc("A/z.penpot", "h1")],
            &[],
            &folders(&["A"]),
        );
        assert!(ops.is_empty());
    }

    #[test]
    fn unrelated_hashes_pair_independently_and_deterministically() {
        let ops = plan_rekeys(
            &[
                miss("f2", "A/b.penpot", "h2", "p1"),
                miss("f1", "A/a.penpot", "h1", "p1"),
            ],
            &[unc("A/b2.penpot", "h2"), unc("A/a2.penpot", "h1")],
            &[surv("A/keep.penpot", "p1")],
            &folders(&["A"]),
        );
        assert_eq!(
            ops,
            vec![
                relocate("f1", "A/a.penpot", "A/a2.penpot"),
                relocate("f2", "A/b.penpot", "A/b2.penpot"),
            ]
        );
    }

    #[test]
    fn clean_project_folder_rename_is_one_rename_project_op() {
        let ops = plan_rekeys(
            &[
                miss("f1", "Old/a.penpot", "h1", "p1"),
                miss("f2", "Old/b.penpot", "h2", "p1"),
            ],
            &[unc("New/a.penpot", "h1"), unc("New/b.penpot", "h2")],
            &[],
            &folders(&[]), // Old is gone from disk
        );
        assert_eq!(
            ops,
            vec![RekeyOp::RenameProject {
                project_id: "p1".into(),
                old_folder: "Old".into(),
                new_folder: "New".into(),
                pairs: vec![
                    RekeyPair {
                        file_id: "f1".into(),
                        old_rel: "Old/a.penpot".into(),
                        new_rel: "New/a.penpot".into(),
                    },
                    RekeyPair {
                        file_id: "f2".into(),
                        old_rel: "Old/b.penpot".into(),
                        new_rel: "New/b.penpot".into(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn project_rename_with_nested_subpaths_keeps_them() {
        let ops = plan_rekeys(
            &[miss("f1", "Old/nested/a.penpot", "h1", "p1")],
            &[unc("New/nested/a.penpot", "h1")],
            &[],
            &folders(&[]),
        );
        assert!(matches!(&ops[0], RekeyOp::RenameProject { pairs, .. }
            if pairs[0].new_rel == "New/nested/a.penpot"));
    }

    #[test]
    fn project_rename_vetoed_when_old_folder_still_on_disk() {
        // mv Old/a.penpot New/a.penpot with Old still existing = a move.
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "p1")],
            &[unc("New/a.penpot", "h1")],
            &[],
            &folders(&["Old"]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/a.penpot")]);
    }

    #[test]
    fn project_rename_vetoed_when_an_entry_survives_under_old_folder() {
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "p1")],
            &[unc("New/a.penpot", "h1")],
            &[surv("Old/keep.penpot", "p1")],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/a.penpot")]);
    }

    #[test]
    fn project_rename_vetoed_on_mixed_project_ids() {
        let ops = plan_rekeys(
            &[
                miss("f1", "Old/a.penpot", "h1", "p1"),
                miss("f2", "Old/b.penpot", "h2", "p2"),
            ],
            &[unc("New/a.penpot", "h1"), unc("New/b.penpot", "h2")],
            &[],
            &folders(&[]),
        );
        assert_eq!(
            ops,
            vec![
                relocate("f1", "Old/a.penpot", "New/a.penpot"),
                relocate("f2", "Old/b.penpot", "New/b.penpot"),
            ]
        );
    }

    #[test]
    fn project_rename_vetoed_when_a_stem_changed_too() {
        // Folder change + file rename in one step: not a pure folder rename.
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "p1")],
            &[unc("New/renamed.penpot", "h1")],
            &[],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/renamed.penpot")]);
    }

    #[test]
    fn project_rename_vetoed_when_a_sibling_is_unpaired() {
        // Old had two files; only one reappears under New (the other was
        // edited during the move, so its hash no longer matches).
        let ops = plan_rekeys(
            &[
                miss("f1", "Old/a.penpot", "h1", "p1"),
                miss("f2", "Old/b.penpot", "h2", "p1"),
            ],
            &[unc("New/a.penpot", "h1"), unc("New/b.penpot", "h-edited")],
            &[],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/a.penpot")]);
    }

    #[test]
    fn project_rename_vetoed_when_new_folder_belongs_to_another_project() {
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "p1")],
            &[unc("New/a.penpot", "h1")],
            &[surv("New/theirs.penpot", "p2")],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/a.penpot")]);
    }

    #[test]
    fn project_rename_allowed_when_new_folder_has_same_project_survivors() {
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "p1")],
            &[unc("New/a.penpot", "h1")],
            &[surv("New/mine.penpot", "p1")],
            &folders(&[]),
        );
        assert!(matches!(&ops[0], RekeyOp::RenameProject { project_id, .. } if project_id == "p1"));
    }

    #[test]
    fn root_level_renames_are_relocates_never_project_renames() {
        let ops = plan_rekeys(
            &[miss("f1", "old.penpot", "h1", "p1")],
            &[unc("new.penpot", "h1")],
            &[],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "old.penpot", "new.penpot")]);
        // Root → folder and folder → root are relocates too.
        let ops = plan_rekeys(
            &[miss("f1", "old.penpot", "h1", "p1")],
            &[unc("A/old.penpot", "h1")],
            &[],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "old.penpot", "A/old.penpot")]);
    }

    #[test]
    fn empty_project_id_never_renames_projects() {
        let ops = plan_rekeys(
            &[miss("f1", "Old/a.penpot", "h1", "")],
            &[unc("New/a.penpot", "h1")],
            &[],
            &folders(&[]),
        );
        assert_eq!(ops, vec![relocate("f1", "Old/a.penpot", "New/a.penpot")]);
    }

    #[test]
    fn no_missing_or_no_unclaimed_is_a_noop_plan() {
        assert!(plan_rekeys(&[], &[], &[], &folders(&[])).is_empty());
        assert!(plan_rekeys(
            &[miss("f1", "A/x.penpot", "h1", "p1")],
            &[],
            &[],
            &folders(&["A"])
        )
        .is_empty());
        assert!(plan_rekeys(&[], &[unc("A/x.penpot", "h1")], &[], &folders(&["A"])).is_empty());
    }
}
