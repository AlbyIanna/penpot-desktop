//! Process supervisor for the embedded Penpot stack (Milestone M1).
//!
//! Responsibility (from PLAN.md): launch embedded Postgres, Valkey, and the
//! Penpot backend JVM as supervised child processes. Clean shutdown kills all
//! children in reverse order (backend → valkey → postgres); a crashed child is
//! restarted with exponential backoff up to a retry limit. All services bind
//! localhost-only on configurable ports (defaults: postgres 5433, valkey 6380,
//! backend 6161). App-internal state (Postgres data dir, valkey dir, logs)
//! lives under the configured `data_dir` (XDG path chosen by the caller),
//! never inside the user's Designs folder.
//!
//! # Public API sketch
//!
//! ```no_run
//! # async fn demo() -> Result<(), supervisor::SupervisorError> {
//! use supervisor::{Supervisor, SupervisorConfig, JvmSpec};
//!
//! let config = SupervisorConfig::new(
//!     "/home/user/.local/share/penpot-local",          // data dir (XDG)
//!     "/home/user/.local/share/penpot-local/assets",   // objects storage (fs backend)
//!     "/opt/homebrew/bin/valkey-server",
//!     "/home/user/.local/share/penpot-local/runtime/backend", // dir containing penpot.jar
//!     JvmSpec::penpot_2_16("/opt/homebrew/opt/openjdk/bin/java"),
//!     "some-pinned-secret-key",
//!     "http://localhost:8686",
//! );
//! let mut supervisor = Supervisor::new(config);
//! let readiness = supervisor.start().await?;
//! println!("postgres at {}", readiness.postgres_uri);
//! // ... run the app ...
//! supervisor.shutdown().await;
//! # Ok(()) }
//! ```

mod child;
mod postgres;
mod probe;
pub mod watchdog;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

pub use postgres::{
    detect_postgres_install, EmbeddedPostgres, PostgresConfig, PostgresInstall,
    OFFLINE_RELEASES_URL,
};
pub use postgresql_embedded::VersionReq;

use child::{ChildSpec, Probe, ReadyState, ServiceHandle};

/// Pinned PostgreSQL version line (a concrete 15.x release available from
/// theseus-rs/postgresql-binaries; downloaded and cached on first run).
pub const DEFAULT_POSTGRES_VERSION: &str = "=15.18.0";

/// `PENPOT_FLAGS` for single-user local mode (PLAN.md "Single-user mode").
pub const DEFAULT_PENPOT_FLAGS: &str =
    "enable-access-tokens disable-email-verification disable-secure-session-cookies disable-onboarding";

/// The supervised services, in start order. `Exporter` is optional (M5) and
/// only present when [`SupervisorConfig::exporter`] is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Postgres,
    Valkey,
    Backend,
    Exporter,
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Service::Postgres => write!(f, "postgres"),
            Service::Valkey => write!(f, "valkey"),
            Service::Backend => write!(f, "backend"),
            Service::Exporter => write!(f, "exporter"),
        }
    }
}

/// Events emitted through the notify hook (and mirrored to `tracing`).
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// A child process has been spawned (not yet ready).
    Starting { service: Service },
    /// The service passed its readiness probe for the first time.
    Ready { service: Service },
    /// The service exited (or failed its probe) unexpectedly.
    Crashed {
        service: Service,
        /// 1-based consecutive failure count.
        attempt: u32,
        /// Whether a restart will be attempted.
        restarting: bool,
    },
    /// The service passed its readiness probe again after a crash restart.
    Restarted { service: Service },
    /// Retries exhausted; the service will stay down.
    GaveUp { service: Service },
    /// The service was stopped as part of an orderly shutdown.
    Stopped { service: Service },
}

/// Callback invoked (synchronously, from supervision tasks) on every event.
/// Keep it cheap; offload heavy work to a channel.
pub type NotifyHook = Arc<dyn Fn(SupervisorEvent) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Notifier(Option<NotifyHook>);

impl Notifier {
    pub(crate) fn emit(&self, event: SupervisorEvent) {
        match &event {
            SupervisorEvent::Starting { service } => info!(%service, "starting"),
            SupervisorEvent::Ready { service } => info!(%service, "ready"),
            SupervisorEvent::Crashed { service, attempt, restarting } => {
                warn!(%service, attempt, restarting, "crashed");
            }
            SupervisorEvent::Restarted { service } => info!(%service, "restarted"),
            SupervisorEvent::GaveUp { service } => warn!(%service, "gave up restarting"),
            SupervisorEvent::Stopped { service } => info!(%service, "stopped"),
        }
        if let Some(hook) = &self.0 {
            hook(event);
        }
    }
}

/// Localhost ports for the three services.
#[derive(Debug, Clone, Copy)]
pub struct Ports {
    pub postgres: u16,
    pub valkey: u16,
    pub backend: u16,
}

impl Default for Ports {
    fn default() -> Self {
        // Conventions from docs/milestones/m1.md (9001/6060 are the m0 spike).
        Ports { postgres: 5433, valkey: 6380, backend: 6161 }
    }
}

/// How to invoke the backend JVM. Kept fully configurable so the integration
/// layer can inject the exact contract extracted from the
/// `penpotapp/backend:2.16.2` image.
#[derive(Debug, Clone)]
pub struct JvmSpec {
    /// Path to the `java` binary (must be the exact JDK major the pinned
    /// Penpot release builds with — `--enable-preview` hard-fails otherwise).
    pub java_path: PathBuf,
    /// JVM flags placed before `-jar`.
    pub flags: Vec<String>,
    /// Path to `penpot.jar`. Relative paths resolve against the backend dir
    /// (the JVM's working directory).
    pub jar: PathBuf,
    /// Arguments after the jar (the image's entrypoint flag: `-m app.main`).
    pub extra_args: Vec<String>,
}

