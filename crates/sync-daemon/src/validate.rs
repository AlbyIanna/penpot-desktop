//! Pre-import validation of an on-disk `.penpot` tree (Direction B).
//!
//! Before ANY import the tree must look like a sane single-file binfile-v3
//! export; otherwise the change is surfaced as a per-file error and nothing
//! destructive happens (the import is retried only when the tree changes
//! again). Checks:
//!
//! 1. every `.json` file parses,
//! 2. `manifest.json` exists at the tree root and lists **exactly one**
//!    penpot file,
//! 3. that file's id matches the sync manifest's fileId for this directory
//!    (when known — a brand-new dir has no expectation),
//! 4. the file-level `files/<id>.json` document exists.

use std::path::Path;

use sync_core::read_tree;

/// Validate the tree rooted at `root`. `expected_file_id` is the sync
/// manifest's fileId for this directory, if it has one. Returns the binfile's
/// file id on success, a human-readable reason on failure.
pub(crate) fn validate_tree(
    root: &Path,
    expected_file_id: Option<&str>,
) -> Result<String, String> {
    let files = read_tree(root).map_err(|e| format!("unreadable tree: {e}"))?;
    if files.is_empty() {
        return Err("directory is empty".to_string());
    }
    for (rel, content) in &files {
        if rel.ends_with(".json") {
            serde_json::from_slice::<serde_json::Value>(content)
                .map_err(|e| format!("invalid JSON in {rel}: {e}"))?;
        }
    }
    let raw = files
        .get("manifest.json")
        .ok_or_else(|| "missing manifest.json at the tree root".to_string())?;
    let manifest: serde_json::Value =
        serde_json::from_slice(raw).expect("parsed above (.json)");
    let list = manifest
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "manifest.json has no `files` array".to_string())?;
    if list.len() != 1 {
        return Err(format!(
            "manifest.json lists {} files; exactly 1 is supported",
            list.len()
        ));
    }
    let file_id = list[0]
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "manifest.json files[0] has no string `id`".to_string())?;
    if let Some(expected) = expected_file_id {
        if file_id != expected {
            return Err(format!(
                "binfile file id {file_id} does not match this directory's manifest entry {expected} \
                 (was the tree copied from another file?)"
            ));
        }
    }
    let file_doc = format!("files/{file_id}.json");
    if !files.contains_key(&file_doc) {
        return Err(format!("missing {file_doc}"));
    }
    Ok(file_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FID: &str = "3a4be581-6d37-8010-8008-51f0c6eb307f";

    /// Minimal valid single-file binfile tree.
    fn write_valid_tree(root: &Path) {
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(
            root.join("manifest.json"),
            format!(
                r#"{{"type":"penpot/export-files","version":1,"generatedBy":"penpot/2.16.2","files":[{{"id":"{FID}","name":"home","features":[]}}],"relations":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            root.join(format!("files/{FID}.json")),
            format!(r#"{{"id":"{FID}","name":"home","revn":3}}"#),
        )
        .unwrap();
    }

    #[test]
    fn valid_tree_passes_and_returns_the_file_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        assert_eq!(validate_tree(tmp.path(), None).unwrap(), FID);
        assert_eq!(validate_tree(tmp.path(), Some(FID)).unwrap(), FID);
    }

    #[test]
    fn binary_blobs_are_not_json_validated() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        std::fs::create_dir_all(tmp.path().join("objects")).unwrap();
        std::fs::write(tmp.path().join("objects/a.png"), [0x89, 0x50, 0xff, 0x00]).unwrap();
        assert!(validate_tree(tmp.path(), Some(FID)).is_ok());
    }

    #[test]
    fn broken_json_anywhere_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        std::fs::create_dir_all(tmp.path().join(format!("files/{FID}/pages"))).unwrap();
        std::fs::write(
            tmp.path().join(format!("files/{FID}/pages/p1.json")),
            b"{truncated",
        )
        .unwrap();
        let err = validate_tree(tmp.path(), Some(FID)).unwrap_err();
        assert!(err.contains("invalid JSON"), "got: {err}");
        assert!(err.contains("pages/p1.json"), "got: {err}");
    }

    #[test]
    fn missing_or_malformed_binfile_manifest_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("readme.txt"), b"not a binfile").unwrap();
        let err = validate_tree(tmp.path(), None).unwrap_err();
        assert!(err.contains("missing manifest.json"), "got: {err}");

        std::fs::write(tmp.path().join("manifest.json"), br#"{"nope":1}"#).unwrap();
        let err = validate_tree(tmp.path(), None).unwrap_err();
        assert!(err.contains("no `files` array"), "got: {err}");
    }

    #[test]
    fn empty_dir_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = validate_tree(tmp.path(), None).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn multi_file_binfiles_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        std::fs::write(
            tmp.path().join("manifest.json"),
            format!(
                r#"{{"files":[{{"id":"{FID}"}},{{"id":"other"}}],"version":1}}"#
            ),
        )
        .unwrap();
        let err = validate_tree(tmp.path(), Some(FID)).unwrap_err();
        assert!(err.contains("lists 2 files"), "got: {err}");
    }

    #[test]
    fn file_id_mismatch_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        let err = validate_tree(tmp.path(), Some("00000000-dead-beef-0000-000000000000"))
            .unwrap_err();
        assert!(err.contains("does not match"), "got: {err}");
    }

    #[test]
    fn missing_file_level_document_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tree(tmp.path());
        std::fs::remove_file(tmp.path().join(format!("files/{FID}.json"))).unwrap();
        let err = validate_tree(tmp.path(), Some(FID)).unwrap_err();
        assert!(err.contains(&format!("missing files/{FID}.json")), "got: {err}");
    }
}
