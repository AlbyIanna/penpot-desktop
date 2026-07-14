//! The proxy-served HTTP surface: `/__api/vault/search`, `/__api/vault/status`
//! and the minimal `/__search` results page (plain HTML/vanilla JS — the
//! lighttable comes in N3; the value here is the query). Merged into the
//! proxy's extra router by the desktop app, so it is same-origin with the
//! SPA: hit links are ordinary `/#/workspace?…` navigations.

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

use crate::db::{SearchError, SearchHandle};
use crate::{query, IndexStatusSnapshot, VaultIndexHandle};

const SEARCH_PAGE_HTML: &str = include_str!("search_page.html");

/// Hard cap on `limit` (the page asks for 50).
const MAX_LIMIT: usize = 200;

struct RouterState {
    search: SearchHandle,
    status_rx: watch::Receiver<IndexStatusSnapshot>,
    team_id: String,
}

/// Build the vault routes for the proxy's extra router. `team_id` is the
/// provisioned single user's default team (deep links need it).
pub fn router(handle: &VaultIndexHandle, team_id: impl Into<String>) -> Router {
    let state = Arc::new(RouterState {
        search: handle.searcher(),
        status_rx: handle.status(),
        team_id: team_id.into(),
    });
    Router::new()
        .route("/__api/vault/search", get(search))
        .route("/__api/vault/status", get(status))
        .route("/__search", get(search_page))
        .with_state(state)
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
