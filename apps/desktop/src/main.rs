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
        .setup(move |app| {
            let handle = app.handle().clone();
            let running = running_setup.clone();
            // The window already shows placeholder-dist ("booting…"); bring
            // the stack up asynchronously and swap the URL when ready.
            tauri::async_runtime::spawn(async move {
                let booted = match AppConfig::resolve() {
                    Ok(config) => boot(config).await,
                    Err(e) => Err(e),
                };
                match booted {
                    Ok(running_app) => {
                        let url: tauri::Url = running_app
                            .bootstrap_url()
                            .parse()
                            .expect("bootstrap url is valid");
                        *running.lock().await = Some(running_app);
                        if let Some(window) = handle.get_webview_window("main") {
                            if let Err(e) = window.navigate(url) {
                                tracing::error!("failed to navigate to penpot: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("boot failed: {e:#}");
                        if let Some(window) = handle.get_webview_window("main") {
                            let _ = window.set_title("Penpot Local — boot failed (see logs)");
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
