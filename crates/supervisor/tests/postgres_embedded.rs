//! Real integration test: start and stop an actual embedded PostgreSQL 15.x.
//!
//! On the first run this downloads the pinned postgres binaries from
//! theseus-rs/postgresql-binaries into `target/tmp/pg-install-cache` (network
//! required once; subsequent runs are offline and much faster).

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use supervisor::{EmbeddedPostgres, PostgresConfig, VersionReq, DEFAULT_POSTGRES_VERSION};

#[tokio::test]
async fn embedded_postgres_start_provision_stop() {
    // Shared cache so repeated test runs don't re-download ~1 min of binaries.
    let install_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pg-install-cache");
    let scratch = tempfile::tempdir().expect("tempdir");

    let config = PostgresConfig {
        install_dir,
        data_dir: scratch.path().join("data"),
        password_file: scratch.path().join(".pgpass"),
        port: 0, // random free port; avoids clashing with a dev instance on 5433
        password: "penpot".to_string(),
        db_name: "penpot".to_string(),
        version: VersionReq::parse(DEFAULT_POSTGRES_VERSION).expect("valid version req"),
        timeout: Duration::from_secs(300),
    };

    let mut pg = EmbeddedPostgres::new(config);
    let uri = pg.start().await.expect("embedded postgres should start");
    let port = pg.port();
    assert_ne!(port, 0, "port should be resolved");
    assert!(
        uri.contains(&format!(":{port}/penpot")),
        "uri should point at the penpot db on the resolved port: {uri}"
    );

    // The server really listens.
    TcpStream::connect(("127.0.0.1", port)).expect("postgres should accept TCP connections");

    // The penpot database was provisioned; a random name was not.
    assert!(pg.database_exists("penpot").await.expect("query db existence"));
    assert!(!pg.database_exists("definitely_not_here").await.expect("query db existence"));

    // Starting is idempotent state-wise: stop closes the port.
    pg.stop().await.expect("postgres should stop cleanly");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port must be closed after stop"
    );
}

#[tokio::test]
async fn embedded_postgres_data_dir_survives_restart() {
    let install_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pg-install-cache");
    let scratch = tempfile::tempdir().expect("tempdir");

    let make_config = || PostgresConfig {
        install_dir: install_dir.clone(),
        data_dir: scratch.path().join("data"),
        password_file: scratch.path().join(".pgpass"),
        port: 0,
        password: "penpot".to_string(),
        db_name: "penpot".to_string(),
        version: VersionReq::parse(DEFAULT_POSTGRES_VERSION).expect("valid version req"),
        timeout: Duration::from_secs(300),
    };

    // First lifecycle: initdb + create the database.
    let mut pg = EmbeddedPostgres::new(make_config());
    pg.start().await.expect("first start");
    pg.stop().await.expect("first stop");
    drop(pg);

    // Second lifecycle over the same data dir: the database must still exist
    // (the data dir is a persistent cache, not a tempdir).
    let mut pg = EmbeddedPostgres::new(make_config());
    pg.start().await.expect("second start");
    assert!(
        pg.database_exists("penpot").await.expect("query db existence"),
        "database created in the first lifecycle must survive a restart"
    );
    pg.stop().await.expect("second stop");
}
