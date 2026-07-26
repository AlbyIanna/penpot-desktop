//! The served HTML pages (chapter C extraction).
//!
//! These modules were already thin clients over an in-process API before the
//! CLI existed — that is why the CLI is the app's THIRD front-end, not a new
//! layer beneath the other two. `boot` (in `app-core`) mounts both routers,
//! which is why this crate is extracted BEFORE `app-core`.

pub mod home;
pub mod navprobe;
