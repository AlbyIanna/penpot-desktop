//! Penpot Local core boot sequence (M1), extracted in chapter C.
//!
//! Shared by every front-end: the Tauri shell, the headless daemon, and the
//! CLI. Resolve configuration -> load-or-generate the pinned
//! `PENPOT_SECRET_KEY` -> start the supervisor (postgres -> valkey -> backend
//! JVM) -> first-boot single-user provisioning over RPC -> start the local
//! proxy with the `/__bootstrap` auto-login route -> expose readiness.
//!
//! `control` and `boot` are mutually recursive — `control::boot_active_vault`
//! calls `boot`, and `boot` takes a `control::RunnerSlot` — so they cannot be
//! separated without introducing a trait. They are one crate on purpose.

mod boot;

pub mod control;
pub mod layout;
pub mod preflight;
pub mod prefs_http;
pub mod status;
pub mod windows;

pub use boot::*;