impl JvmSpec {
    /// The invocation replicated from `penpotapp/backend:2.16.2`'s `run.sh`:
    ///
    /// ```text
    /// java -Dim4java.useV7=true \
    ///      -Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager \
    ///      -Dlog4j2.configurationFile=log4j2.xml \
    ///      -XX:-OmitStackTraceInFastThrow \
    ///      --sun-misc-unsafe-memory-access=allow \
    ///      --enable-native-access=ALL-UNNAMED \
    ///      --enable-preview \
    ///      -jar penpot.jar -m app.main
    /// ```
    ///
    /// `log4j2.xml` and `penpot.jar` are relative to the backend dir (cwd).
    pub fn penpot_2_16(java_path: impl Into<PathBuf>) -> Self {
        JvmSpec {
            java_path: java_path.into(),
            flags: [
                "-Dim4java.useV7=true",
                "-Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager",
                "-Dlog4j2.configurationFile=log4j2.xml",
                "-XX:-OmitStackTraceInFastThrow",
                "--sun-misc-unsafe-memory-access=allow",
                "--enable-native-access=ALL-UNNAMED",
                "--enable-preview",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            jar: PathBuf::from("penpot.jar"),
            extra_args: vec!["-m".into(), "app.main".into()],
        }
    }
}

/// How to run the optional `penpot-exporter` node service (M5 per-board
/// SVG/PNG rendering; **packaged since N2** — the runtime bundle ships
/// `bin/node` v24.16.0, `exporter/`, and the chromium headless shell under
/// `exporter-browsers/`).
///
/// Requirements (bundle payload in packaged mode, host installs in dev):
/// - a `node` binary (upstream image pins v24.16.0 — the bundle's pin;
///   host v25.8.1 verified working in dev — the extracted app is pure JS
///   with zero native `.node` bindings);
/// - the extracted exporter app (dev: `scripts/fetch-penpot.sh` →
///   `runtime/exporter/`; packaged: bundle `exporter/`; entry `app.js`);
/// - a playwright-managed chromium under `browsers_path` (dev:
///   `fetch-penpot.sh --with-browsers`; packaged: the headless shell only —
///   playwright ≥1.49 serves default headless launches from it) — the
///   exporter calls `chromium.launch()` with no `executablePath`, so the
///   system Chrome is never used.
///
/// Known upstream limitation (verified on 2.16.2): the exporter's HTTP
/// server binds **0.0.0.0** — the listen host is not configurable in the
/// compiled bundle. The LAN exposure of the render endpoint remains a
/// documented debt (cookie-authenticated; see docs/milestones/n2.md).
///
/// N2 stale-adoption guard: `Supervisor::start` hard-refuses a busy
/// exporter port (naming the pid) and the supervise loop re-checks per
/// respawn + verifies the `/readyz`-answering pid is its own child.
#[derive(Debug, Clone)]
pub struct ExporterSpec {
    /// Host `node` binary.
    pub node_path: PathBuf,
    /// Directory containing the extracted exporter (`app.js`,
    /// `node_modules/`); also the child's working directory.
    pub exporter_dir: PathBuf,
    /// `PENPOT_HTTP_SERVER_PORT` (readiness probe: `GET /readyz` → 200).
    pub port: u16,
    /// `PLAYWRIGHT_BROWSERS_PATH` — playwright-managed browser cache.
    pub browsers_path: PathBuf,
    /// `PENPOT_TEMPDIR`; `None` = `<data_dir>/exporter-tmp`.
    pub tempdir: Option<PathBuf>,
    /// Readiness timeout (startup is ~30 ms — the browser pool is lazy —
    /// but allow slack for slow disks/first runs).
    pub ready_timeout: Duration,
}

impl ExporterSpec {
    pub fn new(
        node_path: impl Into<PathBuf>,
        exporter_dir: impl Into<PathBuf>,
        port: u16,
        browsers_path: impl Into<PathBuf>,
    ) -> Self {
        ExporterSpec {
            node_path: node_path.into(),
            exporter_dir: exporter_dir.into(),
            port,
            browsers_path: browsers_path.into(),
            tempdir: None,
            ready_timeout: Duration::from_secs(30),
        }
    }
}

/// Crash-restart policy (per service).
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// Consecutive failed starts/crashes tolerated before giving up.
    pub max_retries: u32,
    /// Delay before the first restart; doubles each consecutive failure.
    pub initial_backoff: Duration,
    /// Backoff ceiling.
    pub max_backoff: Duration,
    /// If a service stays ready this long, the failure counter resets.
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(30),
        }
    }
}

