//! Tray/menubar status UI (M3): a Tauri v2 tray icon whose menu shows the
//! last sync time, per-file states (conflicts/errors first), a pause/resume
//! toggle and Quit. All decision logic lives in the pure [`model`] builder
//! (unit-tested); this module only translates a [`model::MenuModel`] into
//! Tauri menu items and reacts to watch-channel changes.
//!
//! macOS rebuilds the native menu each time it is opened, so live updates
//! while the menu is open are not needed — we simply rebuild the whole menu
//! on every snapshot change (cheap: a handful of items) plus a slow tick so
//! the relative "Last sync" label never goes stale.

pub mod model;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Runtime};
use tokio::sync::watch;

use crate::status::{ExportStatusSnapshot, SyncControl, SyncStatusSnapshot};
use model::{
    build_menu_model, icon_pixels, AggregateState, MenuEntry, MenuModel, FILE_ROW_PREFIX,
    GIT_INIT_ID, ICON_SIZE, OPEN_DESIGNS_ID, PAUSE_TOGGLE_ID, QUIT_ID,
};

const TRAY_ID: &str = "penpot-sync-status";

fn icon(state: AggregateState) -> Image<'static> {
    Image::new_owned(icon_pixels(state), ICON_SIZE, ICON_SIZE)
}

fn build_tauri_menu<R: Runtime>(
    app: &AppHandle<R>,
    model: &MenuModel,
) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for entry in &model.entries {
        match entry {
            MenuEntry::Info { id, label } => {
                menu.append(&MenuItem::with_id(app, id, label, false, None::<&str>)?)?;
            }
            MenuEntry::File { key, label } => {
                // Enabled: clicking reveals the file dir in the file manager.
                menu.append(&MenuItem::with_id(
                    app,
                    format!("{FILE_ROW_PREFIX}{key}"),
                    label,
                    true,
                    None::<&str>,
                )?)?;
            }
            MenuEntry::OpenDesigns { label } => {
                menu.append(&MenuItem::with_id(
                    app,
                    OPEN_DESIGNS_ID,
                    label,
                    true,
                    None::<&str>,
                )?)?;
            }
            MenuEntry::GitInit { label } => {
                menu.append(&MenuItem::with_id(app, GIT_INIT_ID, label, true, None::<&str>)?)?;
            }
            MenuEntry::PauseToggle { label } => {
                menu.append(&MenuItem::with_id(
                    app,
                    PAUSE_TOGGLE_ID,
                    label,
                    true,
                    None::<&str>,
                )?)?;
            }
            MenuEntry::Separator => {
                menu.append(&PredefinedMenuItem::separator(app)?)?;
            }
            MenuEntry::Quit { label } => {
                menu.append(&MenuItem::with_id(app, QUIT_ID, label, true, None::<&str>)?)?;
            }
        }
    }
    Ok(menu)
}

