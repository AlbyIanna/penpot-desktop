//! Penpot Local core boot sequence (Milestone M1).
//!
//! Shared by the Tauri shell (`src/main.rs`) and the headless runner
//! (`src/bin/headless.rs`): resolve configuration → load-or-generate the
//! pinned `PENPOT_SECRET_KEY` → start the supervisor (postgres → valkey →
//! backend JVM) → first-boot single-user provisioning over RPC → start the
//! local proxy with the `/__bootstrap` auto-login route → expose readiness.

pub mod checkpoint;
pub mod dialog;
pub mod gitinit;
pub mod home;
pub mod layout;
pub mod overlay;
pub mod preflight;
pub mod reveal;
pub mod status;
pub mod tray;

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
/// smoke test can run against a scratch data dir / alternate ports. External
/// components (runtime artifacts, java, valkey, postgres, watchdog, backend
/// PATH) come from the [`layout`] resolver: env > bundle > dev.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// App-internal state root (`PENPOT_LOCAL_DATA_DIR` or the platform
    /// app-data dir, e.g. `~/Library/Application Support/penpot-local`).
    pub data_dir: PathBuf,
    /// Extracted Penpot artifacts (`PENPOT_LOCAL_RUNTIME_DIR`, the bundle,
    /// or `<repo>/runtime`).
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
    /// Pre-seeded postgres installation (`PENPOT_LOCAL_POSTGRES_INSTALL_DIR`
    /// or the bundle's `postgres/`); `None` = download once into the data dir.
    pub postgres_install_dir: Option<PathBuf>,
    /// Explicit `penpot-watchdog` binary (bundle `bin/`); the
    /// `PENPOT_WATCHDOG_BIN` env var still wins inside the supervisor.
    pub watchdog_bin: Option<PathBuf>,
    /// Dirs prepended to the backend JVM child's PATH (bundle `bin/` with
    /// `identify`/`node`, or `PENPOT_LOCAL_IDENTIFY`/`PENPOT_LOCAL_NODE` dirs).
    pub child_path_prepend: Vec<PathBuf>,
    /// M5 per-board auto-export: exporter child + board-export service.
    /// `None` unless `PENPOT_LOCAL_EXPORTS=1` (default OFF). Packaged since
    /// N2: the runtime bundle ships node + exporter app + headless chromium,
    /// so this works on a clean machine with no host node.
    pub exporter: Option<ExporterLayout>,
    /// The full layout with per-component provenance (logged at boot).
    pub layout: layout::RuntimeLayout,
}

/// Resolved pieces of the optional exporter service (M5 dev-mode; N2 makes
/// it a packaged-mode capability). Resolution per component is env override >
/// bundle payload > dev default — the same precedence as
/// [`layout::resolve_layout`]:
/// - exporter app: `PENPOT_LOCAL_EXPORTER_DIR` → `<runtime>/exporter` (in
///   packaged mode the runtime dir IS the bundle, which ships `exporter/`);
/// - node: `PENPOT_LOCAL_NODE` → bundle `bin/node` (v24.16.0, the upstream
///   pin) → host `/opt/homebrew/bin/node` (dev; v25 verified working);
/// - browsers: `PENPOT_LOCAL_EXPORTER_BROWSERS` → `<runtime>/
///   exporter-browsers` (the bundle ships the chromium headless shell).
#[derive(Debug, Clone)]
pub struct ExporterLayout {
    /// Directory with `app.js` + `node_modules`
    /// (`PENPOT_LOCAL_EXPORTER_DIR`, default `<runtime>/exporter`).
    pub exporter_dir: PathBuf,
    /// Node binary (`PENPOT_LOCAL_NODE` → bundle `bin/node` → homebrew).
    pub node_path: PathBuf,
    /// `PLAYWRIGHT_BROWSERS_PATH` (`PENPOT_LOCAL_EXPORTER_BROWSERS`, default
    /// `<runtime>/exporter-browsers`).
    pub browsers_dir: PathBuf,
    /// Exporter HTTP port (`PENPOT_LOCAL_EXPORTER_PORT`, default 6363).
    pub port: u16,
    /// One `component=… source=… path=…` line per component for the boot
    /// log, same shape as [`layout::RuntimeLayout::describe`].
    pub provenance: Vec<String>,
}

