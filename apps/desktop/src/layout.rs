//! Runtime layout resolution (M4 packaging).
//!
//! Every external component the app needs (Penpot artifacts, java, valkey,
//! postgres binaries, `identify`/`node` for the backend, the
//! `penpot-watchdog` deadman) is resolved with a fixed precedence:
//!
//! 1. **Explicit env overrides** — the existing `PENPOT_LOCAL_*` vars plus
//!    the new ones introduced here. Always win.
//! 2. **A bundled `penpot-runtime/` directory** (the M4 bundle-layout
//!    contract), discovered via `PENPOT_LOCAL_RUNTIME_BUNDLE`, the Tauri v2
//!    resources dir, or executable-adjacent locations (headless-friendly).
//! 3. **Dev defaults** — repo `runtime/` + Homebrew paths, byte-identical to
//!    the pre-M4 behavior. Packaging adds a resolution layer; it never
//!    replaces the dev path.
//!
//! Bundle layout contract (`penpot-runtime/`): `backend/` (penpot.jar, …),
//! `frontend/` (static SPA), `jre/bin/java` (jlink output), `bin/`
//! (valkey-server, identify, penpot-watchdog, optionally node), `postgres/`
//! (a ready postgresql_embedded-compatible installation — either
//! `postgres/bin/initdb` or `postgres/<version>/bin/initdb`), `VERSION`,
//! `MANIFEST.json`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Bundle directory name (inside the Tauri resources dir / next to the exe).
pub const RUNTIME_BUNDLE_DIR_NAME: &str = "penpot-runtime";

/// Env var pointing straight at a `penpot-runtime/` directory (explicit
/// bundle override + the headless fallback for packaged installs).
pub const ENV_RUNTIME_BUNDLE: &str = "PENPOT_LOCAL_RUNTIME_BUNDLE";

/// Marker file that must exist for a directory to be accepted as a bundle.
const BUNDLE_MARKER: &str = "backend/penpot.jar";

/// Where a resolved component came from (logged at boot, one line each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Bundle,
    Dev,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Env => write!(f, "env"),
            Source::Bundle => write!(f, "bundle"),
            Source::Dev => write!(f, "dev"),
        }
    }
}

/// A resolved path plus its provenance.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

impl Resolved {
    fn new(path: impl Into<PathBuf>, source: Source) -> Self {
        Resolved { path: path.into(), source }
    }
}

/// Env overrides captured as data so the precedence logic is a pure,
/// unit-testable function of (overrides, bundle) — no process-global reads.
#[derive(Debug, Clone, Default)]
pub struct EnvOverrides {
    /// `PENPOT_LOCAL_RUNTIME_DIR` — dir containing `backend/` + `frontend/`.
    pub runtime_dir: Option<PathBuf>,
    /// `PENPOT_LOCAL_JAVA`.
    pub java: Option<PathBuf>,
    /// `PENPOT_LOCAL_VALKEY`.
    pub valkey: Option<PathBuf>,
    /// `PENPOT_LOCAL_POSTGRES_INSTALL_DIR` — pre-seeded postgres install
    /// (either `bin/initdb` inside, or `<version>/bin/initdb`); implies
    /// offline (no download is ever attempted, bad contents fail the boot).
    pub postgres_install: Option<PathBuf>,
    /// `PENPOT_WATCHDOG_BIN` (the supervisor also reads it itself; captured
    /// here so the boot log reports the right source).
    pub watchdog_bin: Option<PathBuf>,
    /// `PENPOT_LOCAL_IDENTIFY` — path to ImageMagick's `identify`; its
    /// parent dir is prepended to the backend child's PATH.
    pub identify: Option<PathBuf>,
    /// `PENPOT_LOCAL_NODE` — path to `node` (SVG media processing); its
    /// parent dir is prepended to the backend child's PATH.
    pub node: Option<PathBuf>,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        EnvOverrides {
            runtime_dir: path("PENPOT_LOCAL_RUNTIME_DIR"),
            java: path("PENPOT_LOCAL_JAVA"),
            valkey: path("PENPOT_LOCAL_VALKEY"),
            postgres_install: path("PENPOT_LOCAL_POSTGRES_INSTALL_DIR"),
            watchdog_bin: path(supervisor::watchdog::WATCHDOG_BIN_ENV),
            identify: path("PENPOT_LOCAL_IDENTIFY"),
            node: path("PENPOT_LOCAL_NODE"),
        }
    }
}

