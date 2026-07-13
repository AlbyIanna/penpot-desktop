//! Penpot Local core boot sequence (Milestone M1).
//!
//! Shared by the Tauri shell (`src/main.rs`) and the headless runner
//! (`src/bin/headless.rs`): resolve configuration → load-or-generate the
//! pinned `PENPOT_SECRET_KEY` → start the supervisor (postgres → valkey →
//! backend JVM) → first-boot single-user provisioning over RPC → start the
//! local proxy with the `/__bootstrap` auto-login route → expose readiness.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use http::{header, StatusCode};
use penpot_rpc::{Auth, PenpotClient, Profile};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Entry expression passed to `clojure.main -e` instead of `-m app.main`.
///
/// `app.main/-main` unconditionally starts an nrepl server on **0.0.0.0**:6064
/// (verified in the 2.16.2 jar) — an unauthenticated remote code-execution
/// listener we must not expose from a desktop app. `app.main/start` is the
/// same initialization path minus the nrepl server; the `(deref (promise))`
/// blocks forever like upstream's `-main` does. Verified live: the backend
/// boots identically and nothing listens on 6064.
pub const BACKEND_ENTRY_EXPR: &str = "(do (require 'app.main) (app.main/start) (deref (promise)))";

const SECRET_KEY_FILE: &str = "secret.key";
const CREDENTIALS_FILE: &str = "credentials.json";
const PROVISION_EMAIL: &str = "local@penpot.test";
const PROVISION_FULLNAME: &str = "Penpot Local";
const ACCESS_TOKEN_NAME: &str = "penpot-local-desktop";

/// Resolved application configuration. Everything has env overrides so the
/// smoke test can run against a scratch data dir / alternate ports.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// App-internal state root (`PENPOT_LOCAL_DATA_DIR` or the platform
    /// app-data dir, e.g. `~/Library/Application Support/penpot-local`).
    pub data_dir: PathBuf,
    /// Extracted Penpot artifacts (`PENPOT_LOCAL_RUNTIME_DIR` or `<repo>/runtime`).
    pub runtime_dir: PathBuf,
    /// Proxy listen port (`PENPOT_LOCAL_PROXY_PORT`, default 8686).
    pub proxy_port: u16,
    /// Backend / postgres / valkey ports (defaults 6161 / 5433 / 6380).
    pub ports: supervisor::Ports,
    /// `java` binary (`PENPOT_LOCAL_JAVA`; must match the pinned JDK major).
    pub java_path: PathBuf,
    /// `valkey-server` binary (`PENPOT_LOCAL_VALKEY`).
    pub valkey_path: PathBuf,
    /// The user's designs folder the sync daemon mirrors the DB into
    /// (`PENPOT_LOCAL_DESIGNS_DIR`, default `<data_dir>/designs`).
    pub designs_dir: PathBuf,
}

fn env_port(name: &str, default: u16) -> anyhow::Result<u16> {
    match std::env::var(name) {
        Ok(v) => v
            .parse::<u16>()
            .with_context(|| format!("{name}={v:?} is not a valid port")),
        Err(_) => Ok(default),
    }
}

impl AppConfig {
    /// Resolve config from environment + platform defaults.
    pub fn resolve() -> anyhow::Result<Self> {
        let data_dir = match std::env::var_os("PENPOT_LOCAL_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => directories::ProjectDirs::from("", "", "penpot-local")
                .context("cannot determine the platform app-data directory")?
                .data_dir()
                .to_path_buf(),
        };
        let runtime_dir = match std::env::var_os("PENPOT_LOCAL_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir),
            // Dev default: <repo>/runtime, resolved relative to this crate.
            None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
        };
        if !runtime_dir.join("backend/penpot.jar").is_file() {
            bail!(
                "Penpot runtime artifacts not found under {} — run scripts/fetch-penpot.sh first",
                runtime_dir.display()
            );
        }
        let java_path = PathBuf::from(
            std::env::var("PENPOT_LOCAL_JAVA")
                .unwrap_or_else(|_| "/opt/homebrew/opt/openjdk/bin/java".to_string()),
        );
        let valkey_path = PathBuf::from(
            std::env::var("PENPOT_LOCAL_VALKEY")
                .unwrap_or_else(|_| "/opt/homebrew/bin/valkey-server".to_string()),
        );
        let designs_dir = match std::env::var_os("PENPOT_LOCAL_DESIGNS_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => data_dir.join("designs"),
        };
        Ok(AppConfig {
            data_dir,
            runtime_dir,
            proxy_port: env_port("PENPOT_LOCAL_PROXY_PORT", proxy::DEFAULT_LISTEN_PORT)?,
            ports: supervisor::Ports {
                postgres: env_port("PENPOT_LOCAL_POSTGRES_PORT", 5433)?,
                valkey: env_port("PENPOT_LOCAL_VALKEY_PORT", 6380)?,
                backend: env_port("PENPOT_LOCAL_BACKEND_PORT", proxy::DEFAULT_BACKEND_PORT)?,
            },
            java_path,
            valkey_path,
            designs_dir,
        })
    }

