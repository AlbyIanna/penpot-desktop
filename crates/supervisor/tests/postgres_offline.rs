//! M4 offline proof: a boot with a **pre-seeded** postgres installation
//! performs ZERO network calls.
//!
//! Two independent tripwires guarantee any network attempt fails loudly
//! instead of silently reaching GitHub:
//! 1. `SupervisorConfig::postgres_config()` poisons `releases_url` with
//!    `OFFLINE_RELEASES_URL` (a dead localhost port) whenever
//!    `postgres_install_dir` is set — the download client would dial
//!    127.0.0.1:1 and get connection-refused.
//! 2. This test additionally sets `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` to
//!    the same dead port for the whole process (reqwest honors proxy env
//!    vars by default), so even a hypothetical non-poisoned code path
//!    cannot leave the machine.
//!
//! Both bundle-contract shapes of `penpot-runtime/postgres/` are exercised:
//! a versioned root (`postgres/15.18.0/bin/initdb`, the theseus cache
//! layout) and a flat trusted installation (`postgres/bin/initdb`).
//!
//! Seeding: the test reuses the existing local caches
//! (`~/.cache/penpot-local/pg-install` from the m3 scripts, or
//! `target/tmp/pg-install-cache` from `tests/postgres_embedded.rs`). Only if
//! neither exists does it download ONCE into the target cache — before the
//! proxy poison is applied.

#![cfg(unix)]

use std::net::TcpStream;
use std::path::{Path, PathBuf};

use supervisor::{
    detect_postgres_install, EmbeddedPostgres, JvmSpec, PostgresInstall, SupervisorConfig,
    OFFLINE_RELEASES_URL,
};

const DEAD_PROXY: &str = "http://127.0.0.1:1";

/// Find (or create, network needed once) a real pinned-version installation
/// and return its versioned root (a dir containing `15.18.0/bin/initdb`).
async fn seed_versioned_root() -> PathBuf {
    let candidates = [
        dirs_cache().map(|c| c.join("penpot-local/pg-install")),
        Some(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pg-install-cache")),
    ];
    for candidate in candidates.iter().flatten() {
        if matches!(
            detect_postgres_install(candidate),
            Some(PostgresInstall::VersionedRoot(_))
        ) {
            return candidate.clone();
        }
    }

    // Nothing cached on this machine: download once (BEFORE the proxy poison
    // is applied) into the shared target cache, mirroring
    // tests/postgres_embedded.rs.
    let install_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pg-install-cache");
    let scratch = tempfile::tempdir().expect("tempdir");
    let mut config = base_supervisor_config(scratch.path());
    config.postgres_install_dir = None; // dev path: download allowed
    let mut pg_config = config.postgres_config().expect("dev postgres config");
    pg_config.install_dir = install_dir.clone();
    let mut pg = EmbeddedPostgres::new(pg_config);
    pg.start().await.expect("one-time seeding download+boot");
    pg.stop().await.expect("seeding instance stops");
    install_dir
}

fn dirs_cache() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
}

fn base_supervisor_config(data_dir: &Path) -> SupervisorConfig {
    let mut config = SupervisorConfig::new(
        data_dir,
        data_dir.join("assets"),
        "/usr/bin/valkey-server", // never spawned in this test
        data_dir.join("backend"),
        JvmSpec::penpot_2_16("/usr/bin/java"), // never spawned in this test
        "sekrit",
        "http://localhost:0",
    );
    config.ports.postgres = 0; // random free port
    config
}

/// Full postgres lifecycle (setup → initdb → start → create db → stop) from
/// a pre-seeded install dir, with all network egress poisoned.
async fn boot_offline(install_dir: &Path, expect_trusted: bool) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let mut config = base_supervisor_config(scratch.path());
    config.postgres_install_dir = Some(install_dir.to_path_buf());
    let pg_config = config.postgres_config().expect("pre-seeded config resolves");
    assert_eq!(
        pg_config.trust_installation_dir, expect_trusted,
        "install shape misdetected for {}",
        install_dir.display()
    );
    assert_eq!(
        pg_config.releases_url.as_deref(),
        Some(OFFLINE_RELEASES_URL),
        "pre-seeded installs must poison the releases url"
    );

    let mut pg = EmbeddedPostgres::new(pg_config);
    let uri = pg
        .start()
        .await
        .expect("offline boot from the pre-seeded install dir must succeed");
    let port = pg.port();
    assert_ne!(port, 0);
    TcpStream::connect(("127.0.0.1", port)).expect("postgres listens");
    assert!(pg.database_exists("penpot").await.expect("db query"));
    assert!(uri.contains("/penpot"));
    pg.stop().await.expect("clean stop");
}

#[tokio::test]
async fn preseeded_install_boots_with_zero_network_calls() {
    let versioned_root = seed_versioned_root().await;

    // From here on the whole process is cut off from the network: any HTTP
    // request through reqwest's env-proxy support dials a dead local port.
    // (Safe: this is the only test in this integration-test binary.)
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]
    {
        std::env::set_var(var, DEAD_PROXY);
    }

    // Shape 1: versioned root (theseus cache layout / bundle postgres/).
    boot_offline(&versioned_root, false).await;

    // Shape 2: flat trusted installation (bundle postgres/ pointing straight
    // at bin/). Reuse the same binaries via the version subdirectory
    // (DEFAULT_POSTGRES_VERSION is "=15.18.0" → the extracted dir "15.18.0").
    let flat = versioned_root.join(supervisor::DEFAULT_POSTGRES_VERSION.trim_start_matches('='));
    assert!(
        flat.join("bin/initdb").is_file(),
        "expected a flat installation at {}",
        flat.display()
    );
    boot_offline(&flat, true).await;

    // The data dir (not the install dir) received PGDATA — the install dir
    // was treated as read-only in both shapes.
    assert!(
        !versioned_root.join("data").exists(),
        "the pre-seeded install dir must never be written to"
    );
}
