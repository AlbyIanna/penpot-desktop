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

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Runtime};
use tokio::sync::watch;

use crate::status::{SyncControl, SyncStatusSnapshot};
use model::{
    build_menu_model, icon_pixels, AggregateState, MenuEntry, MenuModel, ICON_SIZE,
    PAUSE_TOGGLE_ID, QUIT_ID,
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
pub fn spawn_tray<R: Runtime>(
    app: &AppHandle<R>,
    rx: watch::Receiver<SyncStatusSnapshot>,
    control: Arc<dyn SyncControl>,
) -> tauri::Result<()> {
    let initial = build_menu_model(&rx.borrow(), Utc::now());
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
            _ => {}
        })
        .build(app)?;
    tracing::info!(tray = TRAY_ID, "sync-status tray icon created");

    // Rebuild the menu + icon on every snapshot change; also on a slow tick
    // so "Last sync: 3m ago" stays honest without any state change.
    let app = app.clone();
    let mut rx = rx;
    tauri::async_runtime::spawn(async move {
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
                _ = tick.tick() => {}
            }
            let model = build_menu_model(&rx.borrow(), Utc::now());
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
