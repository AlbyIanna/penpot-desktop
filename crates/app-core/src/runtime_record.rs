//! The runtime record and control-port auth (chapter C).
//!
//! Before chapter C a CLI had no way to find a running app: nothing in the
//! data dir recorded a pid or a port, and the control server started only when
//! `PENPOT_LOCAL_CONTROL_PORT` was set — which no normal install sets, so
//! `/open`, `/active`, `/list` and `/windows` did not exist at all.
//!
//! `runtime.json` fixes discovery, and the control server becomes
//! unconditional. That changes the threat model: ANY web page can send
//! `POST http://127.0.0.1:<port>/open` cross-origin. The same-origin policy
//! blocks reading the response, not sending the request, and `POST /open`
//! wipes the disposable DB and reconciles from scratch.
//!
//! Two independent defences, both required:
//!  1. a random per-boot token, written to a `0600` `runtime.json`, demanded
//!     on every request;
//!  2. a browser check — a real CLI never sends `Origin` or `Sec-Fetch-Site`,
//!     so either header's presence disqualifies the request outright.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Name of the discovery file the daemon writes into the app data dir.
pub const RECORD_FILE_NAME: &str = "runtime.json";

/// Header carrying the per-boot control token.
pub const TOKEN_HEADER: &str = "x-penpot-control-token";

/// What a running instance publishes so a CLI invocation can find it,
/// authenticate to it, and tell whether it is still alive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub pid: u32,
    pub proxy_port: u16,
    pub control_port: u16,
    pub token: String,
}

fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RECORD_FILE_NAME)
}

/// A fresh control token. Per-boot, never persisted anywhere but the `0600`
/// `runtime.json` — one definition, shared by both binaries (and, from Task
/// 11, `penpot daemon`), so a front-end can never accidentally ship a weaker
/// one. 32 alphanumerics, same generator the pinned `PENPOT_SECRET_KEY` uses.
pub fn new_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Write `runtime.json` with owner-only permissions.
pub fn write_record(data_dir: &Path, record: &RuntimeRecord) -> Result<()> {
    let path = record_path(data_dir);
    std::fs::write(&path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod {}", path.display()))?;
    }
    Ok(())
}

/// Read `runtime.json`. The error names the fix, because "no instance running"
/// is the CLI's most common failure and it must never read as a bug.
pub fn read_record(data_dir: &Path) -> Result<RuntimeRecord> {
    let path = record_path(data_dir);
    let body = std::fs::read(&path).with_context(|| {
        format!(
            "no running Penpot Local instance ({} absent) — start one with `penpot daemon`",
            path.display()
        )
    })?;
    serde_json::from_slice(&body).with_context(|| format!("malformed {}", path.display()))
}

/// Remove the record on shutdown. Idempotent and never fatal: a stale record
/// is a nuisance, but failing a clean shutdown over one would be worse.
pub fn remove_record(data_dir: &Path) {
    let _ = std::fs::remove_file(record_path(data_dir));
}

/// Reject unauthenticated requests, and browser-initiated ones outright.
pub async fn require_token(
    State(expected): State<String>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = req.headers();
    if headers.contains_key("origin") || headers.contains_key("sec-fetch-site") {
        return Err(StatusCode::FORBIDDEN);
    }
    let presented = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if presented.is_empty() || presented != expected {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn guarded(token: &str) -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .layer(axum::middleware::from_fn_with_state(
                token.to_string(),
                require_token,
            ))
            .with_state(token.to_string())
    }

    async fn status_of(req: Request<Body>) -> StatusCode {
        guarded("s3cret").oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn a_request_without_a_token_is_refused() {
        let req = Request::builder().uri("/ping").body(Body::empty()).unwrap();
        assert_eq!(status_of(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_wrong_token_is_refused() {
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_right_token_passes() {
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_browser_request_is_refused_even_with_a_valid_token() {
        // The attack this exists for: a web page CAN send a cross-origin POST
        // to 127.0.0.1. It must never reach POST /open, which wipes the DB.
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_origin_header_is_refused_even_with_a_valid_token() {
        // Older browsers send Origin without Sec-Fetch-Site. A real CLI never
        // sends Origin at all, so its presence alone is disqualifying.
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::FORBIDDEN);
    }

    fn sample() -> RuntimeRecord {
        RuntimeRecord {
            pid: 4242,
            proxy_port: 9062,
            control_port: 9063,
            token: "s3cret".to_string(),
        }
    }

    #[test]
    fn the_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        let read = read_record(dir.path()).unwrap();
        assert_eq!(read.pid, 4242);
        assert_eq!(read.proxy_port, 9062);
        assert_eq!(read.control_port, 9063);
        assert_eq!(read.token, "s3cret");
    }

    #[test]
    fn reading_a_missing_record_names_the_daemon_verb() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_record(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("penpot daemon"),
            "a missing record must tell the user how to fix it, got: {err}"
        );
    }

    #[test]
    fn removing_the_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        remove_record(dir.path());
        remove_record(dir.path());
        assert!(read_record(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        let mode = std::fs::metadata(dir.path().join(RECORD_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "runtime.json must be 0600, got {mode:o}");
    }
}