/// The fully resolved runtime layout.
#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    /// Directory containing `backend/` and `frontend/` (the bundle satisfies
    /// this directly; dev default is the repo `runtime/`).
    pub runtime_dir: Resolved,
    /// `java` binary.
    pub java: Resolved,
    /// `valkey-server` binary.
    pub valkey: Resolved,
    /// Pre-seeded postgres installation; `None` = dev behavior (download
    /// once into the data dir).
    pub postgres_install: Option<Resolved>,
    /// Explicit `penpot-watchdog` binary; `None` = supervisor default
    /// (sibling of the executable).
    pub watchdog_bin: Option<Resolved>,
    /// Dirs prepended to the backend JVM child's PATH (env-override tool
    /// dirs first, then the bundle `bin/`). Empty in pure dev mode.
    pub child_path_prepend: Vec<PathBuf>,
    /// The bundle directory used, if any.
    pub bundle: Option<PathBuf>,
}

impl RuntimeLayout {
    /// One human-readable line per component for the boot log.
    pub fn describe(&self) -> Vec<String> {
        let line = |component: &str, r: &Resolved| {
            format!("component={component} source={} path={}", r.source, r.path.display())
        };
        let mut out = vec![
            line("runtime(backend+frontend)", &self.runtime_dir),
            line("java", &self.java),
            line("valkey", &self.valkey),
        ];
        match &self.postgres_install {
            Some(r) => out.push(line("postgres", r)),
            None => out.push(
                "component=postgres source=dev path=<data_dir>/postgres/install (downloaded once)"
                    .to_string(),
            ),
        }
        match &self.watchdog_bin {
            Some(r) => out.push(line("penpot-watchdog", r)),
            None => out.push(
                "component=penpot-watchdog source=dev path=<sibling of executable>".to_string(),
            ),
        }
        if self.child_path_prepend.is_empty() {
            out.push("component=backend-path source=dev path=<inherited PATH>".to_string());
        } else {
            let dirs: Vec<String> = self
                .child_path_prepend
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            out.push(format!(
                "component=backend-path source=bundle/env prepend={}",
                dirs.join(":")
            ));
        }
        out
    }
}

/// Is `dir` a valid `penpot-runtime/` bundle?
pub fn is_bundle(dir: &Path) -> bool {
    dir.join(BUNDLE_MARKER).is_file()
}

/// Discover the bundle directory. Precedence:
/// 1. `env_bundle` (`PENPOT_LOCAL_RUNTIME_BUNDLE`) — must be valid, a broken
///    explicit override is a hard error (never silently ignored);
/// 2. `<resources>/penpot-runtime` (Tauri v2 path resolver, GUI app);
/// 3. executable-adjacent candidates (headless-friendly):
///    `<exe_dir>/penpot-runtime`, `<exe_dir>/../Resources/penpot-runtime`
///    (macOS .app), `<exe_dir>/../lib/penpot-desktop/penpot-runtime`
///    (Linux packaging layouts).
///
/// Non-env candidates that don't pass [`is_bundle`] are skipped silently
/// (that's the dev case).
pub fn discover_bundle(
    env_bundle: Option<&Path>,
    resource_dir: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = env_bundle {
        anyhow::ensure!(
            is_bundle(dir),
            "{ENV_RUNTIME_BUNDLE}={} is not a penpot-runtime bundle (missing {})",
            dir.display(),
            BUNDLE_MARKER
        );
        return Ok(Some(dir.to_path_buf()));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(resources) = resource_dir {
        candidates.push(resources.join(RUNTIME_BUNDLE_DIR_NAME));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join(RUNTIME_BUNDLE_DIR_NAME));
            candidates.push(exe_dir.join("../Resources").join(RUNTIME_BUNDLE_DIR_NAME));
            candidates
                .push(exe_dir.join("../lib/penpot-desktop").join(RUNTIME_BUNDLE_DIR_NAME));
        }
    }
    Ok(candidates.into_iter().find(|c| is_bundle(c)))
}

