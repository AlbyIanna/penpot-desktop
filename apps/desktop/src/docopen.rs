//! Path -> resolution logic for opening a `.penpot` from Finder, a CLI
//! argument, a drag-drop, or a second launch (D5: "document-based app").
//! Every one of those entry points funnels through the same question —
//! "given this path, what is it, relative to the vault?" — so the answer
//! lives here once, pure and unit-testable, and every caller is a dumb
//! translation on top. Mirrors the `tray/model.rs` / `windows.rs` split:
//! this module is deliberately free of Tauri types, touching the
//! filesystem only for the read-only checks it needs (`is_dir`,
//! `canonicalize`) and never writing to the vault or the DB — the core
//! invariant (DB is a disposable cache, the folder tree is truth) does not
//! apply to a module that never mutates either side of it.

use std::path::{Path, PathBuf};

use sync_core::manifest::Manifest;

/// What a filesystem path resolves to, from one vault's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A `.penpot` dir inside the vault, already known to the manifest.
    InVault { file_id: String, title: String },
    /// A `.penpot` dir inside the vault, but not yet in the manifest —
    /// freshly created on disk and the daemon hasn't imported it. This is
    /// deliberately NOT `External`: the dir already lives in this vault, so
    /// offering to import it would duplicate it. The caller polls until an
    /// id appears instead.
    PendingImport { rel_path: String },
    /// A `.penpot` dir outside the vault. The caller offers to import it.
    External { path: PathBuf },
    /// Not a directory whose name ends in `.penpot`.
    NotAPenpotDir { reason: String },
}

/// Resolve `raw_path` against `vault_root` and the already-loaded
/// `manifest`.
///
/// Security-relevant ordering: both `raw_path` and `vault_root` are
/// canonicalized (resolving `..` and symlinks via
/// [`std::fs::canonicalize`]) *before* the `strip_prefix` in/out-of-vault
/// check below. Doing the check on the raw paths would let a `..`-laden
/// path either dodge the vault-membership check (making an in-vault file
/// look `External`, e.g. `vault/Proj/../Proj/x.penpot`) or, on other
/// inputs, escape the vault boundary entirely. Canonicalizing both sides
/// (not just `raw_path`) also matters on macOS, where the OS tempdir root
/// itself sits behind a symlink (`/var` -> `/private/var`): comparing an
/// un-canonicalized `vault_root` against a canonicalized `raw_path` would
/// spuriously fail `strip_prefix` even for genuinely in-vault paths.
pub fn resolve(raw_path: &Path, vault_root: &Path, manifest: &Manifest) -> Resolved {
    if !raw_path.is_dir() {
        return Resolved::NotAPenpotDir {
            reason: format!("{} is not a directory", raw_path.display()),
        };
    }
    let Some(name) = raw_path.file_name().and_then(|n| n.to_str()) else {
        return Resolved::NotAPenpotDir {
            reason: format!("{} has no usable file name", raw_path.display()),
        };
    };
    if !name.ends_with(".penpot") {
        return Resolved::NotAPenpotDir {
            reason: format!("{name} does not end in .penpot"),
        };
    }

    // Both `raw_path` (just proven a dir above) and `vault_root` (the
    // app's configured vault, assumed to exist) should canonicalize
    // cleanly; fall back to the raw path on failure so a transient FS
    // error degrades to a (safe, if imprecise) External/NotAPenpotDir
    // rather than panicking.
    let canonical_path =
        std::fs::canonicalize(raw_path).unwrap_or_else(|_| raw_path.to_path_buf());
    let canonical_vault =
        std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());

    let Ok(rel) = canonical_path.strip_prefix(&canonical_vault) else {
        return Resolved::External { path: canonical_path };
    };

    // `ManifestEntry::path` is always `/`-separated regardless of platform
    // (see its doc comment), so rebuild the relative path with `/` rather
    // than trusting `Path`'s (platform-dependent) separator or `Display`.
    let rel_path = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");

    match manifest.entry_by_path(&rel_path) {
        Some((file_id, _entry)) => Resolved::InVault {
            file_id: file_id.to_string(),
            title: display_title(&rel_path),
        },
        None => Resolved::PendingImport { rel_path },
    }
}

/// `<project>/<name>.penpot` -> `<name>`, for a vault-relative path.
/// Mirrors `menubar::file_display_name` exactly — same rule, kept in sync
/// by hand since it's a two-line function on each side; do not invent a
/// second convention if this ever needs to change.
pub fn display_title(rel_path: &str) -> String {
    let base = rel_path.rsplit('/').next().unwrap_or(rel_path);
    base.strip_suffix(".penpot").unwrap_or(base).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::manifest::{Manifest, ManifestEntry};

    fn vault_with(root: &std::path::Path, rel: &str, id: &str) -> Manifest {
        std::fs::create_dir_all(root.join(rel)).unwrap();
        let mut m = Manifest::default();
        m.files.insert(id.into(), ManifestEntry {
            path: rel.into(), project_id: "p".into(), project_name: "P".into(),
            revn: 1, db_modified_at: String::new(), last_synced_hash: "h".into(),
            last_synced_at: "2026-07-20T00:00:00Z".into(),
        });
        m
    }

    #[test]
    fn a_known_in_vault_penpot_resolves_to_its_file_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let m = vault_with(root, "Proj/Home.penpot", "fid1");
        match resolve(&root.join("Proj/Home.penpot"), root, &m) {
            Resolved::InVault { file_id, title } => { assert_eq!(file_id, "fid1"); assert_eq!(title, "Home"); }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_non_penpot_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        assert!(matches!(resolve(&tmp.path().join("notes"), tmp.path(), &Manifest::default()),
                         Resolved::NotAPenpotDir { .. }));
    }

    #[test]
    fn an_external_penpot_is_flagged_for_import() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("Loose.penpot")).unwrap();
        assert!(matches!(resolve(&outside.path().join("Loose.penpot"), vault.path(), &Manifest::default()),
                         Resolved::External { .. }));
    }

    #[test]
    fn a_vault_internal_penpot_with_no_manifest_entry_yet_is_pending_not_external() {
        // Freshly created on disk; the daemon has not imported it. It is NOT
        // external — copying it in would duplicate it. The caller polls.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Proj/New.penpot")).unwrap();
        assert!(matches!(resolve(&tmp.path().join("Proj/New.penpot"), tmp.path(), &Manifest::default()),
                         Resolved::PendingImport { .. }));
    }

    #[test]
    fn dotdot_cannot_make_an_in_vault_path_look_external() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let m = vault_with(root, "Proj/Home.penpot", "fid1");
        let sneaky = root.join("Proj").join("..").join("Proj").join("Home.penpot");
        assert!(matches!(resolve(&sneaky, root, &m), Resolved::InVault { .. }));
    }
}
