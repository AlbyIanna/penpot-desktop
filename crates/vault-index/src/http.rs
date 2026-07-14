//! The proxy-served HTTP surface: `/__api/vault/search`, `/__api/vault/status`
//! and the minimal `/__search` results page (plain HTML/vanilla JS — the
//! lighttable comes in N3; the value here is the query). Merged into the
//! proxy's extra router by the desktop app, so it is same-origin with the
//! SPA: hit links are ordinary `/#/workspace?…` navigations.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::boards::{self, FileMeta, Sort};
use crate::db::{SearchError, SearchHandle};
use crate::{query, IndexStatusSnapshot, VaultIndexHandle};

const SEARCH_PAGE_HTML: &str = include_str!("search_page.html");

/// Hard cap on `limit` (the page asks for 50).
const MAX_LIMIT: usize = 200;

struct RouterState {
    search: SearchHandle,
    status_rx: watch::Receiver<IndexStatusSnapshot>,
    team_id: String,
    /// The vault root (designs dir) — the boards listing reads the manifest
    /// and exports-state records from here (read-only).
    vault_root: PathBuf,
}

/// Build the vault routes for the proxy's extra router. `team_id` is the
/// provisioned single user's default team (deep links need it); `vault_root`
/// is the designs dir the boards listing joins the manifest + exports state
/// from.
pub fn router(
    handle: &VaultIndexHandle,
    team_id: impl Into<String>,
    vault_root: impl Into<PathBuf>,
) -> Router {
    let state = Arc::new(RouterState {
        search: handle.searcher(),
        status_rx: handle.status(),
        team_id: team_id.into(),
        vault_root: vault_root.into(),
    });
    Router::new()
        .route("/__api/vault/search", get(search))
        .route("/__api/vault/boards", get(list_boards))
        .route("/__api/vault/status", get(status))
        .route("/__search", get(search_page))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct BoardsParams {
    project: Option<String>,
    sort: Option<String>,
}

/// `GET /__api/vault/boards` — the lighttable listing (N3). Joins the index's
/// board rows with the manifest (project + recency) and exports state
/// (thumbnails), all read-only from disk. Returns the same-shaped payload a
/// rebuilt index would (deterministic).
async fn list_boards(
    State(state): State<Arc<RouterState>>,
    Query(params): Query<BoardsParams>,
) -> Response {
    let search = state.search.clone();
    let vault_root = state.vault_root.clone();
    let team_id = state.team_id.clone();
    let sort = Sort::parse(params.sort.as_deref());
    let project = params.project.clone();
    // rusqlite + fs are synchronous: keep them off the async worker.
    let result = tokio::task::spawn_blocking(move || -> Result<_, SearchError> {
        let rows = search.all_boards()?;
        // Load the manifest once for project + recency metadata.
        let manifest = sync_core::Manifest::load(&vault_root)
            .map_err(|e| SearchError::Other(anyhow::anyhow!("{e}")))?
            .unwrap_or_default();
        let meta: std::collections::BTreeMap<String, FileMeta> = manifest
            .files
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    FileMeta {
                        project: e.project_name.clone(),
                        rel_path: e.path.clone(),
                        last_synced_at: e.last_synced_at.clone(),
                    },
                )
            })
            .collect();
        // One exports-state read per file (not per board) for the stem maps.
        let mut stem_maps: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
            std::collections::BTreeMap::new();
        for (owner, m) in &meta {
            stem_maps.insert(owner.clone(), boards::load_stem_map(&vault_root, &m.rel_path));
        }
        let listing = boards::assemble_cards(
            &rows,
            &meta,
            &team_id,
            |owner, board_id| {
                let m = meta.get(owner)?;
                let stem_map = stem_maps.get(owner)?;
                if !stem_map.contains_key(board_id) {
                    return None;
                }
                // A render row exists AND the png is on disk → real thumb.
                let png = vault_root
                    .join(boards::exports_rel_path(&m.rel_path))
                    .join(format!("{}.png", stem_map.get(board_id).unwrap()));
                png.is_file().then(|| boards::thumb_url(&m.rel_path, board_id))
            },
            project.as_deref(),
            sort,
        );
        Ok(listing)
    })
    .await;
    match result {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err(SearchError::NotReady)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "index not ready yet"})),
        )
            .into_response(),
        Ok(Err(SearchError::Other(e))) => {
            tracing::error!(error = format!("{e:#}"), "vault boards listing failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "boards listing failed"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "vault boards task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "boards listing failed"})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    kind: Option<String>,
    limit: Option<usize>,
}

async fn search(
    State(state): State<Arc<RouterState>>,
    Query(params): Query<SearchParams>,
) -> Response {
    let Some(match_expr) = query::build_match_query(&params.q) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing or empty query parameter q"})),
        )
            .into_response();
    };
    let limit = params.limit.unwrap_or(50).min(MAX_LIMIT);
    let kind = params.kind.clone();
    let search_handle = state.search.clone();
    let started = Instant::now();
    // rusqlite is synchronous: keep it off the async worker.
    let result = tokio::task::spawn_blocking(move || {
        search_handle.search(&match_expr, kind.as_deref(), limit)
    })
    .await;
    let took_ms = started.elapsed().as_secs_f64() * 1000.0;
    let hits = match result {
        Ok(Ok(hits)) => hits,
        Ok(Err(SearchError::NotReady)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "index not ready yet"})),
            )
                .into_response();
        }
        Ok(Err(SearchError::Other(e))) => {
            tracing::error!(error = format!("{e:#}"), "vault search failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "search failed"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "vault search task panicked");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "search failed"})),
            )
                .into_response();
        }
    };
    let hits: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|h| {
            let deep_link = query::workspace_deep_link(
                &state.team_id,
                &h.file_id,
                (!h.page_id.is_empty()).then_some(h.page_id.as_str()),
            );
            let mut v = serde_json::to_value(&h).expect("hit serializes");
            v["deepLink"] = json!(deep_link);
            v
        })
        .collect();
    Json(json!({
        "query": params.q,
        "tookMs": took_ms,
        "count": hits.len(),
        "hits": hits,
    }))
    .into_response()
}

async fn status(State(state): State<Arc<RouterState>>) -> Response {
    let s = state.status_rx.borrow().clone();
    Json(json!({
        "filesIndexed": s.files_indexed,
        "filesPending": s.files_pending,
        "docsTotal": s.docs_total,
        "mutations": s.mutations,
        "lastIndexAt": s.last_index_at,
        "lastError": s.last_error,
    }))
    .into_response()
}

async fn search_page() -> Html<&'static str> {
    Html(SEARCH_PAGE_HTML)
}