/// Resolve the layout from captured env overrides + an optional bundle dir.
/// Pure precedence: env > bundle (component present) > dev default. Bundle
/// components are only picked when they actually exist on disk, so a partial
/// bundle degrades per-component to the dev default rather than breaking.
pub fn resolve_layout(env: &EnvOverrides, bundle: Option<&Path>) -> RuntimeLayout {
    let bundle_file = |rel: &str| -> Option<PathBuf> {
        let p = bundle?.join(rel);
        p.is_file().then_some(p)
    };
    let bundle_dir = |rel: &str| -> Option<PathBuf> {
        let p = bundle?.join(rel);
        p.is_dir().then_some(p)
    };

    let runtime_dir = match &env.runtime_dir {
        Some(dir) => Resolved::new(dir, Source::Env),
        None => match bundle {
            // The bundle contract has backend/ + frontend/ at its root, the
            // exact shape the proxy/supervisor expect of a runtime dir.
            Some(b) => Resolved::new(b, Source::Bundle),
            None => Resolved::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
                Source::Dev,
            ),
        },
    };

    let java = match &env.java {
        Some(p) => Resolved::new(p, Source::Env),
        None => match bundle_file("jre/bin/java") {
            Some(p) => Resolved::new(p, Source::Bundle),
            None => Resolved::new("/opt/homebrew/opt/openjdk/bin/java", Source::Dev),
        },
    };

    let valkey = match &env.valkey {
        Some(p) => Resolved::new(p, Source::Env),
        None => match bundle_file("bin/valkey-server") {
            Some(p) => Resolved::new(p, Source::Bundle),
            None => Resolved::new("/opt/homebrew/bin/valkey-server", Source::Dev),
        },
    };

    let postgres_install = match &env.postgres_install {
        Some(p) => Some(Resolved::new(p, Source::Env)),
        None => bundle_dir("postgres").map(|p| Resolved::new(p, Source::Bundle)),
    };

    let watchdog_bin = match &env.watchdog_bin {
        Some(p) => Some(Resolved::new(p, Source::Env)),
        None => bundle_file(&format!("bin/{}", supervisor::watchdog::WATCHDOG_BIN_NAME))
            .map(|p| Resolved::new(p, Source::Bundle)),
    };

    // Backend child PATH: env-override tool dirs first (they must shadow the
    // bundle), then the bundle bin/ (identify, node, …). Deduplicated,
    // order-preserving.
    let mut child_path_prepend: Vec<PathBuf> = Vec::new();
    for tool in [&env.identify, &env.node].into_iter().flatten() {
        if let Some(parent) = tool.parent() {
            if parent.as_os_str().is_empty() {
                continue; // bare binary name: nothing to prepend
            }
            if !child_path_prepend.iter().any(|p| p == parent) {
                child_path_prepend.push(parent.to_path_buf());
            }
        }
    }
    if let Some(bin) = bundle_dir("bin") {
        if !child_path_prepend.contains(&bin) {
            child_path_prepend.push(bin);
        }
    }

    RuntimeLayout {
        runtime_dir,
        java,
        valkey,
        postgres_install,
        watchdog_bin,
        child_path_prepend,
        bundle: bundle.map(Path::to_path_buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
    }

    /// A complete fake bundle with stub executables (simulation of the
    /// packaged layout — no real binaries involved).
    fn fake_bundle(root: &Path) -> PathBuf {
        let b = root.join(RUNTIME_BUNDLE_DIR_NAME);
        touch(&b.join("backend/penpot.jar"));
        touch(&b.join("frontend/index.html"));
        touch(&b.join("jre/bin/java"));
        touch(&b.join("bin/valkey-server"));
        touch(&b.join("bin/identify"));
        touch(&b.join("bin/node"));
        touch(&b.join("bin/penpot-watchdog"));
        touch(&b.join("postgres/15.18.0/bin/initdb"));
        touch(&b.join("VERSION"));
        b
    }

    #[test]
    fn dev_defaults_without_bundle_or_env() {
        let layout = resolve_layout(&EnvOverrides::default(), None);
        assert_eq!(layout.java.source, Source::Dev);
        assert_eq!(layout.java.path, PathBuf::from("/opt/homebrew/opt/openjdk/bin/java"));
        assert_eq!(layout.valkey.source, Source::Dev);
        assert_eq!(layout.valkey.path, PathBuf::from("/opt/homebrew/bin/valkey-server"));
        assert_eq!(layout.runtime_dir.source, Source::Dev);
        assert!(layout.runtime_dir.path.ends_with("runtime"));
        assert!(layout.postgres_install.is_none());
        assert!(layout.watchdog_bin.is_none());
        assert!(layout.child_path_prepend.is_empty());
    }

    #[test]
    fn bundle_provides_every_component() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(tmp.path());
        let layout = resolve_layout(&EnvOverrides::default(), Some(&bundle));

        assert_eq!(layout.runtime_dir.source, Source::Bundle);
        assert_eq!(layout.runtime_dir.path, bundle);
        assert_eq!(layout.java.source, Source::Bundle);
        assert_eq!(layout.java.path, bundle.join("jre/bin/java"));
        assert_eq!(layout.valkey.source, Source::Bundle);
        assert_eq!(layout.valkey.path, bundle.join("bin/valkey-server"));
        let pg = layout.postgres_install.expect("bundle postgres");
        assert_eq!(pg.source, Source::Bundle);
        assert_eq!(pg.path, bundle.join("postgres"));
        let wd = layout.watchdog_bin.expect("bundle watchdog");
        assert_eq!(wd.source, Source::Bundle);
        assert_eq!(wd.path, bundle.join("bin/penpot-watchdog"));
        assert_eq!(layout.child_path_prepend, vec![bundle.join("bin")]);
    }

    #[test]
    fn env_beats_bundle_beats_dev() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(tmp.path());
        let env = EnvOverrides {
            runtime_dir: Some("/custom/runtime".into()),
            java: Some("/custom/java".into()),
            valkey: Some("/custom/valkey".into()),
            postgres_install: Some("/custom/pg".into()),
            watchdog_bin: Some("/custom/penpot-watchdog".into()),
            identify: Some("/custom/tools/identify".into()),
            node: Some("/other/tools/node".into()),
        };
        let layout = resolve_layout(&env, Some(&bundle));
        for (resolved, expected) in [
            (&layout.runtime_dir, "/custom/runtime"),
            (&layout.java, "/custom/java"),
            (&layout.valkey, "/custom/valkey"),
        ] {
            assert_eq!(resolved.source, Source::Env);
            assert_eq!(resolved.path, PathBuf::from(expected));
        }
        assert_eq!(layout.postgres_install.as_ref().unwrap().source, Source::Env);
        assert_eq!(layout.postgres_install.as_ref().unwrap().path, PathBuf::from("/custom/pg"));
        assert_eq!(layout.watchdog_bin.as_ref().unwrap().source, Source::Env);
        // Env tool dirs come BEFORE the bundle bin dir.
        assert_eq!(
            layout.child_path_prepend,
            vec![
                PathBuf::from("/custom/tools"),
                PathBuf::from("/other/tools"),
                bundle.join("bin"),
            ]
        );
    }

    #[test]
    fn partial_bundle_degrades_per_component_to_dev() {
        let tmp = tempfile::tempdir().unwrap();
        // Bundle with backend/frontend + valkey, but no jre, postgres,
        // watchdog, or bin tools beyond valkey.
        let b = tmp.path().join(RUNTIME_BUNDLE_DIR_NAME);
        touch(&b.join("backend/penpot.jar"));
        touch(&b.join("frontend/index.html"));
        touch(&b.join("bin/valkey-server"));
        let layout = resolve_layout(&EnvOverrides::default(), Some(&b));
        assert_eq!(layout.java.source, Source::Dev, "no jre in bundle → dev java");
        assert_eq!(layout.valkey.source, Source::Bundle);
        assert!(layout.postgres_install.is_none(), "no postgres/ → dev download path");
        assert!(layout.watchdog_bin.is_none(), "no watchdog → sibling-of-exe default");
        assert_eq!(layout.child_path_prepend, vec![b.join("bin")]);
    }

    #[test]
    fn identify_env_with_bare_name_prepends_nothing() {
        let env = EnvOverrides { identify: Some("identify".into()), ..Default::default() };
        let layout = resolve_layout(&env, None);
        assert!(layout.child_path_prepend.is_empty());
    }

    #[test]
    fn duplicate_tool_dirs_are_deduplicated() {
        let env = EnvOverrides {
            identify: Some("/tools/identify".into()),
            node: Some("/tools/node".into()),
            ..Default::default()
        };
        let layout = resolve_layout(&env, None);
        assert_eq!(layout.child_path_prepend, vec![PathBuf::from("/tools")]);
    }

    #[test]
    fn discover_bundle_env_override_must_be_valid() {
        let tmp = tempfile::tempdir().unwrap();
        // Valid explicit bundle.
        let bundle = fake_bundle(tmp.path());
        let found = discover_bundle(Some(&bundle), None).expect("valid env bundle");
        assert_eq!(found, Some(bundle));
        // Broken explicit bundle → hard error, not silent dev fallback.
        let broken = tmp.path().join("not-a-bundle");
        std::fs::create_dir_all(&broken).unwrap();
        assert!(discover_bundle(Some(&broken), None).is_err());
    }

    #[test]
    fn discover_bundle_uses_resources_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(tmp.path());
        let found = discover_bundle(None, Some(tmp.path())).expect("no error");
        assert_eq!(found, Some(bundle));
        // Resources dir without a bundle → None (dev mode), no error.
        let empty = tempfile::tempdir().unwrap();
        let found = discover_bundle(None, Some(empty.path())).expect("no error");
        assert_eq!(found, None);
    }

    #[test]
    fn describe_emits_one_line_per_component() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = fake_bundle(tmp.path());
        let lines = resolve_layout(&EnvOverrides::default(), Some(&bundle)).describe();
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|l| l.starts_with("component=")));
        assert!(lines.iter().any(|l| l.contains("source=bundle")));
    }
}
