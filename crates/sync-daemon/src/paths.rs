//! Filesystem naming: sanitize Penpot project/file names into path
//! components, allocate stable on-disk paths.
//!
//! **Path is identity, names are cosmetic** (PLAN.md): once a fileId has a
//! path in the manifest, [`allocate_file_path`] keeps returning it forever —
//! collisions get a short-id suffix; the manifest records the actual path.
//! D2 adds the other half: [`relocation_target`]/[`relocate_tracked_file`]
//! notice when a tracked file's current name/project no longer match that
//! path and move the directory to follow it (rename/move are first-class
//! verbs — the folder tree must not go stale forever), while still never
//! fighting the dedup suffixes `allocate_file_path` itself produces (see the
//! loop-hazard note on [`relocation_target`]).

use std::path::Path;

use sync_core::{semantic_tree_hash, Manifest};

/// Directory suffix marking a Penpot file dir (`homepage.penpot/`).
pub const PENPOT_DIR_SUFFIX: &str = ".penpot";

/// Marker inside a conflict-copy directory name
/// (`homepage.conflict-2026-07-13T09-04-42Z.penpot/`). Conflict copies are
/// never watched, never synced (the disk walker skips them), never
/// auto-deleted — the user resolves and removes them manually.
pub const CONFLICT_MARKER: &str = ".conflict-";

// Source of truth for the on-disk component length cap. `apps/desktop`'s
// `manage::valid_name` mirrors this value in its own `MAX_NAME_LEN` (it
// can't reference this constant directly — it isn't `pub`) so that names
// accepted by the API can never exceed what this sanitiser would truncate
// on disk. If this value changes, update `MAX_NAME_LEN` in
// `apps/desktop/src/manage.rs` to match.
const MAX_COMPONENT_CHARS: usize = 100;

/// Is this directory name a conflict copy (ends with `.penpot` AND contains
/// the conflict marker)? [`allocate_file_path`] never *produces* such names
/// for regular files, so the check is unambiguous.
pub fn is_conflict_dir_name(name: &str) -> bool {
    name.ends_with(PENPOT_DIR_SUFFIX) && name.contains(CONFLICT_MARKER)
}

/// Conflict-copy path for a file dir: `client-x/home.penpot` →
/// `client-x/home.conflict-<ts>.penpot` (NEXT TO the file, per the conflict
/// rule). The RFC 3339 timestamp has `:` replaced with `-` so the name is
/// portable (Windows) and Finder-friendly.
pub fn conflict_path_for(rel: &str, timestamp_rfc3339: &str) -> String {
    let stem = rel.strip_suffix(PENPOT_DIR_SUFFIX).unwrap_or(rel);
    let ts = timestamp_rfc3339.replace(':', "-");
    format!("{stem}{CONFLICT_MARKER}{ts}{PENPOT_DIR_SUFFIX}")
}

/// Sanitize an arbitrary Penpot name into a single safe path component:
/// path separators / control chars / Windows-reserved chars become `-`,
/// leading dots (hidden files) and trailing dots/spaces are stripped,
/// length is capped, empty results become `untitled`.
pub fn sanitize_component(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .take(MAX_COMPONENT_CHARS)
        .collect();
    while s.starts_with(['.', ' ']) {
        s.remove(0);
    }
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        "untitled".to_string()
    } else {
        s
    }
}

/// First 8 filesystem-safe chars of an id (uuid), for collision suffixes.
fn short_id(id: &str) -> String {
    let s: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if s.is_empty() {
        "x".to_string()
    } else {
        s
    }
}

