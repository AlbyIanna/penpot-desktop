//! D2: the mutation verbs behind `/__home`. Penpot's dashboard is no longer
//! the way a single user manages their files — this module is.
//!
//! Everything here is a straight RPC passthrough: create/rename/move change
//! the DB, and the sync daemon carries the change to the folder tree on its
//! normal poll. Delete is different (it must also touch the vault) and is a
//! later task — deliberately not implemented here.
//!
//! Route shape and registration follow `packages.rs` (the E2/E7 precedent):
//! a `pub fn router(Arc<State>)` merged into the proxy's extra router in
//! `lib.rs::boot`, JSON in / JSON out, blocking work in `spawn_blocking`
//! (none of these four verbs need it — they are pure RPC passthroughs with
//! no local filesystem work).

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use http::StatusCode;
use penpot_rpc::{Auth, PenpotClient};
use serde::Deserialize;
use serde_json::json;

/// Longest name we accept. Penpot itself is more permissive; this is about
/// keeping the derived on-disk directory name sane across filesystems.
const MAX_NAME_LEN: usize = 200;

pub struct ManageState {
    pub backend_base: String,
    pub token: Option<String>,
    pub team_id: String,
    pub vault_root: PathBuf,
    /// Lets a later task (delete) pause the daemon across a two-step
    /// operation. Not wired at boot yet — see `lib.rs::boot` — so this is
    /// `None` until that task threads a real `SyncControl` through.
    pub sync: Option<sync_daemon::SyncControl>,
}

impl ManageState {
    fn client(&self) -> Option<PenpotClient> {
        self.token
            .clone()
            .map(|t| PenpotClient::new(&self.backend_base).with_auth(Auth::Token(t)))
    }
}

/// Validate a user-supplied project/file name.
///
/// This name becomes a DIRECTORY NAME in the user's folder tree once the
/// daemon exports it, so separators and traversal segments are rejected here
/// rather than sanitised — silently rewriting what the user typed would make
/// the name on screen disagree with the name on disk.
pub fn valid_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("name must be at most {MAX_NAME_LEN} characters"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name must not contain path separators".into());
    }
    if name == "." || name == ".." || name.split('/').any(|s| s == "..") {
        return Err("name must not be a path traversal segment".into());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("name must not contain control characters".into());
    }
    Ok(name.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewProjectReq {
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewFileReq {
    pub project_id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameReq {
    /// "file" or "project"
    pub kind: String,
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveReq {
    pub file_ids: Vec<String>,
    pub project_id: String,
}

fn bad_request(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn upstream_error(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn no_token() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "no access token — the stack is still provisioning" })),
    )
        .into_response()
}

async fn new_project(State(st): State<Arc<ManageState>>, Json(req): Json<NewProjectReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    match client.create_project(&st.team_id, &name).await {
        Ok(p) => Json(json!({ "projectId": p.id, "name": name })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn new_file(State(st): State<Arc<ManageState>>, Json(req): Json<NewFileReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    match client.create_file(&req.project_id, &name).await {
        Ok(f) => Json(json!({ "fileId": f.id, "name": name })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn rename(State(st): State<Arc<ManageState>>, Json(req): Json<RenameReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };
    let res = match req.kind.as_str() {
        "file" => client.rename_file(&req.id, &name).await.map(|_| ()),
        "project" => client.rename_project(&req.id, &name).await,
        other => return bad_request(format!("unknown kind {other:?}")),
    };
    match res {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => upstream_error(e),
    }
}

async fn move_files(State(st): State<Arc<ManageState>>, Json(req): Json<MoveReq>) -> Response {
    if req.file_ids.is_empty() {
        return bad_request("fileIds must not be empty");
    }
    let Some(client) = st.client() else { return no_token() };
    let ids: Vec<&str> = req.file_ids.iter().map(|s| s.as_str()).collect();
    match client.move_files(&ids, &req.project_id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => upstream_error(e),
    }
}

/// Build the D2 manage routes for the proxy's extra router.
pub fn router(state: Arc<ManageState>) -> Router {
    Router::new()
        .route("/__api/vault/manage/project", post(new_project))
        .route("/__api/vault/manage/file", post(new_file))
        .route("/__api/vault/manage/rename", post(rename))
        .route("/__api/vault/manage/move", post(move_files))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_trims_and_accepts_ordinary_names() {
        assert_eq!(valid_name("  Homepage  ").unwrap(), "Homepage");
        assert_eq!(valid_name("Client X — v2").unwrap(), "Client X — v2");
    }

    #[test]
    fn valid_name_rejects_empty_and_overlong() {
        assert!(valid_name("").is_err());
        assert!(valid_name("   ").is_err());
        assert!(valid_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn valid_name_rejects_path_separators_and_traversal() {
        // The name becomes a DIRECTORY NAME on disk once the daemon exports
        // it, so a separator or a traversal segment would write outside the
        // project folder.
        for bad in ["a/b", "a\\b", "..", ".", "../escape", "x/../y"] {
            assert!(valid_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn valid_name_rejects_control_characters_and_nul() {
        assert!(valid_name("a\nb").is_err());
        assert!(valid_name("a\0b").is_err());
    }
}
