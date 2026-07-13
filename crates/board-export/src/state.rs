//! `.exports-state.json` — the per-exports-dir record of *which source tree
//! hash* the renders came from. The re-render decision is a pure comparison
//! of the manifest's `lastSyncedHash` against this record: **renders happen
//! only when the hash moved** (no churn on idle cycles).
//!
//! The file lives inside `<name>.exports/` and is written together with the
//! rendered assets in one atomic directory swap, so state and outputs can
//! never disagree. Its leading dot keeps it out of the sync daemon's watcher
//! (dot components are ignored) — not that it matters: the whole `.exports`
//! dir is invisible to sync because it isn't a `.penpot` dir.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// File name of the state record inside an exports dir.
pub const STATE_FILE_NAME: &str = ".exports-state.json";

/// Current schema version written by this crate.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// One rendered board (bookkeeping for humans/tools; the decision logic only
/// uses [`ExportsState::rendered_from_hash`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardRecord {
    pub object_id: String,
    pub page_id: String,
    /// Raw Penpot board name.
    pub name: String,
    /// Filename stem actually used (`<stem>.svg` / `<stem>.png`).
    pub file_stem: String,
}

/// The state document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportsState {
    pub schema_version: u32,
    /// Penpot file id these renders belong to.
    pub file_id: String,
    /// The manifest `lastSyncedHash` the renders were made from.
    pub rendered_from_hash: String,
    /// RFC 3339 UTC timestamp of the render.
    pub rendered_at: String,
    #[serde(default)]
    pub boards: Vec<BoardRecord>,
}

impl ExportsState {
    /// Load the state from an exports dir. `None` when the file is missing,
    /// unreadable, malformed, or a different schema version — all of which
    /// simply mean "needs a fresh render" (the state is disposable
    /// bookkeeping, never a source of truth).
    pub fn load(exports_dir: &Path) -> Option<ExportsState> {
        let raw = std::fs::read(exports_dir.join(STATE_FILE_NAME)).ok()?;
        let state: ExportsState = serde_json::from_slice(&raw).ok()?;
        (state.schema_version == STATE_SCHEMA_VERSION).then_some(state)
    }

    /// Serialize with the repo-wide normalization rules (sorted keys, 2-space
    /// indent, LF, trailing newline) so exports dirs are git-diff-friendly.
    pub fn to_bytes(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).expect("state serializes");
        let mut s = sync_core::dumps(&value);
        s.push('\n');
        s.into_bytes()
    }
}

/// THE re-render decision: render iff there is no usable state or the
/// manifest hash moved since the last render. Pure and total.
pub fn needs_render(last_synced_hash: &str, state: Option<&ExportsState>) -> bool {
    match state {
        Some(s) => s.rendered_from_hash != last_synced_hash,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(hash: &str) -> ExportsState {
        ExportsState {
            schema_version: STATE_SCHEMA_VERSION,
            file_id: "f1".into(),
            rendered_from_hash: hash.into(),
            rendered_at: "2026-07-13T00:00:00Z".into(),
            boards: vec![BoardRecord {
                object_id: "b1".into(),
                page_id: "p1".into(),
                name: "Cover".into(),
                file_stem: "Cover".into(),
            }],
        }
    }

    #[test]
    fn decision_table() {
        // No state at all → render.
        assert!(needs_render("h1", None));
        // State from the same hash → up to date, DO NOT render (no churn).
        assert!(!needs_render("h1", Some(&state("h1"))));
        // Hash moved → render.
        assert!(needs_render("h2", Some(&state("h1"))));
    }

    #[test]
    fn save_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = state("h1");
        std::fs::write(tmp.path().join(STATE_FILE_NAME), s.to_bytes()).unwrap();
        assert_eq!(ExportsState::load(tmp.path()), Some(s));
    }

    #[test]
    fn missing_corrupt_or_future_schema_means_render() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing.
        assert_eq!(ExportsState::load(tmp.path()), None);
        // Corrupt JSON.
        std::fs::write(tmp.path().join(STATE_FILE_NAME), b"{nope").unwrap();
        assert_eq!(ExportsState::load(tmp.path()), None);
        // Future schema version.
        let mut s = state("h1");
        s.schema_version = 999;
        std::fs::write(tmp.path().join(STATE_FILE_NAME), s.to_bytes()).unwrap();
        assert_eq!(ExportsState::load(tmp.path()), None);
        assert!(needs_render("h1", ExportsState::load(tmp.path()).as_ref()));
    }

    #[test]
    fn serialized_form_is_normalized() {
        let bytes = state("h1").to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with('\n'));
        assert!(!text.contains('\r'));
        // Sorted keys: boards < fileId < renderedAt < renderedFromHash < schemaVersion.
        let b = text.find("\"boards\"").unwrap();
        let f = text.find("\"fileId\"").unwrap();
        let sv = text.find("\"schemaVersion\"").unwrap();
        assert!(b < f && f < sv);
    }
}
