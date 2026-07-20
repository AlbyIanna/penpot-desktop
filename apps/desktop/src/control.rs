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
//!
//! D4 adds [`VaultRunner::reboot_in_place`] — the same stop→boot dance
//! ([`VaultRunner::stop_then_boot`], factored out so the two operations never
//! drift into two copies of the boot sequence) run against the SAME vault, to
//! apply preferences that are baked in at boot (plugins/CSP live in
//! `config.js`, read once at script load; the supervisor cannot hot-add the
//! exporter child). The one fact that must differ between the two callers —
//! whether the disposable DB/index gets wiped — is pulled out into the pure
//! [`wipes_disposable_state`], and whether the crash marker gets written into
//! [`writes_switch_marker`], so both decisions are unit-testable without
//! booting a stack. See their doc comments for the reasoning.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{FromRef, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::vault::{self, SwitchMarker, VaultRef, VaultRegistry};
use crate::windows::{OpenWindow, WindowRegistry};
use crate::{boot, AppConfig, RunningApp};

/// D4 — a late-bound handle to the owning [`VaultRunner`], threaded through
/// [`boot`] so routes built there (the Preferences page, `prefs_http.rs`) can
/// call back into the very runner that wraps the stack being built —
/// `POST /__api/prefs/reboot` needs [`VaultRunner::reboot_in_place`] and
/// `POST /__api/prefs/vault` needs [`VaultRunner::switch_to`], but neither
/// exists yet at the point `boot()` constructs its router (a `VaultRunner`
/// only comes into being by WRAPPING the [`RunningApp`] `boot()` returns).
///
/// [`boot_active_vault`] creates this slot empty, passes it into the first
/// `boot()` call (so the very first proxy's router captures the same `Arc`),
/// then fills it in immediately after `VaultRunner::new` returns. Every later
/// reboot/switch (`stop_then_boot`) reuses that already-filled slot — the
/// `VaultRunner` itself never changes identity across a switch or reboot,
/// only the [`RunningApp`] it wraps does, so nothing needs re-filling.
/// `None` only in the brief window between the first `boot()` returning and
/// that fill-in; routes see this the same way the palette/menu bar's
/// late-bound slots do — a "still starting" response, never a panic.
pub type RunnerSlot = Arc<AsyncMutex<Option<Arc<VaultRunner>>>>;

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

/// Which stop→boot operation is running. The only two the supervisor knows:
/// changing vaults, or rebooting the same one in place to apply a boot-time
/// preference. Kept as data (not just two call sites) so the property that
/// matters most — a reboot must never pay a switch's cost — is a pure
/// function a test can assert directly, without booting a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackOp {
    /// Changing which vault is open.
    Switch,
    /// Same vault, re-reading preferences that only take effect at boot.
    RebootInPlace,
}

/// Whether `op` must wipe the disposable Penpot DB cluster + index
/// ([`vault::reset_disposable_state`]) before booting.
///
/// A switch changes vaults, so invariant 2 (P0, zero cross-vault spill)
/// requires zero residue of the old vault before the new one reconciles —
/// wiping is the mechanism. A reboot in place keeps the SAME vault: nothing
/// crossed a vault boundary, so there is nothing to scrub. Wiping anyway
/// would force a full re-import of every file just to apply a settings
/// change — correct (the invariant would still hold), but the wrong cost for
/// flipping a checkbox.
pub fn wipes_disposable_state(op: StackOp) -> bool {
    matches!(op, StackOp::Switch)
}

