//! Penpot Local desktop shell (Tauri v2).
//!
//! Opens a single window on the placeholder "booting" page, runs the shared
//! boot sequence (supervisor → provisioning → proxy) in the background, then
//! navigates the window to `/__bootstrap` (auto-login → `/`). Closing the
//! window / exiting the app shuts the supervised children down cleanly.

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex as StdMutex};

use penpot_desktop::control::{self, VaultRunner};
use penpot_desktop::menubar::{self, LiveVault, LiveVaultSlot, MenuCtx};
use penpot_desktop::navwatch::NavWatch;
use penpot_desktop::overlay::{self, ProxyUrlSlot};
use penpot_desktop::windows::{OpenWindow, WindowRegistry, HOME_LABEL};
use penpot_desktop::AppConfig;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tokio::sync::Mutex;

// D3: `navigation_policy` (the shared `on_navigation` closure builder) and
// `open_file_window` (window-per-file's Tauri-side builder) now live in
// `penpot_desktop::menubar` rather than here — see that module's doc comment
// for why: menubar's dispatcher (File > Open…/Open Recent) is the first real
// CALLER of `open_file_window`, and it is a module of the `penpot_desktop`
// LIBRARY, while this file is the separate binary crate that only depends on
// the library. A library module cannot call a function defined in the
// binary, so the window-construction code had to move to the library side of
// that boundary, beside its caller. The home window below still goes through
// `menubar::navigation_policy` so the redirect rule is defined exactly once.

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

    // D3: the window registry (Task 1) — every open window, which file (if
    // any) it shows, and which one is key. Constructed here, before the
    // Builder chain, so both the home window (built in `.setup()` below) and
    // every later file window (`menubar::open_file_window`) register into
    // the SAME registry instance.
    let window_registry = WindowRegistry::new();
    // D3: late-bound proxy origin / team id / vault root the menu bar's
    // dispatcher needs — see `menubar::LiveVault`'s doc comment. Empty until
    // boot completes, refreshed again on every N5 vault switch.
    let menu_live: LiveVaultSlot = Arc::new(StdMutex::new(LiveVault::default()));

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
            // D3: the navigation-policy closure moved to `menubar::
            // navigation_policy` (was inlined here through D0-D2) so
            // `menubar::open_file_window` can attach the exact same rules to
            // every file window instead of hand-copying this closure — see
            // that function's doc comment for why a second copy would be a
            // hazard.
            let watch = NavWatch::from_env();
            let home_window = WebviewWindowBuilder::new(app, HOME_LABEL, WebviewUrl::default())
                .title("Penpot Local")
                .inner_size(1280.0, 800.0)
                .resizable(true)
                .on_navigation(menubar::navigation_policy(&app.handle().clone(), HOME_LABEL, watch))
                .build()?;

            // D3: register the home window itself — nothing did this before
            // Task 6, since window-per-file had no live caller yet (see
            // `menubar`'s module doc). Without this the Window menu would
            // never list "Penpot Local", and `key_has_file` would have
            // nothing to key off of until the first file window opened.
            window_registry.insert(OpenWindow {
                label: HOME_LABEL.into(),
                file_id: None,
                title: "Penpot Local".into(),
            });
            window_registry.set_key(HOME_LABEL);

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
            // D3: the app-DATA dir (recent-files.json lives here, NOT the
            // vault — see `recent.rs`'s module doc). Known synchronously,
            // unlike proxy_url/team_id/vault_root, which need a booted stack.
            let menu_data_dir = config.as_ref().ok().map(|c| c.data_dir.clone()).unwrap_or_default();
            // M5: the "Exports:" tray row exists only when the board-export
            // service will run (PENPOT_LOCAL_EXPORTS=1 resolved a layout);
            // the bridge late-binds it exactly like the sync-status one.
            let exports_enabled = config
                .as_ref()
                .map(|c| c.exporter.is_some())
                .unwrap_or(false);
            let export_bridge = penpot_desktop::status::ExportStatusBridge::new();
            let exports_rx = exports_enabled.then(|| export_bridge.subscribe());

            // D3: a `MenuCtx` carrying only the fixed pieces (registry, data
            // dir, the live slot) — used by the vault-switch closure below to
            // refresh `menu_live`'s team id / vault root after a switch and
            // rebuild the menu. `on_open_vault` isn't known yet at this point
            // (this closure IS what becomes it), so it's filled in below once
            // the final `menu_ctx` is built — `registry`/`live` are
            // `Arc`-shared, so that later value change is invisible here: a
            // rebuild through this ctx reaches the exact same registry/live.
            let menu_ctx_for_vault = MenuCtx {
                registry: window_registry.clone(),
                data_dir: menu_data_dir.clone(),
                live: menu_live.clone(),
                on_open_vault: None,
            };

            // N5: the tray "Open Vault…" action drives the vault switch through
            // the runner. macOS-only native folder picker (manual-QA surface —
            // the mechanism itself is gated headlessly, PLAN2 design item 4).
            // D3 reuses this SAME callback for File > Open Vault… — sharing it
            // (rather than re-deriving the switch flow in menubar/mod.rs) is
            // what "do not reimplement" means for that command.
            let on_open_vault: Option<penpot_desktop::tray::OpenVaultCb> = if demo {
                None
            } else {
                let holder = running_setup.clone();
                let handle_cb = app.handle().clone();
                let bridge_cb = bridge.clone();
                let export_bridge_cb = export_bridge.clone();
                let menu_live_cb = menu_live.clone();
                let menu_ctx_cb = menu_ctx_for_vault.clone();
                Some(Arc::new(move || {
                    let holder = holder.clone();
                    let handle_cb = handle_cb.clone();
                    let bridge_cb = bridge_cb.clone();
                    let export_bridge_cb = export_bridge_cb.clone();
                    let menu_live_cb = menu_live_cb.clone();
                    let menu_ctx_cb = menu_ctx_cb.clone();
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
                                // D3: team id (and possibly proxy port, though
                                // that's stable across switches — see
                                // `VaultRunner::proxy_url`'s doc) can change
                                // with the new vault; refresh the menu's live
                                // facts and rebuild so the Window menu / File
                                // > Open… dispatch reflect the new vault
                                // immediately instead of the one just left.
                                let team_id = runner.team_id().await.unwrap_or_default();
                                let access_token = runner.access_token().await.unwrap_or_default();
                                {
                                    let mut live = menu_live_cb.lock().unwrap_or_else(|p| p.into_inner());
                                    live.proxy_url = runner.proxy_url();
                                    live.team_id = team_id;
                                    live.access_token = access_token;
                                    live.vault_root = vref.root();
                                }
                                menubar::on_window_set_changed(&handle_cb, &menu_ctx_cb);
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

            // D3: the full `MenuCtx` (now that `on_open_vault` exists) and
            // the initial menu install. `install` registers the ONE global
            // `on_menu_event` listener — see menubar's module doc for why
            // that must happen exactly once — so this must run before the
            // tray (or anything else) could plausibly race a second call in.
            let menu_ctx = MenuCtx {
                registry: window_registry.clone(),
                data_dir: menu_data_dir,
                live: menu_live.clone(),
                on_open_vault: on_open_vault.clone(),
            };
            if let Err(e) = menubar::install(app.handle(), &menu_ctx) {
                tracing::error!("failed to install the native menu bar: {e}");
            }

            // Keep the registry (and therefore the Window menu / key-window-
            // dependent enabled state) honest for the home window too: forget
            // it if it's ever destroyed, and track focus so `key_has_file`
            // reflects reality the instant the user switches windows.
            let menu_ctx_for_home = menu_ctx.clone();
            let handle_for_home = app.handle().clone();
            home_window.on_window_event(move |event| match event {
                WindowEvent::Destroyed => {
                    menu_ctx_for_home.registry.remove(HOME_LABEL);
                    menubar::on_window_set_changed(&handle_for_home, &menu_ctx_for_home);
                }
                WindowEvent::Focused(true) => {
                    menu_ctx_for_home.registry.set_key(HOME_LABEL);
                    menubar::on_window_set_changed(&handle_for_home, &menu_ctx_for_home);
                }
                _ => {}
            });

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
            let menu_ctx_boot = menu_ctx.clone();
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
                        // D3: publish the menu bar's late-bound facts (see
                        // `menubar::LiveVault`) now that a stack exists, then
                        // rebuild so File > Open…/New File/the View pages
                        // stop no-op'ing the instant boot finishes.
                        let team_id = runner.team_id().await.unwrap_or_default();
                        let access_token = runner.access_token().await.unwrap_or_default();
                        {
                            let mut live =
                                menu_live.lock().unwrap_or_else(|p| p.into_inner());
                            live.proxy_url = runner.proxy_url();
                            live.team_id = team_id;
                            live.access_token = access_token;
                            live.vault_root = runner.active().root();
                        }
                        menubar::on_window_set_changed(&handle, &menu_ctx_boot);
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
