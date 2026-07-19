//! D2 delete: move a file's `.penpot` directory out of the live vault tree
//! instead of removing it.
//!
//! Why this exists at all: the core invariant resurrects anything that is on
//! disk but missing from the DB (`crates/sync-daemon/src/plan.rs`,
//! `(Some, Some, None) => ImportInPlace`) — that is how a wiped database
//! rebuilds itself from the folder tree. "The user deleted this file" reaches
//! the daemon as that exact same state, so an RPC-only delete comes back at
//! the next startup reconciliation. Deleting therefore has to leave the live
//! tree AND the manifest, together.
//!
//! It moves rather than removes because the folder tree is the user's own
//! work and the source of truth. `.trash/` is a dot-directory, which every
//! scanner already skips (daemon walk, FS watcher, vault index), so trashed
//! files are inert without any new exclusion logic.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::manifest::Manifest;

/// Dot-prefixed on purpose — see the module docs.
pub const TRASH_DIR_NAME: &str = ".trash";

/// Where trashed files live for a given vault.
pub fn trash_dir(vault_root: &Path) -> PathBuf {
    vault_root.join(TRASH_DIR_NAME)
}

/// What a successful trash did, for logging and for the API response.
#[derive(Debug, Clone)]
pub struct TrashOutcome {
    pub trashed_path: PathBuf,
    pub former_rel_path: String,
}

/// Move `file_id`'s directory into the vault trash and drop its manifest entry.
///
/// `stamp` is caller-supplied (an RFC3339-ish compact timestamp) so this stays
/// deterministic and unit-testable. Order matters: the directory moves first,
/// and the manifest is only rewritten once the move succeeded — a crash
/// between the two leaves a manifest entry pointing at a missing directory,
/// which the daemon already tolerates, whereas the reverse would leave a live
/// directory with no manifest entry and re-import it as a brand new file.
pub fn trash_file(vault_root: &Path, file_id: &str, stamp: &str) -> Result<TrashOutcome> {
    let mut manifest = Manifest::load(vault_root)
        .context("loading manifest")?
        .ok_or_else(|| anyhow!("no manifest in {}", vault_root.display()))?;

    let entry = manifest
        .files
        .get(file_id)
        .ok_or_else(|| anyhow!("file id {file_id} is not in the manifest"))?;
    let rel = entry.path.clone();

    // The manifest is a plain JSON file living in the user's own folder tree
    // (the conflict rule assumes the user can hand-edit things on disk), and
    // `trash_file` is the only thing standing between that field and a
    // filesystem move. Every writer today goes through `sanitize_component`
    // in `crates/sync-daemon/src/paths.rs`, but this function must not rely
    // on that discipline: `vault_root.join(rel)` with an absolute `rel`
    // discards `vault_root` entirely, and a `..`-laden `rel` walks out of
    // it. Reject before any filesystem work (and before touching the
    // manifest) so a bad entry can neither move anything nor half-complete
    // by dropping its own entry.
    if !is_safe_vault_rel(&rel) {
        return Err(anyhow!(
            "manifest entry for {file_id} has an unsafe path, refusing to trash it: {rel:?}"
        ));
    }

    let src = vault_root.join(&rel);
    let dest = unique_dest(vault_root, &rel, stamp)?;

    if src.exists() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).context("creating trash dir")?;
        }
        std::fs::rename(&src, &dest)
            .with_context(|| format!("moving {} to {}", src.display(), dest.display()))?;
    }

    manifest.files.remove(file_id);
    manifest.save(vault_root).context("saving manifest")?;

    Ok(TrashOutcome { trashed_path: dest, former_rel_path: rel })
}

/// True iff `rel` is a safe within-vault relative path: not empty, not
/// absolute, and with no `.`/`..`/prefix components, so `vault_root.join`
/// can never be tricked into leaving the vault. Mirrors
/// `apps/desktop/src/home.rs::is_safe_vault_rel` (kept as a private copy
/// here rather than a shared dependency — `sync-core` must not depend on
/// `apps/desktop`).
fn is_safe_vault_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let p = Path::new(rel);
    p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// `<vault>/.trash/<stamp>-<basename>` , suffixed if that already exists so a
