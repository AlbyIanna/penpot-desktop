//! SIMULATION of M4 packaged mode — no real bundle, no processes spawned.
//!
//! A fake `penpot-runtime/` directory with stub executables stands in for
//! the real bundle (the bundle builder is a separate deliverable). The test
//! drives the exact resolution path the packaged app uses —
//! `AppConfig::resolve_with_resources(<resources dir>)` →
//! `penpot_desktop::supervisor_config(..)` — and asserts the **spawned
//! command lines and environments** the supervisor would use: java from the
//! bundle jre, valkey from the bundle bin, bundle bin/ prepended to the
//! backend child's PATH (identify/node), pre-seeded offline postgres, and
//! the bundled penpot-watchdog.
//!
//! Env vars are set process-wide, which is safe here: integration-test
//! binaries are per-file, and the tests below run serially via a mutex.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use penpot_desktop::{layout, supervisor_config, AppConfig};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
}

/// Build a fake bundle matching the M4 bundle-layout contract.
fn fake_bundle(root: &Path) -> PathBuf {
    let b = root.join(layout::RUNTIME_BUNDLE_DIR_NAME);
    touch(&b.join("backend/penpot.jar"));
    touch(&b.join("backend/log4j2.xml"));
    touch(&b.join("frontend/index.html"));
    touch(&b.join("jre/bin/java"));
    touch(&b.join("bin/valkey-server"));
    touch(&b.join("bin/identify"));
    touch(&b.join("bin/node"));
    touch(&b.join("bin/penpot-watchdog"));
    touch(&b.join("postgres/15.18.0/bin/initdb"));
    touch(&b.join("VERSION"));
    touch(&b.join("MANIFEST.json"));
    b
}

struct EnvGuard(Vec<&'static str>);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for var in &self.0 {
            std::env::remove_var(var);
        }
    }
}

const LAYOUT_VARS: &[&str] = &[
    "PENPOT_LOCAL_DATA_DIR",
    "PENPOT_LOCAL_RUNTIME_BUNDLE",
    "PENPOT_LOCAL_RUNTIME_DIR",
    "PENPOT_LOCAL_JAVA",
    "PENPOT_LOCAL_VALKEY",
    "PENPOT_LOCAL_POSTGRES_INSTALL_DIR",
    "PENPOT_LOCAL_IDENTIFY",
    "PENPOT_LOCAL_NODE",
    "PENPOT_WATCHDOG_BIN",
];

fn clean_env() -> EnvGuard {
    for var in LAYOUT_VARS {
        std::env::remove_var(var);
    }
    EnvGuard(LAYOUT_VARS.to_vec())
}

fn env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[test]
fn packaged_mode_resolves_every_component_from_the_bundle() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = clean_env();
    let resources = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let bundle = fake_bundle(resources.path());
    std::env::set_var("PENPOT_LOCAL_DATA_DIR", data.path());

    // The GUI passes the Tauri resources dir; the bundle is found there.
    let config = AppConfig::resolve_with_resources(Some(resources.path().to_path_buf()))
        .expect("packaged-mode config resolves");

    assert_eq!(config.runtime_dir, bundle);
    assert_eq!(config.java_path, bundle.join("jre/bin/java"));
    assert_eq!(config.valkey_path, bundle.join("bin/valkey-server"));
    assert_eq!(config.postgres_install_dir.as_deref(), Some(bundle.join("postgres").as_path()));
    assert_eq!(config.watchdog_bin.as_deref(), Some(bundle.join("bin/penpot-watchdog").as_path()));
    assert_eq!(config.child_path_prepend, vec![bundle.join("bin")]);

    // --- the exact command lines the supervisor would spawn ------------
    let sup = supervisor_config(&config, "sekrit", "http://localhost:8686");

    let (java, args, env) = supervisor::backend_command(&sup);
    assert_eq!(java, bundle.join("jre/bin/java"), "JVM must come from the bundled jre");
    assert!(args.iter().any(|a| a == "penpot.jar"));
    assert!(
        args.windows(2).any(|w| w[0] == "-e" && w[1].contains("app.main/start")),
        "nrepl-free entry must survive packaging: {args:?}"
    );
    let path = env_get(&env, "PATH").expect("backend child PATH must be set in packaged mode");
    assert!(
        path.starts_with(bundle.join("bin").to_str().unwrap()),
        "bundle bin/ (identify, node) must lead the backend PATH: {path}"
    );

    let (valkey, _valkey_args) = supervisor::valkey_command(&sup);
    assert_eq!(valkey, bundle.join("bin/valkey-server"));

    let pg = sup.postgres_config().expect("bundle postgres resolves");
    assert_eq!(pg.install_dir, bundle.join("postgres"));
    assert!(!pg.trust_installation_dir, "versioned-root bundle shape");
    assert_eq!(
        pg.releases_url.as_deref(),
        Some(supervisor::OFFLINE_RELEASES_URL),
        "bundled postgres must never attempt a download"
    );
    // PGDATA stays in the app data dir, never inside the read-only bundle.
    assert!(pg.data_dir.starts_with(data.path()));

    assert_eq!(
        sup.orphan_watchdog_bin.as_deref(),
        Some(bundle.join("bin/penpot-watchdog").as_path())
    );
}