    /// The origin the webview and the backend's `PENPOT_PUBLIC_URI` use.
    pub fn public_uri(&self) -> String {
        format!("http://localhost:{}", self.proxy_port)
    }

    pub fn storage_dir(&self) -> PathBuf {
        self.data_dir.join("assets")
    }
}

/// Load the pinned `PENPOT_SECRET_KEY` from the data dir, generating it on
/// first boot. Losing/rotating this key invalidates every session and access
/// token (M0 gotcha), so it must be stable across restarts.
///
/// TODO(M4): move the secret (and `credentials.json`) into the OS keychain;
/// a mode-0600 file in the app-data dir is the M1 stopgap.
fn load_or_generate_secret(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join(SECRET_KEY_FILE);
    if path.is_file() {
        let key = std::fs::read_to_string(&path)?.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    write_private_file(&path, key.as_bytes())?;
    Ok(key)
}

/// Persisted single-user credentials (first-boot provisioning output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    pub password: String,
    /// Personal access token for daemon RPC (`Authorization: Token …`).
    pub access_token: Option<String>,
    pub profile_id: Option<String>,
}

fn write_private_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn random_password() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

/// Ensure the single user exists and we hold a valid access token.
/// Handles all three states:
/// - fresh install (no credentials file) → register + mint token;
/// - normal boot → validate the stored token;
/// - wiped DB with an existing credentials file (the DB is a disposable
///   cache!) → re-register with the same email/password, mint a new token.
async fn provision(backend_base: &str, data_dir: &Path) -> anyhow::Result<(Credentials, Profile)> {
    let creds_path = data_dir.join(CREDENTIALS_FILE);
    let mut creds: Credentials = if creds_path.is_file() {
        serde_json::from_str(&std::fs::read_to_string(&creds_path)?)
            .with_context(|| format!("corrupt {}", creds_path.display()))?
    } else {
        Credentials {
            email: PROVISION_EMAIL.to_string(),
            password: random_password(),
            access_token: None,
            profile_id: None,
        }
    };

    // 1. Fast path: stored access token still valid.
    if let Some(token) = &creds.access_token {
        let client = PenpotClient::new(backend_base).with_auth(Auth::Token(token.clone()));
        if let Ok(profile) = client.get_profile().await {
            return Ok((creds, profile));
        }
        tracing::warn!("stored access token rejected; re-provisioning");
    }

    // 2. Login with the stored password; on wrong-credentials (fresh or wiped
    //    DB) register the profile first.
    let client = PenpotClient::new(backend_base);
    let session = match client.login_with_password(&creds.email, &creds.password).await {
        Ok(outcome) => outcome.auth_token,
        Err(login_err) => {
            tracing::info!("login failed ({login_err}); registering single user");
            let prep = client
                .prepare_register_profile(&creds.email, &creds.password, PROVISION_FULLNAME)
                .await
                .context("prepare-register-profile failed")?;
            let reg = client
                .register_profile(&prep.token)
                .await
                .context("register-profile failed")?;
            match reg.auth_token {
                Some(token) => token,
                None => {
                    client
                        .login_with_password(&creds.email, &creds.password)
                        .await
                        .context("login after registration failed")?
                        .auth_token
                }
            }
        }
    };

    // 3. Mint (and persist) a fresh access token for daemon RPC.
    let client = client.with_auth(Auth::Cookie(session));
    let token = client
        .create_access_token(ACCESS_TOKEN_NAME, None)
        .await
        .context("create-access-token failed")?;
    let profile = client.get_profile().await.context("get-profile failed")?;
    creds.access_token = Some(token.token);
    creds.profile_id = Some(profile.id.clone());
    write_private_file(&creds_path, serde_json::to_string_pretty(&creds)?.as_bytes())?;
    Ok((creds, profile))
}

