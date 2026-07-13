//! Tree hashing, reproducing the two tiers of `scripts/roundtrip.py`:
//!
//! - **tree hash** (formatting tier): sha256 over sorted
//!   `(relpath, sha256-hex(content))` pairs — exact byte protocol:
//!   `update(rel); update(b"\0"); update(hexdigest-ascii); update(b"\n")`.
//! - **semantic hash** (volatile-strip tier): same, but every `.json` first
//!   has `createdAt`/`modifiedAt` stripped recursively and is re-serialized
//!   with the normalizer; binary files are hashed raw.
//!
//! The ledger (`lastSyncedHash`) stores the semantic hash. Never hash or
//! compare zip containers — only extracted trees.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

use crate::normalize::{dumps, VOLATILE_KEYS};
use crate::{Error, Result};

/// Lowercase hex sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Read every file under `root` into `{relpath (with '/') : bytes}`.
pub fn read_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for path in crate::util::walk_files(root)? {
        let rel = crate::util::rel_path(root, &path);
        let content = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        out.insert(rel, content);
    }
    Ok(out)
}

/// sha256 over sorted `(relpath, content-sha256)` pairs — byte-identical to
/// `roundtrip.py::tree_hash`. BTreeMap iteration order (UTF-8 byte order)
/// equals Python's `sorted()` code-point order.
pub fn tree_hash(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    for (rel, content) in files {
        h.update(rel.as_bytes());
        h.update(b"\0");
        h.update(sha256_hex(content).as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

/// Recursively drop the volatile keys (`createdAt`, `modifiedAt`) wherever
/// they appear as object keys. Values are untouched otherwise.
pub fn strip_volatile(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !VOLATILE_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), strip_volatile(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_volatile).collect()),
        other => other.clone(),
    }
}

/// Semantic view of a tree: every `.json` parsed, volatile-stripped and
/// re-serialized with the normalizer (+ trailing newline); other files
/// (binaries) pass through raw. Mirrors `roundtrip.py::semantic_files`.
pub fn semantic_view(files: &BTreeMap<String, Vec<u8>>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for (rel, content) in files {
        let content = if rel.ends_with(".json") {
            let value: Value = serde_json::from_slice(content).map_err(|e| Error::Json {
                path: rel.into(),
                source: e,
            })?;
            let mut s = dumps(&strip_volatile(&value));
            s.push('\n');
            s.into_bytes()
        } else {
            content.clone()
        };
        out.insert(rel.clone(), content);
    }
    Ok(out)
}

/// The ledger hash of a directory tree: semantic view + tree hash.
/// This is the value stored as `lastSyncedHash` in the manifest, and it is
/// invariant across in-place export/import cycles (M0-verified).
pub fn semantic_tree_hash(root: &Path) -> Result<String> {
    let files = read_tree(root)?;
    Ok(tree_hash(&semantic_view(&files)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_known_vector() {
        // echo -n "hi" | shasum -a 256
        assert_eq!(
            sha256_hex(b"hi"),
            "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
        );
    }

    #[test]
    fn tree_hash_known_vector_matches_python() {
        // python3: tree_hash({"a.txt": b"hi", "b/c.json": b"{}"}) from
        // roundtrip.py == the constant below (computed with CPython).
        let mut files = BTreeMap::new();
        files.insert("a.txt".to_string(), b"hi".to_vec());
        files.insert("b/c.json".to_string(), b"{}".to_vec());
        assert_eq!(
            tree_hash(&files),
            "aff8dadd273102e06afcd27893f0fb539924d6445010bec649a338f9e494ed8c"
        );
    }

    #[test]
    fn strip_volatile_everywhere() {
        let v = json!({
            "createdAt": "x", "modifiedAt": "y", "keep": 1,
            "nested": {"modifiedAt": 2, "list": [{"createdAt": 3, "z": 4}]}
        });
        assert_eq!(
            strip_volatile(&v),
            json!({"keep": 1, "nested": {"list": [{"z": 4}]}})
        );
    }

    #[test]
    fn semantic_view_leaves_binaries_raw() {
        let mut files = BTreeMap::new();
        files.insert("objects/a.png".to_string(), vec![0x89u8, 0x50, 0x00, 0xff]);
        files.insert(
            "f.json".to_string(),
            br#"{"modifiedAt": "t", "a": 1}"#.to_vec(),
        );
        let sem = semantic_view(&files).unwrap();
        assert_eq!(sem["objects/a.png"], vec![0x89u8, 0x50, 0x00, 0xff]);
        assert_eq!(sem["f.json"], b"{\n  \"a\": 1\n}\n".to_vec());
    }
}
