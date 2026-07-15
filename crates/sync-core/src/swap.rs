//! Two-phase directory swap (POSIX `rename(2)` cannot replace a non-empty
//! directory) plus startup crash recovery.
//!
//! Protocol for replacing `<name>.penpot/`:
//!
//! 1. Caller writes the new tree into a sibling staging dir
//!    `<name>.penpot.tmp-<rand>/` (see [`stage_path_for`]).
//! 2. [`commit_dir_swap`]: rename target → `<name>.penpot.old-<rand>/`,
//!    rename tmp → target, delete old.
//!
//! Crash states and how [`cleanup_orphans`] recovers them at startup:
//!
//! | state on disk                        | recovery                          |
//! |--------------------------------------|-----------------------------------|
//! | tmp + intact target                  | delete tmp (data is in target/DB) |
//! | old + missing target (+ maybe tmp)   | restore old → target, delete tmp  |
//! | old + intact target                  | delete old (swap completed)       |
//! | stale `.penpot-sync.json.tmp-*` file | delete it                         |
//!
//! When both `old` and `tmp` survive with the target missing, `old` wins: it
//! is the last known-good tree, while `tmp` may be a partial write — and the
//! new data still lives in the DB and will re-sync.

use std::path::{Path, PathBuf};

use crate::util::{is_suffix, unique_suffix};
use crate::{Error, Result};

/// Directory suffix that marks a Penpot file dir (`homepage.penpot/`).
const PENPOT_DIR_SUFFIX: &str = ".penpot";

/// Fresh staging-path sibling for `target` (`<target-name>.tmp-<rand>` in the
/// same parent directory). Not created — the caller writes the tree there.
pub fn stage_path_for(target: &Path) -> PathBuf {
    orphan_sibling(target, "tmp")
}

fn orphan_sibling(target: &Path, kind: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!("{name}.{kind}-{}", unique_suffix()))
}

/// Phase two of the swap: atomically-ish replace `target` with the fully
/// written `staged` directory. On success `staged` has become `target` and
/// any previous target is gone. If the final rename fails, the previous
/// target is rolled back into place and an error is returned.
pub fn commit_dir_swap(staged: &Path, target: &Path) -> Result<()> {
    if !staged.is_dir() {
        return Err(Error::Swap(format!(
            "staged dir {} does not exist or is not a directory",
            staged.display()
        )));
    }
    let old = if target.symlink_metadata().is_ok() {
        let old = orphan_sibling(target, "old");
        std::fs::rename(target, &old).map_err(|e| Error::io(target, e))?;
        Some(old)
    } else {
        None
    };
    match std::fs::rename(staged, target) {
        Ok(()) => {
            if let Some(old) = old {
                // Best effort: a leftover old dir is swept by cleanup_orphans.
                let _ = std::fs::remove_dir_all(&old);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(old) = &old {
                let _ = std::fs::rename(old, target); // roll back
            }
            Err(Error::io(target, e))
        }
    }
}

/// What [`cleanup_orphans`] did, for logging/tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Orphaned staging dirs (and stale manifest tmp files) deleted.
    pub removed_tmp: Vec<PathBuf>,
    /// Old dirs deleted because their target exists (completed swaps).
    pub removed_old: Vec<PathBuf>,
    /// Old dirs renamed back to their target (interrupted swaps): `(old, target)`.
    pub restored: Vec<(PathBuf, PathBuf)>,
}

/// If `name` is `<base>.{tmp|old}-<12hex>` where `<base>` ends with
/// `.penpot` (dirs) or is `.penpot-sync.json` (manifest tmp file), return
/// `(base, kind)`.
fn parse_orphan_name(name: &str) -> Option<(&str, &str)> {
    for kind in ["tmp", "old"] {
        let marker = format!(".{kind}-");
        if let Some(pos) = name.rfind(&marker) {
            let (base, rest) = name.split_at(pos);
            let suffix = &rest[marker.len()..];
            if is_suffix(suffix)
                && (base.ends_with(PENPOT_DIR_SUFFIX) || base == crate::MANIFEST_FILE_NAME)
            {
                return Some((base, kind));
            }
        }
    }
    None
}

/// Startup sweep: walk `root` recursively and recover/remove every orphaned
/// `.tmp-*` / `.old-*` leftover of an interrupted swap (see module docs for
/// the state table). Idempotent. Does not descend into `*.penpot` payload
/// dirs (nothing of ours can be orphaned inside them).
pub fn cleanup_orphans(root: &Path) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        let mut olds: Vec<(PathBuf, PathBuf)> = Vec::new(); // (old path, target)
        let mut tmps: Vec<PathBuf> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map_err(|e| Error::io(&path, e))?.is_dir();
            match parse_orphan_name(&name) {
                Some((base, "old")) if is_dir => olds.push((path, dir.join(base))),
                Some((_, "tmp")) => tmps.push(path),
                _ => {
                    // Never descend into the package home: packages are git
                    // repos the sync layer must not touch (E2 daemon-blindness
                    // invariant — mirrors the watcher/walker guards). Without
                    // this, the sweep would recurse in and restore/delete any
                    // entry matching the swap-orphan grammar inside a package.
                    if is_dir
                        && !name.ends_with(PENPOT_DIR_SUFFIX)
                        && name != crate::PACKAGES_DIR_NAME
                    {
                        subdirs.push(path);
                    }
                }
            }
        }
        // Olds first: restore if the target is missing, delete otherwise.
        // (Deterministic order so "restore the oldest suffix" is stable.)
        olds.sort();
        for (old_path, target) in olds {
            if target.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&old_path).map_err(|e| Error::io(&old_path, e))?;
                report.removed_old.push(old_path);
            } else {
                std::fs::rename(&old_path, &target).map_err(|e| Error::io(&old_path, e))?;
                report.restored.push((old_path, target));
            }
        }
        // Tmps are always safe to drop: either the swap completed (target
        // holds the data) or it didn't (the DB still holds the data).
        for tmp in tmps {
            let meta = match tmp.symlink_metadata() {
                Ok(m) => m,
                Err(_) => continue, // already gone
            };
            if meta.is_dir() {
                std::fs::remove_dir_all(&tmp).map_err(|e| Error::io(&tmp, e))?;
            } else {
                std::fs::remove_file(&tmp).map_err(|e| Error::io(&tmp, e))?;
            }
            report.removed_tmp.push(tmp);
        }
        stack.extend(subdirs);
    }
    report.removed_tmp.sort();
    report.removed_old.sort();
    report.restored.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_orphan_name_shapes() {
        assert_eq!(
            parse_orphan_name("homepage.penpot.tmp-0123456789ab"),
            Some(("homepage.penpot", "tmp"))
        );
        assert_eq!(
            parse_orphan_name("homepage.penpot.old-abcdef012345"),
            Some(("homepage.penpot", "old"))
        );
        assert_eq!(
            parse_orphan_name(".penpot-sync.json.tmp-0123456789ab"),
            Some((".penpot-sync.json", "tmp"))
        );
        // Wrong suffix length / case / base — never touched.
        assert_eq!(parse_orphan_name("homepage.penpot.tmp-123"), None);
        assert_eq!(parse_orphan_name("homepage.penpot.tmp-0123456789AB"), None);
        assert_eq!(parse_orphan_name("notes.old-0123456789ab"), None);
        assert_eq!(parse_orphan_name("homepage.penpot"), None);
        assert_eq!(parse_orphan_name("my.tmp-dir.penpot"), None);
    }
}
