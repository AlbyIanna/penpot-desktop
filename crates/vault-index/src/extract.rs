//! Pure extraction of searchable documents from a normalized binfile-v3
//! tree (the on-disk `.penpot` dir layout, verified against the 2.16.2
//! backend's `app/binfile/v3.clj` writer):
//!
//! - `files/<fid>.json` — file document (not indexed; the manifest already
//!   maps file ↔ path).
//! - `files/<fid>/pages/<pid>.json` — page document.
//! - `files/<fid>/pages/<pid>/<shape-id>.json` — one JSON per shape:
//!   `type == "frame"` (≠ root frame) is a **board**, `type == "text"`
//!   carries the rich-text `content` tree.
//! - `files/<fid>/components/<id>.json` — component names.
//! - `files/<fid>/colors/<id>.json` — library color names + hex values.
//! - `files/<fid>/typographies/<id>.json` — typography names (+ font family).
//!
//! Input is the `{relpath: bytes}` map of the tree (use
//! `sync_core::semantic_view` so the extraction sees exactly the bytes the
//! `lastSyncedHash` ledger hashes). Everything here is total: unknown paths
//! and shapes are skipped, missing fields default to empty strings — a
//! malformed file yields fewer docs, never an error.

use serde_json::Value;
use std::collections::BTreeMap;

/// Penpot's root frame pseudo-id (shapes parented here are page-level).
pub const ROOT_FRAME_ID: &str = "00000000-0000-0000-0000-000000000000";

/// What a document is; stored in the index and returned with every hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// A text layer's content (the flagship corpus: search inside designs).
    Text,
    /// A board (top-level frame) name.
    Board,
    /// A library component name.
    Component,
    /// A library color name + value.
    Color,
    /// A typography name + font family.
    Typography,
    /// An installed package (E4): one row per locked package, keyed OUTSIDE the
    /// sync manifest (`pkg:<id>` owner). Its body aggregates the package's
    /// name/id/version plus the searchable names in its `.penpot` source tree,
    /// and it deep-links to the package's materialized vault file. This is the
    /// unit the flat gallery browses — no tier, badge, or ranking beyond bm25.
    Package,
}

impl DocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Text => "text",
            DocKind::Board => "board",
            DocKind::Component => "component",
            DocKind::Color => "color",
            DocKind::Typography => "typography",
            DocKind::Package => "package",
        }
    }
}

/// Build the aggregated search body for a package's `.penpot` source tree: the
/// name (and text body) of every doc the tree yields, whitespace-joined and
/// deduped in first-seen order. Pure over the semantic view (`{relpath: bytes}`
/// as returned by `sync_core::read_tree`/`semantic_view`), so it stays testable
/// and is stable across the import uuid churn (only names/text, never ids).
pub fn package_tree_terms(files: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for doc in extract_docs(files) {
        for term in [doc.name, doc.body] {
            let term = term.trim().to_string();
            if !term.is_empty() && seen.insert(term.clone()) {
                out.push(term);
            }
        }
    }
    out
}

/// One searchable document extracted from a `.penpot` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRow {
    pub kind: DocKind,
    /// Human label (shape/asset name).
    pub name: String,
    /// The FTS-indexed text.
    pub body: String,
    /// Penpot file uuid (from the tree path — the deep-link target).
    pub file_id: String,
    /// Page uuid, when the doc lives on a page (shapes); empty otherwise.
    pub page_id: String,
    /// The shape/asset uuid this doc points at.
    pub object_id: String,
    /// Containing board uuid: the frame itself for boards, the `frameId`
    /// for text layers (may be the root frame id), the main instance for
    /// components; empty when unknown.
    pub board_id: String,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Collect every `"text"` string value in a rich-text `content` tree
