//! Two-phase directory swap + crash recovery. The mid-crash states are
//! constructed manually, exactly as an interrupted commit would leave them.

use std::fs;
use std::path::Path;

use sync_core::{cleanup_orphans, commit_dir_swap, stage_path_for};

fn write_tree(dir: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn read_marker(dir: &Path) -> String {
    fs::read_to_string(dir.join("marker.txt")).unwrap()
}

#[test]
fn swap_replaces_existing_target_and_removes_old() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    write_tree(&target, &[("marker.txt", "v1"), ("files/a.json", "{}")]);

    let staged = stage_path_for(&target);
    assert_eq!(staged.parent(), target.parent());
    assert!(staged
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("homepage.penpot.tmp-"));
    write_tree(&staged, &[("marker.txt", "v2"), ("files/b.json", "{}")]);

    commit_dir_swap(&staged, &target).unwrap();
    assert_eq!(read_marker(&target), "v2");
    assert!(target.join("files/b.json").exists());
    assert!(!target.join("files/a.json").exists(), "old payload replaced");
    assert!(!staged.exists());
    // No tmp/old leftovers.
    let leftovers: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "homepage.penpot")
        .collect();
    assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
}

#[test]
fn swap_works_when_target_does_not_exist_yet() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("fresh.penpot");
    let staged = stage_path_for(&target);
    write_tree(&staged, &[("marker.txt", "v1")]);
    commit_dir_swap(&staged, &target).unwrap();
    assert_eq!(read_marker(&target), "v1");
}

#[test]
fn swap_with_missing_staged_dir_is_an_error_and_target_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    write_tree(&target, &[("marker.txt", "v1")]);
    let staged = stage_path_for(&target);
    let err = commit_dir_swap(&staged, &target).unwrap_err();
    assert!(err.to_string().contains("staged dir"));
    assert_eq!(read_marker(&target), "v1");
}

/// Crash state 1: staged tmp written, crash before any rename.
/// Target must stay; tmp must be swept.
#[test]
fn cleanup_removes_orphan_tmp_next_to_intact_target() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("proj/homepage.penpot");
    write_tree(&target, &[("marker.txt", "good")]);
    let staged = stage_path_for(&target);
    write_tree(&staged, &[("marker.txt", "partial")]);

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.removed_tmp, vec![staged.clone()]);
    assert!(report.removed_old.is_empty());
    assert!(report.restored.is_empty());
    assert!(!staged.exists());
    assert_eq!(read_marker(&target), "good");
}

/// Crash state 2: target renamed aside to old, crash before tmp was renamed
/// in. Target is missing → old must be restored, tmp swept. No data loss.
#[test]
fn cleanup_restores_old_when_target_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    let old = tmp.path().join("homepage.penpot.old-0123456789ab");
    let staged = tmp.path().join("homepage.penpot.tmp-fedcba987654");
    write_tree(&old, &[("marker.txt", "last-known-good")]);
    write_tree(&staged, &[("marker.txt", "maybe-partial")]);

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.restored, vec![(old.clone(), target.clone())]);
    assert_eq!(report.removed_tmp, vec![staged.clone()]);
    assert!(!old.exists());
    assert!(!staged.exists());
    assert_eq!(read_marker(&target), "last-known-good");
}

/// The startup sweep must never descend into `.penpot-packages/` — packages are
/// git repos the sync layer must not touch (E2 daemon-blindness invariant). An
/// entry inside a package that happens to match the swap-orphan grammar is left
/// completely alone, while the same entry at the vault root IS handled.
#[test]
fn cleanup_never_touches_the_package_home() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join(".penpot-packages").join("some-pkg");
    let in_pkg_old = pkg.join("widget.penpot.old-0123456789ab");
    let in_pkg_tmp = pkg.join("gadget.penpot.tmp-fedcba987654");
    write_tree(&in_pkg_old, &[("marker.txt", "package-internal")]);
    write_tree(&in_pkg_tmp, &[("marker.txt", "package-internal")]);
    // A genuine root-level orphan (control): this one MUST be swept.
    let root_tmp = tmp.path().join("homepage.penpot.tmp-0011223344ff");
    write_tree(&root_tmp, &[("marker.txt", "partial")]);

    let report = cleanup_orphans(tmp.path()).unwrap();

    // Everything inside .penpot-packages/ is untouched.
    assert!(in_pkg_old.exists(), "package orphan-named dir must not be restored/deleted");
    assert!(in_pkg_tmp.exists(), "package orphan-named dir must not be deleted");
    assert!(report.restored.is_empty());
    assert!(report.removed_old.is_empty());
    // The root-level orphan was swept as usual.
    assert_eq!(report.removed_tmp, vec![root_tmp.clone()]);
    assert!(!root_tmp.exists());
}

