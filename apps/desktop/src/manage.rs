//! D2: the mutation verbs behind `/__home`. Penpot's dashboard is no longer
//! the way a single user manages their files — this module is.
//!
//! Create/rename/move are straight RPC passthroughs: they change the DB, and
//! the sync daemon carries the change to the folder tree on its normal poll.
//!
//! Delete is the one verb that touches the vault. It must, because the core
//! invariant resurrects anything on disk but missing from the DB, and "the
//! user deleted this" is indistinguishable from "the DB was wiped". It runs
//! with the sync daemon PAUSED because both possible orderings expose a state
//! the daemon would otherwise repair: trash-then-delete looks like a new file
//! in the DB and gets re-exported, delete-then-trash looks like a wiped DB and
//! gets re-imported.
//!
//! "Paused" here means [`sync_daemon::SyncControl::pause_and_wait_idle`], not
//! the bare flag flip — a poll cycle or startup reconciliation already in
//! flight when we ask to pause keeps writing to disk/DB until it finishes,
//! flag or no flag, so the delete route waits for the daemon's own ack that
//! it is genuinely idle before touching anything (a timeout is a hard error,
//! never a silent proceed). The whole guarded operation additionally runs in
//! a spawned task the request handler merely awaits, so a client disconnect
//! can't cut the pause/trash sequence short — see `delete_file`'s and
//! `PauseGuard`'s doc comments for both.
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
use std::sync::{Arc, OnceLock};

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
    /// Late-bound pause/resume handle for `delete_file`.
    ///
    /// `boot` (`lib.rs`) merges this router into the proxy BEFORE it spawns
    /// the sync daemon — see the comment at the merge site — so a real
    /// `SyncControl` cannot exist yet when `ManageState` is constructed and
    /// handed to axum as an immutable `Arc`. This starts empty; `boot` holds
    /// the same `Arc<OnceLock<_>>` and calls `.set()` on it the moment the
    /// daemon spawns (mirrors `home.rs`'s late-bound `strip_rx`, just with a
    /// `OnceLock` instead of a `watch` channel since there's nothing to
    /// stream — one value, set once).
    ///
    /// `delete_file` REJECTS a request that arrives before this is set,
    /// rather than either silently skipping the pause (which would corrupt
    /// state per the module docs above) or blocking indefinitely (the daemon
    /// never spawns at all when boot has no access token / default team —
    /// see `lib.rs::boot`'s `sync_daemon` match — so blocking could hang a
    /// request forever).
    pub sync: Arc<OnceLock<sync_daemon::SyncControl>>,
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
    /// Target project. REQUIRED in practice — omitting it is a 400, not a
    /// fallback to the source file's project. Looking that up would cost an
    /// extra round trip, and the caller always knows it: the home page reads
    /// it off the card it just clicked.
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReq {
    pub file_id: String,
}

/// Compact, filesystem-safe stamp (`YYYYMMDD-HHMMSS`) from a unix timestamp.
/// Split out from the handler so it is testable without a clock.
pub fn trash_stamp_from(unix_secs: i64) -> String {
    // Deliberately not RFC3339: colons are illegal in directory names on
    // Windows and awkward everywhere else.
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Avoids pulling in `chrono`
/// just to name a directory.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Ensures a daemon pause is always released: on the happy path, on any
/// `Err` from the RPC delete or the trash move, and even if the code in
/// between panics. `Drop` is the only one of the three that a plain
/// `pause(); ...; resume();` pair cannot give you — an early `?` return
/// already skips a bare trailing `resume()`, and a panic skips it too.
///
/// Construction goes through `pause_and_wait_idle`, not the bare `pause()`
/// flag flip: flipping the flag only stops FUTURE daemon work, it does not
/// wait for a poll cycle or startup reconciliation that is already
/// mid-flight (and mid-write) at the moment we call it. Without that wait
/// the daemon could still be exporting a file to disk between our RPC
/// delete and the trash move — exactly the unprotected midpoint this whole
/// module exists to prevent. A timeout is therefore surfaced to the caller
/// as an error (see `delete_file`), never treated as "probably paused now".
struct PauseGuard(sync_daemon::SyncControl);

impl PauseGuard {
    async fn new(sync: sync_daemon::SyncControl) -> Result<Self, sync_daemon::PauseAckError> {
        match sync.pause_and_wait_idle().await {
            Ok(()) => Ok(PauseGuard(sync)),
            Err(e) => {
                // pause_and_wait_idle already flipped the flag before it hit
                // the timeout; release it ourselves since no guard exists to
                // do it in `Drop` — otherwise a timed-out attempt would leave
                // the daemon paused forever.
                sync.resume();
                Err(e)
            }
        }
    }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        self.0.resume();
    }
}

fn sync_not_ready() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "sync daemon not ready yet — the stack is still starting up" })),
    )
        .into_response()
}

fn pause_not_acknowledged(e: sync_daemon::PauseAckError) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": format!("sync daemon did not confirm it stopped touching the vault: {e}; nothing was deleted")
        })),
    )
        .into_response()
}

