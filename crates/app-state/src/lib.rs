//! Per-machine app state (chapter C): three JSON stores in the app data dir.
//!
//! Extracted verbatim from `apps/desktop`. These files describe this machine's
//! session — preferences, recently opened files, the last finished operation —
//! and never live inside the user's vault.

pub mod last_op;
pub mod prefs;
pub mod recent;
