//! Penpot Local desktop shell (Tauri v2).
//!
//! Opens a single window on the placeholder "booting" page, runs the shared
//! boot sequence (supervisor → provisioning → proxy) in the background, then
//! navigates the window to `/__bootstrap` (auto-login → `/`). Closing the
//! window / exiting the app shuts the supervised children down cleanly.

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use std::sync::Arc;

use penpot_desktop::{boot, AppConfig, RunningApp};
use tauri::{Manager, RunEvent};
use tokio::sync::Mutex;

type SharedApp = Arc<Mutex<Option<RunningApp>>>;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let running: SharedApp = Arc::new(Mutex::new(None));
    let running_setup = running.clone();

    let app = tauri::Builder::default()
        // M5: single-instance guard — MUST be the first plugin registered so
        // it runs before anything else. A second launch never boots its own
        // supervisor stack (M4 finding: postgres would refuse the shared data
        // dir); instead the running instance gets this callback and refocuses
        // its window while the second process exits immediately.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tracing::info!("second launch detected: focusing the existing window");
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .setup(move |app| {
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
                )
            };
            if let Err(e) = tray_result {
                tracing::error!("failed to create the sync-status tray: {e}");
            }
            // The window already shows placeholder-dist ("booting…"); bring
            // the stack up asynchronously and swap the URL when ready.
            tauri::async_runtime::spawn(async move {
                let booted = match config {
                    Ok(config) => boot(config).await,
                    Err(e) => Err(e),
                };
                match booted {
                    Ok(running_app) => {
                        let url: tauri::Url = running_app
                            .bootstrap_url()
                            .parse()
                            .expect("bootstrap url is valid");
                        // Bind the tray to the real sync daemon (no-op in
                        // demo mode, where the tray watches the mock).
                        if !demo {
                            match (running_app.sync_status(), running_app.sync_control()) {
                                (Some(status), Some(control)) => {
                                    bridge.attach(status, control);
                                    tracing::info!("tray bound to the sync daemon");
                                }
                                _ => tracing::warn!(
                                    "sync daemon not running; tray stays in its idle state"
                                ),
                            }
                            // M5: bind the "Exports:" row to board-export.
                            if let Some(status) = running_app.export_status() {
                                export_bridge.attach(status);
                                tracing::info!("tray bound to the board-export service");
                            }
                        }
                        *running.lock().await = Some(running_app);
                        if let Some(window) = handle.get_webview_window("main") {
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
                        if let Some(window) = handle.get_webview_window("main") {
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
                if let Some(app) = running.lock().await.take() {
                    app.shutdown().await;
                }
            });
        }
    });
}
