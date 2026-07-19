//! Filesystem naming: sanitize Penpot project/file names into path
//! components, allocate stable on-disk paths.
//!
//! **Path is identity, names are cosmetic** (PLAN.md): once a fileId has a
//! path in the manifest it keeps it forever (renames in Penpot do not move
//! the directory in M2); collisions get a short-id suffix; the manifest
//! records the actual path.

use std::path::Path;

use sync_core::Manifest;

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