// ---------------------------------------------------------------------------
// /__bootstrap — auto-login route (webview entry point)
// ---------------------------------------------------------------------------

struct BootstrapState {
    backend_base: String,
    email: String,
    password: String,
    /// One-shot guard: the route works once per boot (the Tauri window uses
    /// it on startup); afterwards it answers 403. Reset on login failure so
    /// a transient error can be retried.
    used: AtomicBool,
}

/// Server-side login: call `login-with-password` upstream with the stored
/// credentials, relay the `auth-token` cookie onto our own response and
/// redirect to `/`. Localhost-only by construction (the proxy binds 127.0.0.1).
async fn bootstrap_login(State(state): State<Arc<BootstrapState>>) -> Response {
    if state.used.swap(true, Ordering::SeqCst) {
        return (StatusCode::FORBIDDEN, "bootstrap already used this boot").into_response();
    }
    let client = PenpotClient::new(&state.backend_base);
    match client.login_with_password(&state.email, &state.password).await {
        Ok(outcome) => {
            // Mirror upstream's cookie attributes (minus Expires — session
            // renewal happens server-side on later requests anyway).
            let cookie = format!(
                "auth-token={}; Path=/; HttpOnly; SameSite=Lax",
                outcome.auth_token
            );
            (
                StatusCode::FOUND,
                [(header::SET_COOKIE, cookie), (header::LOCATION, "/".to_string())],
            )
                .into_response()
        }
        Err(e) => {
            state.used.store(false, Ordering::SeqCst);
            tracing::error!("bootstrap login failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("bootstrap login failed: {e}"),
            )
                .into_response()
        }
    }
}

/// Extra routes merged into the proxy router: the auto-login bootstrap and
/// the rewritten `js/config.js` (upstream's frontend container rewrites that
/// file at boot injecting `penpotFlags` / `penpotPublicURI`; the extracted
/// static build is unpatched, so we serve the equivalent).
fn extra_router(state: Arc<BootstrapState>, config_js: String) -> Router {
    Router::new()
        .route("/__bootstrap", get(bootstrap_login))
        .with_state(state)
        .route(
            "/js/config.js",
            get(move || {
                let body = config_js.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "application/javascript")],
                        body,
                    )
                }
            }),
        )
}

fn render_config_js(flags: &str, public_uri: &str) -> String {
    format!("var penpotFlags = \"{flags}\";\nvar penpotPublicURI = \"{public_uri}\";\n")
}

// ---------------------------------------------------------------------------
// Boot sequence
// ---------------------------------------------------------------------------

/// A fully booted stack: supervisor children running, user provisioned,
/// proxy serving. Call [`RunningApp::shutdown`] for an orderly stop.
pub struct RunningApp {
    /// Proxy origin, e.g. `http://localhost:8686`.
    pub proxy_url: String,
    /// The provisioned single user's profile.
    pub profile: Profile,
    /// The provisioned credentials (also persisted in the data dir).
    pub credentials: Credentials,
    supervisor: supervisor::Supervisor,
    proxy_shutdown: Option<oneshot::Sender<()>>,
    proxy_task: Option<JoinHandle<anyhow::Result<()>>>,
    sync_daemon: Option<sync_daemon::SyncDaemonHandle>,
}

impl RunningApp {
    /// The URL the webview should open: performs auto-login then lands on `/`.
    pub fn bootstrap_url(&self) -> String {
        format!("{}/__bootstrap", self.proxy_url)
    }

    /// Orderly shutdown: stop the sync daemon first (so no export/import is
    /// in flight when the backend goes away), then the proxy, then the
    /// supervised children (backend → valkey → postgres). Idempotent via
    /// consuming `self`.
    pub async fn shutdown(mut self) {
        if let Some(daemon) = self.sync_daemon.take() {
            daemon.stop().await;
        }
        if let Some(tx) = self.proxy_shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.proxy_task.take() {
            let _ = task.await;
        }
        self.supervisor.shutdown().await;
    }
}

