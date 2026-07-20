//! D4 — the native Preferences page (`/__preferences`) plus its supporting
//! same-origin routes. Mounted into the proxy's extra router exactly like
//! `home.rs`'s `/__home`: `include_str!` page serving, plain JSON routes,
//! same auth-cookie-for-free shape (this router sits behind the same proxy,
//! same session, no separate login).
//!
//! Routes (own):
//! - `GET /__preferences` — the page.
//! - `GET /__api/prefs` — `{preferences, vault:{path,name}, syncPaused,
//!   rendersRunning}`. `preferences` is the persisted store (`prefs::load`);
//!   the other three fields are LIVE facts read off the running stack (via
//!   the late-bound [`control::RunnerSlot`]), independent of what the store
//!   says — e.g. `syncEnabled: false` in the store but the daemon not yet
//!   re-applied (a narrow race at boot) would still show correctly here.
//! - `POST /__api/prefs` `{...preferences}` — the FULL preferences object
//!   (the page always resends everything it last read from `GET`, never a
//!   partial patch — same contract [`prefs::save`] itself keeps). Saves it,
//!   applies whatever is live (sync pause/resume, the renders on/off
//!   switch), and reports `{ok, needsReboot}` — `needsReboot` comes straight
//!   from [`prefs::needs_reboot`]; nothing here re-derives which settings
//!   are boot-time, exactly per that function's own doc.
//! - `POST /__api/prefs/reboot` — [`control::VaultRunner::reboot_in_place`],
//!   then `{ok}`. The page calls this ONLY from an explicit "Apply &
//!   Restart" the user clicks — this route itself never decides to reboot on
//!   its own; `POST /__api/prefs` never calls it either. Silently rebooting
//!   the supervised stack under an open workspace window is exactly what the
//!   D4 plan forbids.
//! - `POST /__api/prefs/vault` `{path}` — delegates verbatim to
//!   [`control::VaultRunner::switch_to`], the N5 zero-cross-vault-spill
//!   machinery. This route does not, and must not, reimplement any part of
//!   that switch — see that method's module doc for why.
//!
//! This module never writes to the vault: every route here either touches
//! `preferences.json` (in the app DATA dir, never the vault — see
//! `prefs.rs`'s module doc) or delegates to `VaultRunner`, which owns the
//! vault-touching machinery itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::control::RunnerSlot;
use crate::prefs::{self, Preferences};

const PREFERENCES_PAGE_HTML: &str = include_str!("preferences.html");

struct PrefsState {
    /// The app DATA dir (never the vault — `preferences.json` lives here).
    data_dir: PathBuf,
    /// D4's late-bound handle to the live `VaultRunner` (see
    /// `control::RunnerSlot`'s doc) — `None` only in the brief window before
    /// boot finishes installing it.
    runner: RunnerSlot,
}

/// Build the D4 Preferences routes for the proxy's extra router.
pub fn router(data_dir: impl Into<PathBuf>, runner: RunnerSlot) -> Router {
    let state = Arc::new(PrefsState { data_dir: data_dir.into(), runner });
    Router::new()
        .route("/__preferences", get(preferences_page))
        .route("/__api/prefs", get(get_prefs).post(post_prefs))
        .route("/__api/prefs/reboot", post(post_reboot))
        .route("/__api/prefs/vault", post(post_vault))
        .with_state(state)
}

async fn preferences_page() -> Html<&'static str> {
    Html(PREFERENCES_PAGE_HTML)
}

/// The vault's display name: the trailing path component of its ABSOLUTE
/// root, tolerant of a trailing slash and of Windows-style separators (the
/// same two things `home.rs`'s `basename` deliberately does NOT need to
/// handle, since that one only ever sees vault-RELATIVE `/`-separated
/// paths — this one sees the raw OS path).
fn vault_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
}

/// A response for every route below when the live runner isn't installed
/// yet (the brief pre-boot-completion window `control::RunnerSlot` docs) —
/// same "still starting" posture the rest of the app's late-bound slots take
/// (e.g. `overlay::toggle_palette`'s "before boot completed; ignoring"),
/// just surfaced as an HTTP response instead of a logged no-op, since this
/// IS the HTTP boundary.
fn runner_not_ready() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": "the local stack is still starting; try again in a moment",
        })),
    )
        .into_response()
}

async fn get_prefs(State(state): State<Arc<PrefsState>>) -> Response {
    let preferences = prefs::load(&state.data_dir);
    let runner = state.runner.lock().await.clone();

    let (vault, sync_paused, renders_running) = match runner {
        Some(runner) => {
            let active = runner.active();
            let vault = json!({ "path": active.path.clone(), "name": vault_name(&active.path) });
            let sync_paused = match runner.sync_control().await {
                Some(control) => control.is_paused(),
                None => false,
            };
            let renders_running = runner.export_status().await.is_some();
            (vault, sync_paused, renders_running)
        }
        None => (json!({ "path": "", "name": "" }), false, false),
    };

    Json(json!({
        "preferences": preferences,
        "vault": vault,
        "syncPaused": sync_paused,
        "rendersRunning": renders_running,
    }))
    .into_response()
}