impl RestartPolicy {
    /// Exponential backoff for the given 1-based attempt, capped at `max_backoff`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Full supervisor configuration. Construct via [`SupervisorConfig::new`] and
/// override fields as needed.
#[derive(Clone)]
pub struct SupervisorConfig {
    /// App-internal state root (XDG data dir). Postgres/valkey subdirs are
    /// created under it. Never inside the user's Designs folder.
    pub data_dir: PathBuf,
    /// `PENPOT_OBJECTS_STORAGE_FS_DIRECTORY` (fs storage backend).
    pub storage_dir: PathBuf,
    pub ports: Ports,
    /// Path to the `valkey-server` binary.
    pub valkey_path: PathBuf,
    /// Directory containing the extracted backend artifacts (`penpot.jar`,
    /// `log4j2.xml`, …); used as the JVM's working directory.
    pub backend_dir: PathBuf,
    pub jvm: JvmSpec,
    /// Pinned `PENPOT_SECRET_KEY` (must be persisted by the caller or every
    /// restart invalidates all sessions/access tokens — M0 gotcha #7).
    pub secret_key: String,
    /// `PENPOT_PUBLIC_URI` — the proxy origin, e.g. `http://localhost:8686`.
    pub public_uri: String,
    /// `PENPOT_FLAGS` value.
    pub penpot_flags: String,
    /// Extra backend env vars appended last (they override the generated ones).
    pub extra_backend_env: Vec<(String, String)>,
    /// Pre-seeded PostgreSQL installation (M4 bundle `postgres/` dir or an
    /// existing binaries cache). When set, the embedded postgres uses these
    /// binaries as-is and **never downloads anything** (`releases_url` is
    /// poisoned with [`OFFLINE_RELEASES_URL`]); a directory that does not
    /// contain a usable installation fails the boot loudly instead of
    /// falling back to a download. `None` keeps the dev behavior: download
    /// once into `<data_dir>/postgres/install`, offline afterwards.
    pub postgres_install_dir: Option<PathBuf>,
    /// Directories prepended to the backend JVM child's `PATH` (bundle
    /// `bin/` with `identify`/`node`, or dirs derived from env overrides).
    /// Empty = inherit the parent PATH untouched (dev behavior).
    pub child_path_prepend: Vec<PathBuf>,
    /// Optional `penpot-exporter` child (M5 board rendering). `None` (the
    /// default) = no exporter process, byte-identical to pre-M5 behavior.
    pub exporter: Option<ExporterSpec>,
    /// Postgres database name provisioned on first start.
    pub db_name: String,
    /// Password for the embedded Postgres `postgres` superuser.
    pub db_password: String,
    /// Version requirement for the embedded Postgres binaries.
    pub postgres_version: VersionReq,
    pub restart: RestartPolicy,
    /// Grace period between SIGTERM and SIGKILL on shutdown.
    pub shutdown_grace: Duration,
    /// Readiness timeout for valkey (PING).
    pub valkey_ready_timeout: Duration,
    /// Readiness timeout for the backend (`GET /readyz`); the JVM + migrations
    /// can take a while on first boot.
    pub backend_ready_timeout: Duration,
    /// How often the postgres watchdog probes the port (0 disables it).
    pub postgres_check_interval: Duration,
    /// Spawn the SIGKILL orphan watchdog (see the [`watchdog`] module docs).
    /// If the watchdog binary cannot be located, boot proceeds with a loud
    /// warning rather than failing.
    pub orphan_watchdog: bool,
    /// Explicit path to the `penpot-watchdog` binary. Default resolution:
    /// `PENPOT_WATCHDOG_BIN` env → this field → sibling of the current exe.
    pub orphan_watchdog_bin: Option<PathBuf>,
    /// Orphan-watchdog SIGTERM→SIGKILL grace period after parent death.
    pub orphan_watchdog_grace: Duration,
    pub notify: Option<NotifyHook>,
}

impl SupervisorConfig {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        storage_dir: impl Into<PathBuf>,
        valkey_path: impl Into<PathBuf>,
        backend_dir: impl Into<PathBuf>,
        jvm: JvmSpec,
        secret_key: impl Into<String>,
        public_uri: impl Into<String>,
    ) -> Self {
        SupervisorConfig {
            data_dir: data_dir.into(),
            storage_dir: storage_dir.into(),
            ports: Ports::default(),
            valkey_path: valkey_path.into(),
            backend_dir: backend_dir.into(),
            jvm,
            secret_key: secret_key.into(),
            public_uri: public_uri.into(),
            penpot_flags: DEFAULT_PENPOT_FLAGS.to_string(),
            extra_backend_env: Vec::new(),
            postgres_install_dir: None,
            child_path_prepend: Vec::new(),
            exporter: None,
            db_name: "penpot".to_string(),
            db_password: "penpot".to_string(),
            postgres_version: VersionReq::parse(DEFAULT_POSTGRES_VERSION)
                .expect("default postgres version is valid"),
            restart: RestartPolicy::default(),
            shutdown_grace: Duration::from_secs(10),
            valkey_ready_timeout: Duration::from_secs(15),
            backend_ready_timeout: Duration::from_secs(180),
            postgres_check_interval: Duration::from_secs(5),
            orphan_watchdog: true,
            orphan_watchdog_bin: None,
            orphan_watchdog_grace: watchdog::DEFAULT_GRACE,
            notify: None,
        }
    }

    /// Build the [`PostgresConfig`], honoring a pre-seeded installation.
    ///
    /// Errors only when `postgres_install_dir` is set but does not contain a
    /// usable installation — the pre-seeded path is offline-only, it never
    /// silently falls back to downloading.
    pub fn postgres_config(&self) -> Result<PostgresConfig, SupervisorError> {
        let root = self.data_dir.join("postgres");
        let (install_dir, trust, releases_url) = match &self.postgres_install_dir {
            Some(dir) => match detect_postgres_install(dir) {
                Some(PostgresInstall::Trusted(dir)) => {
                    (dir, true, Some(OFFLINE_RELEASES_URL.to_string()))
                }
                Some(PostgresInstall::VersionedRoot(dir)) => {
                    (dir, false, Some(OFFLINE_RELEASES_URL.to_string()))
                }
                None => {
                    return Err(SupervisorError::ServiceFailed {
                        service: Service::Postgres,
                        reason: format!(
                            "pre-seeded postgres install dir {} contains neither bin/initdb \
                             nor a <version>/bin/initdb subdirectory",
                            dir.display()
                        ),
                    })
                }
            },
            None => (root.join("install"), false, None),
        };
        Ok(PostgresConfig {
            install_dir,
            trust_installation_dir: trust,
            releases_url,
            data_dir: root.join("data"),
            password_file: root.join(".pgpass"),
            port: self.ports.postgres,
            password: self.db_password.clone(),
            db_name: self.db_name.clone(),
            version: self.postgres_version.clone(),
            timeout: Duration::from_secs(300),
        })
    }
    /// PGDATA location (needed by the orphan watchdog independently of
    /// whether the postgres config resolves).
    fn postgres_data_dir(&self) -> PathBuf {
        self.data_dir.join("postgres").join("data")
    }

    fn valkey_dir(&self) -> PathBuf {
        self.data_dir.join("valkey")
    }
}