/// Truthy values accepted for `PENPOT_LOCAL_EXPORTS`.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).unwrap_or_default().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Env overrides for the exporter pieces, captured as data (pure resolution
/// → unit-testable without touching the process environment).
#[derive(Debug, Clone, Default)]
pub struct ExporterEnvOverrides {
    /// `PENPOT_LOCAL_EXPORTER_DIR`.
    pub exporter_dir: Option<PathBuf>,
    /// `PENPOT_LOCAL_NODE` (shared with the backend-PATH override).
    pub node: Option<PathBuf>,
    /// `PENPOT_LOCAL_EXPORTER_BROWSERS`.
    pub browsers_dir: Option<PathBuf>,
}

impl ExporterEnvOverrides {
    fn from_env() -> Self {
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        ExporterEnvOverrides {
            exporter_dir: path("PENPOT_LOCAL_EXPORTER_DIR"),
            node: path("PENPOT_LOCAL_NODE"),
            browsers_dir: path("PENPOT_LOCAL_EXPORTER_BROWSERS"),
        }
    }
}

/// Pre-flight the exporter layout: every requirement is checked here so a
/// missing piece fails the boot with a message that says exactly what to
/// run, instead of a crash-looping child.
///
/// Precedence per component (N2): env override > bundle payload > dev
/// default. `bundle` is the discovered `penpot-runtime/` dir, if any — in
/// packaged mode it usually equals `runtime_dir`, but an explicit
/// `PENPOT_LOCAL_RUNTIME_DIR` env override may split the two, so the bundle
/// is consulted independently for `bin/node`.
pub fn resolve_exporter_layout(
    env: &ExporterEnvOverrides,
    runtime_dir: &Path,
    port: u16,
    bundle: Option<&Path>,
) -> anyhow::Result<ExporterLayout> {
    let mut provenance = Vec::new();
    let mut record = |component: &str, source: layout::Source, path: &Path| {
        provenance.push(format!(
            "component={component} source={source} path={}",
            path.display()
        ));
    };

    let (exporter_dir, exporter_src) = match &env.exporter_dir {
        Some(dir) => (dir.clone(), layout::Source::Env),
        None => {
            let in_runtime = runtime_dir.join("exporter");
            match bundle {
                Some(b) if b != runtime_dir && b.join("exporter/app.js").is_file()
                    && !in_runtime.join("app.js").is_file() =>
                {
                    (b.join("exporter"), layout::Source::Bundle)
                }
                Some(b) if b == runtime_dir && in_runtime.join("app.js").is_file() => {
                    (in_runtime, layout::Source::Bundle)
                }
                _ => (in_runtime, layout::Source::Dev),
            }
        }
    };
    record("exporter", exporter_src, &exporter_dir);
    if !exporter_dir.join("app.js").is_file() {
        bail!(
            "PENPOT_LOCAL_EXPORTS is enabled but the exporter app is missing under {} — \
             run scripts/fetch-penpot.sh (dev), rebuild the runtime bundle with the \
             exporter payload (packaged), or set PENPOT_LOCAL_EXPORTER_DIR",
            exporter_dir.display()
        );
    }

    let (node_path, node_src) = match &env.node {
        Some(p) => (p.clone(), layout::Source::Env),
        None => match bundle.map(|b| b.join("bin/node")).filter(|p| p.is_file()) {
            Some(p) => (p, layout::Source::Bundle),
            None => (PathBuf::from("/opt/homebrew/bin/node"), layout::Source::Dev),
        },
    };
    record("exporter-node", node_src, &node_path);
    if !node_path.is_file() {
        bail!(
            "PENPOT_LOCAL_EXPORTS is enabled but node was not found at {} — \
             install node (dev; upstream pins v24, v25 is verified working), rebuild \
             the runtime bundle with bin/node (packaged), or set PENPOT_LOCAL_NODE",
            node_path.display()
        );
    }

    let (browsers_dir, browsers_src) = match &env.browsers_dir {
        Some(dir) => (dir.clone(), layout::Source::Env),
        None => {
            let in_runtime = runtime_dir.join("exporter-browsers");
            let src = match bundle {
                Some(b) if b == runtime_dir => layout::Source::Bundle,
                _ => layout::Source::Dev,
            };
            (in_runtime, src)
        }
    };
    record("exporter-browsers", browsers_src, &browsers_dir);
    let has_chromium = std::fs::read_dir(&browsers_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("chromium"))
        })
        .unwrap_or(false);
    if !has_chromium {
        bail!(
            "PENPOT_LOCAL_EXPORTS is enabled but no playwright chromium under {} — \
             run scripts/fetch-penpot.sh --with-browsers (dev), rebuild the runtime \
             bundle with exporter-browsers/ (packaged), or set \
             PENPOT_LOCAL_EXPORTER_BROWSERS",
            browsers_dir.display()
        );
    }
    Ok(ExporterLayout { exporter_dir, node_path, browsers_dir, port, provenance })
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
    /// Resolve config from environment + platform defaults (headless entry:
    /// no Tauri resources dir; the bundle can still be found via
    /// `PENPOT_LOCAL_RUNTIME_BUNDLE` or executable-adjacent discovery).
    pub fn resolve() -> anyhow::Result<Self> {
        Self::resolve_with_resources(None)
    }

    /// Resolve config, additionally considering `<resource_dir>/penpot-runtime`
    /// as a bundle location (the Tauri v2 GUI passes its resources dir here).
    pub fn resolve_with_resources(resource_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let data_dir = match std::env::var_os("PENPOT_LOCAL_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => directories::ProjectDirs::from("", "", "penpot-local")
                .context("cannot determine the platform app-data directory")?
                .data_dir()
                .to_path_buf(),
        };

        // --- runtime layout: env > bundle > dev --------------------------
        let env_overrides = layout::EnvOverrides::from_env();
        let env_bundle = std::env::var_os(layout::ENV_RUNTIME_BUNDLE).map(PathBuf::from);
        let bundle = layout::discover_bundle(env_bundle.as_deref(), resource_dir.as_deref())?;
        let resolved = layout::resolve_layout(&env_overrides, bundle.as_deref());

        let runtime_dir = resolved.runtime_dir.path.clone();
        if !runtime_dir.join("backend/penpot.jar").is_file() {
            bail!(
                "Penpot runtime artifacts not found under {} — run scripts/fetch-penpot.sh first",
                runtime_dir.display()
            );
        }
        let designs_dir = match std::env::var_os("PENPOT_LOCAL_DESIGNS_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => data_dir.join("designs"),
        };
        // --- M5/N2 exporter (default OFF; packaged-mode capable) ----------
        let exporter = if env_flag("PENPOT_LOCAL_EXPORTS") {
            let port = env_port("PENPOT_LOCAL_EXPORTER_PORT", 6363)?;
            Some(resolve_exporter_layout(
                &ExporterEnvOverrides::from_env(),
                &runtime_dir,
                port,
                bundle.as_deref(),
            )?)
        } else {
            None
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
            java_path: resolved.java.path.clone(),
            valkey_path: resolved.valkey.path.clone(),
            designs_dir,
            postgres_install_dir: resolved.postgres_install.as_ref().map(|r| r.path.clone()),
            watchdog_bin: resolved.watchdog_bin.as_ref().map(|r| r.path.clone()),
            child_path_prepend: resolved.child_path_prepend.clone(),
            exporter,
            layout: resolved,
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
            // N3: land on the lighttable home (our page), not the SPA. The
            // auth cookie is set here, so card clicks deep-link straight into
            // `/#/workspace?…` with the session already established.
            (
                StatusCode::FOUND,
                [(header::SET_COOKIE, cookie), (header::LOCATION, "/__home".to_string())],
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
    board_export: Option<board_export::BoardExportHandle>,
    vault_index: Option<vault_index::VaultIndexHandle>,
}

impl RunningApp {
    /// The URL the webview should open: performs auto-login then lands on `/`.
    pub fn bootstrap_url(&self) -> String {
        format!("{}/__bootstrap", self.proxy_url)
    }

    /// The sync daemon's status stream (tray UI), if the daemon started.
    pub fn sync_status(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<sync_daemon::SyncStatusSnapshot>> {
        self.sync_daemon.as_ref().map(|d| d.status())
    }

    /// The sync daemon's pause/resume handle, if the daemon started.
    pub fn sync_control(&self) -> Option<sync_daemon::SyncControl> {
        self.sync_daemon.as_ref().map(|d| d.control())
    }

    /// The board-export service's status stream (tray "Exports:" line), if
    /// the service started (`PENPOT_LOCAL_EXPORTS=1`).
    pub fn export_status(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<board_export::ExportStatusSnapshot>> {
        self.board_export.as_ref().map(|b| b.status())
    }

    /// Orderly shutdown: stop the board-export service first (renders talk
    /// to the exporter/backend), then the sync daemon (so no export/import
    /// is in flight when the backend goes away), then the proxy, then the
    /// supervised children (exporter → backend → valkey → postgres).
    /// Idempotent via consuming `self`.
    pub async fn shutdown(mut self) {
        // The vault index only reads disk; stop it first (cheap, instant).
        if let Some(index) = self.vault_index.take() {
            index.stop().await;
        }
        if let Some(exports) = self.board_export.take() {
            exports.stop().await;
        }
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

/// Build the [`supervisor::SupervisorConfig`] for a resolved [`AppConfig`].
/// Pure (no spawning, no fs writes) so packaged-mode resolution can be
/// asserted in tests down to the exact command lines.
pub fn supervisor_config(
    config: &AppConfig,
    secret_key: &str,
    public_uri: &str,
) -> supervisor::SupervisorConfig {
    let mut jvm = supervisor::JvmSpec::penpot_2_16(&config.java_path);
    // Replace `-m app.main` with the nrepl-free entry (see BACKEND_ENTRY_EXPR).
    jvm.extra_args = vec!["-e".into(), BACKEND_ENTRY_EXPR.into()];

    let mut sup_config = supervisor::SupervisorConfig::new(
        &config.data_dir,
        config.storage_dir(),
        &config.valkey_path,
        config.runtime_dir.join("backend"),
        jvm,
        secret_key,
        public_uri,
    );
    sup_config.ports = config.ports;
    // M4 packaged mode: pre-seeded postgres (offline), bundled watchdog, and
    // the bundle bin/ (identify/node) on the backend child's PATH. All None/
    // empty in dev mode — behavior byte-identical to pre-M4.
    sup_config.postgres_install_dir = config.postgres_install_dir.clone();
    sup_config.orphan_watchdog_bin = config.watchdog_bin.clone();
    sup_config.child_path_prepend = config.child_path_prepend.clone();
    // M5: optional exporter child (node app.js, shared valkey, matching
    // secret key — see supervisor::exporter_command). None in default mode.
    sup_config.exporter = config.exporter.as_ref().map(|e| {
        supervisor::ExporterSpec::new(
            &e.node_path,
            &e.exporter_dir,
            e.port,
            &e.browsers_dir,
        )
    });
    sup_config
}

/// Run the full boot sequence. On first run in dev mode this downloads the
/// embedded Postgres binaries (network needed once); a packaged install with
/// a bundled `postgres/` is fully offline from the very first boot. Also
/// registers the single user; afterwards everything is offline and idempotent.
pub async fn boot(config: AppConfig) -> anyhow::Result<RunningApp> {
    // M5 pre-flight (PLAN.md risk 8): refuse non-BMP (emoji) characters in
    // any load-bearing path BEFORE the supervisor spawns anything — the JDK
    // cannot load the backend jar from such a path and would crash-loop.
    // The error stays downcastable so the GUI can show a native dialog.
    preflight::check_app_paths(&config)?;

    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;

    // One clear line per component: where it came from (env|bundle|dev).
    for line in config.layout.describe() {
        tracing::info!("runtime layout: {line}");
    }
    if let Some(exporter) = &config.exporter {
        for line in &exporter.provenance {
            tracing::info!("runtime layout: {line}");
        }
    }

    let secret_key = load_or_generate_secret(&config.data_dir)?;
    let public_uri = config.public_uri();

    // --- supervised children -------------------------------------------
    let sup_config = supervisor_config(&config, &secret_key, &public_uri);

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

    // --- vault index (N1: offline full-content search) --------------------
    // Reads only the designs tree + manifest (never the DB); its SQLite db
    // lives in the data dir OUTSIDE the vault and is disposable (delete it →
    // rebuilt from disk alone). Routes are same-origin via the proxy.
    let vault_index = vault_index::spawn(vault_index::IndexConfig::new(
        &config.designs_dir,
        config.data_dir.join("vault-index").join("index.sqlite3"),
    ));
    let team_id = profile.default_team_id.clone().unwrap_or_default();
    let vault_routes = vault_index::router(&vault_index, team_id.clone(), &config.designs_dir);

    // --- lighttable home + activity/conflict strip (N3) -------------------
    // The strip serves a status source late-bound below: the real sync daemon
    // (production) or the MockStatusSource (PENPOT_LOCAL_TRAY_DEMO=1 — so the
    // strip is drivable windowless in CI). The channel exists now; its feeder
    // task is spawned after the daemon so the router can be built here.
    let (strip_tx, strip_rx) =
        tokio::sync::watch::channel(sync_daemon::SyncStatusSnapshot::default());
    let home_routes = home::router(config.designs_dir.clone(), strip_rx);

    // --- N4b "Checkpoint now" (manual, git-native vault checkpoint) --------
    let checkpoint_routes = checkpoint::router(config.designs_dir.clone());

    let extra = extra_router(bootstrap_state, config_js)
        .merge(vault_routes)
        .merge(home_routes)
        .merge(checkpoint_routes);

    let mut proxy_config = proxy::ProxyConfig::new(
        config.runtime_dir.join("frontend"),
        config.storage_dir(),
    );
    proxy_config.listen_addr = ([127, 0, 0, 1], config.proxy_port).into();
    proxy_config.backend_addr = ([127, 0, 0, 1], config.ports.backend).into();
    // M5: with the exporter child running, /api/export becomes a real proxy
    // (the Penpot UI's export button needs it); otherwise the 502 stub stays.
    proxy_config.exporter_addr = config
        .exporter
        .as_ref()
        .map(|e| ([127, 0, 0, 1], e.port).into());

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

    // --- activity/conflict strip feeder (N3) ------------------------------
    // Bind the strip's status source: the MockStatusSource demo loop when
    // PENPOT_LOCAL_TRAY_DEMO=1 (drivable windowless in CI — it exercises the
    // Conflict/Error states and the reveal action deterministically), else
    // the real sync daemon's live snapshots.
    if env_flag("PENPOT_LOCAL_TRAY_DEMO") {
        tracing::info!("N3 strip: serving MockStatusSource demo frames (PENPOT_LOCAL_TRAY_DEMO)");
        tokio::spawn(async move {
            let frames = status::MockStatusSource::demo_frames();
            let mut i = 0usize;
            loop {
                let frame = frames[i % frames.len()].clone();
                i += 1;
                if strip_tx.send(frame).is_err() {
                    break; // strip receiver gone (proxy stopped)
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    } else if let Some(daemon) = &sync_daemon {
        let mut rx = daemon.status();
        tokio::spawn(async move {
            loop {
                let snapshot = rx.borrow_and_update().clone();
                if strip_tx.send(snapshot).is_err() {
                    break;
                }
                if rx.changed().await.is_err() {
                    break; // daemon gone
                }
            }
        });
    } else {
        // No daemon and not in demo mode: leave the strip on its default
        // (empty) snapshot. Dropping strip_tx here would be fine too.
        drop(strip_tx);
    }

    // --- board-export service (M5: per-board SVG/PNG next to sources) -----
    // Self-contained consumer of the sync manifest (read-only) — it never
    // talks to the sync daemon. Status is surfaced via tracing AND the
    // watch channel behind [`RunningApp::export_status`] — the tray's
    // "Exports:" row subscribes to it through status::ExportStatusBridge.
    let board_export = match (&config.exporter, &credentials.access_token) {
        (Some(exporter), Some(token)) => {
            let rpc = PenpotClient::new(&readiness.backend_base_url)
                .with_auth(Auth::Token(token.clone()));
            let export_config = board_export::ExportConfig::new(
                &config.designs_dir,
                format!("http://127.0.0.1:{}", exporter.port),
                &readiness.backend_base_url,
                &credentials.email,
                &credentials.password,
                &profile.id,
            );
            tracing::info!(
                root = %config.designs_dir.display(),
                exporter_port = exporter.port,
                "starting board-export service"
            );
            Some(board_export::spawn(rpc, export_config))
        }
        (Some(_), None) => {
            tracing::warn!(
                "board-export NOT started: exporter is enabled but no access token was provisioned"
            );
            None
        }
        (None, _) => None,
    };

    Ok(RunningApp {
        proxy_url: public_uri,
        profile,
        credentials,
        supervisor: sup,
        proxy_shutdown: Some(shutdown_tx),
        proxy_task: Some(proxy_task),
        sync_daemon,
        board_export,
        vault_index: Some(vault_index),
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

    // ---------------- M5 exporter layout pre-flight ----------------

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// A runtime dir with a complete exporter setup + a fake node binary.
    fn fake_exporter_runtime(root: &Path) -> (PathBuf, ExporterEnvOverrides) {
        let runtime = root.join("runtime");
        touch(&runtime.join("exporter/app.js"));
        touch(&runtime.join("exporter-browsers/chromium_headless_shell-1228/marker"));
        let node = root.join("bin/node");
        touch(&node);
        let env = ExporterEnvOverrides { node: Some(node), ..Default::default() };
        (runtime, env)
    }

    #[test]
    fn exporter_layout_resolves_when_everything_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let (runtime, env) = fake_exporter_runtime(tmp.path());
        let layout = resolve_exporter_layout(&env, &runtime, 6363, None).unwrap();
        assert_eq!(layout.exporter_dir, runtime.join("exporter"));
        assert_eq!(layout.browsers_dir, runtime.join("exporter-browsers"));
        assert_eq!(layout.node_path, tmp.path().join("bin/node"));
        assert_eq!(layout.port, 6363);
    }

    #[test]
    fn exporter_layout_fails_loudly_per_missing_piece() {
        let tmp = tempfile::tempdir().unwrap();
        let (runtime, env) = fake_exporter_runtime(tmp.path());

        // Missing exporter app.
        let bare = tmp.path().join("bare-runtime");
        std::fs::create_dir_all(&bare).unwrap();
        let err = resolve_exporter_layout(&env, &bare, 6363, None).unwrap_err();
        assert!(err.to_string().contains("fetch-penpot.sh"), "{err}");

        // Missing node.
        let no_node = ExporterEnvOverrides {
            node: Some(tmp.path().join("nope/node")),
            ..Default::default()
        };
        let err = resolve_exporter_layout(&no_node, &runtime, 6363, None).unwrap_err();
        assert!(err.to_string().contains("PENPOT_LOCAL_NODE"), "{err}");

        // Missing browsers.
        let no_browsers = ExporterEnvOverrides {
            browsers_dir: Some(tmp.path().join("empty-browsers")),
            ..env.clone()
        };
        let err = resolve_exporter_layout(&no_browsers, &runtime, 6363, None).unwrap_err();
        assert!(err.to_string().contains("--with-browsers"), "{err}");
    }

    #[test]
    fn exporter_env_overrides_win() {
        let tmp = tempfile::tempdir().unwrap();
        let (runtime, mut env) = fake_exporter_runtime(tmp.path());
        // Alternate exporter dir + browsers via env.
        let alt = tmp.path().join("alt");
        touch(&alt.join("exporter/app.js"));
        touch(&alt.join("browsers/chromium-x/marker"));
        env.exporter_dir = Some(alt.join("exporter"));
        env.browsers_dir = Some(alt.join("browsers"));
        let layout = resolve_exporter_layout(&env, &runtime, 7000, None).unwrap();
        assert_eq!(layout.exporter_dir, alt.join("exporter"));
        assert_eq!(layout.browsers_dir, alt.join("browsers"));
        assert_eq!(layout.port, 7000);
    }

    /// N2: a full bundle provides exporter app, browsers AND node — all three
    /// resolve source=bundle with zero host requirements.
    #[test]
    fn exporter_layout_resolves_from_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("penpot-runtime");
        touch(&bundle.join("backend/penpot.jar"));
        touch(&bundle.join("exporter/app.js"));
        touch(&bundle.join("exporter-browsers/chromium_headless_shell-1228/marker"));
        touch(&bundle.join("bin/node"));
        // Packaged mode: the runtime dir IS the bundle.
        let layout = resolve_exporter_layout(
            &ExporterEnvOverrides::default(),
            &bundle,
            6363,
            Some(&bundle),
        )
        .unwrap();
        assert_eq!(layout.exporter_dir, bundle.join("exporter"));
        assert_eq!(layout.node_path, bundle.join("bin/node"), "bundle node beats homebrew");
        assert_eq!(layout.browsers_dir, bundle.join("exporter-browsers"));
        assert_eq!(layout.provenance.len(), 3);
        assert!(
            layout.provenance.iter().all(|l| l.contains("source=bundle")),
            "{:?}",
            layout.provenance
        );
    }

    /// N2: env overrides still beat the bundle for every exporter component.
    #[test]
    fn exporter_env_overrides_beat_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("penpot-runtime");
        touch(&bundle.join("exporter/app.js"));
        touch(&bundle.join("exporter-browsers/chromium_headless_shell-1228/marker"));
        touch(&bundle.join("bin/node"));
        let alt = tmp.path().join("alt");
        touch(&alt.join("exporter/app.js"));
        touch(&alt.join("browsers/chromium-x/marker"));
        touch(&alt.join("node"));
        let env = ExporterEnvOverrides {
            exporter_dir: Some(alt.join("exporter")),
            browsers_dir: Some(alt.join("browsers")),
            node: Some(alt.join("node")),
        };
        let layout = resolve_exporter_layout(&env, &bundle, 6363, Some(&bundle)).unwrap();
        assert_eq!(layout.exporter_dir, alt.join("exporter"));
        assert_eq!(layout.node_path, alt.join("node"));
        assert_eq!(layout.browsers_dir, alt.join("browsers"));
        assert!(
            layout.provenance.iter().all(|l| l.contains("source=env")),
            "{:?}",
            layout.provenance
        );
    }

    /// N2: a bundle without the exporter payload degrades to the dev error
    /// (clear message, no silent fallback to a half-working setup).
    #[test]
    fn bundle_without_exporter_payload_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("penpot-runtime");
        touch(&bundle.join("backend/penpot.jar"));
        let err = resolve_exporter_layout(
            &ExporterEnvOverrides::default(),
            &bundle,
            6363,
            Some(&bundle),
        )
        .unwrap_err();
        assert!(err.to_string().contains("exporter app is missing"), "{err}");
    }

    #[test]
    fn supervisor_config_maps_the_exporter_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let (runtime, env) = fake_exporter_runtime(tmp.path());
        let exporter = resolve_exporter_layout(&env, &runtime, 6467, None).unwrap();
        let config = AppConfig {
            data_dir: tmp.path().join("data"),
            runtime_dir: runtime.clone(),
            proxy_port: 8686,
            ports: supervisor::Ports::default(),
            java_path: "/usr/bin/java".into(),
            valkey_path: "/usr/bin/valkey-server".into(),
            designs_dir: tmp.path().join("designs"),
            postgres_install_dir: None,
            watchdog_bin: None,
            child_path_prepend: vec![],
            exporter: Some(exporter),
            layout: layout::resolve_layout(&layout::EnvOverrides::default(), None),
        };
        let sup = supervisor_config(&config, "sekrit", "http://localhost:8686");
        let spec = sup.exporter.expect("exporter spec mapped");
        assert_eq!(spec.port, 6467);
        assert_eq!(spec.exporter_dir, runtime.join("exporter"));
        assert_eq!(spec.browsers_path, runtime.join("exporter-browsers"));

        // Without the layout: no exporter child (default OFF).
        let mut config_off = config;
        config_off.exporter = None;
        let sup = supervisor_config(&config_off, "sekrit", "http://localhost:8686");
        assert!(sup.exporter.is_none());
    }
}