/// What can go wrong inside the guarded delete task spawned by `delete_file`
/// (see its doc comment for why the work is spawned rather than run inline).
enum DeleteError {
    /// The daemon never confirmed it was idle — see `PauseGuard::new`.
    /// Nothing was touched: no RPC delete, no trash move.
    PauseNotAcknowledged(sync_daemon::PauseAckError),
    /// The RPC delete or the trash move itself failed — see `delete_inner`.
    Delete(String),
}

async fn delete_file(State(st): State<Arc<ManageState>>, Json(req): Json<DeleteReq>) -> Response {
    let Some(client) = st.client() else { return no_token() };

    // See the `sync` field doc on `ManageState`: reject rather than skip the
    // pause or block forever.
    let Some(sync) = st.sync.get().cloned() else { return sync_not_ready() };

    // The guarded operation (pause-and-wait-idle → RPC delete → trash move →
    // resume) runs in a DETACHED task, and this handler awaits its
    // `JoinHandle` rather than running the work inline: axum drops a
    // handler's future outright if the client disconnects mid-request, but
    // a spawned task keeps running regardless of who is (or isn't) still
    // awaiting it. Run inline, a disconnect could resume the daemon while
    // `trash_file`'s `spawn_blocking` (itself uncancellable) is still moving
    // the directory, or land between the RPC delete and the trash move with
    // no guard left to protect it — the file would be gone from the DB but
    // still on disk, and the next reconciliation would resurrect it as a
    // "new" file.
    let st_for_task = st.clone();
    let file_id = req.file_id.clone();
    let task: tokio::task::JoinHandle<Result<String, DeleteError>> = tokio::spawn(async move {
        let _pause = PauseGuard::new(sync).await.map_err(DeleteError::PauseNotAcknowledged)?;
        delete_inner(&st_for_task, &client, &file_id).await.map_err(DeleteError::Delete)
    });

    match task.await {
        Ok(Ok(rel)) => Json(json!({ "ok": true, "trashedPath": rel })).into_response(),
        Ok(Err(DeleteError::PauseNotAcknowledged(e))) => pause_not_acknowledged(e),
        Ok(Err(DeleteError::Delete(e))) => upstream_error(e),
        Err(join_err) => upstream_error(format!("delete task failed unexpectedly: {join_err}")),
    }
}

async fn delete_inner(
    st: &ManageState,
    client: &PenpotClient,
    file_id: &str,
) -> Result<String, String> {
    // DB first: if this fails, the vault is untouched and the user still has
    // their file. The reverse (trash first) would leave the DB authoritative
    // over an empty tree.
    client.delete_file(file_id).await.map_err(|e| format!("delete-file RPC failed: {e}"))?;

    let stamp = trash_stamp_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let vault = st.vault_root.clone();
    let id = file_id.to_string();
    let outcome = tokio::task::spawn_blocking(move || sync_core::trash::trash_file(&vault, &id, &stamp))
        .await
        .map_err(|e| format!("trash task panicked: {e}"))?
        .map_err(|e| format!("trashing the file failed: {e}"))?;

    Ok(outcome
        .trashed_path
        .strip_prefix(&st.vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| outcome.trashed_path.to_string_lossy().to_string()))
}

