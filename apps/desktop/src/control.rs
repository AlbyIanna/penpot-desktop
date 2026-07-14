//! N5 — the vault switch mechanism and its headless control surface.
//!
//! [`VaultRunner`] owns the live [`RunningApp`] and turns "open a different
//! vault" into the exact M2 operation, pointed at a new tree:
//!
//! 1. write the switch marker (crash safety) BEFORE anything is wiped;
//! 2. stop the current stack cleanly ([`RunningApp::shutdown`] — sync daemon,
//!    index, proxy, supervised postgres/valkey/JVM all go down);
//! 3. wipe the disposable Penpot DB cluster + the vault index
//!    ([`crate::vault::reset_disposable_state`]) — zero residue of the old
//!    vault before the new one reconciles (invariant 2, P0);
//! 4. point the registry's active vault at the new root;
//! 5. [`boot`] the target: re-provision the single user, reconcile from the
//!    new tree (each file re-imported under its ORIGINAL id via the manifest),
//!    rebuild the per-vault index, re-render thumbnails;
//! 6. clear the switch marker.
//!
//! A SIGKILL anywhere in 1–6 leaves the marker on disk; the next boot's
//! [`boot_active_vault`] sees it and completes the switch forward to the
//! target (never a half-switched hybrid). Because the wipe (step 3) always
//! precedes the reconcile (step 5), forward-completion is safe from any
//! interruption point: the previous vault's DB state is gone first.
//!
//! [`serve_control`] exposes this over a localhost-only HTTP port
//! (`PENPOT_LOCAL_CONTROL_PORT`) so the N5 gate can drive switches WITHOUT the
//! GUI dialog. It is a test/automation affordance: the GUI's File > Open Vault
//! calls [`VaultRunner::switch_to`] directly, no HTTP involved.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::vault::{self, SwitchMarker, VaultRef, VaultRegistry};
use crate::{boot, AppConfig, RunningApp};

/// Env var: the localhost control port. Unset ⇒ no control server (every
/// pre-N5 flow is byte-identical; nothing new binds).
pub const CONTROL_PORT_ENV: &str = "PENPOT_LOCAL_CONTROL_PORT";
/// Env var (test-only): sleep this many ms mid-switch, right after the DB is
/// wiped and before the target boots, to widen the SIGKILL window
/// deterministically for the crash-recovery gate.
pub const SWITCH_TEST_DELAY_ENV: &str = "PENPOT_LOCAL_SWITCH_TEST_DELAY_MS";