/// (root → paragraph-set → paragraph → text nodes), in document order.
fn collect_text(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("text") {
                if !t.is_empty() {
                    out.push(t.clone());
                }
            }
            if let Some(children) = map.get("children") {
                collect_text(children, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

/// The text of a text shape's `content` tree, whitespace-joined.
pub fn text_shape_body(content: &Value) -> String {
    let mut parts = Vec::new();
    collect_text(content, &mut parts);
    parts.join(" ")
}

fn parse_json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok()
}

fn shape_doc(file_id: &str, page_id: &str, v: &Value) -> Option<DocRow> {
    let id = s(v, "id");
    if id.is_empty() {
        return None;
    }
    let name = s(v, "name");
    match v.get("type").and_then(Value::as_str) {
        Some("frame") if id != ROOT_FRAME_ID => Some(DocRow {
            kind: DocKind::Board,
            body: name.clone(),
            name,
            file_id: file_id.to_string(),
            page_id: page_id.to_string(),
            object_id: id.clone(),
            board_id: id,
        }),
        Some("text") => {
            let body = v.get("content").map(text_shape_body).unwrap_or_default();
            if body.is_empty() && name.is_empty() {
                return None;
            }
            Some(DocRow {
                kind: DocKind::Text,
                body: if body.is_empty() { name.clone() } else { body },
                name,
                file_id: file_id.to_string(),
                page_id: page_id.to_string(),
                object_id: id,
                board_id: s(v, "frameId"),
            })
        }
        _ => None,
    }
}

fn component_doc(file_id: &str, object_id: &str, v: &Value) -> Option<DocRow> {
    let name = s(v, "name");
    if name.is_empty() {
        return None;
    }
    let path = s(v, "path");
    Some(DocRow {
        kind: DocKind::Component,
        body: if path.is_empty() { name.clone() } else { format!("{path} {name}") },
        name,
        file_id: file_id.to_string(),
        page_id: s(v, "mainInstancePage"),
        object_id: object_id.to_string(),
        board_id: s(v, "mainInstanceId"),
    })
}

fn color_doc(file_id: &str, object_id: &str, v: &Value) -> Option<DocRow> {
    let name = s(v, "name");
    let color = s(v, "color");
    if name.is_empty() && color.is_empty() {
        return None;
    }
    Some(DocRow {
        kind: DocKind::Color,
        body: [name.as_str(), color.as_str()]
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        name: if name.is_empty() { color } else { name },
        file_id: file_id.to_string(),
        page_id: String::new(),
        object_id: object_id.to_string(),
        board_id: String::new(),
    })
}

fn typography_doc(file_id: &str, object_id: &str, v: &Value) -> Option<DocRow> {
    let name = s(v, "name");
    if name.is_empty() {
        return None;
    }
    let family = s(v, "fontFamily");
    Some(DocRow {
        kind: DocKind::Typography,
        body: if family.is_empty() { name.clone() } else { format!("{name} {family}") },
        name,
        file_id: file_id.to_string(),
        page_id: String::new(),
        object_id: object_id.to_string(),
        board_id: String::new(),
    })
}

/// Extract every searchable doc from a tree (`{relpath: bytes}`, `/`
/// separators, as returned by `sync_core::read_tree`/`semantic_view`).
/// Deterministic: output order follows the BTreeMap's sorted relpaths.
pub fn extract_docs(files: &BTreeMap<String, Vec<u8>>) -> Vec<DocRow> {
    let mut out = Vec::new();
    for (rel, bytes) in files {
        let Some(stem) = rel.strip_suffix(".json") else { continue };
        let parts: Vec<&str> = stem.split('/').collect();
        let doc = match parts.as_slice() {
            ["files", fid, "pages", pid, _sid] => {
                parse_json(bytes).and_then(|v| shape_doc(fid, pid, &v))
            }
            ["files", fid, "components", id] => {
                parse_json(bytes).and_then(|v| component_doc(fid, id, &v))
            }
            ["files", fid, "colors", id] => {
                parse_json(bytes).and_then(|v| color_doc(fid, id, &v))
            }
            ["files", fid, "typographies", id] => {
                parse_json(bytes).and_then(|v| typography_doc(fid, id, &v))
            }
            _ => None,
        };
        if let Some(doc) = doc {
            out.push(doc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(entries: &[(&str, Value)]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(rel, v)| (rel.to_string(), serde_json::to_vec(v).unwrap()))
            .collect()
    }

    const FID: &str = "3a4be581-6d37-8010-8008-51f0c6eb307f";
    const PID: &str = "3a4be581-6d37-8010-8008-51f0c6eb3080";

    /// The exact normalized shapes observed in a real 2.16.2 export
    /// (m0/roundtrip-work) plus a text shape with the upstream rich-text
    /// content structure.
    #[test]
    fn extracts_boards_and_text_from_real_shapes() {
        let board_id = "9b2d22e2-c7fa-4ef6-b74a-d48ee3e6162e";
        let text_id = "eb22daf7-5b9f-40b5-98f3-5cb0b7c70e5a";
        let files = tree(&[
            // file + page docs are present but not indexed
            (&format!("files/{FID}.json"), json!({"id": FID, "name": "homepage"})),
            (&format!("files/{FID}/pages/{PID}.json"), json!({"id": PID, "index": 0, "name": "Page 1"})),
            // the root frame must NOT become a board
            (
                &format!("files/{FID}/pages/{PID}/{ROOT_FRAME_ID}.json"),
                json!({"id": ROOT_FRAME_ID, "type": "frame", "name": "Root Frame"}),
            ),
            (
                &format!("files/{FID}/pages/{PID}/{board_id}.json"),
                json!({
                    "id": board_id, "type": "frame", "name": "Checkout Flow",
                    "frameId": ROOT_FRAME_ID, "parentId": ROOT_FRAME_ID,
                    "x": 0, "y": 0, "width": 800, "height": 600,
                    "fills": [{"fillColor": "#FFFFFF", "fillOpacity": 1}],
                }),
            ),
            (
                &format!("files/{FID}/pages/{PID}/{text_id}.json"),
                json!({
                    "id": text_id, "type": "text", "name": "CTA label",
                    "frameId": board_id, "parentId": board_id,
                    "content": {
                        "type": "root",
                        "children": [{
                            "type": "paragraph-set",
                            "children": [{
                                "type": "paragraph",
                                "children": [
                                    {"text": "Proceed to ", "fontSize": "14"},
                                    {"text": "checkout button", "fontWeight": "700"}
                                ]
                            }]
                        }]
                    },
                }),
            ),
            // a rect must be skipped
            (
                &format!("files/{FID}/pages/{PID}/aaaa0000-0000-0000-0000-000000000001.json"),
                json!({"id": "aaaa0000-0000-0000-0000-000000000001", "type": "rect", "name": "RT Rect A"}),
            ),
        ]);
        let docs = extract_docs(&files);
        assert_eq!(docs.len(), 2, "{docs:?}");
        let board = docs.iter().find(|d| d.kind == DocKind::Board).unwrap();
        assert_eq!(board.name, "Checkout Flow");
        assert_eq!(board.file_id, FID);
        assert_eq!(board.page_id, PID);
        assert_eq!(board.object_id, board_id);
        assert_eq!(board.board_id, board_id);
        let text = docs.iter().find(|d| d.kind == DocKind::Text).unwrap();
        assert_eq!(text.body, "Proceed to  checkout button");
        assert_eq!(text.name, "CTA label");
        assert_eq!(text.board_id, board_id, "text hit points at its containing board");
        assert_eq!(text.page_id, PID);
        assert_eq!(text.object_id, text_id);
    }

    #[test]
    fn extracts_library_assets() {
        let files = tree(&[
            (
                &format!("files/{FID}/components/c-1.json"),
                json!({"id": "c-1", "name": "Primary Button", "path": "Buttons",
                       "mainInstanceId": "mi-1", "mainInstancePage": PID}),
            ),
            (
                &format!("files/{FID}/colors/col-1.json"),
                json!({"id": "col-1", "name": "Brand Teal", "color": "#12b886", "opacity": 1}),
            ),
            // unnamed color still indexed by its value
            (
                &format!("files/{FID}/colors/col-2.json"),
                json!({"id": "col-2", "color": "#ff0000", "opacity": 1}),
            ),
            (
                &format!("files/{FID}/typographies/t-1.json"),
                json!({"id": "t-1", "name": "Heading XL", "fontFamily": "Source Sans Pro",
                       "fontId": "sourcesanspro", "fontSize": "36"}),
            ),
        ]);
        let docs = extract_docs(&files);
        assert_eq!(docs.len(), 4, "{docs:?}");
        let comp = docs.iter().find(|d| d.kind == DocKind::Component).unwrap();
        assert_eq!(comp.body, "Buttons Primary Button");
        assert_eq!(comp.page_id, PID);
        assert_eq!(comp.board_id, "mi-1");
        let color = docs.iter().find(|d| d.name == "Brand Teal").unwrap();
        assert_eq!(color.body, "Brand Teal #12b886");
        let unnamed = docs.iter().find(|d| d.object_id == "col-2").unwrap();
        assert_eq!(unnamed.name, "#ff0000");
        let typo = docs.iter().find(|d| d.kind == DocKind::Typography).unwrap();
        assert_eq!(typo.body, "Heading XL Source Sans Pro");
    }

    #[test]
    fn unicode_text_survives_extraction() {
        let files = tree(&[(
            &format!("files/{FID}/pages/{PID}/t1.json"),
            json!({"id": "t1", "type": "text", "name": "Überschrift",
                   "frameId": ROOT_FRAME_ID,
                   "content": {"type": "root", "children": [{"type": "paragraph-set",
                       "children": [{"type": "paragraph",
                           "children": [{"text": "Diseño 検索 emoji 🎨"}]}]}]}}),
        )]);
        let docs = extract_docs(&files);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].body, "Diseño 検索 emoji 🎨");
    }

    #[test]
    fn malformed_and_alien_files_are_skipped_not_errors() {
        let mut files = tree(&[(
            &format!("files/{FID}/pages/{PID}/ok.json"),
            json!({"id": "ok", "type": "frame", "name": "B"}),
        )]);
        files.insert(format!("files/{FID}/pages/{PID}/broken.json"), b"{nope".to_vec());
        files.insert("manifest.json".to_string(), b"{}".to_vec());
        files.insert("files/weird.txt".to_string(), b"not json".to_vec());
        files.insert(
            format!("files/{FID}/colors/empty.json"),
            serde_json::to_vec(&json!({"id": "empty"})).unwrap(),
        );
        let docs = extract_docs(&files);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "B");
    }

    #[test]
    fn empty_text_shape_with_a_name_indexes_the_name() {
        let files = tree(&[(
            &format!("files/{FID}/pages/{PID}/t1.json"),
            json!({"id": "t1", "type": "text", "name": "Placeholder", "frameId": ROOT_FRAME_ID,
                   "content": {"type": "root", "children": []}}),
        )]);
        let docs = extract_docs(&files);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].body, "Placeholder");
    }
}
