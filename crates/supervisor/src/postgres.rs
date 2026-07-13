//! Embedded PostgreSQL via the `postgresql_embedded` crate (pinned 15.x
//! binaries from theseus-rs/postgresql-binaries, downloaded once and cached
//! under the install dir; fully offline afterwards).

use std::path::PathBuf;
use std::time::Duration;

use postgresql_embedded::{PostgreSQL, Settings, VersionReq};
use tracing::{debug, warn};

use crate::probe;
use crate::{Notifier, RestartPolicy, Service, SupervisorEvent};

/// Configuration for the embedded Postgres instance.
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    /// Where the postgres binaries are extracted (cache; survives restarts).
    pub install_dir: PathBuf,
    /// PGDATA.
    pub data_dir: PathBuf,
    /// Superuser password file used at initdb time.
    pub password_file: PathBuf,
    /// TCP port on 127.0.0.1 (0 = pick a free port at start).
    pub port: u16,
    /// Password for the `postgres` bootstrap superuser.
    pub password: String,
    /// Database created on first start if missing.
    pub db_name: String,
    /// Pinned version requirement (e.g. `=15.18.0`).
    pub version: VersionReq,
    /// Timeout for pg_ctl start/stop (first boot includes initdb).
    pub timeout: Duration,
}

/// A running (or startable) embedded Postgres. Dropping it stops the server
/// (the inner `PostgreSQL` runs `pg_ctl stop` in its own `Drop`).
pub struct EmbeddedPostgres {
    inner: PostgreSQL,
    db_name: String,
}

impl EmbeddedPostgres {
    pub fn new(config: PostgresConfig) -> Self {
        // Settings::new() picks temp dirs and a random password; we override
        // everything we care about with pinned, persistent locations.
        let mut settings = Settings::new();
        settings.version = config.version;
        settings.installation_dir = config.install_dir;
        settings.data_dir = config.data_dir;
        settings.password_file = config.password_file;
        settings.host = "127.0.0.1".to_string();
        settings.port = config.port;
        settings.username = "postgres".to_string();
        settings.password = config.password;
        settings.temporary = false; // the data dir is persistent, not a tempdir
        settings.timeout = Some(config.timeout);
        EmbeddedPostgres {
            inner: PostgreSQL::new(settings),
            db_name: config.db_name,
        }
    }

    /// Download/extract binaries if needed, initdb if needed, start the
    /// server, and ensure the configured database exists. Returns the
    /// connection URI (with credentials).
    pub async fn start(&mut self) -> Result<String, postgresql_embedded::Error> {
        self.inner.setup().await?;
        self.inner.start().await?;
        if !self.inner.database_exists(&self.db_name).await? {
            debug!(db = %self.db_name, "creating database");
            self.inner.create_database(&self.db_name).await?;
        }
        Ok(self.uri())
    }

    /// `pg_ctl stop` (fast shutdown), waiting for completion.
    pub async fn stop(&mut self) -> Result<(), postgresql_embedded::Error> {
        self.inner.stop().await
    }

    /// Connection URI (with credentials) for the configured database.
    pub fn uri(&self) -> String {
        self.inner.settings().url(&self.db_name)
    }

    /// The actual port (resolved if the config asked for 0).
    pub fn port(&self) -> u16 {
        self.inner.settings().port
    }

    pub async fn database_exists(
        &self,
        name: &str,
    ) -> Result<bool, postgresql_embedded::Error> {
        self.inner.database_exists(name).await
    }

    pub(crate) fn settings_clone(&self) -> Settings {
        self.inner.settings().clone()
    }
}

/// Watchdog for the embedded Postgres: unlike valkey/backend there is no
/// waitable child handle (pg_ctl detaches the postmaster), so liveness is a
/// periodic TCP probe; on sustained failure the server is restarted with the
/// usual backoff policy.
pub(crate) async fn watchdog(
    settings: Settings,
    port: u16,
    interval: Duration,
    policy: RestartPolicy,
    notifier: Notifier,
) {
    let service = Service::Postgres;
    loop {
        tokio::time::sleep(interval).await;
        if probe::tcp_open(port).await.is_ok() {
            continue;
        }
        // Debounce a transient refusal before declaring a crash.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if probe::tcp_open(port).await.is_ok() {
            continue;
        }

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            if attempt > policy.max_retries {
                notifier.emit(SupervisorEvent::Crashed { service, attempt, restarting: false });
                notifier.emit(SupervisorEvent::GaveUp { service });
                return;
            }
            notifier.emit(SupervisorEvent::Crashed { service, attempt, restarting: true });
            tokio::time::sleep(policy.backoff(attempt)).await;

            // A transient handle over the same settings; `mem::forget` keeps
            // its Drop (pg_ctl stop) from tearing down the server we just
            // started — the Supervisor's own EmbeddedPostgres still owns
            // shutdown for this data dir. Leaks only a Settings struct, and
            // only on the (rare) postgres-crash path.
            let mut pg = PostgreSQL::new(settings.clone());
            match pg.start().await {
                Ok(()) => {
                    std::mem::forget(pg);
                    notifier.emit(SupervisorEvent::Restarted { service });
                    break;
                }
                Err(error) => {
                    warn!(%error, attempt, "postgres restart attempt failed");
                    std::mem::forget(pg);
                }
            }
        }
    }
}