/// Whether `op` needs the crash-safety marker (`vault-switch.json`,
/// [`vault::SwitchMarker`]) written before it starts.
///
/// The marker exists because a switch moves the registry's `active` pointer
/// from one vault to another WHILE the DB is briefly wiped in between: a
/// SIGKILL in that window could leave the registry pointing at a target
/// whose DB was never reconciled, or — worse — leave it still pointing at
/// the previous vault after that vault's DB is already gone. The marker
/// records the intended target so the next boot completes the switch
/// forward instead of guessing.
///
/// A reboot in place never repoints the registry and never wipes anything —
/// it is the same vault before and after. The worst an interrupted reboot
/// leaves behind is a stopped stack for that one, unchanged vault with its
/// DB fully intact — exactly what a completely ordinary boot (no marker, no
/// wipe, `StartupMode::Normal`) already resolves correctly. There is no
/// half-changed pointer to recover, so a marker would guard against a
/// failure mode that structurally cannot occur here.
pub fn writes_switch_marker(op: StackOp) -> bool {
    matches!(op, StackOp::Switch)
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
    /// D4 — the same late-bound slot this runner was constructed with (see
    /// [`RunnerSlot`]'s doc). Every reboot/switch re-passes it into `boot()`
    /// unchanged, so the Preferences routes built by a later boot keep
    /// resolving to this exact runner.
    runner_slot: RunnerSlot,
}