#[test]
fn env_overrides_beat_the_bundle_in_packaged_mode() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = clean_env();
    let resources = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let bundle = fake_bundle(resources.path());
    let custom_java = tools.path().join("custom-java");
    let custom_identify = tools.path().join("imagemagick/identify");
    touch(&custom_java);
    touch(&custom_identify);

    std::env::set_var("PENPOT_LOCAL_DATA_DIR", data.path());
    std::env::set_var("PENPOT_LOCAL_JAVA", &custom_java);
    std::env::set_var("PENPOT_LOCAL_IDENTIFY", &custom_identify);

    let config = AppConfig::resolve_with_resources(Some(resources.path().to_path_buf()))
        .expect("config resolves");

    // Env java wins over the bundle jre; everything else stays bundled.
    assert_eq!(config.java_path, custom_java);
    assert_eq!(config.valkey_path, bundle.join("bin/valkey-server"));
    // The identify override's dir leads the PATH, bundle bin/ follows.
    assert_eq!(
        config.child_path_prepend,
        vec![tools.path().join("imagemagick"), bundle.join("bin")]
    );

    let sup = supervisor_config(&config, "sekrit", "http://localhost:8686");
    let (java, _, env) = supervisor::backend_command(&sup);
    assert_eq!(java, custom_java);
    let path = env_get(&env, "PATH").unwrap();
    let expected_prefix = format!(
        "{}:{}",
        tools.path().join("imagemagick").display(),
        bundle.join("bin").display()
    );
    assert!(path.starts_with(&expected_prefix), "PATH order wrong: {path}");
}

#[test]
fn dev_mode_is_unchanged_when_no_bundle_exists() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = clean_env();
    let data = tempfile::tempdir().unwrap();
    // Simulate the dev environment: explicit runtime dir (as the smoke
    // scripts use), no bundle anywhere.
    let runtime = tempfile::tempdir().unwrap();
    touch(&runtime.path().join("backend/penpot.jar"));
    std::env::set_var("PENPOT_LOCAL_DATA_DIR", data.path());
    std::env::set_var("PENPOT_LOCAL_RUNTIME_DIR", runtime.path());

    let config = AppConfig::resolve().expect("dev config resolves");
    assert_eq!(config.runtime_dir, runtime.path());
    assert_eq!(config.java_path, PathBuf::from("/opt/homebrew/opt/openjdk/bin/java"));
    assert_eq!(config.valkey_path, PathBuf::from("/opt/homebrew/bin/valkey-server"));
    assert!(config.postgres_install_dir.is_none(), "dev mode downloads into the data dir");
    assert!(config.watchdog_bin.is_none(), "dev mode uses the sibling-of-exe default");
    assert!(config.child_path_prepend.is_empty(), "dev mode inherits PATH untouched");

    let sup = supervisor_config(&config, "sekrit", "http://localhost:8686");
    let (_, _, env) = supervisor::backend_command(&sup);
    assert!(env_get(&env, "PATH").is_none(), "dev backend inherits the parent PATH");
    let pg = sup.postgres_config().expect("dev postgres config");
    assert!(pg.releases_url.is_none());
    assert!(pg.install_dir.starts_with(data.path()));
}

#[test]
fn explicit_bundle_env_var_works_headless() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = clean_env();
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let bundle = fake_bundle(root.path());
    std::env::set_var("PENPOT_LOCAL_DATA_DIR", data.path());
    std::env::set_var("PENPOT_LOCAL_RUNTIME_BUNDLE", &bundle);

    // Headless entry point: no resources dir at all.
    let config = AppConfig::resolve().expect("headless bundle config resolves");
    assert_eq!(config.runtime_dir, bundle);
    assert_eq!(config.java_path, bundle.join("jre/bin/java"));
    assert_eq!(config.postgres_install_dir.as_deref(), Some(bundle.join("postgres").as_path()));

    // A broken explicit bundle is a hard error, never a silent dev fallback.
    let broken = root.path().join("empty");
    std::fs::create_dir_all(&broken).unwrap();
    std::env::set_var("PENPOT_LOCAL_RUNTIME_BUNDLE", &broken);
    assert!(AppConfig::resolve().is_err());
}