/// Crash state 3: tmp already renamed to target, crash before old deletion.
/// Target intact → old is deleted.
#[test]
fn cleanup_deletes_old_when_target_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    let old = tmp.path().join("homepage.penpot.old-0123456789ab");
    write_tree(&target, &[("marker.txt", "new")]);
    write_tree(&old, &[("marker.txt", "previous")]);

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.removed_old, vec![old.clone()]);
    assert!(report.restored.is_empty());
    assert!(!old.exists());
    assert_eq!(read_marker(&target), "new");
}

/// Two old dirs for the same missing target (double crash): one is restored,
/// the other deleted — deterministically, never both left behind.
#[test]
fn cleanup_double_crash_restores_exactly_one_old() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    let old1 = tmp.path().join("homepage.penpot.old-000000000001");
    let old2 = tmp.path().join("homepage.penpot.old-fffffffffffe");
    write_tree(&old1, &[("marker.txt", "older")]);
    write_tree(&old2, &[("marker.txt", "newer")]);

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.restored.len(), 1);
    assert_eq!(report.removed_old.len(), 1);
    assert!(target.is_dir());
    assert!(!old1.exists());
    assert!(!old2.exists());
    // Sorted processing → the lexicographically first old wins.
    assert_eq!(read_marker(&target), "older");
}

/// Stale manifest tmp files (interrupted atomic save) are swept too.
#[test]
fn cleanup_sweeps_stale_manifest_tmp_files() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = tmp.path().join(".penpot-sync.json.tmp-0123456789ab");
    fs::write(&stale, b"{}").unwrap();
    fs::write(tmp.path().join(".penpot-sync.json"), b"{}").unwrap();

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.removed_tmp, vec![stale.clone()]);
    assert!(!stale.exists());
    assert!(tmp.path().join(".penpot-sync.json").exists());
}

/// The sweep must never touch user data: dirs without our exact
/// `.penpot.{tmp|old}-<12hex>` shape survive, including inside project dirs,
/// and payload inside `*.penpot` dirs is never descended into.
#[test]
fn cleanup_never_touches_lookalike_user_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let keep = [
        "notes.old-0123456789ab",           // base doesn't end in .penpot
        "homepage.penpot.tmp-123",          // suffix too short
        "homepage.penpot.tmp-0123456789AB", // uppercase hex
        "homepage.penpot.backup",           // no marker at all
        "proj/design.penpot",               // real payload dir
    ];
    for rel in keep {
        write_tree(&tmp.path().join(rel), &[("marker.txt", "keep")]);
    }
    // A file (not dir) named like an old dir: not restorable dir state; keep it.
    fs::write(tmp.path().join("stray.penpot.old-0123456789ab"), b"x").unwrap();

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report, sync_core::CleanupReport::default());
    for rel in keep {
        assert!(tmp.path().join(rel).exists(), "{rel} was touched");
    }
    assert!(tmp.path().join("stray.penpot.old-0123456789ab").exists());
}

/// Orphans nested under project folders are found; cleanup is idempotent.
#[test]
fn cleanup_recurses_into_project_folders_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("client-x/homepage.penpot");
    let orphan = tmp.path().join("client-x/homepage.penpot.tmp-0123456789ab");
    write_tree(&target, &[("marker.txt", "good")]);
    write_tree(&orphan, &[("marker.txt", "junk")]);

    let report1 = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report1.removed_tmp, vec![orphan.clone()]);
    let report2 = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report2, sync_core::CleanupReport::default());
    assert_eq!(read_marker(&target), "good");
}

/// End-to-end: interrupt a swap manually between the two renames, run the
/// startup sweep, and verify the tree is back to the pre-swap state.
#[test]
fn interrupted_swap_then_cleanup_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("homepage.penpot");
    write_tree(&target, &[("marker.txt", "v1"), ("files/a.json", "{}")]);
    let staged = stage_path_for(&target);
    write_tree(&staged, &[("marker.txt", "v2")]);

    // Simulate the first half of commit_dir_swap: target → old. Crash.
    let old = tmp.path().join("homepage.penpot.old-0123456789ab");
    fs::rename(&target, &old).unwrap();

    let report = cleanup_orphans(tmp.path()).unwrap();
    assert_eq!(report.restored.len(), 1);
    assert_eq!(report.removed_tmp.len(), 1);
    assert_eq!(read_marker(&target), "v1");
    assert!(target.join("files/a.json").exists());

    // And the swap can then be redone cleanly.
    let staged2 = stage_path_for(&target);
    write_tree(&staged2, &[("marker.txt", "v2")]);
    commit_dir_swap(&staged2, &target).unwrap();
    assert_eq!(read_marker(&target), "v2");
}