async fn post_prefs(State(state): State<Arc<PrefsState>>, Json(new): Json<Preferences>) -> Response {
    let old = prefs::load(&state.data_dir);
    // Computed BEFORE anything is applied — a property of the (old, new)
    // pair alone, per `prefs::needs_reboot`'s own doc.
    let needs_reboot = prefs::needs_reboot(&old, &new);

    if let Err(e) = prefs::save(&state.data_dir, &new) {
        tracing::error!(error = %e, "D4: failed to save preferences");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response();
    }

    // Apply whatever CAN take effect live, best-effort: a hiccup reaching the
    // running stack must not block the save that already succeeded above —
    // `needsReboot` already tells the caller what still needs a restart, and
    // a live-apply failure here just means that part waits for the next
    // reboot too (never worse than what `needsReboot` alone would promise).
    if let Some(runner) = state.runner.lock().await.clone() {
        if let Some(control) = runner.sync_control().await {
            if new.sync_enabled {
                control.resume();
            } else {
                control.pause();
            }
        }
        // LIVE going OFF, a no-op returning `false` going ON with no exporter
        // child spawned (see `RunningApp::set_renders_enabled`'s doc) — either
        // way `needsReboot` (computed above, from the pure function) is what
        // tells the page whether ON actually took.
        runner.set_renders_enabled(new.exports_enabled).await;
    }

    Json(json!({ "ok": true, "needsReboot": needs_reboot })).into_response()
}

async fn post_reboot(State(state): State<Arc<PrefsState>>) -> Response {
    let Some(runner) = state.runner.lock().await.clone() else {
        return runner_not_ready();
    };
    match runner.reboot_in_place().await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "D4: reboot-in-place failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct VaultSwitchReq {
    path: String,
}

async fn post_vault(State(state): State<Arc<PrefsState>>, Json(req): Json<VaultSwitchReq>) -> Response {
    let path = req.path.trim();
    if path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "empty path" })),
        )
            .into_response();
    }
    let Some(runner) = state.runner.lock().await.clone() else {
        return runner_not_ready();
    };
    // N5 — delegate to `VaultRunner::switch_to` VERBATIM. This route must
    // never reimplement any part of the zero-cross-vault-spill machinery
    // (the crash-safe marker, the disposable-state wipe, the registry
    // repoint) — that is the whole point of routing a Preferences-initiated
    // switch through the exact same method the tray and File > Open Vault…
    // already use.
    match runner.switch_to(Path::new(path)).await {
        Ok(vref) => {
            Json(json!({ "ok": true, "active": { "id": vref.id, "path": vref.path } })).into_response()
        }
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "D4: preferences-initiated vault switch failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_name_takes_the_trailing_path_component() {
        assert_eq!(vault_name("/Users/alby/Designs"), "Designs");
        assert_eq!(vault_name("/Users/alby/Designs/"), "Designs");
        assert_eq!(vault_name(r"C:\Users\alby\Designs"), "Designs");
        assert_eq!(vault_name("Designs"), "Designs");
        assert_eq!(vault_name(""), "");
    }

    // ---------------------------------------------------------------------
    // HTTP-level round trip: POST /__api/prefs saves + reports needsReboot;
    // GET /__api/prefs reads back exactly what was saved. No live runner is
    // installed in this test (the slot stays empty throughout), so this also
    // pins that both routes degrade gracefully — a save must succeed and a
    // read must return sane defaults for the live fields — rather than
    // panicking or 500ing while the stack is "still starting".
    // ---------------------------------------------------------------------

    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt;

    fn empty_runner_slot() -> RunnerSlot {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn save_round_trips_and_reports_needs_reboot() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(tmp.path().to_path_buf(), empty_runner_slot());

        // A live-only change (sync off): needsReboot must be false.
        let live_change = json!({
            "syncEnabled": false,
            "exportsEnabled": true,
            "pluginsEnabled": true,
            "cspEnabled": true,
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/__api/prefs")
                    .header("content-type", "application/json")
                    .body(Body::from(live_change.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["needsReboot"], false, "sync-only change must not ask for a reboot");

        // Read it back: the saved value round-trips through the store, and
        // the live fields degrade to sane defaults with no runner installed.
        let resp = app
            .clone()
            .oneshot(Request::get("/__api/prefs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["preferences"]["syncEnabled"], false);
        assert_eq!(body["vault"]["path"], "");
        assert_eq!(body["syncPaused"], false);
        assert_eq!(body["rendersRunning"], false);
        assert_eq!(prefs::load(tmp.path()).sync_enabled, false, "the file on disk must match");

        // Now a boot-time change on top (plugins off): needsReboot must flip
        // true. The page always resends the FULL object, so this carries the
        // sync-off value forward too — exactly like a real page load would.
        let boot_time_change = json!({
            "syncEnabled": false,
            "exportsEnabled": true,
            "pluginsEnabled": false,
            "cspEnabled": true,
        });
        let resp = app
            .oneshot(
                Request::post("/__api/prefs")
                    .header("content-type", "application/json")
                    .body(Body::from(boot_time_change.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["needsReboot"], true, "disabling plugins is boot-time");
        assert_eq!(prefs::load(tmp.path()).plugins_enabled, false);
    }

    #[tokio::test]
    async fn reboot_and_vault_routes_report_not_ready_with_no_live_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(tmp.path().to_path_buf(), empty_runner_slot());

        let resp = app
            .clone()
            .oneshot(Request::post("/__api/prefs/reboot").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resp = app
            .oneshot(
                Request::post("/__api/prefs/vault")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "path": "/tmp/somewhere" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn vault_switch_rejects_an_empty_path_before_touching_the_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(tmp.path().to_path_buf(), empty_runner_slot());
        let resp = app
            .oneshot(
                Request::post("/__api/prefs/vault")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "path": "  " }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_preferences_page_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(tmp.path().to_path_buf(), empty_runner_slot());
        let resp = app
            .oneshot(Request::get("/__preferences").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("<title>"), "page must render a real HTML document");
    }
}