/// Run the full boot sequence. On first run this downloads the embedded
/// Postgres binaries (network needed once) and registers the single user;
/// afterwards everything is offline and idempotent.
pub async fn boot(config: AppConfig) -> anyhow::Result<RunningApp> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;

    let secret_key = load_or_generate_secret(&config.data_dir)?;
    let public_uri = config.public_uri();

    // --- supervised children -------------------------------------------
    let mut jvm = supervisor::JvmSpec::penpot_2_16(&config.java_path);
    // Replace `-m app.main` with the nrepl-free entry (see BACKEND_ENTRY_EXPR).
    jvm.extra_args = vec!["-e".into(), BACKEND_ENTRY_EXPR.into()];

    let mut sup_config = supervisor::SupervisorConfig::new(
        &config.data_dir,
        config.storage_dir(),
        &config.valkey_path,
        config.runtime_dir.join("backend"),
        jvm,
        &secret_key,
        &public_uri,
    );
    sup_config.ports = config.ports;

    let mut sup = supervisor::Supervisor::new(sup_config);
    let readiness = sup.start().await.context("supervisor failed to start")?;
    tracing::info!(backend = %readiness.backend_base_url, "penpot stack ready");

    // --- single-user provisioning ---------------------------------------
    let (credentials, profile) = provision(&readiness.backend_base_url, &config.data_dir)
        .await
        .context("single-user provisioning failed")?;
    tracing::info!(email = %credentials.email, profile = %profile.id, "single user provisioned");

    // --- proxy ------------------------------------------------------------
    let bootstrap_state = Arc::new(BootstrapState {
        backend_base: readiness.backend_base_url.clone(),
        email: credentials.email.clone(),
        password: credentials.password.clone(),
        used: AtomicBool::new(false),
    });
    let config_js = render_config_js(supervisor::DEFAULT_PENPOT_FLAGS, &public_uri);
    let extra = extra_router(bootstrap_state, config_js);

    let mut proxy_config = proxy::ProxyConfig::new(
        config.runtime_dir.join("frontend"),
        config.storage_dir(),
    );
    proxy_config.listen_addr = ([127, 0, 0, 1], config.proxy_port).into();
    proxy_config.backend_addr = ([127, 0, 0, 1], config.ports.backend).into();

    let bound = proxy::Proxy::bind_with_router(proxy_config, extra)
        .await
        .context("proxy failed to bind")?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let proxy_task = tokio::spawn(bound.serve_with_shutdown(async move {
        let _ = shutdown_rx.await;
    }));

    // --- sync daemon (M2: one-way DB→FS + startup reconciliation) ---------
    // Hook point per docs/milestones/m1.md: right after provision() we hold
    // the backend base URL + the persisted access token. Spawned after the
    // proxy because export-binfile artifact downloads go through the proxy
    // (`PENPOT_PUBLIC_URI` host).
    let sync_daemon = match (&credentials.access_token, &profile.default_team_id) {
        (Some(token), Some(team_id)) => {
            let rpc = PenpotClient::new(&readiness.backend_base_url)
                .with_auth(Auth::Token(token.clone()));
            let sync_config = sync_daemon::SyncConfig::new(&config.designs_dir, team_id.clone());
            tracing::info!(
                root = %config.designs_dir.display(),
                team = %team_id,
                "starting sync daemon"
            );
            Some(sync_daemon::spawn(rpc, sync_config))
        }
        _ => {
            tracing::warn!(
                "sync daemon NOT started: missing access token or default team id in the provisioned profile"
            );
            None
        }
    };

    Ok(RunningApp {
        proxy_url: public_uri,
        profile,
        credentials,
        supervisor: sup,
        proxy_shutdown: Some(shutdown_tx),
        proxy_task: Some(proxy_task),
        sync_daemon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_js_renders_both_globals() {
        let js = render_config_js("enable-access-tokens", "http://localhost:8686");
        assert_eq!(
            js,
            "var penpotFlags = \"enable-access-tokens\";\nvar penpotPublicURI = \"http://localhost:8686\";\n"
        );
    }

    #[test]
    fn backend_entry_avoids_dash_m() {
        // Guard against accidentally reverting to `-m app.main` (which would
        // re-expose the 0.0.0.0:6064 nrepl listener).
        assert!(BACKEND_ENTRY_EXPR.contains("app.main/start"));
        assert!(!BACKEND_ENTRY_EXPR.contains("-main"));
    }
}