/// Connection endpoints, available once [`Supervisor::start`] returns.
#[derive(Debug, Clone)]
pub struct Readiness {
    /// Full Postgres URI (with credentials) for direct DB access.
    pub postgres_uri: String,
    /// `redis://…` URI the backend was pointed at.
    pub valkey_uri: String,
    /// Backend HTTP base, e.g. `http://127.0.0.1:6161`.
    pub backend_base_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("postgres: {0}")]
    Postgres(#[from] postgresql_embedded::Error),
    #[error("{service} failed to become ready: {reason}")]
    ServiceFailed { service: Service, reason: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Command line + environment for the backend JVM, replicating the
/// `penpotapp/backend:2.16.2` container contract. Pure function → unit-tested
/// without spawning anything.
pub fn backend_command(config: &SupervisorConfig) -> (PathBuf, Vec<String>, Vec<(String, String)>) {
    let mut args: Vec<String> = config.jvm.flags.clone();
    args.push("-jar".into());
    args.push(config.jvm.jar.to_string_lossy().into_owned());
    args.extend(config.jvm.extra_args.iter().cloned());

    let mut env: Vec<(String, String)> = vec![
        // Bind localhost only; port per config.
        ("PENPOT_HTTP_SERVER_HOST".into(), "127.0.0.1".into()),
        ("PENPOT_HTTP_SERVER_PORT".into(), config.ports.backend.to_string()),
        (
            "PENPOT_DATABASE_URI".into(),
            format!("postgresql://127.0.0.1:{}/{}", config.ports.postgres, config.db_name),
        ),
        // The embedded-postgres bootstrap superuser is always `postgres`.
        ("PENPOT_DATABASE_USERNAME".into(), "postgres".into()),
        ("PENPOT_DATABASE_PASSWORD".into(), config.db_password.clone()),
        ("PENPOT_REDIS_URI".into(), format!("redis://127.0.0.1:{}/0", config.ports.valkey)),
        ("PENPOT_OBJECTS_STORAGE_BACKEND".into(), "fs".into()),
        (
            "PENPOT_OBJECTS_STORAGE_FS_DIRECTORY".into(),
            config.storage_dir.to_string_lossy().into_owned(),
        ),
        ("PENPOT_SECRET_KEY".into(), config.secret_key.clone()),
        ("PENPOT_PUBLIC_URI".into(), config.public_uri.clone()),
        ("PENPOT_TELEMETRY_ENABLED".into(), "false".into()),
        ("PENPOT_FLAGS".into(), config.penpot_flags.clone()),
    ];
    // Bundle-provided tools (`identify`, optionally `node`) must be findable
    // by the JVM child: prepend the configured dirs to the inherited PATH.
    // Empty prepend list = leave PATH alone (dev behavior, child inherits).
    if !config.child_path_prepend.is_empty() {
        let mut paths: Vec<PathBuf> = config.child_path_prepend.clone();
        if let Some(inherited) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&inherited));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            env.push(("PATH".into(), joined.to_string_lossy().into_owned()));
        }
    }
    // Later entries override earlier ones when applied to the Command.
    env.extend(config.extra_backend_env.iter().cloned());

    (config.jvm.java_path.clone(), args, env)
}

/// Command line + environment for the optional `penpot-exporter` node child,
/// replicating the recipe verified live in the M5 spike (macOS arm64,
/// Penpot 2.16.2). Pure function → unit-tested without spawning anything.
///
/// Env contract (config names are `PENPOT_` + kebab-case, case-insensitive):
/// - `PENPOT_SECRET_KEY` **must match the backend's** — the exporter derives
///   its management key via HKDF(blake2b512, secret, "exporter"); a mismatch
///   makes `upload-tempfile` fail on every render.
/// - `PENPOT_PUBLIC_URI` is the proxy origin: the exporter's browser loads
///   `<public-uri>/render.html` and POSTs
///   `<public-uri>/api/management/methods/upload-tempfile`.
/// - `PENPOT_REDIS_URI` shares the supervised valkey (connected at startup;
///   the image default `redis://redis/0` would fail).
pub fn exporter_command(
    config: &SupervisorConfig,
    spec: &ExporterSpec,
) -> (PathBuf, Vec<String>, Vec<(String, String)>) {
    let tempdir = spec
        .tempdir
        .clone()
        .unwrap_or_else(|| config.data_dir.join("exporter-tmp"));
    let env: Vec<(String, String)> = vec![
        ("PENPOT_SECRET_KEY".into(), config.secret_key.clone()),
        ("PENPOT_PUBLIC_URI".into(), config.public_uri.clone()),
        ("PENPOT_REDIS_URI".into(), format!("redis://127.0.0.1:{}/0", config.ports.valkey)),
        ("PENPOT_HTTP_SERVER_PORT".into(), spec.port.to_string()),
        ("PENPOT_TEMPDIR".into(), tempdir.to_string_lossy().into_owned()),
        (
            "PLAYWRIGHT_BROWSERS_PATH".into(),
            spec.browsers_path.to_string_lossy().into_owned(),
        ),
    ];
    (spec.node_path.clone(), vec!["app.js".into()], env)
}

/// Command line for `valkey-server`: localhost-only bind, configured port, no
/// persistence (msgbus only).
pub fn valkey_command(config: &SupervisorConfig) -> (PathBuf, Vec<String>) {
    let args = vec![
        "--port".into(),
        config.ports.valkey.to_string(),
        "--bind".into(),
        "127.0.0.1".into(),
        "--save".into(),
        String::new(), // disable RDB snapshots
        "--appendonly".into(),
        "no".into(),
        "--daemonize".into(),
        "no".into(),
        "--dir".into(),
        config.valkey_dir().to_string_lossy().into_owned(),
    ];
    (config.valkey_path.clone(), args)
}

/// A single live pid slot the watchdog feeder polls. `None` until the child is
/// spawned, then carries its pid so a SIGKILL during the readiness window can't
/// orphan it.
#[cfg(unix)]
type WatchdogSlot = Arc<std::sync::Mutex<Option<u32>>>;

/// The growable, shared set of [`WatchdogSlot`]s the feeder task reads.
#[cfg(unix)]
type WatchdogSlots = Arc<std::sync::Mutex<Vec<WatchdogSlot>>>;