fn test_switch_delay() -> Option<Duration> {
    std::env::var(SWITCH_TEST_DELAY_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
}

/// Owns the live stack and performs vault switches. Cheap to `clone` the
/// `Arc` and share between the control server, the signal loop and the GUI.
pub struct VaultRunner {
    /// The live stack. `None` only briefly, mid-switch, while the old stack is
    /// down and the new one is booting.
    app: AsyncMutex<Option<RunningApp>>,
    /// Base configuration; a switch clones it and overrides `designs_dir`.
    base_config: AppConfig,
    /// App data dir (registry, markers, disposable state all live here).
    data_dir: PathBuf,
    /// The currently-open vault.
    active: Mutex<VaultRef>,
    /// Serializes switches (only one teardown/reboot at a time).
    switch_lock: AsyncMutex<()>,
}

impl VaultRunner {
    fn new(app: RunningApp, base_config: AppConfig, active: VaultRef) -> Arc<Self> {
        let data_dir = base_config.data_dir.clone();
        Arc::new(VaultRunner {
            app: AsyncMutex::new(Some(app)),
            base_config,
            data_dir,
            active: Mutex::new(active),
            switch_lock: AsyncMutex::new(()),
        })
    }

    /// The proxy origin (stable across switches — same proxy port).
    pub fn proxy_url(&self) -> String {
        self.base_config.public_uri()
    }

    /// The currently-open vault.
    pub fn active(&self) -> VaultRef {
        self.active.lock().expect("active mutex").clone()
    }

    /// The known-vaults list, straight from the registry (source of truth).
    pub fn list(&self) -> Vec<VaultRef> {
        VaultRegistry::load(&self.data_dir)
            .map(|r| r.vaults)
            .unwrap_or_default()
    }

    /// The current stack's sync daemon status stream, if running (tray bind).
    pub async fn sync_status(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<sync_daemon::SyncStatusSnapshot>> {
        self.app.lock().await.as_ref().and_then(|a| a.sync_status())
    }

    /// The current stack's sync control handle, if running (tray + SIGUSR1).
    pub async fn sync_control(&self) -> Option<sync_daemon::SyncControl> {
        self.app.lock().await.as_ref().and_then(|a| a.sync_control())
    }

    /// The current stack's board-export status stream, if running (tray bind).
    pub async fn export_status(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<board_export::ExportStatusSnapshot>> {
        self.app.lock().await.as_ref().and_then(|a| a.export_status())
    }

    /// Stop the current stack (process-exit path). Idempotent.
    pub async fn shutdown(&self) {
        if let Some(app) = self.app.lock().await.take() {
            app.shutdown().await;
        }
    }

    /// Open a different vault: the full reset+reconcile switch (see module
    /// docs). Returns the target's [`VaultRef`]. On a boot failure the switch
    /// marker is intentionally left in place so the next boot recovers.
    pub async fn switch_to(&self, target: &Path) -> Result<VaultRef> {
        // Serialize: only one switch at a time.
        let _guard = self.switch_lock.lock().await;

        let target_ref = vault::ensure_vault(target)
            .context("cannot prepare the target vault (identity marker)")?;
        let target_root = target_ref.root();
        let previous = self.active();
        tracing::info!(
            from = %previous.path, to = %target_ref.path,
            "vault switch: begin"
        );

        // (1) Marker BEFORE any wipe — crash safety.
        let marker = SwitchMarker {
            target: target_ref.path.clone(),
            target_id: target_ref.id.clone(),
            previous: Some(previous.path.clone()),
            previous_id: Some(previous.id.clone()),
            started_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        };
        vault::write_switch_marker(&self.data_dir, &marker)
            .context("cannot write the switch marker")?;

        // (2) Stop the current stack.
        if let Some(app) = self.app.lock().await.take() {
            app.shutdown().await;
        }
        tracing::info!("vault switch: previous stack stopped");

        // (3) Wipe the disposable DB + index — zero residue of the old vault.
        tracing::info!("vault switch: wiping DB (Penpot cache + index reset)");
        vault::reset_disposable_state(&self.data_dir)
            .context("cannot wipe the disposable Penpot DB / index")?;

        // Test-only widened SIGKILL window (marker present, DB gone, no stack).
        if let Some(delay) = test_switch_delay() {
            tracing::warn!(ms = delay.as_millis(), "vault switch: TEST delay (widened crash window)");
            tokio::time::sleep(delay).await;
        }

        // (4) Registry: active = target (record both vaults).
        {
            let mut reg = VaultRegistry::load(&self.data_dir).unwrap_or_default();
            reg.upsert(&previous);
            reg.upsert(&target_ref);
            reg.set_active(&target_ref.path);
            reg.save(&self.data_dir).context("cannot persist the vault registry")?;
        }

        // (5) Boot the target: reconcile from the new tree.
        tracing::info!(vault = %target_root.display(), "vault switch: booting target");
        let mut cfg = self.base_config.clone();
        cfg.designs_dir = target_root.clone();
        let app = boot(cfg).await.context("booting the target vault failed")?;
        *self.app.lock().await = Some(app);
        *self.active.lock().expect("active mutex") = target_ref.clone();

        // (6) Clear the marker — the switch landed.
        vault::clear_switch_marker(&self.data_dir).context("cannot clear the switch marker")?;
        tracing::info!(vault = %target_root.display(), "vault switch: complete");
        Ok(target_ref)
    }
}

/// Resolve the active vault (honoring an interrupted-switch marker), boot it,
/// and return a [`VaultRunner`] wrapping the live stack. Shared by the
/// headless runner and the GUI shell so both get identical N5 semantics.
///
/// `base_config.designs_dir` is the env/default resolution from
/// [`AppConfig::resolve`]; this function may override it (registry active or a
/// recovery target) before booting.
pub async fn boot_active_vault(base_config: AppConfig) -> Result<Arc<VaultRunner>> {
    let data_dir = base_config.data_dir.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;

    let env_was_set = std::env::var_os("PENPOT_LOCAL_DESIGNS_DIR").is_some();
    let registry = VaultRegistry::load(&data_dir)?;
    let marker = vault::read_switch_marker(&data_dir)?;
    let mode = vault::decide_startup(
        marker.as_ref(),
        &base_config.designs_dir,
        env_was_set,
        registry.active.as_deref(),
    );

    let (vault_root, recovering) = match mode {
        vault::StartupMode::Normal { vault } => (vault, false),
        vault::StartupMode::RecoverForward { target, target_id } => {
            tracing::warn!(
                target = %target.display(), target_id = %target_id,
                "vault switch recovery: an interrupted switch was found; completing it forward"
            );
            // Wipe first — guarantees zero residue regardless of how far the
            // interrupted switch got (it may have wiped already; idempotent).
            vault::reset_disposable_state(&data_dir)
                .context("recovery: cannot wipe the disposable Penpot DB / index")?;
            (target, true)
        }
    };

    // Ensure the vault has an identity marker (mints one for a brand-new root).
    let vref = vault::ensure_vault(&vault_root)?;

    // Record the active vault in the registry.
    let mut registry = registry;
    registry.upsert(&vref);
    registry.set_active(&vref.path);
    registry.save(&data_dir).context("cannot persist the vault registry")?;

    // Boot the resolved vault.
    let mut config = base_config.clone();
    config.designs_dir = vref.root();
    tracing::info!(vault = %vref.path, id = %vref.id, "opening vault");
    let app = boot(config).await?;

    // Recovery landed → clear the marker.
    if recovering {
        vault::clear_switch_marker(&data_dir).context("recovery: cannot clear the switch marker")?;
        tracing::info!(vault = %vref.path, "vault switch recovery: complete (single consistent vault)");
    }

    Ok(VaultRunner::new(app, base_config, vref))
}

// ---------------------------------------------------------------------------
// Localhost control HTTP server (test/automation only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenReq {
    path: String,
}

fn vault_json(v: &VaultRef) -> serde_json::Value {
    json!({ "id": v.id, "path": v.path })
}

async fn control_health() -> &'static str {
    "ok"
}

async fn control_active(State(runner): State<Arc<VaultRunner>>) -> Response {
    Json(vault_json(&runner.active())).into_response()
}

async fn control_list(State(runner): State<Arc<VaultRunner>>) -> Response {
    let active = runner.active();
    let vaults: Vec<serde_json::Value> = runner.list().iter().map(vault_json).collect();
    Json(json!({ "active": vault_json(&active), "vaults": vaults })).into_response()
}

async fn control_open(
    State(runner): State<Arc<VaultRunner>>,
    Json(req): Json<OpenReq>,
) -> Response {
    let path = PathBuf::from(req.path.trim());
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "empty path"}))).into_response();
    }
    match runner.switch_to(&path).await {
        Ok(vref) => Json(json!({ "ok": true, "active": vault_json(&vref) })).into_response(),
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "vault switch failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

/// The control routes (own server — localhost only).
pub fn control_router(runner: Arc<VaultRunner>) -> Router {
    Router::new()
        .route("/health", get(control_health))
        .route("/active", get(control_active))
        .route("/list", get(control_list))
        .route("/open", post(control_open))
        .with_state(runner)
}

/// Serve the control routes on `127.0.0.1:<port>` until the process exits.
/// Localhost-only by construction. Spawn this only when
/// [`CONTROL_PORT_ENV`] is set (the N5 gate sets it).
pub async fn serve_control(runner: Arc<VaultRunner>, port: u16) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("control server cannot bind {addr}"))?;
    tracing::info!(%addr, "vault control server listening (test/automation)");
    axum::serve(listener, control_router(runner))
        .await
        .context("control server error")?;
    Ok(())
}