/// Directory name (single component under the sync root) for a project.
///
/// Reuses the directory any existing manifest entry of the same project
/// already lives in (stability). Otherwise sanitizes the project name,
/// guards against the `.penpot` suffix (a project dir ending in `.penpot`
/// would be mistaken for a file dir by the disk walker), and suffixes with a
/// short project id if a *different* project already owns that name.
pub fn project_dir_name(manifest: &Manifest, project_id: &str, project_name: &str) -> String {
    if let Some(entry) = manifest
        .files
        .values()
        .find(|e| e.project_id == project_id && e.path.contains('/'))
    {
        if let Some(first) = entry.path.split('/').next() {
            return first.to_string();
        }
    }
    let mut base = sanitize_component(project_name);
    if base.ends_with(PENPOT_DIR_SUFFIX) {
        base = format!(
            "{}-penpot",
            &base[..base.len() - PENPOT_DIR_SUFFIX.len()]
        );
        if base == "-penpot" {
            base = "untitled-penpot".to_string();
        }
    }
    let owned_by_other = |name: &str| {
        manifest
            .files
            .values()
            .any(|e| e.project_id != project_id && e.path.split('/').next() == Some(name))
    };
    if owned_by_other(&base) {
        format!("{base}-{}", short_id(project_id))
    } else {
        base
    }
}

/// Manifest-relative path (`<project_dir>/<name>.penpot`, `/` separators) for
/// a file. An existing manifest entry's path always wins (path is identity).
/// New paths avoid collisions with both other manifest entries and anything
/// already on disk (a user-created dir must never be swap-destroyed by an
/// export for a different file).
pub fn allocate_file_path(
    manifest: &Manifest,
    sync_root: &Path,
    project_dir: &str,
    file_id: &str,
    file_name: &str,
) -> String {
    if let Some(entry) = manifest.files.get(file_id) {
        return entry.path.clone();
    }
    // A file named e.g. `x.conflict-1` would otherwise produce a dir name
    // that [`is_conflict_dir_name`] matches — and conflict copies are
    // ignored by the watcher and the disk walker. Keep real files watchable.
    let base = sanitize_component(file_name).replace(CONFLICT_MARKER, "-conflict-");
    let taken = |rel: &str| {
        manifest.entry_by_path(rel).is_some() || sync_root.join(rel).symlink_metadata().is_ok()
    };
    let candidate = format!("{project_dir}/{base}{PENPOT_DIR_SUFFIX}");
    if !taken(&candidate) {
        return candidate;
    }
    let with_short = format!(
        "{project_dir}/{base}-{}{PENPOT_DIR_SUFFIX}",
        short_id(file_id)
    );
    if !taken(&with_short) {
        return with_short;
    }
    // Last resort: the full id is unique by construction.
    format!("{project_dir}/{base}-{file_id}{PENPOT_DIR_SUFFIX}")
}

/// Decide whether a *tracked* file's on-disk path should be relocated to
/// follow its current DB name/project, and if so, what the new path should
/// be. `None` means "leave it where it is".
///
/// LOOP HAZARD (why this isn't just "does the path match the bare name"):
/// [`allocate_file_path`] may originally have placed this file at a
/// dedup-suffixed path (`Proj/Hello-ab12.penpot`) because `Proj/Hello.penpot`
/// was already taken by a different file at allocation time. If relocation
/// only compared the entry's current path against the *bare* base
/// candidate, that suffixed file — whose name never actually changed — would
/// look "wrong" on every single poll and get "relocated" back onto the exact
/// same suffixed path, forever, spamming a rename every cycle. So a path
/// counts as already-correct if it matches ANY candidate
/// [`allocate_file_path`] would produce for this file id today: the bare
/// base, the short-id suffix, or the full-id suffix.
pub fn relocation_target(
    manifest: &Manifest,
    sync_root: &Path,
    file_id: &str,
    project_dir: &str,
    file_name: &str,
) -> Option<String> {
    let entry = manifest.files.get(file_id)?;
    let base = sanitize_component(file_name).replace(CONFLICT_MARKER, "-conflict-");
    let self_candidates = [
        format!("{project_dir}/{base}{PENPOT_DIR_SUFFIX}"),
        format!("{project_dir}/{base}-{}{PENPOT_DIR_SUFFIX}", short_id(file_id)),
        format!("{project_dir}/{base}-{file_id}{PENPOT_DIR_SUFFIX}"),
    ];
    if self_candidates.iter().any(|c| c == &entry.path) {
        return None;
    }
    let old_path = entry.path.as_str();
    let taken = |rel: &str| {
        // A candidate that differs from this file's OWN current path only in
        // case (macOS/Windows default filesystems are case-insensitive but
        // case-preserving) is not a collision with someone else — it's the
        // very directory we're about to rename in place. Without this, a
        // case-only DB rename (`hello` -> `Hello`) would see its own dir via
        // `symlink_metadata` and conclude the destination is "taken",
        // permanently skipping the rename (or, worse, suffixing away from
        // itself every poll).
        if rel.eq_ignore_ascii_case(old_path) {
            return false;
        }
        manifest.files.iter().any(|(id, e)| id != file_id && e.path == rel)
            || sync_root.join(rel).symlink_metadata().is_ok()
    };
    // Never overwrite: walk the same base/short-id/full-id ladder
    // allocate_file_path uses, and if even the (unique-by-construction)
    // full-id candidate is somehow taken, give up rather than relocate onto
    // a path that already belongs to something else.
    self_candidates.into_iter().find(|c| !taken(c))
}