/// The supervisor. Owns the three services; kills them on [`shutdown`] and
/// (best-effort) on drop — no orphans.
///
/// [`shutdown`]: Supervisor::shutdown
pub struct Supervisor {
    config: SupervisorConfig,
    notifier: Notifier,
    postgres: Option<EmbeddedPostgres>,
    postgres_watchdog: Option<tokio::task::JoinHandle<()>>,
    valkey: Option<ServiceHandle>,
    backend: Option<ServiceHandle>,
    exporter: Option<ServiceHandle>,
    /// SIGKILL orphan watchdog (parent-death cleanup; see [`watchdog`]).
    #[cfg(unix)]
    orphan_watchdog: Option<Arc<std::sync::Mutex<watchdog::WatchdogHandle>>>,
    /// Task re-feeding the current child pid set to the orphan watchdog.
    #[cfg(unix)]
    watchdog_feeder: Option<tokio::task::JoinHandle<()>>,
    /// Live pid slots the feeder polls. Shared + growable so the feeder can
    /// start BEFORE the children exist and pick each one up the moment it is
    /// spawned — a SIGKILL during a child's readiness window must not orphan
    /// it (post-M5 debt #1: the exporter was orphaned exactly this way).
    #[cfg(unix)]
    watchdog_slots: WatchdogSlots,
    shutdown_done: bool,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        let notifier = Notifier(config.notify.clone());
        Supervisor {
            config,
            notifier,
            postgres: None,
            postgres_watchdog: None,
            valkey: None,
            backend: None,
            exporter: None,
            #[cfg(unix)]
            orphan_watchdog: None,
            #[cfg(unix)]
            watchdog_feeder: None,
            #[cfg(unix)]
            watchdog_slots: Arc::new(std::sync::Mutex::new(Vec::new())),
            shutdown_done: false,
        }
    }

    /// Start postgres → valkey → backend, returning once all three are ready.
    ///
    /// On the very first run the embedded Postgres binaries are downloaded
    /// (network required once); afterwards everything is offline.
    pub async fn start(&mut self) -> Result<Readiness, SupervisorError> {
        std::fs::create_dir_all(&self.config.data_dir)?;
        std::fs::create_dir_all(&self.config.storage_dir)?;
        std::fs::create_dir_all(self.config.valkey_dir())?;

        // --- 0. SIGKILL orphan watchdog (armed before any child spawns) --
        #[cfg(unix)]
        {
            self.spawn_orphan_watchdog();
            // The pid feeder starts NOW (empty slot list, grows per spawn):
            // children are watchdog-covered from their first second, not
            // only once the whole boot succeeded (post-M5 debt #1).
            self.spawn_watchdog_feeder();
        }

        // --- 1. Postgres (embedded) -------------------------------------
        self.notifier.emit(SupervisorEvent::Starting { service: Service::Postgres });
        let mut pg = EmbeddedPostgres::new(self.config.postgres_config()?);
        let postgres_uri = pg.start().await?;
        self.notifier.emit(SupervisorEvent::Ready { service: Service::Postgres });
        let pg_settings = pg.settings_clone();
        self.postgres = Some(pg);
        #[cfg(unix)]
        self.push_watchdog_pids();
        if !self.config.postgres_check_interval.is_zero() {
            self.postgres_watchdog = Some(tokio::spawn(postgres::watchdog(
                pg_settings,
                self.config.ports.postgres,
                self.config.postgres_check_interval,
                self.config.restart,
                self.notifier.clone(),
            )));
        }

        // --- 2. Valkey ----------------------------------------------------
        let (valkey_program, valkey_args) = valkey_command(&self.config);
        let valkey_spec = ChildSpec {
            service: Service::Valkey,
            program: valkey_program,
            args: valkey_args,
            envs: Vec::new(),
            cwd: Some(self.config.valkey_dir()),
            probe: Probe::ValkeyPing { port: self.config.ports.valkey },
            ready_timeout: self.config.valkey_ready_timeout,
            listener_port: None,
        };
        let valkey = ServiceHandle::spawn(
            valkey_spec,
            self.config.restart,
            self.config.shutdown_grace,
            self.notifier.clone(),
        );
        #[cfg(unix)]
        self.register_watchdog_slot(&valkey);
        valkey.wait_ready().await.map_err(|reason| SupervisorError::ServiceFailed {
            service: Service::Valkey,
            reason,
        })?;
        self.valkey = Some(valkey);
        #[cfg(unix)]
        self.push_watchdog_pids();

        // --- 3. Backend JVM ------------------------------------------------
        let (program, args, envs) = backend_command(&self.config);
        let backend_spec = ChildSpec {
            service: Service::Backend,
            program,
            args,
            envs,
            cwd: Some(self.config.backend_dir.clone()),
            probe: Probe::HttpOk { port: self.config.ports.backend, path: "/readyz".into() },
            ready_timeout: self.config.backend_ready_timeout,
            listener_port: None,
        };
        let backend = ServiceHandle::spawn(
            backend_spec,
            self.config.restart,
            self.config.shutdown_grace,
            self.notifier.clone(),
        );
        #[cfg(unix)]
        self.register_watchdog_slot(&backend);
        backend.wait_ready().await.map_err(|reason| SupervisorError::ServiceFailed {
            service: Service::Backend,
            reason,
        })?;
        self.backend = Some(backend);
        #[cfg(unix)]
        self.push_watchdog_pids();

        // --- 4. Exporter (optional, M5; N2 stale-adoption fix) -------------
        if let Some(spec) = self.config.exporter.clone() {
            // Post-M5 debt #1: a port that is already busy BEFORE we spawn is
            // a hard boot error naming the pid — never adopt the /readyz
            // answers of a process we do not own (a SIGKILL-orphaned exporter
            // passes the probe while our own child dies with EADDRINUSE and
            // every render then fails with a secret-key mismatch).
            if probe::port_has_listener(spec.port).await {
                let pids = tokio::task::spawn_blocking(move || probe::listener_pids(spec.port))
                    .await
                    .unwrap_or_default();
                let pid_desc = if pids.is_empty() {
                    "unknown pid (lsof unavailable)".to_string()
                } else {
                    format!(
                        "pid(s) {}",
                        pids.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    )
                };
                return Err(SupervisorError::ServiceFailed {
                    service: Service::Exporter,
                    reason: format!(
                        "exporter port {} is already in use by {pid_desc} — refusing to adopt \
                         a stale exporter. Kill that process or set \
                         PENPOT_LOCAL_EXPORTER_PORT to a free port.",
                        spec.port
                    ),
                });
            }
            self.notifier.emit(SupervisorEvent::Starting { service: Service::Exporter });
            let (program, args, envs) = exporter_command(&self.config, &spec);
            // PENPOT_TEMPDIR must exist (the exporter writes render tempfiles
            // there before uploading them).
            if let Some((_, tempdir)) = envs.iter().find(|(k, _)| k == "PENPOT_TEMPDIR") {
                std::fs::create_dir_all(tempdir)?;
            }
            let exporter_spec = ChildSpec {
                service: Service::Exporter,
                program,
                args,
                envs,
                cwd: Some(spec.exporter_dir.clone()),
                probe: Probe::HttpOk { port: spec.port, path: "/readyz".into() },
                ready_timeout: spec.ready_timeout,
                listener_port: Some(spec.port),
            };
            let exporter = ServiceHandle::spawn(
                exporter_spec,
                self.config.restart,
                self.config.shutdown_grace,
                self.notifier.clone(),
            );
            #[cfg(unix)]
            self.register_watchdog_slot(&exporter);
            exporter.wait_ready().await.map_err(|reason| SupervisorError::ServiceFailed {
                service: Service::Exporter,
                reason,
            })?;
            self.exporter = Some(exporter);
            #[cfg(unix)]
            self.push_watchdog_pids();
        }

        Ok(Readiness {
            postgres_uri,
            valkey_uri: format!("redis://127.0.0.1:{}/0", self.config.ports.valkey),
            backend_base_url: format!("http://127.0.0.1:{}", self.config.ports.backend),
        })
    }

    /// Orderly shutdown, reverse start order: backend → valkey → postgres.
    /// SIGTERM first, SIGKILL after the grace period. Idempotent.
    pub async fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;

        // Stop the pid feeder first so it doesn't race the teardown below.
        #[cfg(unix)]
        if let Some(feeder) = self.watchdog_feeder.take() {
            feeder.abort();
        }
        // Stop the watchdog first so it doesn't resurrect postgres below.
        if let Some(watchdog) = self.postgres_watchdog.take() {
            watchdog.abort();
        }
        if let Some(exporter) = self.exporter.take() {
            exporter.shutdown().await;
        }
        if let Some(backend) = self.backend.take() {
            backend.shutdown().await;
        }
        if let Some(valkey) = self.valkey.take() {
            valkey.shutdown().await;
        }
        if let Some(mut pg) = self.postgres.take() {
            if let Err(error) = pg.stop().await {
                warn!(%error, "postgres did not stop cleanly");
            }
            self.notifier.emit(SupervisorEvent::Stopped { service: Service::Postgres });
        }

        // CLEAN shutdown: the children above were stopped properly, so close
        // the protocol with `bye` — the orphan watchdog exits WITHOUT killing
        // anything. (Every other exit path leaves the pipe to hit EOF, which
        // IS the kill trigger.)
        #[cfg(unix)]
        if let Some(watchdog) = self.orphan_watchdog.take() {
            let watchdog = Arc::clone(&watchdog);
            // `bye` blocks up to a few seconds waiting for the watchdog to
            // exit; do it off the async runtime.
            let joined = tokio::task::spawn_blocking(move || {
                watchdog
                    .lock()
                    .expect("watchdog mutex")
                    .bye(Duration::from_secs(3));
            })
            .await;
            if joined.is_err() {
                warn!("orphan watchdog bye task panicked");
            } else {
                info!("orphan watchdog dismissed (bye)");
            }
        }
    }

    /// Spawn the SIGKILL orphan watchdog (never fails the boot; logs loudly
    /// when the binary is missing). See [`watchdog`] module docs.
    #[cfg(unix)]
    fn spawn_orphan_watchdog(&mut self) {
        if !self.config.orphan_watchdog {
            return;
        }
        let pgdata = self.config.postgres_data_dir();
        let Some(bin) =
            watchdog::WatchdogHandle::locate_bin(self.config.orphan_watchdog_bin.as_deref())
        else {
            warn!(
                "orphan watchdog binary '{}' not found ({} env, config, or sibling of the \
                 current executable) — if this process is SIGKILLed, postgres/valkey/backend \
                 will be orphaned and keep holding their ports",
                watchdog::WATCHDOG_BIN_NAME,
                watchdog::WATCHDOG_BIN_ENV,
            );
            return;
        };
        match watchdog::WatchdogHandle::spawn(
            &bin,
            self.config.orphan_watchdog_grace,
            Some(&pgdata),
        ) {
            Ok(handle) => {
                info!(pid = handle.pid(), bin = %bin.display(), "orphan watchdog armed");
                self.orphan_watchdog = Some(Arc::new(std::sync::Mutex::new(handle)));
            }
            Err(error) => {
                warn!(%error, bin = %bin.display(), "failed to spawn orphan watchdog");
            }
        }
    }

    /// Register a freshly spawned service's live pid slot with the watchdog
    /// feeder — called right after `ServiceHandle::spawn`, BEFORE waiting for
    /// readiness, so a SIGKILL during the readiness window cannot orphan the
    /// child. Also pushes the current set immediately (best-effort; the
    /// feeder re-sends within a second once the pid lands in the slot).
    #[cfg(unix)]
    fn register_watchdog_slot(&self, handle: &ServiceHandle) {
        self.watchdog_slots
            .lock()
            .expect("watchdog slots mutex")
            .push(handle.pid_slot());
        self.push_watchdog_pids();
    }

    /// Current child pid set: postmaster (from `postmaster.pid`, since
    /// pg_ctl detaches it) + every registered supervised pid slot (valkey,
    /// backend, exporter — filled as each service spawns).
    #[cfg(unix)]
    fn collect_child_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        if let Some(pid) = watchdog::read_postmaster_pid(&self.config.postgres_data_dir()) {
            pids.push(pid);
        }
        for slot in self.watchdog_slots.lock().expect("watchdog slots mutex").iter() {
            if let Some(pid) = *slot.lock().expect("pid mutex") {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Send the current pid set to the orphan watchdog (best-effort).
    #[cfg(unix)]
    fn push_watchdog_pids(&self) {
        let Some(watchdog) = &self.orphan_watchdog else { return };
        let pids = self.collect_child_pids();
        if let Err(error) = watchdog
            .lock()
            .expect("watchdog mutex")
            .send_pids(&pids)
        {
            warn!(%error, "orphan watchdog pipe write failed");
        }
    }

    /// Background task re-sending the pid set whenever it changes (crash
    /// respawns give children new pids; the watchdog must track the CURRENT
    /// set, not the boot-time one). Started at the very top of `start()` —
    /// it reads the shared, growable slot list, so children spawned later in
    /// the boot are covered from their first second (post-M5 debt #1).
    #[cfg(unix)]
    fn spawn_watchdog_feeder(&mut self) {
        let Some(watchdog) = &self.orphan_watchdog else { return };
        let watchdog = Arc::clone(watchdog);
        let pgdata = self.config.postgres_data_dir();
        let slots = Arc::clone(&self.watchdog_slots);
        let mut last = self.collect_child_pids();
        self.watchdog_feeder = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut pids = Vec::new();
                if let Some(pid) = watchdog::read_postmaster_pid(&pgdata) {
                    pids.push(pid);
                }
                {
                    let slots = slots.lock().expect("watchdog slots mutex");
                    for slot in slots.iter() {
                        if let Some(pid) = *slot.lock().expect("pid mutex") {
                            pids.push(pid);
                        }
                    }
                }
                pids.sort_unstable();
                pids.dedup();
                if pids != last {
                    let result = watchdog
                        .lock()
                        .expect("watchdog mutex")
                        .send_pids(&pids);
                    match result {
                        Ok(()) => last = pids,
                        Err(error) => {
                            warn!(%error, "orphan watchdog pipe closed; stopping pid feeder");
                            return;
                        }
                    }
                }
            }
        }));
    }
}

