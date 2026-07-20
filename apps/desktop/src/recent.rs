//! D3 Task 3: the "recently opened" store — what the File > Open Recent menu
//! reads from.
//!
//! Nothing today records "the user opened this file". The vault index's
//! `Sort::Recency` orders by `last_synced_at`, which is when the sync daemon
//! last wrote the file to disk — a fact about the sync daemon, not about the
//! user. Reusing it for Open Recent would list files the user never opened
//! (and omit ones opened but not yet re-synced), so this is a dedicated
//! store fed only by the "a window opened this file" event.
//!
//! It lives in the app **data dir**, not the vault: it's per-machine UI
//! state, not user work, and must not travel with a cloned vault — the same
//! reasoning as E7's consent ledger (`crates/sync-core/src/consent.rs`).
//! Unlike the consent ledger, a broken recent-files file is not a security
//! boundary — it's a convenience menu — so [`list_recent`] never surfaces an
//! error: a missing or corrupt store just means an empty Open Recent list,
//! never a reason to fail opening the app.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name of the recent-files store, at the root of the app's DATA dir
/// (NOT the vault).
pub const RECENT_FILE_NAME: &str = "recent-files.json";

/// Cap on stored entries. Applied on every [`record_open`], so the file on
/// disk never grows past this regardless of how many files get opened over
/// the app's lifetime.
pub const RECENT_LIMIT: usize = 10;

/// One "the user opened this file" event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub file_id: String,
    pub title: String,
    pub page_id: Option<String>,
    /// RFC 3339 UTC timestamp of when this open happened.
    pub opened_at: String,
}

/// On-disk shape: newest-first list of entries. A bare array (rather than an
/// object with a schema-version envelope like the consent ledger) is
/// deliberate — this is disposable UI convenience state, not a document
/// whose shape needs to evolve safely across versions.
type Store = Vec<RecentEntry>;

fn store_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(RECENT_FILE_NAME)
}

/// Record that `entry.file_id` was just opened: move it to the front if
/// already present (no duplicates), otherwise insert it at the front, then
/// truncate to [`RECENT_LIMIT`] and write back atomically.
pub fn record_open(data_dir: &Path, entry: RecentEntry) -> anyhow::Result<()> {
    // A corrupt existing store must not block recording a fresh open — start
    // from empty in that case, same degrade-don't-fail posture as
    // `list_recent`. A missing data dir is created below by `atomic_write`.
    let mut store: Store = read_store(data_dir).unwrap_or_default();
    store.retain(|e| e.file_id != entry.file_id);
    store.insert(0, entry);
    store.truncate(RECENT_LIMIT);

    let path = store_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&store)?;
    atomic_write(&path, &json)?;
    Ok(())
}

/// The recent-files list, newest first, capped at `limit`. Never errors: a
/// missing or corrupt store degrades to an empty list — a broken piece of
/// UI-state must never stop the app from opening.
pub fn list_recent(data_dir: &Path, limit: usize) -> Vec<RecentEntry> {
    let mut store = read_store(data_dir).unwrap_or_default();
    store.truncate(limit);
    store
}

/// Read and parse the store, if present and valid JSON. `Ok(None)`-shaped
/// callers don't exist here on purpose — every caller in this module wants
/// "give me a `Store`, empty if anything went wrong", so this returns
/// `Option` and both callers fold it with `unwrap_or_default`.
fn read_store(data_dir: &Path) -> Option<Store> {
    let bytes = std::fs::read(store_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write `bytes` to `path` atomically: write a sibling `.tmp` file, fsync
/// it, then rename over `path`. Same shape as `sync-core`'s crate-private
/// helper of the same name — not reused because it isn't exported past that
/// crate's boundary, and this store's atomicity needs are identical but
/// don't warrant a new shared crate.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, at: &str) -> RecentEntry {
        RecentEntry { file_id: id.into(), title: id.to_uppercase(), page_id: None, opened_at: at.into() }
    }

    #[test]
    fn most_recent_first() {
        let tmp = tempfile::tempdir().unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T10:00:00Z")).unwrap();
        record_open(tmp.path(), entry("b", "2026-07-20T11:00:00Z")).unwrap();
        let ids: Vec<String> = list_recent(tmp.path(), 10).into_iter().map(|e| e.file_id).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn reopening_moves_to_front_without_duplicating() {
        let tmp = tempfile::tempdir().unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T10:00:00Z")).unwrap();
        record_open(tmp.path(), entry("b", "2026-07-20T11:00:00Z")).unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T12:00:00Z")).unwrap();
        let ids: Vec<String> = list_recent(tmp.path(), 10).into_iter().map(|e| e.file_id).collect();
        assert_eq!(ids, vec!["a", "b"], "reopen must move to front, not duplicate");
    }

    #[test]
    fn the_list_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(RECENT_LIMIT + 5) {
            record_open(tmp.path(), entry(&format!("f{i}"), &format!("2026-07-20T10:{i:02}:00Z"))).unwrap();
        }
        assert_eq!(list_recent(tmp.path(), RECENT_LIMIT).len(), RECENT_LIMIT);
    }

    #[test]
    fn a_missing_or_corrupt_store_is_an_empty_list_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_recent(tmp.path(), 10).is_empty());
        std::fs::write(tmp.path().join(RECENT_FILE_NAME), b"{ this is not json").unwrap();
        assert!(list_recent(tmp.path(), 10).is_empty(), "corrupt store must degrade, not panic");
    }
}
