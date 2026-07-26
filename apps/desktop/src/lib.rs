//! Penpot Local desktop shell: the Tauri-facing half of the app.
//!
//! The boot sequence, the vault runner, and every GUI-free verb now live in
//! `app-core` and its sibling crates (chapter C). What remains here is the
//! native chrome — menus, tray, windows, overlay — plus its own pure model
//! layer, and the navigation policy that keeps the SPA inside the app.

pub mod dialog;
pub mod menubar;
pub mod navwatch;
pub mod overlay;
pub mod tray;

/// Re-exported so `main.rs`, `bin/headless.rs` and the two integration tests
/// under `tests/` keep spelling these at their pre-chapter-C paths.
/// `supervisor_config` is here for `tests/packaged_layout_sim.rs`, which
/// asserts the packaged-mode command lines through it.
pub use app_core::{
    boot, control, layout, preflight, prefs_http, status, supervisor_config, windows, AppConfig,
    RunningApp,
};