/// Result of [`relocate_tracked_file`].
#[derive(Debug, PartialEq, Eq)]
pub enum RelocationOutcome {
    /// Current path already matches the desired name/project — the common
    /// case, checked on every poll.
    NotNeeded,
    /// The directory was moved on disk and `manifest`'s entry updated.
    Moved { from: String, to: String },
    /// A relocation was due, but nothing exists on disk yet under the old
    /// path (the file has never been exported). Nothing to *move* — the
    /// manifest entry is retargeted so the next export writes straight to
    /// the new location.
    Retargeted { to: String },
    /// A relocation was due, but the on-disk tree changed since
    /// `lastSyncedHash` (or is unreadable). Moving it now would silently
    /// relocate an uncommitted local edit out from under the user, ahead of
    /// the normal conflict guard ever seeing it. Left untouched at the old
    /// path; the caller's own conflict guard handles it from there.
    SkippedLocalChanges,
    /// A relocation was due, but by the time this ran the destination was
    /// already claimed (race with another process/poll). Left untouched
    /// rather than risking an overwrite; retried next poll.
    SkippedDestinationTaken,
}

/// If `file_id`'s current name/project imply a path other than its manifest
/// entry, move its directory on disk to match and update `manifest` in
/// place (caller still owns persisting the manifest to disk). Never
/// overwrites anything and never moves a tree with uncommitted local
/// changes — see [`relocation_target`] and [`RelocationOutcome`] for the
/// exact rules. A no-op for untracked files (nothing to relocate: a brand
/// new file goes through [`allocate_file_path`] instead).
pub fn relocate_tracked_file(
    manifest: &mut Manifest,
    sync_root: &Path,
    file_id: &str,
    project_dir: &str,
    file_name: &str,
) -> std::io::Result<RelocationOutcome> {
    let Some(new_rel) = relocation_target(manifest, sync_root, file_id, project_dir, file_name)
    else {
        return Ok(RelocationOutcome::NotNeeded);
    };
    let old_rel = manifest.files.get(file_id).expect("checked by relocation_target").path.clone();
    let old_target = sync_root.join(&old_rel);
    if !old_target.is_dir() {
        manifest.files.get_mut(file_id).expect("checked above").path = new_rel.clone();
        return Ok(RelocationOutcome::Retargeted { to: new_rel });
    }

    // Conflict guard: same rule as Engine::export_file's swap guard. Never
    // relocate a tree that itself changed since lastSyncedHash out from
    // under a local edit; leave it at the old path and let the normal
    // conflict-detection path (which runs next, keyed off the manifest path)
    // handle it.
    let last_synced_hash = manifest.files.get(file_id).expect("checked above").last_synced_hash.clone();
    match semantic_tree_hash(&old_target) {
        Ok(disk_hash) if disk_hash == last_synced_hash => {}
        _ => return Ok(RelocationOutcome::SkippedLocalChanges),
    }

    let new_target = sync_root.join(&new_rel);
    // Case-only rename: on a case-insensitive filesystem `new_target` IS
    // `old_target` (same inode), so the naive existence check below would
    // always see it as "taken" and refuse to move. Skip that check for the
    // one case where the destination is provably the source itself.
    let case_only_rename = new_rel.eq_ignore_ascii_case(&old_rel);
    if !case_only_rename && new_target.symlink_metadata().is_ok() {
        return Ok(RelocationOutcome::SkippedDestinationTaken);
    }
    if let Some(parent) = new_target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&old_target, &new_target)?;
    manifest.files.get_mut(file_id).expect("checked above").path = new_rel.clone();

    // Best-effort cleanup: the old project dir might now be empty. Only ever
    // remove_dir (never remove_dir_all) so this can only ever remove a
    // directory it can prove is empty — never the file's own content, never
    // unrelated files a user left alongside it.
    if let Some(old_parent) = old_target.parent() {
        if old_parent != sync_root {
            if let Ok(mut it) = std::fs::read_dir(old_parent) {
                if it.next().is_none() {
                    let _ = std::fs::remove_dir(old_parent);
                }
            }
        }
    }

    Ok(RelocationOutcome::Moved { from: old_rel, to: new_rel })
}