impl Drop for Supervisor {
    /// Best-effort no-orphan guarantee if the supervisor is dropped without
    /// `shutdown()`: abort supervision tasks and SIGKILL any live children.
    /// The embedded Postgres handle stops its server in its own `Drop`.
    fn drop(&mut self) {
        if self.shutdown_done {
            return;
        }
        #[cfg(unix)]
        if let Some(feeder) = self.watchdog_feeder.take() {
            feeder.abort();
        }
        if let Some(watchdog) = self.postgres_watchdog.take() {
            watchdog.abort();
        }
        if let Some(exporter) = self.exporter.take() {
            exporter.kill_now();
        }
        if let Some(backend) = self.backend.take() {
            backend.kill_now();
        }
        if let Some(valkey) = self.valkey.take() {
            valkey.kill_now();
        }
        // `self.postgres` drops after this body: postgresql_embedded's Drop
        // runs `pg_ctl stop` synchronously if the server is still up.
        //
        // `self.orphan_watchdog` (if any) also drops WITHOUT `bye`: the pipe
        // write end closes when this process exits, and the watchdog then
        // re-checks/kills the last-known pids — a deliberate backstop for
        // this best-effort path (panic, early drop, boot failure).
    }
}

/// Await a `watch::Receiver<ReadyState>` until Ready (Ok) or Failed (Err).
pub(crate) async fn await_ready(rx: &mut watch::Receiver<ReadyState>) -> Result<(), String> {
    loop {
        match &*rx.borrow() {
            ReadyState::Ready => return Ok(()),
            ReadyState::Failed(reason) => return Err(reason.clone()),
            ReadyState::Pending => {}
        }
        if rx.changed().await.is_err() {
            return Err("supervision task exited before readiness".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SupervisorConfig {
        let mut config = SupervisorConfig::new(
            "/data/penpot-local",
            "/data/penpot-local/assets",
            "/usr/bin/valkey-server",
            "/data/penpot-local/runtime/backend",
            JvmSpec::penpot_2_16("/usr/bin/java"),
            "sekrit",
            "http://localhost:8686",
        );
        config.ports = Ports { postgres: 5433, valkey: 6380, backend: 6161 };
        config
    }

    fn env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        // Last occurrence wins, mirroring Command::env application order.
        env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RestartPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(30),
        };
        assert_eq!(policy.backoff(1), Duration::from_millis(500));
        assert_eq!(policy.backoff(2), Duration::from_secs(1));
        assert_eq!(policy.backoff(3), Duration::from_secs(2));
        assert_eq!(policy.backoff(7), Duration::from_secs(30)); // 32s capped
        assert_eq!(policy.backoff(100), Duration::from_secs(30)); // no overflow
    }

    #[test]
    fn backend_command_replicates_image_contract() {
        let config = test_config();
        let (program, args, _env) = backend_command(&config);
        assert_eq!(program, PathBuf::from("/usr/bin/java"));
        // Flags exactly as in penpotapp/backend:2.16.2 run.sh, then -jar, then
        // the entrypoint flag.
        assert_eq!(
            args,
            vec![
                "-Dim4java.useV7=true",
                "-Djava.util.logging.manager=org.apache.logging.log4j.jul.LogManager",
                "-Dlog4j2.configurationFile=log4j2.xml",
                "-XX:-OmitStackTraceInFastThrow",
                "--sun-misc-unsafe-memory-access=allow",
                "--enable-native-access=ALL-UNNAMED",
                "--enable-preview",
                "-jar",
                "penpot.jar",
                "-m",
                "app.main",
            ]
        );
    }

    #[test]
    fn backend_env_is_complete() {
        let config = test_config();
        let (_, _, env) = backend_command(&config);
        assert_eq!(env_get(&env, "PENPOT_HTTP_SERVER_HOST"), Some("127.0.0.1"));
        assert_eq!(env_get(&env, "PENPOT_HTTP_SERVER_PORT"), Some("6161"));
        assert_eq!(
            env_get(&env, "PENPOT_DATABASE_URI"),
            Some("postgresql://127.0.0.1:5433/penpot")
        );
        assert_eq!(env_get(&env, "PENPOT_DATABASE_USERNAME"), Some("postgres"));
        assert_eq!(env_get(&env, "PENPOT_DATABASE_PASSWORD"), Some("penpot"));
        assert_eq!(env_get(&env, "PENPOT_REDIS_URI"), Some("redis://127.0.0.1:6380/0"));
        assert_eq!(env_get(&env, "PENPOT_OBJECTS_STORAGE_BACKEND"), Some("fs"));
        assert_eq!(
            env_get(&env, "PENPOT_OBJECTS_STORAGE_FS_DIRECTORY"),
            Some("/data/penpot-local/assets")
        );
        assert_eq!(env_get(&env, "PENPOT_SECRET_KEY"), Some("sekrit"));
        assert_eq!(env_get(&env, "PENPOT_PUBLIC_URI"), Some("http://localhost:8686"));
        assert_eq!(env_get(&env, "PENPOT_TELEMETRY_ENABLED"), Some("false"));
        assert_eq!(env_get(&env, "PENPOT_FLAGS"), Some(DEFAULT_PENPOT_FLAGS));
    }

    #[test]
    fn extra_backend_env_overrides_generated_values() {
        let mut config = test_config();
        config.extra_backend_env =
            vec![("PENPOT_FLAGS".into(), "enable-prepl-server".into())];
        let (_, _, env) = backend_command(&config);
        assert_eq!(env_get(&env, "PENPOT_FLAGS"), Some("enable-prepl-server"));
    }

    /// Write an executable-ish marker file, creating parent dirs.
    fn touch(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
    }

    #[test]
    fn postgres_config_dev_default_downloads_into_data_dir() {
        let config = test_config();
        let pg = config.postgres_config().expect("dev config resolves");
        assert_eq!(pg.install_dir, PathBuf::from("/data/penpot-local/postgres/install"));
        assert!(!pg.trust_installation_dir);
        assert_eq!(pg.releases_url, None, "dev mode keeps the crate default releases url");
    }

    #[test]
    fn postgres_config_preseeded_flat_install_is_trusted_and_offline() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("bin/initdb"));
        let mut config = test_config();
        config.postgres_install_dir = Some(dir.path().to_path_buf());
        let pg = config.postgres_config().expect("flat install resolves");
        assert_eq!(pg.install_dir, dir.path());
        assert!(pg.trust_installation_dir);
        assert_eq!(pg.releases_url.as_deref(), Some(OFFLINE_RELEASES_URL));
        // PGDATA stays in the app data dir, never inside the (read-only) bundle.
        assert_eq!(pg.data_dir, PathBuf::from("/data/penpot-local/postgres/data"));
    }

    #[test]
    fn postgres_config_preseeded_versioned_root_is_offline() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("15.18.0/bin/initdb"));
        let mut config = test_config();
        config.postgres_install_dir = Some(dir.path().to_path_buf());
        let pg = config.postgres_config().expect("versioned root resolves");
        assert_eq!(pg.install_dir, dir.path());
        assert!(!pg.trust_installation_dir, "the crate resolves the version subdir itself");
        assert_eq!(pg.releases_url.as_deref(), Some(OFFLINE_RELEASES_URL));
    }

    #[test]
    fn postgres_config_preseeded_garbage_fails_loudly_instead_of_downloading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README"), b"nothing here").unwrap();
        let mut config = test_config();
        config.postgres_install_dir = Some(dir.path().to_path_buf());
        let err = config.postgres_config().expect_err("must not fall back to download");
        assert!(err.to_string().contains("initdb"), "unexpected error: {err}");
    }

    #[test]
    fn detect_postgres_install_classifies_both_shapes() {
        let flat = tempfile::tempdir().unwrap();
        touch(&flat.path().join("bin/initdb"));
        assert_eq!(
            detect_postgres_install(flat.path()),
            Some(PostgresInstall::Trusted(flat.path().to_path_buf()))
        );

        let versioned = tempfile::tempdir().unwrap();
        touch(&versioned.path().join("15.18.0/bin/initdb"));
        assert_eq!(
            detect_postgres_install(versioned.path()),
            Some(PostgresInstall::VersionedRoot(versioned.path().to_path_buf()))
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(detect_postgres_install(empty.path()), None);
        assert_eq!(detect_postgres_install(&empty.path().join("missing")), None);

        // A non-version subdir with binaries is NOT a versioned root.
        let odd = tempfile::tempdir().unwrap();
        touch(&odd.path().join("latest/bin/initdb"));
        assert_eq!(detect_postgres_install(odd.path()), None);
    }

    #[test]
    fn backend_path_untouched_without_prepend() {
        let config = test_config();
        let (_, _, env) = backend_command(&config);
        assert!(
            env_get(&env, "PATH").is_none(),
            "dev mode must inherit the parent PATH untouched"
        );
    }

    #[test]
    fn backend_path_prepends_bundle_bin_dirs() {
        let mut config = test_config();
        config.child_path_prepend =
            vec![PathBuf::from("/bundle/bin"), PathBuf::from("/override/dir")];
        let (_, _, env) = backend_command(&config);
        let path = env_get(&env, "PATH").expect("PATH must be set for the JVM child");
        assert!(
            path.starts_with("/bundle/bin:/override/dir"),
            "prepend dirs must come first: {path}"
        );
        // The inherited PATH survives after the prepends.
        if let Ok(parent_path) = std::env::var("PATH") {
            if let Some(first) = parent_path.split(':').next() {
                if !first.is_empty() {
                    assert!(path.contains(first), "inherited PATH must be preserved: {path}");
                }
            }
        }
    }

    #[test]
    fn exporter_command_replicates_the_spike_recipe() {
        let config = test_config();
        let spec = ExporterSpec::new(
            "/usr/bin/node",
            "/data/penpot-local/runtime/exporter",
            6467,
            "/data/penpot-local/runtime/exporter-browsers",
        );
        let (program, args, env) = exporter_command(&config, &spec);
        assert_eq!(program, PathBuf::from("/usr/bin/node"));
        assert_eq!(args, vec!["app.js"]);
        // Secret key MUST be the backend's (HKDF-derived exporter key).
        assert_eq!(env_get(&env, "PENPOT_SECRET_KEY"), Some("sekrit"));
        // Public URI is the proxy origin (render.html + upload-tempfile).
        assert_eq!(env_get(&env, "PENPOT_PUBLIC_URI"), Some("http://localhost:8686"));
        // Shares the supervised valkey (default redis://redis/0 would fail).
        assert_eq!(env_get(&env, "PENPOT_REDIS_URI"), Some("redis://127.0.0.1:6380/0"));
        assert_eq!(env_get(&env, "PENPOT_HTTP_SERVER_PORT"), Some("6467"));
        assert_eq!(
            env_get(&env, "PENPOT_TEMPDIR"),
            Some("/data/penpot-local/exporter-tmp"),
            "tempdir defaults under the data dir"
        );
        assert_eq!(
            env_get(&env, "PLAYWRIGHT_BROWSERS_PATH"),
            Some("/data/penpot-local/runtime/exporter-browsers")
        );
    }

    #[test]
    fn exporter_tempdir_override_wins() {
        let config = test_config();
        let mut spec = ExporterSpec::new("/usr/bin/node", "/x", 6467, "/b");
        spec.tempdir = Some(PathBuf::from("/custom/tmp"));
        let (_, _, env) = exporter_command(&config, &spec);
        assert_eq!(env_get(&env, "PENPOT_TEMPDIR"), Some("/custom/tmp"));
    }

    #[test]
    fn exporter_is_off_by_default() {
        assert!(test_config().exporter.is_none());
    }

    #[test]
    fn valkey_command_is_localhost_only_without_persistence() {
        let config = test_config();
        let (program, args) = valkey_command(&config);
        assert_eq!(program, PathBuf::from("/usr/bin/valkey-server"));
        let joined = args.join(" ");
        assert!(joined.contains("--port 6380"));
        assert!(joined.contains("--bind 127.0.0.1"));
        assert!(joined.contains("--appendonly no"));
        assert!(joined.contains("--daemonize no"));
        // `--save` followed by an empty argument disables RDB snapshots.
        let save_idx = args.iter().position(|a| a == "--save").unwrap();
        assert_eq!(args[save_idx + 1], "");
    }
}