/// Read + parse [`CONTROL_PORT_ENV`]; `None` (or an invalid value) disables the
/// control server.
pub fn control_port_from_env() -> Option<u16> {
    std::env::var(CONTROL_PORT_ENV).ok().and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_port_parsing() {
        std::env::remove_var(CONTROL_PORT_ENV);
        assert_eq!(control_port_from_env(), None);
        std::env::set_var(CONTROL_PORT_ENV, "8949");
        assert_eq!(control_port_from_env(), Some(8949));
        std::env::set_var(CONTROL_PORT_ENV, "not-a-port");
        assert_eq!(control_port_from_env(), None);
        std::env::remove_var(CONTROL_PORT_ENV);
    }

    #[test]
    fn test_delay_parsing() {
        std::env::remove_var(SWITCH_TEST_DELAY_ENV);
        assert_eq!(test_switch_delay(), None);
        std::env::set_var(SWITCH_TEST_DELAY_ENV, "0");
        assert_eq!(test_switch_delay(), None, "0 disables the delay");
        std::env::set_var(SWITCH_TEST_DELAY_ENV, "5000");
        assert_eq!(test_switch_delay(), Some(Duration::from_millis(5000)));
        std::env::remove_var(SWITCH_TEST_DELAY_ENV);
    }

    fn vref(id: &str, path: &str) -> VaultRef {
        VaultRef { id: id.into(), path: path.into() }
    }

    #[test]
    fn vault_json_shape() {
        let v = vref("id-a", "/vaults/a");
        let j = vault_json(&v);
        assert_eq!(j["id"], "id-a");
        assert_eq!(j["path"], "/vaults/a");
    }
}
