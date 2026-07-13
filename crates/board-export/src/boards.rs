//! Board (top-level frame) discovery from a `get-file` RPC response.
//!
//! A "board" in Penpot is an object of `type: "frame"` other than the
//! page's root frame (the zero uuid). Pages keep their file order
//! (`data.pages`); boards within a page are sorted by `(name, id)` for a
//! deterministic export order (the `objects` JSON map is unordered).

use serde_json::Value;

/// The root frame id present on every page — never a board.
pub const ROOT_FRAME_ID: &str = "00000000-0000-0000-0000-000000000000";

/// One exportable board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub page_id: String,
    pub object_id: String,
    /// Raw Penpot board name (unsanitized).
    pub name: String,
}

/// Extract every board from a full `get-file` response, in deterministic
/// order (page file-order, then name/id within a page). Tolerant of missing
/// pieces: a malformed page yields no boards rather than an error.
pub fn list_boards(file: &Value) -> Vec<Board> {
    let data = &file["data"];
    let Some(pages) = data["pages"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for page_id in pages.iter().filter_map(Value::as_str) {
        let Some(objects) = data["pagesIndex"][page_id]["objects"].as_object() else {
            continue;
        };
        let mut page_boards: Vec<Board> = objects
            .iter()
            .filter(|(id, obj)| {
                id.as_str() != ROOT_FRAME_ID && obj["type"].as_str() == Some("frame")
            })
            .map(|(id, obj)| Board {
                page_id: page_id.to_string(),
                object_id: id.clone(),
                name: obj["name"].as_str().unwrap_or("Board").to_string(),
            })
            .collect();
        page_boards.sort_by(|a, b| {
            a.name.cmp(&b.name).then_with(|| a.object_id.cmp(&b.object_id))
        });
        out.extend(page_boards);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file_fixture() -> Value {
        json!({
            "id": "f1",
            "data": {
                "pages": ["p2", "p1"],
                "pagesIndex": {
                    "p1": { "objects": {
                        ROOT_FRAME_ID: {"type": "frame", "name": "Root Frame"},
                        "b-zz": {"type": "frame", "name": "Alpha"},
                        "r1": {"type": "rect", "name": "Not a board"}
                    }},
                    "p2": { "objects": {
                        ROOT_FRAME_ID: {"type": "frame", "name": "Root Frame"},
                        "b-2": {"type": "frame", "name": "Cover"},
                        "b-1": {"type": "frame", "name": "Cover"},
                        "txt": {"type": "text", "name": "Title"}
                    }}
                }
            }
        })
    }

    #[test]
    fn boards_in_page_order_then_name_id() {
        let boards = list_boards(&file_fixture());
        // p2 first (file page order), its two "Cover" boards ordered by id;
        // then p1's "Alpha".
        assert_eq!(
            boards
                .iter()
                .map(|b| (b.page_id.as_str(), b.object_id.as_str(), b.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("p2", "b-1", "Cover"),
                ("p2", "b-2", "Cover"),
                ("p1", "b-zz", "Alpha"),
            ]
        );
    }

    #[test]
    fn root_frame_and_non_frames_are_excluded() {
        let boards = list_boards(&file_fixture());
        assert!(boards.iter().all(|b| b.object_id != ROOT_FRAME_ID));
        assert!(boards.iter().all(|b| b.object_id != "r1" && b.object_id != "txt"));
    }

    #[test]
    fn empty_or_malformed_data_yields_no_boards() {
        assert_eq!(list_boards(&json!({})), Vec::new());
        assert_eq!(list_boards(&json!({"data": {"pages": []}})), Vec::new());
        // Page listed but missing from pagesIndex: skipped, not an error.
        assert_eq!(
            list_boards(&json!({"data": {"pages": ["ghost"], "pagesIndex": {}}})),
            Vec::new()
        );
    }

    #[test]
    fn unnamed_board_gets_fallback_name() {
        let file = json!({"data": {"pages": ["p"], "pagesIndex": {"p": {"objects": {
            "b": {"type": "frame"}
        }}}}});
        assert_eq!(list_boards(&file)[0].name, "Board");
    }
}
