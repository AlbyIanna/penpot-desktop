//! Trimming an unzipped binfile tree down to a single file's own subtree (E3).
//!
//! The E3 spike (`docs/ecosystem-spikes/e3-linking.md` §5/§9) proved that the
//! ONLY `export-binfile` flag combination which preserves a linked consumer's
//! `componentFile=<libId>` reference on disk is `(includeLibraries=true,
//! embedAssets=false)` — but that export writes the WHOLE referenced library as
//! a second `files/<libId>/…` subtree plus a `relations` entry (the
//! `include-libraries` anti-pattern §7). The daemon must borrow only that
//! export's *reference-preserving* behaviour, then trim the inlined library
//! away, leaving a one-file `.penpot` tree whose instance still carries a bare
//! `componentFile=<libId>` id reference.
//!
//! This is the Rust port of the spike's proven `trim_to_single_file`
//! (`scripts/ecosystem-spike/e3_probe.py`), run over the unzipped stage dir
//! BEFORE [`crate::normalize_tree`]/hashing.

use std::path::Path;

use crate::{Error, Result};

/// Trim an unzipped `include-libraries` binfile tree in place so it contains
/// ONLY the `keep_id` file's own subtree:
///
/// - `manifest.json` `files` list is filtered to the single `keep_id` entry and
///   `relations` is reset to `[]`.
/// - every other `files/<otherId>.json` + `files/<otherId>/…` subtree (the
///   inlined library) is deleted.
///
/// The result is the E3 on-disk representation: the consumer keeps
/// `componentFile=<libId>` as a bare id reference with the library NOT inlined.
/// Idempotent — running it on an already-single-file tree is a no-op.
pub fn trim_to_single_file(tree_dir: &Path, keep_id: &str) -> Result<()> {
    let manifest_path = tree_dir.join("manifest.json");
    let raw = std::fs::read(&manifest_path).map_err(|e| Error::io(&manifest_path, e))?;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| Error::Json {
            path: manifest_path.clone(),
            source: e,
        })?;

    if let Some(files) = manifest.get_mut("files").and_then(|v| v.as_array_mut()) {
        files.retain(|f| f.get("id").and_then(|i| i.as_str()) == Some(keep_id));
    }
    if let Some(obj) = manifest.as_object_mut() {
        obj.insert("relations".to_string(), serde_json::json!([]));
    }
    // normalize_tree re-normalizes formatting afterwards; we just need valid,
    // byte-diffable JSON here (sorted keys, LF, trailing newline).
    let mut s = crate::normalize::dumps(&manifest);
    s.push('\n');
    std::fs::write(&manifest_path, s.as_bytes()).map_err(|e| Error::io(&manifest_path, e))?;

    let files_dir = tree_dir.join("files");
    if files_dir.is_dir() {
        for entry in std::fs::read_dir(&files_dir).map_err(|e| Error::io(&files_dir, e))? {
            let entry = entry.map_err(|e| Error::io(&files_dir, e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `files/<id>.json` (the file's root) and `files/<id>/…` (its blobs)
            // both stem to `<id>`; keep exactly the ones for `keep_id`.
            let stem = name.strip_suffix(".json").unwrap_or(&name);
            if stem == keep_id {
                continue;
            }
            let path = entry.path();
            let ft = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if ft.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|e| Error::io(&path, e))?;
            } else {
                std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// A two-file include-libraries export (consumer + inlined library) trims to
    /// the consumer's own subtree only, with a one-entry manifest and no
    /// relations — the library subtree is gone but `componentFile=<libId>`
    /// inside the consumer's own json is untouched.
    #[test]
    fn trims_inlined_library_leaving_bare_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cons = "11111111-1111-1111-1111-111111111111";
        let lib = "22222222-2222-2222-2222-222222222222";
        write(
            root,
            "manifest.json",
            serde_json::to_vec(&json!({
                "files": [{"id": cons, "name": "consumer"}, {"id": lib, "name": "library"}],
                "relations": [[cons, lib]],
                "type": "penpot/export-files",
                "version": 1,
            }))
            .unwrap()
            .as_slice(),
        );
        // Consumer root json carries a componentFile pointing at the library id.
        write(
            root,
            &format!("files/{cons}.json"),
            serde_json::to_vec(&json!({"id": cons, "componentRef": lib}))
                .unwrap()
                .as_slice(),
        );
        write(root, &format!("files/{cons}/pages/p1.json"), b"{}");
        // Inlined library subtree (must be deleted).
        write(
            root,
            &format!("files/{lib}.json"),
            serde_json::to_vec(&json!({"id": lib}))
                .unwrap()
                .as_slice(),
        );
        write(root, &format!("files/{lib}/components/c1.json"), b"{}");

        trim_to_single_file(root, cons).unwrap();

        // Manifest: single file entry, empty relations.
        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("manifest.json")).unwrap()).unwrap();
        let files = m.get("files").unwrap().as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].get("id").unwrap().as_str(), Some(cons));
        assert_eq!(m.get("relations").unwrap().as_array().unwrap().len(), 0);

        // Consumer subtree kept; library subtree gone.
        assert!(root.join(format!("files/{cons}.json")).exists());
        assert!(root.join(format!("files/{cons}/pages/p1.json")).exists());
        assert!(!root.join(format!("files/{lib}.json")).exists());
        assert!(!root.join(format!("files/{lib}")).exists());

        // The bare componentFile-style reference inside the consumer is intact.
        let c: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join(format!("files/{cons}.json"))).unwrap())
                .unwrap();
        assert_eq!(c.get("componentRef").unwrap().as_str(), Some(lib));
    }

    /// Trimming an already-single-file tree is a no-op (idempotent).
    #[test]
    fn trim_is_idempotent_on_single_file_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cons = "11111111-1111-1111-1111-111111111111";
        write(
            root,
            "manifest.json",
            serde_json::to_vec(&json!({
                "files": [{"id": cons, "name": "consumer"}],
                "relations": [],
                "version": 1,
            }))
            .unwrap()
            .as_slice(),
        );
        write(root, &format!("files/{cons}.json"), b"{}");

        trim_to_single_file(root, cons).unwrap();

        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(m.get("files").unwrap().as_array().unwrap().len(), 1);
        assert!(root.join(format!("files/{cons}.json")).exists());
    }
}