/// second delete of the same name in the same second cannot clobber the first.
///
/// The exists-check-then-rename below is not atomic: two concurrent
/// `trash_file` calls picking the same stamp+name could both observe the
/// slot as free before either renames into it. That's a safe failure mode,
/// not a data-loss one — `std::fs::rename` never merges a directory into an
/// existing one, so the loser's `rename` simply errors instead of silently
/// clobbering the winner's trashed copy. Do not "fix" this with a scheme
/// that could overwrite instead of failing.
fn unique_dest(vault_root: &Path, rel: &str, stamp: &str) -> Result<PathBuf> {
    let base = Path::new(rel)
        .file_name()
        .ok_or_else(|| anyhow!("manifest path has no file name: {rel}"))?
        .to_string_lossy()
        .to_string();
    let dir = trash_dir(vault_root);
    let first = dir.join(format!("{stamp}-{base}"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..1000 {
        let cand = dir.join(format!("{stamp}-{n}-{base}"));
        if !cand.exists() {
            return Ok(cand);
        }
    }
    Err(anyhow!("could not find a free trash name for {rel}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};

    fn seed(root: &Path, file_id: &str, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
        std::fs::write(root.join(rel).join("file.json"), b"{}").unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            file_id.to_string(),
            ManifestEntry {
                path: rel.to_string(),
                project_id: "p1".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: "h".into(),
                last_synced_at: "2026-07-19T00:00:00Z".into(),
            },
        );
        m.save(root).unwrap();
    }

    #[test]
    fn trash_moves_the_directory_and_drops_the_manifest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");

        let out = trash_file(root, "f1", "20260719-120000").unwrap();

        // The original is gone from the live tree...
        assert!(!root.join("Proj/hello.penpot").exists());
        // ...and present under the trash dir, contents intact.
        assert!(out.trashed_path.starts_with(trash_dir(root)));
        assert!(out.trashed_path.join("file.json").exists());
        assert_eq!(out.former_rel_path, "Proj/hello.penpot");

        // The manifest no longer knows about it — this is what stops the
        // startup reconciliation from resurrecting it.
        let m = Manifest::load(root).unwrap().unwrap();
        assert!(!m.files.contains_key("f1"), "manifest entry survived: {:?}", m.files);
    }

    #[test]
    fn trashed_file_is_invisible_to_the_dot_directory_skip_rule() {
        // The whole design rests on scanners skipping dot-dirs. Pin the shape
        // of the path we produce so a rename of TRASH_DIR_NAME to something
        // undotted fails loudly here rather than silently resurrecting files.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        let out = trash_file(root, "f1", "20260719-120000").unwrap();
        let rel = out.trashed_path.strip_prefix(root).unwrap();
        let first = rel.components().next().unwrap().as_os_str().to_string_lossy().to_string();
        assert!(first.starts_with('.'), "trash root must be a dot-dir, got {first}");
    }

    #[test]
    fn trashing_twice_does_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        let a = trash_file(root, "f1", "20260719-120000").unwrap();
        seed(root, "f2", "Proj/hello.penpot");
        let b = trash_file(root, "f2", "20260719-120000").unwrap();
        assert_ne!(a.trashed_path, b.trashed_path, "same-stamp trashes collided");
        assert!(a.trashed_path.join("file.json").exists(), "first trash was clobbered");
    }

    #[test]
    fn unknown_file_id_is_an_error_not_a_silent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        assert!(trash_file(root, "nope", "20260719-120000").is_err());
        assert!(root.join("Proj/hello.penpot").exists(), "unrelated file was touched");
    }

    #[test]
    fn missing_directory_still_drops_the_manifest_entry() {
        // Disk already gone (user deleted it in Finder) but the manifest still
        // lists it: dropping the entry is exactly what must happen, otherwise
        // the entry lingers forever.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        seed(root, "f1", "Proj/hello.penpot");
        std::fs::remove_dir_all(root.join("Proj/hello.penpot")).unwrap();
        trash_file(root, "f1", "20260719-120000").unwrap();
        let m = Manifest::load(root).unwrap().unwrap();
        assert!(!m.files.contains_key("f1"));
    }

    // -- manifest path validation: the manifest is a plain JSON file living
    // in the user's own folder tree (see the conflict rule), so a corrupted
    // or hand-edited entry must not be able to make trash_file touch
    // anything outside the vault. These must fail against the unfixed code:
    // `vault_root.join(rel)` with an absolute `rel` discards `vault_root`
    // entirely, and a `..`-laden `rel` walks out of it.

    #[test]
    fn absolute_manifest_path_is_rejected_and_outside_dir_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A real directory *outside* the vault that an unvalidated absolute
        // path could resolve to and move.
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), b"do not move me").unwrap();

        let mut m = Manifest::default();
        m.files.insert(
            "f1".to_string(),
            ManifestEntry {
                path: victim.to_string_lossy().to_string(), // absolute path
                project_id: "p1".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: "h".into(),
                last_synced_at: "2026-07-19T00:00:00Z".into(),
            },
        );
        m.save(root).unwrap();

        let err = trash_file(root, "f1", "20260719-120000")
            .expect_err("absolute manifest path must be rejected");
        assert!(
            err.to_string().contains(&victim.to_string_lossy().to_string())
                || err.to_string().to_lowercase().contains("absolute"),
            "error should name the offending path: {err}"
        );

        // The outside directory must still exist, untouched.
        assert!(victim.exists(), "directory outside the vault was moved/removed");
        assert!(victim.join("keep.txt").exists());

        // A rejected delete must not half-complete: the manifest entry
        // survives.
        let m2 = Manifest::load(root).unwrap().unwrap();
        assert!(
            m2.files.contains_key("f1"),
            "manifest entry was dropped despite the trash being rejected"
        );
    }

    #[test]
    fn dotdot_manifest_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A sibling directory next to the vault root that `..` could escape
        // into.
        std::fs::create_dir_all(root.join("../escaped-victim")).ok();

        seed(root, "f1", "Proj/hello.penpot");
        // Overwrite with a `..`-laden path.
        let mut m = Manifest::load(root).unwrap().unwrap();
        m.files.get_mut("f1").unwrap().path = "../escaped-victim".to_string();
        m.save(root).unwrap();

        let err = trash_file(root, "f1", "20260719-120000")
            .expect_err("a `..`-laden manifest path must be rejected");
        assert!(
            err.to_string().contains("..") || err.to_string().to_lowercase().contains("parent"),
            "error should name the offending path: {err}"
        );

        let m2 = Manifest::load(root).unwrap().unwrap();
        assert!(
            m2.files.contains_key("f1"),
            "manifest entry was dropped despite the trash being rejected"
        );

        std::fs::remove_dir_all(root.join("../escaped-victim")).ok();
    }

    #[test]
    fn empty_manifest_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".to_string(),
            ManifestEntry {
                path: String::new(),
                project_id: "p1".into(),
                project_name: "Proj".into(),
                revn: 1,
                db_modified_at: String::new(),
                last_synced_hash: "h".into(),
                last_synced_at: "2026-07-19T00:00:00Z".into(),
            },
        );
        m.save(root).unwrap();

        let err = trash_file(root, "f1", "20260719-120000")
            .expect_err("an empty manifest path must be rejected");
        assert!(!err.to_string().is_empty());

        let m2 = Manifest::load(root).unwrap().unwrap();
        assert!(
            m2.files.contains_key("f1"),
            "manifest entry was dropped despite the trash being rejected"
        );
    }
}