impl VaultRunner {
    fn new(
        app: RunningApp,
        base_config: AppConfig,
        active: VaultRef,
        runner_slot: RunnerSlot,
    ) -> Arc<Self> {
        let data_dir = base_config.data_dir.clone();
        Arc::new(VaultRunner {
            app: AsyncMutex::new(Some(app)),
            base_config,
            data_dir,
            active: Mutex::new(active),
            switch_lock: AsyncMutex::new(()),
            runner_slot,
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

    /// D4 — live "stop renders" control, delegating to
    /// [`RunningApp::set_renders_enabled`]. `false` both when the running
    /// stack refused the request (turning renders back on with no exporter
    /// child spawned — see that method's docs) AND in the brief mid-switch
    /// window where there is no stack at all to ask; either way `false` means
    /// the caller must reboot to get renders running.
    pub async fn set_renders_enabled(&self, on: bool) -> bool {
        match self.app.lock().await.as_ref() {
            Some(app) => app.set_renders_enabled(on).await,
            None => false,
        }
    }

    /// The active vault's default team id, if the stack is up. D3's menu bar
    /// needs this to build workspace deep links (`vault_index::
    /// workspace_deep_link`) when dispatching File > Open… / Open Recent —
    /// same "peek into the live `RunningApp`" shape as `sync_status` /
    /// `sync_control` / `export_status` above. `None` only in the same brief
    /// window those return `None` in (mid-switch, or provisioning never
    /// produced a default team).
    pub async fn team_id(&self) -> Option<String> {
        self.app.lock().await.as_ref().and_then(|a| a.profile.default_team_id.clone())
    }

    /// The active vault's RPC access token, if the stack is up. D3's menu
    /// bar needs this for the commands that genuinely must go through the
    /// backend (New File, New Project, Import…) rather than touching the
    /// vault directly — the core invariant for THIS task is narrower than
    /// the app-wide one: nothing in the menu bar may write to the vault
    /// itself, only the sync daemon does that, on its own schedule, after
    /// the RPC call lands in the DB. Same "peek into the live `RunningApp`"
    /// shape as `team_id`/`sync_status` above.
    pub async fn access_token(&self) -> Option<String> {
        self.app.lock().await.as_ref().and_then(|a| a.credentials.access_token.clone())
    }

    /// Stop the current stack (process-exit path). Idempotent.
    pub async fn shutdown(&self) {
        if let Some(app) = self.app.lock().await.take() {
            app.shutdown().await;
        }
    }

    /// The shared stop→(optional wipe)→(hook)→boot dance behind both
    /// [`switch_to`](Self::switch_to) and
    /// [`reboot_in_place`](Self::reboot_in_place) — the ONE place that
    /// sequence is written, so the two operations can never drift into two
    /// hand-copied (and silently divergent) boot dances.
    ///
    /// Stops the current stack, wipes the disposable DB/index iff `op` calls
    /// for it ([`wipes_disposable_state`] — the single, pure, testable
    /// decision), then runs `between_wipe_and_boot` (a switch uses it to
    /// repoint the registry at the new vault; a reboot passes a no-op — same
    /// vault, nothing to repoint), then boots `target_root` and installs the
    /// result as the live stack. Leaves `self.app` empty on a boot failure,
    /// same as before this was factored out — callers own recovery
    /// (`switch_to` leaves its marker in place; a reboot has none to leave).
    async fn stop_then_boot(
        &self,
        op: StackOp,
        target_root: &Path,
        between_wipe_and_boot: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.shutdown().await;
        tracing::info!(?op, "stack stopped");

        if wipes_disposable_state(op) {
            // `wipes_disposable_state` is only true for `StackOp::Switch`, so
            // this branch only ever fires for a switch — the literal text
            // below is load-bearing: the N5 gate (`scripts/n5-vaults.sh`)
            // greps the log for exactly "vault switch: wiping DB" to catch
            // the widened SIGKILL window.
            tracing::info!(?op, "vault switch: wiping DB (Penpot cache + index reset)");
            vault::reset_disposable_state(&self.data_dir)
                .context("cannot wipe the disposable Penpot DB / index")?;
        }

        // Test-only widened SIGKILL window (used by the N5 switch gate; the
        // env var is switch-specific, so this is a no-op for a reboot).
        if let Some(delay) = test_switch_delay() {
            tracing::warn!(ms = delay.as_millis(), ?op, "TEST delay (widened crash window)");
            tokio::time::sleep(delay).await;
        }

        between_wipe_and_boot()?;

        tracing::info!(vault = %target_root.display(), ?op, "booting");
        let mut cfg = self.base_config.clone();
        cfg.designs_dir = target_root.to_path_buf();
        let app = boot(cfg, self.runner_slot.clone())
            .await
            .context("booting the vault failed")?;
        *self.app.lock().await = Some(app);
        Ok(())
    }

    /// Open a different vault: the full reset+reconcile switch (see module
    /// docs). Returns the target's [`VaultRef`]. On a boot failure the switch
    /// marker is intentionally left in place so the next boot recovers.
    pub async fn switch_to(&self, target: &Path) -> Result<VaultRef> {
        // Serialize: only one switch/reboot at a time.
        let _guard = self.switch_lock.lock().await;

        let target_ref = vault::ensure_vault(target)
            .context("cannot prepare the target vault (identity marker)")?;
        let target_root = target_ref.root();
        let previous = self.active();
        tracing::info!(
            from = %previous.path, to = %target_ref.path,
            "vault switch: begin"
        );

        // (1) Marker BEFORE any wipe — crash safety (see `writes_switch_marker`
        // for why a switch needs this and a reboot in place does not).
        debug_assert!(writes_switch_marker(StackOp::Switch));
        let marker = SwitchMarker {
            target: target_ref.path.clone(),
            target_id: target_ref.id.clone(),
            previous: Some(previous.path.clone()),
            previous_id: Some(previous.id.clone()),
            started_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        };
        vault::write_switch_marker(&self.data_dir, &marker)
            .context("cannot write the switch marker")?;

        // (2)-(5): stop, wipe (invariant 2 — zero residue of the old vault),
        // then — between the wipe and the boot — repoint the registry at the
        // target, then boot it and reconcile from the new tree.
        let target_ref_for_registry = target_ref.clone();
        self.stop_then_boot(StackOp::Switch, &target_root, || {
            let mut reg = VaultRegistry::load(&self.data_dir).unwrap_or_default();
            reg.upsert(&previous);
            reg.upsert(&target_ref_for_registry);
            reg.set_active(&target_ref_for_registry.path);
            reg.save(&self.data_dir).context("cannot persist the vault registry")
        })
        .await
        .context("booting the target vault failed")?;
        *self.active.lock().expect("active mutex") = target_ref.clone();

        // (6) Clear the marker — the switch landed.
        vault::clear_switch_marker(&self.data_dir).context("cannot clear the switch marker")?;
        tracing::info!(vault = %target_root.display(), "vault switch: complete");
        Ok(target_ref)
    }

    /// D4 — reboot the supervised stack in place: stop it, then boot it
    /// again against the SAME vault, so a freshly re-read `Preferences`
    /// (`crate::prefs`) takes effect for the settings that are only read at
    /// boot (plugins/CSP baked into `config.js` at script load; the
    /// supervisor cannot hot-add the exporter child — see module docs).
    ///
    /// Deliberately does NOT wipe the disposable DB/index
    /// ([`wipes_disposable_state`] returns `false` for
    /// [`StackOp::RebootInPlace`]) and does NOT write the crash marker
    /// ([`writes_switch_marker`], same reasoning) — see their docs. The
    /// vault, the registry's `active` pointer, and `self.active` are all
    /// unchanged by a reboot; only the running stack is torn down and
    /// re-raised, via the same [`stop_then_boot`](Self::stop_then_boot) a
    /// switch uses, with the wipe skipped and the registry hook a no-op.
    pub async fn reboot_in_place(&self) -> Result<()> {
        // Serialize with `switch_to` — only one stop/boot dance at a time.
        let _guard = self.switch_lock.lock().await;

        let active = self.active();
        let vault_root = active.root();
        tracing::info!(vault = %active.path, "reboot in place: begin");
        debug_assert!(!wipes_disposable_state(StackOp::RebootInPlace));
        debug_assert!(!writes_switch_marker(StackOp::RebootInPlace));

        self.stop_then_boot(StackOp::RebootInPlace, &vault_root, || Ok(()))
            .await
            .context("rebooting the vault in place failed")?;

        tracing::info!(vault = %active.path, "reboot in place: complete");
        Ok(())
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

    // Boot the resolved vault. D4: the runner slot starts empty — `boot()`'s
    // Preferences router captures this SAME `Arc` and only sees it filled in
    // a few lines below, once `VaultRunner::new` exists to fill it with (see
    // `RunnerSlot`'s doc for why the ordering has to be this way round).
    let runner_slot: RunnerSlot = Arc::new(AsyncMutex::new(None));
    let mut config = base_config.clone();
    config.designs_dir = vref.root();
    tracing::info!(vault = %vref.path, id = %vref.id, "opening vault");
    let app = boot(config, runner_slot.clone()).await?;

    // Recovery landed → clear the marker.
    if recovering {
        vault::clear_switch_marker(&data_dir).context("recovery: cannot clear the switch marker")?;
        tracing::info!(vault = %vref.path, "vault switch recovery: complete (single consistent vault)");
    }

    let runner = VaultRunner::new(app, base_config, vref, runner_slot.clone());
    *runner_slot.lock().await = Some(runner.clone());
    Ok(runner)
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

/// D5 Task 2 — one `OpenWindow` in the documented `GET /windows` shape.
fn window_json(w: &OpenWindow) -> serde_json::Value {
    // `fileId` is `null` for the home window (no file), a string otherwise —
    // `Option<String>`'s `Serialize` gives us exactly that via `json!`.
    json!({ "label": w.label, "fileId": w.file_id, "title": w.title })
}

/// D5 Task 2 — the `{"windows":[...]}` body, pulled out of the handler so
/// the shape is testable without going through an HTTP request (mirrors
/// `vault_json`/the `vault_json_shape` test above).
fn windows_json(reg: &WindowRegistry) -> serde_json::Value {
    let windows: Vec<serde_json::Value> = reg.list().iter().map(window_json).collect();
    json!({ "windows": windows })
}

/// D5 Task 2 — `GET /windows`: the open-window set, so the gate can observe
/// that opening a document actually raised a window for it (see the module
/// doc's `RunnerSlot`-style reasoning: a shell cannot reach into the Tauri
/// process's `WindowRegistry` any other way). Read-only; never mutates.
async fn control_windows(State(windows): State<WindowRegistry>) -> Response {
    Json(windows_json(&windows)).into_response()
}

/// D5 Task 2 — the control router's combined state: the vault runner (the
/// existing `/health`/`/active`/`/list`/`/open` routes) plus the window
/// registry (`/windows`, new here). Handlers keep extracting whichever piece
/// they need via `State<Arc<VaultRunner>>` / `State<WindowRegistry>`
/// unchanged — `FromRef` below is what makes both work off one `with_state`
/// call, the standard axum shape for "more than one thing in state" rather
/// than forking this into two servers or two ports.
#[derive(Clone)]
struct ControlState {
    runner: Arc<VaultRunner>,
    windows: WindowRegistry,
}

impl FromRef<ControlState> for Arc<VaultRunner> {
    fn from_ref(state: &ControlState) -> Self {
        state.runner.clone()
    }
}

impl FromRef<ControlState> for WindowRegistry {
    fn from_ref(state: &ControlState) -> Self {
        state.windows.clone()
    }
}

/// The control routes (own server — localhost only).
pub fn control_router(runner: Arc<VaultRunner>, windows: WindowRegistry) -> Router {
    Router::new()
        .route("/health", get(control_health))
        .route("/active", get(control_active))
        .route("/list", get(control_list))
        .route("/open", post(control_open))
        .route("/windows", get(control_windows))
        .with_state(ControlState { runner, windows })
}

/// Serve the control routes on `127.0.0.1:<port>` until the process exits.
/// Localhost-only by construction. Spawn this only when
/// [`CONTROL_PORT_ENV`] is set (the N5 gate sets it).
///
/// `windows` is the live [`WindowRegistry`] backing `GET /windows` (D5 Task
/// 2). The headless runner (no Tauri, no windows ever open) passes an empty
/// registry it never populates — `/windows` there always answers
/// `{"windows":[]}`, which is honest: nothing IS open. The GUI shell passes
/// the SAME registry the menu bar / window-per-file machinery populates, so
/// `/windows` reflects reality there.
pub async fn serve_control(runner: Arc<VaultRunner>, windows: WindowRegistry, port: u16) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("control server cannot bind {addr}"))?;
    tracing::info!(%addr, "vault control server listening (test/automation)");
    axum::serve(listener, control_router(runner, windows))
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

    // -----------------------------------------------------------------
    // D5 Task 2 — `GET /windows`'s documented shape:
    // `{"windows":[{"label","fileId":<opt>,"title"},...]}`. Tested at the
    // serialization function directly (no HTTP round trip needed — mirrors
    // `vault_json_shape` above) so it stays fast and doesn't need a real
    // `VaultRunner` (heavy: only `boot()` can build one).
    // -----------------------------------------------------------------

    #[test]
    fn windows_json_has_the_documented_shape_and_both_entries() {
        let reg = WindowRegistry::new();
        // One home window (no file), one file window — exactly the two
        // shapes `OpenWindow` can take.
        reg.insert(OpenWindow {
            label: "main".into(),
            file_id: None,
            title: "Penpot Local".into(),
        });
        reg.insert(OpenWindow {
            label: "file-fid1".into(),
            file_id: Some("fid1".into()),
            title: "Home".into(),
        });

        let body = windows_json(&reg);
        let windows = body["windows"].as_array().expect("windows is an array");
        assert_eq!(windows.len(), 2);

        // `list()` sorts home first, then files by title — see
        // `WindowRegistry::list`'s doc.
        assert_eq!(windows[0]["label"], "main");
        assert!(windows[0]["fileId"].is_null(), "home window has no file id");
        assert_eq!(windows[0]["title"], "Penpot Local");

        assert_eq!(windows[1]["label"], "file-fid1");
        assert_eq!(windows[1]["fileId"], "fid1");
        assert_eq!(windows[1]["title"], "Home");
    }

    // -----------------------------------------------------------------
    // D4 Task 3 — the riskiest property, pulled out into pure functions so
    // it is testable without booting a stack (see module docs and the two
    // functions' doc comments for the full reasoning).
    // -----------------------------------------------------------------

    #[test]
    fn a_vault_switch_wipes_disposable_state() {
        assert!(
            wipes_disposable_state(StackOp::Switch),
            "changing vaults must wipe the disposable DB/index (invariant 2, P0)"
        );
    }

    #[test]
    fn a_reboot_in_place_does_not_wipe_disposable_state() {
        assert!(
            !wipes_disposable_state(StackOp::RebootInPlace),
            "same vault, no boundary crossed — wiping would force a needless full re-import"
        );
    }

    #[test]
    fn only_a_vault_switch_writes_the_crash_marker() {
        // A switch moves the registry's active pointer across a DB wipe —
        // an interrupted switch needs the marker to complete forward.
        assert!(writes_switch_marker(StackOp::Switch));
        // A reboot in place never repoints the registry and never wipes;
        // an interrupted reboot self-heals via an entirely ordinary boot
        // of the same, unchanged vault — there is no half-changed pointer
        // for a marker to protect.
        assert!(!writes_switch_marker(StackOp::RebootInPlace));
    }
}
