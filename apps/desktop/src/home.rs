//! N3 lighttable home: the `/__home` landing page plus its supporting
//! same-origin routes — thumbnail serving, the activity/conflict strip, and
//! the reveal-in-file-manager action. All served through the proxy's extra
//! router (auth cookie + RPC for free), plain HTML/vanilla JS (no framework),
//! matching N1's `/__search`.
//!
//! Routes (own):
//! - `GET /__home` — the board grid page (the real post-boot landing view).
//! - `GET /__api/vault/thumb?rel&board&fmt` — serves a board's N2 render from
//!   its `.exports` dir; the served filename (stem) comes from the trusted
//!   exports-state record, so the only client input is a within-vault
//!   `.penpot` path + a board uuid + `png`/`svg`.
//! - `GET /__api/vault/strip` — the activity/conflict strip model (a poll
//!   endpoint: the page refreshes it on a short interval). Fed off the sync
//!   daemon's `SyncStatusSnapshot` (or the MockStatusSource in CI).
//! - `GET /__api/vault/reveal?path=<vault-rel>` — reveal a file/conflict copy
//!   in the OS file manager (reuses the M5 reveal machinery). Path is
//!   constrained to within the vault (no `..`, no absolute escape).
//!
//! The board *listing* itself (`/__api/vault/boards`) lives in the read-only
//! `vault-index` crate; this module only serves the page, the pixels, the
//! strip and the reveal verb.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http::{header, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

use sync_daemon::{FileState, SyncStatusSnapshot};

const HOME_PAGE_HTML: &str = include_str!("home.html");
const PALETTE_PAGE_HTML: &str = include_str!("palette.html");

struct HomeState {
    vault_root: PathBuf,
    strip_rx: watch::Receiver<SyncStatusSnapshot>,
}

/// Build the N3 home routes for the proxy's extra router. `strip_rx` is the
/// late-bound status source (real daemon or mock — see `boot`).
pub fn router(vault_root: impl Into<PathBuf>, strip_rx: watch::Receiver<SyncStatusSnapshot>) -> Router {
    let state = Arc::new(HomeState { vault_root: vault_root.into(), strip_rx });
    Router::new()
        .route("/__home", get(home_page))
        .route("/__palette", get(palette_page))
        .route("/__api/vault/thumb", get(thumb))
        .route("/__api/vault/strip", get(strip))
        .route("/__api/vault/reveal", get(reveal_action))
        .with_state(state)
}

async fn home_page() -> Html<&'static str> {
    Html(HOME_PAGE_HTML)
}

/// The N4 quick-open palette page (shown in the overlay window / tray).
async fn palette_page() -> Html<&'static str> {
    Html(PALETTE_PAGE_HTML)
}

// ---------------------------------------------------------------------------
// Thumbnail serving
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ThumbParams {
    rel: String,
    board: String,
    #[serde(default)]
    fmt: Option<String>,
}

async fn thumb(State(state): State<Arc<HomeState>>, Query(p): Query<ThumbParams>) -> Response {
    // Only png/svg; default png.
    let (ext, content_type) = match p.fmt.as_deref() {
        None | Some("png") => ("png", "image/png"),
        Some("svg") => ("svg", "image/svg+xml"),
        Some(other) => {
            return (StatusCode::BAD_REQUEST, format!("unsupported fmt {other:?}")).into_response()
        }
    };
    // `rel` must be a within-vault `.penpot` path (no traversal).
    if !is_safe_vault_rel(&p.rel) || !p.rel.ends_with(".penpot") {
        return (StatusCode::BAD_REQUEST, "invalid rel path").into_response();
    }
    let vault_root = state.vault_root.clone();
    let rel = p.rel.clone();
    let board = p.board.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        // The served stem comes from the trusted exports-state record.
        vault_index::resolve_thumb_path(&vault_root, &rel, &board, ext)
    })
    .await;
    match resolved {
        Ok(Some(path)) => match tokio::fs::read(&path).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "thumb read failed");
                StatusCode::NOT_FOUND.into_response()
            }
        },
        // No render yet (degraded mode): 404 → the page shows its placeholder.
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "thumb task panicked");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Activity / conflict strip
// ---------------------------------------------------------------------------

/// One strip line, serialized camelCase.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StripRow {
    /// The `.penpot` dir path, relative to the vault.
    pub rel_path: String,
    /// Display name (the basename).
    pub name: String,
    /// One of: synced | pending | importing | exporting | conflict | error.
    pub state: String,
    /// Conflict copy path / error message, when applicable.
    pub detail: Option<String>,
    /// True for the first-class conflict state.
    pub is_conflict: bool,
    /// For a conflict: the path to reveal the preserved DB copy (vault-rel).
    pub conflict_copy_path: Option<String>,
}

/// The whole strip model, serialized camelCase.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StripModel {
    pub last_sync_at: Option<String>,
    pub paused: bool,
    pub last_error: Option<String>,
    pub conflicts: usize,
    pub rows: Vec<StripRow>,
}

