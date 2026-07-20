//! Penpot Local desktop shell (Tauri v2).
//!
//! Opens a single window on the placeholder "booting" page, runs the shared
//! boot sequence (supervisor → provisioning → proxy) in the background, then
//! navigates the window to `/__bootstrap` (auto-login → `/`). Closing the
//! window / exiting the app shuts the supervised children down cleanly.

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::sync::Arc;

use penpot_desktop::control::{self, VaultRunner};
use penpot_desktop::navwatch::{self, Decision, NavWatch};
use penpot_desktop::overlay::{self, ProxyUrlSlot};
use penpot_desktop::windows::{self, OpenWindow, Reuse, WindowRegistry, HOME_LABEL};
use penpot_desktop::AppConfig;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tokio::sync::Mutex;

/// Build the shared `on_navigation` policy closure for the window labelled
/// `label`: D1/D2's rule that `#/auth/*` and `#/dashboard` are cancelled and
/// redirected to `/__home`, applied to whichever window's webview reports the
/// navigation (see `navwatch::decide`).
///
/// D0 wrote this closure inline for the home window only. D3 needs the exact
/// same rules on every file window `open_file_window` creates below — a
/// second, hand-copied closure per window would let the copies drift, and a
/// drifted copy is exactly how `#/dashboard` would quietly become reachable
/// again from a file window, undoing D1/D2. So the closure is built once,
/// here, parameterized only by which window it should redirect (the target
/// of `CancelAndRedirect` must be the window that navigated, not always
/// `HOME_LABEL`) — the redirect *rule* itself (`navwatch::decide`) is never
/// duplicated.
fn navigation_policy(
    app: &tauri::AppHandle,
    label: &str,
    watch: NavWatch,
) -> impl Fn(&tauri::Url) -> bool + Send + 'static {
    let nav_handle = app.clone();
    let label = label.to_string();
    move |url| {
        let url_s = url.to_string();
        // Observe first: the log is the spike's primary evidence.
        watch.record("on_navigation", &url_s);
        match navwatch::decide(&url_s, watch.redirect_enabled()) {
            Decision::Allow => true,
            Decision::CancelAndRedirect(path) => {
                // Cannot navigate from inside the handler (we are on the
                // webview's navigation path); hop to the app thread and
                // navigate there.
                let h = nav_handle.clone();
                let label = label.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(w) = h.get_webview_window(&label) {
                        // See the D0 HAZARD note this closure was lifted
                        // from: joining against `w.url()` trusts whatever the
                        // window is currently pointed at. Every window this
                        // policy is attached to (home via `/__bootstrap`,
                        // file windows via their deep link) is always already
                        // on the proxy origin by the time a redirect-eligible
                        // navigation can fire, so this still resolves against
                        // `http://localhost:<proxy-port>`.
                        if let Ok(base) = w.url() {
                            if let Ok(dest) = base.join(&path) {
                                let _ = w.navigate(dest);
                            }
                        }
                    }
                });
                false // cancel the web-route navigation
            }
        }
    }
}

/// D3 hook point: called whenever the set of open windows changes (a file
/// window opened or closed). A later task rebuilds the native Window menu
/// from `WindowRegistry::list()` here — for now this is a no-op so the two
/// call sites below (`open_file_window`'s `Create` arm and its
/// `on_window_event` `Destroyed` handler) have exactly one place to grow that
/// wiring, instead of the menu-rebuild call being invented twice later.
fn on_window_set_changed(_app: &tauri::AppHandle, _reg: &WindowRegistry) {
    // Intentionally empty: see the doc comment above.
}