/// Cosmetic import name for a file dir path (`client-x/homepage.penpot` →
/// `homepage`). The backend schema requires a `name` field on import but
/// ignores it for binfile-v3 archives.
pub fn file_stem_of(rel_path: &str) -> String {
    let last = rel_path.rsplit('/').next().unwrap_or(rel_path);
    last.strip_suffix(PENPOT_DIR_SUFFIX).unwrap_or(last).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::ManifestEntry;

    fn entry(path: &str, project_id: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            project_id: project_id.to_string(),
            project_name: "P".to_string(),
            revn: 0,
            db_modified_at: String::new(),
            last_synced_hash: "h".to_string(),
            last_synced_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize_component("Homepage"), "Homepage");
        assert_eq!(sanitize_component("a/b\\c:d"), "a-b-c-d");
        assert_eq!(sanitize_component("..hidden"), "hidden");
        assert_eq!(sanitize_component("trailing. . "), "trailing");
        assert_eq!(sanitize_component(""), "untitled");
        assert_eq!(sanitize_component("///"), "---");
        assert_eq!(sanitize_component("  .  "), "untitled");
        assert_eq!(sanitize_component("emoji 🎨 ok"), "emoji 🎨 ok");
        assert_eq!(sanitize_component("tab\there"), "tab-here");
        assert_eq!(sanitize_component("win<>:\"|?*"), "win-------");
        // length cap
        let long = "x".repeat(500);
        assert_eq!(sanitize_component(&long).chars().count(), 100);
    }

    #[test]
    fn project_dir_reuses_existing_mapping() {
        let mut m = Manifest::default();
        m.files
            .insert("f1".into(), entry("Weird Dir Name/a.penpot", "p1"));
        // Even though the project was renamed in the DB, the dir is stable.
        assert_eq!(project_dir_name(&m, "p1", "Renamed"), "Weird Dir Name");
    }

    #[test]
    fn project_dir_collision_suffixes_with_id() {
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/a.penpot", "p1"));
        // A different project with the same (sanitized) name.
        let name = project_dir_name(&m, "0aaabbbb-cccc-dddd-eeee-ffff00001111", "Client");
        assert_eq!(name, "Client-0aaabbbb");
        // The same project just reuses it.
        assert_eq!(project_dir_name(&m, "p1", "Client"), "Client");
    }

    #[test]
    fn project_dir_never_ends_with_penpot_suffix() {
        let m = Manifest::default();
        let name = project_dir_name(&m, "p1", "logos.penpot");
        assert_eq!(name, "logos-penpot");
        assert!(!name.ends_with(PENPOT_DIR_SUFFIX));
        // Leading dot stripped first, so this is just "penpot" (safe).
        assert_eq!(project_dir_name(&m, "p2", ".penpot"), "penpot");
        assert_eq!(project_dir_name(&m, "p3", "x.penpot"), "x-penpot");
    }

    #[test]
    fn file_path_identity_wins_over_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/old-name.penpot", "p1"));
        // File renamed in Penpot: path unchanged (path is identity).
        assert_eq!(
            allocate_file_path(&m, tmp.path(), "Client", "f1", "new name"),
            "Client/old-name.penpot"
        );
    }

    #[test]
    fn file_path_collisions_get_short_then_full_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/home.penpot", "p1"));
        let p2 = allocate_file_path(&m, tmp.path(), "Client", "2222abcd-0000", "home");
        assert_eq!(p2, "Client/home-2222abcd.penpot");
        m.files.insert("2222abcd-0000".into(), entry(&p2, "p1"));
        // Pathological: same name AND same short id prefix → full id.
        let p3 = allocate_file_path(&m, tmp.path(), "Client", "2222abcd-9999", "home");
        assert_eq!(p3, "Client/home-2222abcd-9999.penpot");
    }

    #[test]
    fn file_path_avoids_existing_disk_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Client/home.penpot")).unwrap();
        let m = Manifest::default();
        // A user-created (unmanaged) dir occupies the natural path → suffix.
        let p = allocate_file_path(&m, tmp.path(), "Client", "abcd1234-1", "home");
        assert_eq!(p, "Client/home-abcd1234.penpot");
    }

    // ------------------------------------------------------------------
    // D2: relocation on rename/move — a tracked file's directory follows
    // its current DB name/project instead of staying pinned forever.
    // ------------------------------------------------------------------

    #[test]
    fn relocation_target_flags_a_renamed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/old-name.penpot", "p1"));
        assert_eq!(
            relocation_target(&m, tmp.path(), "f1", "Client", "new name"),
            Some("Client/new name.penpot".to_string())
        );
    }

    #[test]
    fn relocation_target_leaves_a_dedup_suffixed_file_alone() {
        // f1 was allocated Client/home-2222abcd.penpot because Client/home.penpot
        // was already taken by f0 at allocation time. f1's NAME never changed
        // — this must say "do not relocate", or it would rename itself back
        // onto the exact same suffixed path every single poll forever (the
        // loop hazard documented on relocation_target).
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f0".into(), entry("Client/home.penpot", "p1"));
        m.files.insert(
            "2222abcd-0000".into(),
            entry("Client/home-2222abcd.penpot", "p1"),
        );
        assert_eq!(
            relocation_target(&m, tmp.path(), "2222abcd-0000", "Client", "home"),
            None
        );
    }

    #[test]
    fn relocation_target_follows_a_project_move() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("OldProj/home.penpot", "p1"));
        assert_eq!(
            relocation_target(&m, tmp.path(), "f1", "NewProj", "home"),
            Some("NewProj/home.penpot".to_string())
        );
    }

    #[test]
    fn relocation_target_unchanged_name_and_project_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/home.penpot", "p1"));
        assert_eq!(relocation_target(&m, tmp.path(), "f1", "Client", "home"), None);
    }

    #[test]
    fn relocation_target_avoids_overwriting_a_taken_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        // f1 renamed to "home", but Client/home.penpot is already taken by a
        // different tracked file — must suffix, never collide.
        m.files.insert("other".into(), entry("Client/home.penpot", "p1"));
        m.files.insert("f1".into(), entry("Client/old.penpot", "p1"));
        assert_eq!(
            relocation_target(&m, tmp.path(), "f1", "Client", "home"),
            Some("Client/home-f1.penpot".to_string())
        );
    }

    #[test]
    fn relocation_target_gives_up_rather_than_overwrite_when_every_candidate_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let file_id = "2222abcd-9999-cccc-dddd-eeeeeeeeeeee";
        let mut m = Manifest::default();
        m.files.insert(file_id.into(), entry("Client/old.penpot", "p1"));
        // Base and short-id candidates taken by other tracked files; the
        // full-id candidate taken by an untracked dir already on disk.
        m.files.insert("other1".into(), entry("Client/home.penpot", "p1"));
        m.files
            .insert("other2".into(), entry("Client/home-2222abcd.penpot", "p1"));
        std::fs::create_dir_all(tmp.path().join(format!("Client/home-{file_id}.penpot"))).unwrap();
        assert_eq!(relocation_target(&m, tmp.path(), file_id, "Client", "home"), None);
    }

    /// Build a tracked dir on disk with real content and a manifest entry
    /// whose `last_synced_hash` actually matches it (the "no local changes"
    /// state the conflict guard requires before a relocation proceeds).
    fn synced_dir(tmp: &Path, rel: &str, project_id: &str) -> ManifestEntry {
        std::fs::create_dir_all(tmp.join(rel)).unwrap();
        std::fs::write(tmp.join(rel).join("marker.txt"), b"payload").unwrap();
        let hash = semantic_tree_hash(&tmp.join(rel)).unwrap();
        let mut e = entry(rel, project_id);
        e.last_synced_hash = hash;
        e
    }

    #[test]
    fn relocate_tracked_file_moves_the_dir_and_updates_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".into(),
            synced_dir(tmp.path(), "Client/old-name.penpot", "p1"),
        );

        let outcome = relocate_tracked_file(&mut m, tmp.path(), "f1", "Client", "new name").unwrap();
        assert_eq!(
            outcome,
            RelocationOutcome::Moved {
                from: "Client/old-name.penpot".to_string(),
                to: "Client/new name.penpot".to_string(),
            }
        );
        assert_eq!(m.files["f1"].path, "Client/new name.penpot");
        assert!(!tmp.path().join("Client/old-name.penpot").exists());
        assert!(tmp.path().join("Client/new name.penpot").is_dir());
        assert_eq!(
            std::fs::read(tmp.path().join("Client/new name.penpot/marker.txt")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn relocate_tracked_file_removes_a_now_empty_old_project_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".into(),
            synced_dir(tmp.path(), "OldProj/home.penpot", "p1"),
        );

        relocate_tracked_file(&mut m, tmp.path(), "f1", "NewProj", "home").unwrap();
        assert!(tmp.path().join("NewProj/home.penpot").is_dir());
        // OldProj is now empty — it should be gone, not left as clutter.
        assert!(!tmp.path().join("OldProj").exists());
    }

    #[test]
    fn relocate_tracked_file_keeps_a_still_populated_old_project_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".into(),
            synced_dir(tmp.path(), "OldProj/home.penpot", "p1"),
        );
        std::fs::create_dir_all(tmp.path().join("OldProj/other.penpot")).unwrap();
        m.files
            .insert("f2".into(), entry("OldProj/other.penpot", "p1"));

        relocate_tracked_file(&mut m, tmp.path(), "f1", "NewProj", "home").unwrap();
        // OldProj still has other.penpot in it — must not be swept away.
        assert!(tmp.path().join("OldProj/other.penpot").is_dir());
    }

    #[test]
    fn relocate_tracked_file_never_overwrites_never_destroys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".into(),
            synced_dir(tmp.path(), "Client/old-name.penpot", "p1"),
        );
        // Something else already sits at the bare desired destination.
        std::fs::create_dir_all(tmp.path().join("Client/new name.penpot")).unwrap();
        std::fs::write(
            tmp.path().join("Client/new name.penpot/theirs.txt"),
            b"not mine",
        )
        .unwrap();

        let outcome = relocate_tracked_file(&mut m, tmp.path(), "f1", "Client", "new name").unwrap();
        // relocation_target routes around the taken bare path onto the
        // short-id-suffixed candidate; the untracked dir at the bare path is
        // left completely untouched either way.
        assert_eq!(
            outcome,
            RelocationOutcome::Moved {
                from: "Client/old-name.penpot".to_string(),
                to: "Client/new name-f1.penpot".to_string(),
            }
        );
        assert_eq!(
            std::fs::read(tmp.path().join("Client/new name.penpot/theirs.txt")).unwrap(),
            b"not mine"
        );
    }

    #[test]
    fn relocate_tracked_file_skips_when_disk_has_local_changes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Client/old-name.penpot")).unwrap();
        let mut m = Manifest::default();
        // last_synced_hash deliberately left at the entry() helper's dummy
        // value ("h"), which will never match a real tree's hash — this
        // simulates a local edit made since the last successful sync.
        m.files.insert("f1".into(), entry("Client/old-name.penpot", "p1"));

        let outcome = relocate_tracked_file(&mut m, tmp.path(), "f1", "Client", "new name").unwrap();
        assert_eq!(outcome, RelocationOutcome::SkippedLocalChanges);
        assert_eq!(m.files["f1"].path, "Client/old-name.penpot");
        assert!(tmp.path().join("Client/old-name.penpot").is_dir());
        assert!(!tmp.path().join("Client/new name.penpot").exists());
    }

    #[test]
    fn relocate_tracked_file_retargets_when_nothing_is_on_disk_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert("f1".into(), entry("Client/old-name.penpot", "p1"));

        let outcome = relocate_tracked_file(&mut m, tmp.path(), "f1", "Client", "new name").unwrap();
        assert_eq!(
            outcome,
            RelocationOutcome::Retargeted {
                to: "Client/new name.penpot".to_string()
            }
        );
        assert_eq!(m.files["f1"].path, "Client/new name.penpot");
    }

    #[test]
    fn relocate_tracked_file_case_only_rename_does_not_look_like_a_collision() {
        // Case-only DB rename ("hello" -> "Hello"): on a case-insensitive
        // filesystem (macOS/Windows default) the destination IS the source
        // (same inode), so a naive existence check would always call it
        // "taken" and either refuse the rename forever or error. Must
        // succeed and actually update the case on disk.
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::default();
        m.files.insert(
            "f1".into(),
            synced_dir(tmp.path(), "Client/hello.penpot", "p1"),
        );

        let outcome = relocate_tracked_file(&mut m, tmp.path(), "f1", "Client", "Hello").unwrap();
        assert_eq!(
            outcome,
            RelocationOutcome::Moved {
                from: "Client/hello.penpot".to_string(),
                to: "Client/Hello.penpot".to_string(),
            }
        );
        assert_eq!(m.files["f1"].path, "Client/Hello.penpot");
        assert!(tmp.path().join("Client/Hello.penpot").is_dir());
    }

    #[test]
    fn conflict_names_are_recognized_and_timestamp_is_fs_safe() {
        let p = conflict_path_for("Client/home.penpot", "2026-07-13T09:04:42Z");
        assert_eq!(p, "Client/home.conflict-2026-07-13T09-04-42Z.penpot");
        assert!(!p.contains(':'));
        assert!(is_conflict_dir_name(p.rsplit('/').next().unwrap()));
        // Regular file dirs are not conflict copies.
        assert!(!is_conflict_dir_name("home.penpot"));
        // The marker without the .penpot suffix is not a conflict copy either
        // (e.g. a project dir the user named that way).
        assert!(!is_conflict_dir_name("notes.conflict-old"));
        // Root-level file dir.
        assert_eq!(
            conflict_path_for("home.penpot", "2026-01-02T03:04:05Z"),
            "home.conflict-2026-01-02T03-04-05Z.penpot"
        );
    }

    #[test]
    fn allocated_paths_never_look_like_conflict_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Manifest::default();
        let p = allocate_file_path(&m, tmp.path(), "Client", "f1", "x.conflict-1");
        assert_eq!(p, "Client/x-conflict-1.penpot");
        assert!(!is_conflict_dir_name(p.rsplit('/').next().unwrap()));
    }

    #[test]
    fn file_stem() {
        assert_eq!(file_stem_of("Client/home.penpot"), "home");
        assert_eq!(file_stem_of("home.penpot"), "home");
        assert_eq!(file_stem_of("a/b/c"), "c");
    }
}