/// The trailing path component (basename) of a `/`-separated vault-relative
/// path.
fn basename(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Pure: turn a daemon snapshot into the strip model. Deterministic (the
/// snapshot's `files` map is already sorted by path).
pub fn build_strip_model(snap: &SyncStatusSnapshot) -> StripModel {
    let mut conflicts = 0usize;
    let rows = snap
        .files
        .iter()
        .map(|(rel_path, fs)| {
            let (state, detail, is_conflict, copy) = match fs {
                FileState::Synced => ("synced", None, false, None),
                FileState::Pending => ("pending", None, false, None),
                FileState::Importing => ("importing", None, false, None),
                FileState::Exporting => ("exporting", None, false, None),
                FileState::Conflict { copy_path } => {
                    conflicts += 1;
                    ("conflict", Some(copy_path.clone()), true, Some(copy_path.clone()))
                }
                FileState::Error { message } => ("error", Some(message.clone()), false, None),
            };
            StripRow {
                name: basename(rel_path).to_string(),
                rel_path: rel_path.clone(),
                state: state.to_string(),
                detail,
                is_conflict,
                conflict_copy_path: copy,
            }
        })
        .collect();
    StripModel {
        last_sync_at: snap.last_sync_at.clone(),
        paused: snap.paused,
        last_error: snap.last_error.clone(),
        conflicts,
        rows,
    }
}

async fn strip(State(state): State<Arc<HomeState>>) -> Response {
    let snap = state.strip_rx.borrow().clone();
    Json(build_strip_model(&snap)).into_response()
}

// ---------------------------------------------------------------------------
// Reveal in file manager (reuses the M5 reveal machinery)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RevealParams {
    path: String,
}

/// True iff `rel` is a safe within-vault relative path: not empty, not
/// absolute, and with no `.`/`..`/prefix components (so a join can never
/// escape the vault root).
pub fn is_safe_vault_rel(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let p = Path::new(rel);
    p.components().all(|c| matches!(c, Component::Normal(_)))
}

async fn reveal_action(
    State(state): State<Arc<HomeState>>,
    Query(p): Query<RevealParams>,
) -> Response {
    if !is_safe_vault_rel(&p.path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "path must be a within-vault relative path"})),
        )
            .into_response();
    }
    let abs = state.vault_root.join(&p.path);
    // Fire-and-forget reveal (GUI-only; a no-op error is logged, never fatal).
    // The target may be a `.penpot` dir or a `.conflict-…penpot` copy.
    crate::reveal::reveal(&abs);
    tracing::info!(path = %abs.display(), "N3 reveal requested");
    Json(json!({"ok": true, "revealed": p.path})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn snap(files: &[(&str, FileState)], last_sync: Option<&str>) -> SyncStatusSnapshot {
        SyncStatusSnapshot {
            last_sync_at: last_sync.map(str::to_string),
            files: files.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>(),
            paused: false,
            last_error: None,
        }
    }

    #[test]
    fn strip_model_maps_every_state_and_counts_conflicts() {
        let s = snap(
            &[
                ("Client A/home.penpot", FileState::Synced),
                ("Client A/brand.penpot", FileState::Exporting),
                (
                    "Client B/camp.penpot",
                    FileState::Conflict {
                        copy_path: "Client B/camp.conflict-2026-07-14T00-00-00Z.penpot".into(),
                    },
                ),
                ("Client C/x.penpot", FileState::Error { message: "backend 502".into() }),
            ],
            Some("2026-07-14T10:00:00Z"),
        );
        let m = build_strip_model(&s);
        assert_eq!(m.rows.len(), 4);
        assert_eq!(m.conflicts, 1);
        assert_eq!(m.last_sync_at.as_deref(), Some("2026-07-14T10:00:00Z"));
        // Rows are in path order (BTreeMap): brand, home, camp, x.
        let by_name = |n: &str| m.rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by_name("home.penpot").state, "synced");
        assert_eq!(by_name("brand.penpot").state, "exporting");
        let conflict = by_name("camp.penpot");
        assert_eq!(conflict.state, "conflict");
        assert!(conflict.is_conflict);
        assert_eq!(
            conflict.conflict_copy_path.as_deref(),
            Some("Client B/camp.conflict-2026-07-14T00-00-00Z.penpot")
        );
        let err = by_name("x.penpot");
        assert_eq!(err.state, "error");
        assert_eq!(err.detail.as_deref(), Some("backend 502"));
    }

    #[test]
    fn empty_snapshot_is_an_empty_strip() {
        let m = build_strip_model(&SyncStatusSnapshot::default());
        assert!(m.rows.is_empty());
        assert_eq!(m.conflicts, 0);
        assert_eq!(m.last_sync_at, None);
    }

    #[test]
    fn basename_of_nested_and_flat_paths() {
        assert_eq!(basename("Client A/home.penpot"), "home.penpot");
        assert_eq!(basename("root.penpot"), "root.penpot");
    }

    #[test]
    fn safe_vault_rel_rejects_traversal_and_absolute() {
        assert!(is_safe_vault_rel("Client A/home.penpot"));
        assert!(is_safe_vault_rel("a.conflict-2026.penpot"));
        assert!(!is_safe_vault_rel(""));
        assert!(!is_safe_vault_rel("../etc/passwd"));
        assert!(!is_safe_vault_rel("/etc/passwd"));
        assert!(!is_safe_vault_rel("a/../../b"));
        assert!(!is_safe_vault_rel("./a"));
    }
}