/// Open `file_id` in its own window (D3: window-per-file). If a window for
/// this file is already open, this FOCUSES it instead of creating a second
/// one — that decision is `windows::reuse_or_create`, unit-tested in
/// `windows.rs` without a Tauri runtime; this function is the thin Tauri-side
/// translation of it, kept out of `windows.rs` so that module stays
/// Tauri-free (mirrors `overlay::toggle_palette`'s split for the N4 palette).
///
/// `title` becomes both the window title and the `OpenWindow` record's title
/// (the Window menu's future display string) — window-per-file's whole point
/// is that the title IS the filename.
pub fn open_file_window(
    app: &tauri::AppHandle,
    reg: &WindowRegistry,
    proxy_url: &str,
    team_id: &str,
    file_id: &str,
    page_id: Option<&str>,
    title: &str,
) -> tauri::Result<()> {
    match windows::reuse_or_create(file_id, reg) {
        Reuse::Focus(label) => {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
            Ok(())
        }
        Reuse::Create(label) => {
            // vault_index::workspace_deep_link builds the exact
            // `/#/workspace?team-id=…&file-id=…[&page-id=…]` path — hand-
            // assembling it here would risk drifting from the sanitization
            // that function applies (crates/vault-index/src/query.rs:32).
            let path = vault_index::workspace_deep_link(team_id, file_id, page_id);
            let full = format!("{}{}", proxy_url.trim_end_matches('/'), path);
            let url: tauri::Url = full.parse().map_err(tauri::Error::InvalidUrl)?;

            let watch = NavWatch::from_env();
            let window = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
                .title(title)
                .inner_size(1280.0, 800.0)
                .resizable(true)
                // D1/D2: the same navigation policy as the home window — see
                // `navigation_policy` above.
                .on_navigation(navigation_policy(app, &label, watch))
                .build()?;

            reg.insert(OpenWindow {
                label: label.clone(),
                file_id: Some(file_id.to_string()),
                title: title.to_string(),
            });

            // Keep the registry honest: forget the window the moment it is
            // destroyed, so a closed file never lingers as a ghost entry in
            // the future Window menu, and lets a later "open this file"
            // request build a fresh window instead of trying to focus one
            // that no longer exists.
            let reg_for_close = reg.clone();
            let app_for_close = app.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Destroyed = event {
                    reg_for_close.remove(&label);
                    on_window_set_changed(&app_for_close, &reg_for_close);
                }
            });

            on_window_set_changed(app, reg);
            Ok(())
        }
    }
}