/// Create the tray icon and keep it in sync with the status watch channel.
///
/// `rx`/`control` are the M3 daemon status contract
/// (see [`crate::status`]) — today they come from
/// [`crate::status::MockStatusSource`], at integration time from the real
/// sync daemon; this function is agnostic.
///
/// Quit goes through `app.exit(0)`, i.e. the existing `RunEvent::Exit`
/// clean-shutdown path in `main.rs` (children are never orphaned).
///
/// `designs_dir` (M5): the sync root, enabling the file-manager actions
/// ("Open Designs Folder", "Enable git versioning", per-file reveal). Pass
/// `None` when it isn't known (config resolution failed / demo mode) — the
/// menu then keeps its pre-M5 shape.
///
/// `exports_rx` (M5): the board-export service's status stream (via
/// [`crate::status::ExportStatusBridge`]); `Some` only when
/// `PENPOT_LOCAL_EXPORTS=1` — it adds the "Exports: …" info row.
pub fn spawn_tray<R: Runtime>(
    app: &AppHandle<R>,
    rx: watch::Receiver<SyncStatusSnapshot>,
    control: Arc<dyn SyncControl>,
    designs_dir: Option<PathBuf>,
    exports_rx: Option<watch::Receiver<ExportStatusSnapshot>>,
) -> tauri::Result<()> {
    let designs_available = designs_dir.is_some();
    let exports_snapshot = exports_rx.as_ref().map(|r| r.borrow().clone());
    let initial = build_menu_model(
        &rx.borrow(),
        Utc::now(),
        designs_available,
        exports_snapshot.as_ref(),
    );
    let menu = build_tauri_menu(app, &initial)?;

    let rx_for_events = rx.clone();
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(initial.aggregate))
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("Penpot Local — sync status")
        .on_menu_event(move |app, event| match event.id.as_ref() {
            PAUSE_TOGGLE_ID => {
                let paused = rx_for_events.borrow().paused;
                if paused {
                    tracing::info!("tray: resume syncing");
                    control.resume();
                } else {
                    tracing::info!("tray: pause syncing");
                    control.pause();
                }
            }
            QUIT_ID => {
                tracing::info!("tray: quit requested");
                app.exit(0);
            }
            OPEN_DESIGNS_ID => {
                if let Some(designs) = &designs_dir {
                    tracing::info!(dir = %designs.display(), "tray: open designs folder");
                    crate::reveal::open_folder(designs);
                }
            }
            GIT_INIT_ID => {
                if let Some(designs) = designs_dir.clone() {
                    tracing::info!(dir = %designs.display(), "tray: enable git versioning");
                    // git is fast but external: keep it off the UI thread.
                    tauri::async_runtime::spawn_blocking(move || {
                        match crate::gitinit::run_git_init(&designs) {
                            Ok(out) => {
                                tracing::info!("git init helper:\n{}", out.trim_end());
                                crate::dialog::native_info_dialog(
                                    "Penpot Local — git versioning",
                                    out.trim_end(),
                                );
                            }
                            Err(e) => {
                                tracing::error!("git init helper failed: {e:#}");
                                crate::dialog::native_error_dialog(
                                    "Penpot Local — git versioning failed",
                                    &format!("{e:#}"),
                                );
                            }
                        }
                    });
                }
            }
            other => {
                if let (Some(key), Some(designs)) =
                    (other.strip_prefix(FILE_ROW_PREFIX), &designs_dir)
                {
                    let path = designs.join(key);
                    tracing::info!(path = %path.display(), "tray: reveal file in file manager");
                    crate::reveal::reveal(&path);
                }
            }
        })
        .build(app)?;
    tracing::info!(tray = TRAY_ID, "sync-status tray icon created");

    // Rebuild the menu + icon on every snapshot change (sync or exports);
    // also on a slow tick so "Last sync: 3m ago" stays honest without any
    // state change.
    let app = app.clone();
    let mut rx = rx;
    let mut exports_rx = exports_rx;
    tauri::async_runtime::spawn(async move {
        // Await an exports change; pends forever when the line is absent.
        // A closed sender drops the receiver (the exports row disappears)
        // and the sync channel keeps driving rebuilds.
        async fn exports_changed(rx: &mut Option<watch::Receiver<ExportStatusSnapshot>>) {
            match rx {
                Some(r) => {
                    if r.changed().await.is_err() {
                        tracing::warn!("tray: exports status channel closed; row removed");
                        *rx = None;
                    }
                }
                None => std::future::pending().await,
            }
        }
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.tick().await; // consume the immediate first tick
        let mut last_aggregate = initial.aggregate;
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped (daemon gone) — freeze the menu as-is.
                        tracing::warn!("tray: status channel closed; menu frozen");
                        break;
                    }
                }
                _ = exports_changed(&mut exports_rx) => {}
                _ = tick.tick() => {}
            }
            let exports_snapshot = exports_rx.as_ref().map(|r| r.borrow().clone());
            let model = build_menu_model(
                &rx.borrow(),
                Utc::now(),
                designs_available,
                exports_snapshot.as_ref(),
            );
            match build_tauri_menu(&app, &model) {
                Ok(menu) => {
                    if let Err(e) = tray.set_menu(Some(menu)) {
                        tracing::warn!("tray: set_menu failed: {e}");
                    }
                }
                Err(e) => tracing::warn!("tray: menu rebuild failed: {e}"),
            }
            if model.aggregate != last_aggregate {
                last_aggregate = model.aggregate;
                if let Err(e) = tray.set_icon(Some(icon(model.aggregate))) {
                    tracing::warn!("tray: set_icon failed: {e}");
                }
                // set_icon resets the template flag on some platforms.
                let _ = tray.set_icon_as_template(true);
            }
        }
    });
    Ok(())
}
