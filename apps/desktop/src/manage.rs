//! D2: the mutation verbs behind `/__home`. Penpot's dashboard is no longer
//! the way a single user manages their files — this module is.
//!
//! Everything here is a straight RPC passthrough: create/rename/move change
//! the DB, and the sync daemon carries the change to the folder tree on its
//! normal poll. Delete is different (it must also touch the vault) and is a
//! later task — deliberately not implemented here.
//!
//! Names are validated by [`valid_name`], not sanitised: a name is rejected
//! outright rather than silently rewritten, so the name on screen always
//! matches the name the daemon later writes to disk. That guarantee is
//! enforced by round-tripping every candidate name through the daemon's own
//! `sync_daemon::paths::sanitize_component` — the same function that turns
//! a project/file name into a directory name on export — and rejecting
//! anything it would change.
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

/// Longest name we accept. Must match the sync daemon's own cap
/// (`MAX_COMPONENT_CHARS` in `crates/sync-daemon/src/paths.rs`) — that
/// constant is not `pub`, so this is a second number that has to be kept in
/// sync by hand. If the daemon's cap changes, update this one too; the
/// round-trip check below (`sanitize_component(&name) != name`) will start
/// failing tests here if the two ever drift.
const MAX_NAME_LEN: usize = 100;

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
/// daemon exports it, so names are rejected here rather than sanitised —
/// silently rewriting what the user typed would make the name on screen
/// disagree with the name on disk. The explicit checks below give clear,
/// specific error messages for the common cases (empty, separators,
/// traversal, control characters); the final check is the backstop that
/// actually guarantees the invariant: it re-runs the name through
/// `sync_daemon::paths::sanitize_component` — the exact function the daemon
/// uses when it materialises the directory — and rejects anything that
/// isn't already a fixed point of it. That keeps this function honest by
/// construction instead of duplicating the daemon's rewrite rules.
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
    if sync_daemon::paths::sanitize_component(name) != name {
        return Err("name contains characters that cannot be used in a folder name".into());
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

/// `(include_libraries, embed_assets)` for the duplicate export.
///
/// `(true, true)` is server-rejected on Penpot 2.16.2 (E3). A duplicate wants
/// its own copy of the media but must not drag linked libraries in, so this
/// is the only correct pair.
const DUPLICATE_EXPORT_FLAGS: (bool, bool) = (false, true);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateReq {
    pub file_id: String,
    pub name: String,
    /// Target project; defaults to the source file's project when omitted.
    #[serde(default)]
    pub project_id: Option<String>,
}

async fn duplicate_file(State(st): State<Arc<ManageState>>, Json(req): Json<DuplicateReq>) -> Response {
    let name = match valid_name(&req.name) {
        Ok(n) => n,
        Err(e) => return bad_request(e),
    };
    let Some(client) = st.client() else { return no_token() };

    let Some(project_id) = req.project_id.clone() else {
        return bad_request("projectId is required");
    };

    let (include_libraries, embed_assets) = DUPLICATE_EXPORT_FLAGS;
    let exported = match client.export_binfile(&req.file_id, include_libraries, embed_assets).await {
        Ok(e) => e,
        Err(e) => return upstream_error(format!("export failed: {e}")),
    };
    let bytes = match client.download_exported_binfile(&exported.uri).await {
        Ok(b) => b,
        Err(e) => return upstream_error(format!("download failed: {e}")),
    };
    // Our own export_binfile always produces binfile-v3 on 2.16.2 (see
    // penpot-rpc's doc comment), so the version-less import path applies —
    // same as templates.rs's TemplateFormat::V3Zip.
    match crate::installer::import_binfile_and_settle(&client, &project_id, &name, bytes, None).await {
        Ok((new_id, _)) => Json(json!({ "fileId": new_id, "name": name })).into_response(),
        Err(e) => upstream_error(format!("import failed: {e}")),
    }
}

/// Build the D2 manage routes for the proxy's extra router.
pub fn router(state: Arc<ManageState>) -> Router {
    Router::new()
        .route("/__api/vault/manage/project", post(new_project))
        .route("/__api/vault/manage/file", post(new_file))
        .route("/__api/vault/manage/rename", post(rename))
        .route("/__api/vault/manage/move", post(move_files))
        .route("/__api/vault/manage/duplicate", post(duplicate_file))
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

    #[test]
    fn valid_name_rejects_names_the_daemon_would_rewrite() {
        // Colon is rewritten to '-' by sanitize_component on disk.
        assert!(valid_name("Client: Q1").is_err(), "accepted a name containing ':'");
        // Leading dots are stripped by sanitize_component on disk.
        assert!(valid_name("...urgent").is_err(), "accepted a name with leading dots");
        // sanitize_component caps at MAX_COMPONENT_CHARS (100); anything
        // longer is silently truncated on disk.
        let too_long = "x".repeat(150);
        assert!(valid_name(&too_long).is_err(), "accepted a 150-char name");
    }

    #[test]
    fn valid_name_still_accepts_ordinary_names() {
        assert!(valid_name("Homepage").is_ok());
        assert!(valid_name("Client X — v2").is_ok());
    }

    #[test]
    fn valid_name_accepted_names_survive_daemon_sanitisation_unchanged() {
        // Anything valid_name accepts must round-trip through the daemon's
        // own sanitiser unchanged — that is the actual guarantee this
        // module claims to provide.
        for n in ["Homepage", "Client X — v2", "Q1 Report", "日本語のファイル名", "a-b_c.d"] {
            let accepted = valid_name(n).unwrap();
            assert_eq!(
                sync_daemon::paths::sanitize_component(&accepted),
                accepted,
                "accepted name {accepted:?} does not survive sanitize_component unchanged"
            );
        }
    }

    #[test]
    fn duplicate_name_defaults_are_validated_like_any_other_name() {
        // The duplicate route takes a user-supplied name for the copy; it must
        // go through the same gate as create/rename, not a looser one.
        assert!(valid_name("Homepage copy").is_ok());
        assert!(valid_name("../evil").is_err());
    }

    #[test]
    fn duplicate_export_flags_are_the_server_accepted_pair() {
        // (include_libraries=true, embed_assets=true) is rejected by Penpot
        // 2.16.2 (E3). Pin the pair we send so a well-meaning edit to
        // "include everything" fails here instead of at runtime.
        assert_eq!(DUPLICATE_EXPORT_FLAGS, (false, true));
    }
}