/// The live vault runner (owns the stack; swaps it on `File > Open Vault`).
/// `None` until boot completes.
type SharedRunner = Arc<Mutex<Option<Arc<VaultRunner>>>>;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let running: SharedRunner = Arc::new(Mutex::new(None));
    let running_setup = running.clone();

    // N4: the palette overlay's proxy origin, filled once boot completes.
    // Shared by the global shortcut handler and the tray "Quick open…" entry.
    let proxy_slot: ProxyUrlSlot = Arc::new(std::sync::Mutex::new(None));
    let proxy_slot_setup = proxy_slot.clone();
    let proxy_slot_boot = proxy_slot.clone();
    let proxy_slot_shortcut = proxy_slot.clone();

    // N4: the global shortcut (default Cmd/Ctrl+K, configurable via
    // PENPOT_LOCAL_PALETTE_SHORTCUT) toggling the palette overlay window.
    let palette_shortcut = overlay::configured_shortcut();
    let global_shortcut_plugin = tauri_plugin_global_shortcut::Builder::new()
        .with_shortcut(palette_shortcut)
        .expect("valid palette shortcut")
        .with_handler(move |app, _shortcut, event| {
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                overlay::toggle_palette(app, &proxy_slot_shortcut);
            }
        })
        .build();

    let app = tauri::Builder::default()
        // M5: single-instance guard — MUST be the first plugin registered so
        // it runs before anything else. A second launch never boots its own
        // supervisor stack (M4 finding: postgres would refuse the shared data
        // dir); instead the running instance gets this callback and refocuses
        // its window while the second process exits immediately.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tracing::info!("second launch detected: focusing the existing window");
            if let Some(window) = app.get_webview_window(HOME_LABEL) {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(global_shortcut_plugin)
        .setup(move |app| {
            // D0: the main window is built HERE (not from tauri.conf.json) so a
            // navigation handler can be attached — `on_navigation` is a builder
            // method and a config-declared window gives us no builder.
            //
            // D3: the navigation-policy closure moved to the top-level
            // `navigation_policy` function (was inlined here through D0-D2) so
            // `open_file_window` can attach the exact same rules to every file
            // window instead of hand-copying this closure — see that
            // function's doc comment for why a second copy would be a hazard.
            let watch = NavWatch::from_env();
            WebviewWindowBuilder::new(app, HOME_LABEL, WebviewUrl::default())
                .title("Penpot Local")
                .inner_size(1280.0, 800.0)
                .resizable(true)
                .on_navigation(navigation_policy(&app.handle().clone(), HOME_LABEL, watch))
                .build()?;

            let handle = app.handle().clone();
            let running = running_setup.clone();

            // --- M3 sync-status tray -----------------------------------
            // The tray must be created here (main thread, before the async
            // boot completes), but the real sync daemon only exists at the
            // end of boot() — so the tray subscribes to a DaemonStatusBridge
            // now and the boot task attaches the daemon to it below.
            // PENPOT_LOCAL_TRAY_DEMO=1 keeps the scripted mock instead (menu
            // QA without a running stack).
            let demo = std::env::var_os("PENPOT_LOCAL_TRAY_DEMO").is_some();
            let bridge = penpot_desktop::status::DaemonStatusBridge::new();
            // The bundled `penpot-runtime/` (M4) lives in the Tauri
            // resources dir; in dev there is no bundle there and the
            // resolver falls back to env overrides + repo runtime/.
            // Resolved HERE (cheap, sync) so the tray knows the designs dir
            // for its file-manager actions (M5); the boot task consumes the
            // same resolution below — a resolve error is reported there,
            // exactly like pre-M5.
            let resource_dir = app.path().resource_dir().ok();
            let config = AppConfig::resolve_with_resources(resource_dir);
            let designs_dir = config.as_ref().ok().map(|c| c.designs_dir.clone());
            // M5: the "Exports:" tray row exists only when the board-export
            // service will run (PENPOT_LOCAL_EXPORTS=1 resolved a layout);
            // the bridge late-binds it exactly like the sync-status one.
            let exports_enabled = config
                .as_ref()
                .map(|c| c.exporter.is_some())
                .unwrap_or(false);
            let export_bridge = penpot_desktop::status::ExportStatusBridge::new();
            let exports_rx = exports_enabled.then(|| export_bridge.subscribe());

            // N5: the tray "Open Vault…" action drives the vault switch through
            // the runner. macOS-only native folder picker (manual-QA surface —
            // the mechanism itself is gated headlessly, PLAN2 design item 4).
            let on_open_vault: Option<penpot_desktop::tray::OpenVaultCb> = if demo {
                None
            } else {
                let holder = running_setup.clone();
                let handle_cb = app.handle().clone();
                let bridge_cb = bridge.clone();
                let export_bridge_cb = export_bridge.clone();
                Some(Arc::new(move || {
                    let holder = holder.clone();
                    let handle_cb = handle_cb.clone();
                    let bridge_cb = bridge_cb.clone();
                    let export_bridge_cb = export_bridge_cb.clone();
                    tauri::async_runtime::spawn(async move {
                        // Pick a folder off the UI thread (osascript blocks).
                        let picked = tauri::async_runtime::spawn_blocking(|| {
                            penpot_desktop::dialog::choose_folder("Choose your design vault")
                        })
                        .await
                        .ok()
                        .flatten();
                        let Some(path) = picked else { return };
                        let runner = { holder.lock().await.clone() };
                        let Some(runner) = runner else {
                            tracing::warn!("open vault: still booting; ignoring switch request");
                            return;
                        };
                        match runner.switch_to(&path).await {
                            Ok(vref) => {
                                tracing::info!(vault = %vref.path, "open vault: switch complete");
                                // Re-bind the tray to the new stack.
                                if let (Some(status), Some(sc)) =
                                    (runner.sync_status().await, runner.sync_control().await)
                                {
                                    bridge_cb.attach(status, sc);
                                }
                                if let Some(ex) = runner.export_status().await {
                                    export_bridge_cb.attach(ex);
                                }
                                // Reload the window onto the new vault's home.
                                if let Some(window) = handle_cb.get_webview_window(HOME_LABEL) {
                                    if let Ok(url) = format!("{}/__bootstrap", runner.proxy_url())
                                        .parse::<tauri::Url>()
                                    {
                                        let _ = window.navigate(url);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("open vault failed: {e:#}");
                                penpot_desktop::dialog::native_error_dialog(
                                    "Penpot Local — Open Vault failed",
                                    &format!("{e:#}"),
                                );
                            }
                        }
                    });
                }) as penpot_desktop::tray::OpenVaultCb)
            };

            let tray_result = if demo {
                let mock = Arc::new(penpot_desktop::status::MockStatusSource::new(
                    Default::default(),
                ));
                let result = penpot_desktop::tray::spawn_tray(
                    app.handle(),
                    mock.subscribe(),
                    mock.control(),
                    designs_dir,
                    None,
                    proxy_slot_setup.clone(),
                    None,
                );
                tauri::async_runtime::spawn(async move {
                    mock.play_demo(std::time::Duration::from_secs(4)).await;
                });
                result
            } else {
                penpot_desktop::tray::spawn_tray(
                    app.handle(),
                    bridge.subscribe(),
                    bridge.control(),
                    designs_dir,
                    exports_rx,
                    proxy_slot_setup.clone(),
                    on_open_vault,
                )
            };
            if let Err(e) = tray_result {
                tracing::error!("failed to create the sync-status tray: {e}");
            }
            // The window already shows placeholder-dist ("booting…"); bring
            // the stack up asynchronously and swap the URL when ready.
            tauri::async_runtime::spawn(async move {
                // N5: resolve the active vault (registry + interrupted-switch
                // recovery), then boot it; the runner owns the stack and swaps
                // it on `File > Open Vault`.
                let booted = match config {
                    Ok(config) => control::boot_active_vault(config).await,
                    Err(e) => Err(e),
                };
                match booted {
                    Ok(runner) => {
                        // N4: publish the proxy origin so the palette overlay
                        // (global shortcut + tray) can reach /__palette.
                        if let Ok(mut slot) = proxy_slot_boot.lock() {
                            *slot = Some(runner.proxy_url());
                        }
                        // D0: the spike gate points the window at /__navprobe.
                        // Absent the override this is byte-identical to before.
                        let start = std::env::var("PENPOT_LOCAL_START_URL")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| format!("{}/__bootstrap", runner.proxy_url()));
                        let url: tauri::Url =
                            start.parse().expect("start url is valid");
                        // Bind the tray to the real sync daemon (no-op in
                        // demo mode, where the tray watches the mock).
                        if !demo {
                            match (runner.sync_status().await, runner.sync_control().await) {
                                (Some(status), Some(control)) => {
                                    bridge.attach(status, control);
                                    tracing::info!("tray bound to the sync daemon");
                                }
                                _ => tracing::warn!(
                                    "sync daemon not running; tray stays in its idle state"
                                ),
                            }
                            // M5: bind the "Exports:" row to board-export.
                            if let Some(status) = runner.export_status().await {
                                export_bridge.attach(status);
                                tracing::info!("tray bound to the board-export service");
                            }
                        }
                        *running.lock().await = Some(runner);
                        if let Some(window) = handle.get_webview_window(HOME_LABEL) {
                            if let Err(e) = window.navigate(url) {
                                tracing::error!("failed to navigate to penpot: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("boot failed: {e:#}");
                        // M5 pre-flight failures get a friendlier surface:
                        // name the offending path in the title + a native
                        // dialog. The process stays alive showing the error
                        // (nothing was spawned; there is nothing to crash-
                        // loop) and exits cleanly when the user quits.
                        let title = match e.downcast_ref::<penpot_desktop::preflight::NonBmpPath>()
                        {
                            Some(v) => {
                                penpot_desktop::dialog::native_error_dialog(
                                    "Penpot Local cannot start",
                                    &v.to_string(),
                                );
                                format!(
                                    "Penpot Local — cannot start: emoji in the {} path: {}",
                                    v.label,
                                    v.path.display()
                                )
                            }
                            None => "Penpot Local — boot failed (see logs)".to_string(),
                        };
                        if let Some(window) = handle.get_webview_window(HOME_LABEL) {
                            let _ = window.set_title(&title);
                        }
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Penpot Local tauri application");

    // Route SIGTERM/SIGINT through the normal exit path: tao installs no
    // signal handler of its own, so without this the event loop dies without
    // RunEvent::Exit and the supervised children (postgres/valkey/java) are
    // orphaned — postgres keeps holding its port and breaks the next boot.
    #[cfg(unix)]
    {
        let handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            handle.exit(0);
        });
    }

    app.run(move |_handle, event| {
        if let RunEvent::Exit = event {
            // Blocking is fine here: the event loop is done; make sure no
            // child processes outlive the app.
            let running = running.clone();
            tauri::async_runtime::block_on(async move {
                if let Some(runner) = running.lock().await.take() {
                    runner.shutdown().await;
                }
            });
        }
    });
}