/// Build the D2 manage routes for the proxy's extra router.
pub fn router(state: Arc<ManageState>) -> Router {
    Router::new()
        .route("/__api/vault/manage/project", post(new_project))
        .route("/__api/vault/manage/file", post(new_file))
        .route("/__api/vault/manage/rename", post(rename))
        .route("/__api/vault/manage/move", post(move_files))
        .route("/__api/vault/manage/duplicate", post(duplicate_file))
        .route("/__api/vault/manage/delete", post(delete_file))
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

    #[test]
    fn delete_stamp_is_filename_safe() {
        // The stamp lands in a directory name inside .trash/.
        let s = super::trash_stamp_from(1_753_000_000);
        assert!(!s.contains(':'), "colon in {s} breaks on some filesystems");
        assert!(!s.contains('/'), "separator in {s}");
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "unexpected char in {s}");
    }

    #[test]
    fn trash_stamp_from_matches_independently_computed_utc_strings() {
        // Expected values computed via Python's datetime.utcfromtimestamp
        // (a completely separate implementation from civil_from_days), not
        // by re-deriving them from the function under test:
        //   0            -> 1970-01-01 00:00:00 UTC (the epoch itself)
        //   1_000_000_000 -> 2001-09-09 01:46:40 UTC (the well-known "1
        //                    billion seconds" moment)
        //   1_700_000_000 -> 2023-11-14 22:13:20 UTC
        assert_eq!(super::trash_stamp_from(0), "19700101-000000");
        assert_eq!(super::trash_stamp_from(1_000_000_000), "20010909-014640");
        assert_eq!(super::trash_stamp_from(1_700_000_000), "20231114-221320");
    }

    /// Spawn a real (offline) daemon so the pause/resume handle is the
    /// genuine `sync_daemon::SyncControl`, not a test double — mirrors
    /// `status.rs`'s `offline_daemon()` helper. The backend URL is
    /// unroutable, so the engine just retries reconciliation in the
    /// background; the control handle works regardless. The `TempDir` is
    /// returned so the caller can keep it alive for the test's duration —
    /// dropping it cleans the directory up (it previously leaked a
    /// `std::env::temp_dir()` subdirectory on every test run).
    fn offline_sync_control() -> (tempfile::TempDir, sync_daemon::SyncDaemonHandle, sync_daemon::SyncControl) {
        let tmp = tempfile::tempdir().expect("create temp dir for the offline sync daemon fixture");
        let client = penpot_rpc::PenpotClient::new("http://127.0.0.1:9");
        let handle = sync_daemon::spawn(client, sync_daemon::SyncConfig::new(tmp.path(), "team"));
        let control = handle.control();
        (tmp, handle, control)
    }

    #[tokio::test]
    async fn pause_guard_resumes_on_normal_drop() {
        let (_tmp, daemon, control) = offline_sync_control();
        assert!(!control.is_paused());
        {
            // The offline daemon's startup loop hits its `if
            // *pause_rx.borrow()` check (and acks) before it ever tries the
            // network — see the scheduling note on
            // `pause_guard_resumes_even_when_the_guarded_work_panics` below —
            // so this resolves promptly rather than hitting the real
            // `pause_and_wait_idle` timeout.
            let _guard = super::PauseGuard::new(control.clone())
                .await
                .expect("offline daemon acks pause before touching the network");
            assert!(control.is_paused(), "guard construction must pause");
        }
        assert!(!control.is_paused(), "guard drop must resume");
        // NOT `daemon.stop().await`: resuming just above unblocks the
        // daemon's startup reconciliation, which immediately retries against
        // the unroutable backend for real — `stop()`'s shutdown signal is
        // only checked between retry attempts (`with_retry`'s backoff sleep
        // isn't selected against it), so awaiting a graceful stop here would
        // block this test for the retry budget's full ~90s worst case
        // instead of the sub-second run it is today. `drop` mirrors
        // `status.rs`'s `offline_daemon()` helper: the leaked background
        // task is harmless (it only ever talks to 127.0.0.1:9) and dies with
        // the test process.
        drop(daemon);
    }

    #[tokio::test]
    async fn pause_guard_resumes_even_when_the_guarded_work_panics() {
        // The whole point of using a Drop guard instead of a manual
        // pause/.../resume pair: it must release the pause even when the
        // code in between never reaches the resume call, e.g. because it
        // panics (mirrors a bug in delete_inner, not just a returned Err).
        //
        // `PauseGuard::new` is async (it awaits `pause_and_wait_idle`), so a
        // plain `std::panic::catch_unwind` around it can no longer stand in
        // for the guarantee we actually need: `delete_file` now runs the
        // whole guarded operation inside `tokio::spawn` (see its doc
        // comment), and that IS the real panic boundary in production —
        // tokio catches a panicking task's unwind internally, which runs
        // `Drop` for everything on that task's stack exactly like
        // `catch_unwind` would. Mirroring that here tests the actual
        // mechanism instead of a synthetic stand-in for it.
        //
        // Scheduling note: `#[tokio::test]` defaults to the single-threaded
        // (current-thread) runtime, which never interleaves tasks WITHIN one
        // poll — but it also doesn't guarantee poll ORDER between two tasks
        // that are both already queued (the daemon task from
        // `offline_sync_control`, spawned first, and the guard task spawned
        // below). If the daemon happened to get its first poll before the
        // guard task ever calls `pause()`, it would race into the
        // unroutable RPC and not check the flag again until that call times
        // out — well past `PauseGuard::new`'s timeout. So `pause()` is
        // called directly, HERE, synchronously, before spawning anything:
        // that runs as part of this task's current poll, guaranteed to
        // land before either the daemon's or the guard task's first poll.
        let (_tmp, daemon, control) = offline_sync_control();
        control.pause();
        let control_for_panic = control.clone();
        let task = tokio::spawn(async move {
            let _guard = super::PauseGuard::new(control_for_panic)
                .await
                .expect("daemon is already paused above; the ack should be immediate");
            panic!("simulated failure inside the guarded delete operation");
        });
        let result = task.await;
        assert!(result.is_err(), "the panic must have actually happened");
        assert!(result.unwrap_err().is_panic(), "must be a panic, not e.g. a cancellation");
        assert!(!control.is_paused(), "Drop must run and resume even after a panic");
        // See the comment on `drop(daemon)` in the test above: resuming here
        // sends the daemon into a real (slow) retry storm against the
        // unroutable backend, so a graceful `stop().await` would make this
        // test take on the order of a minute instead of milliseconds.
        drop(daemon);
    }
}
