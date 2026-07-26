//! Verbs over a vault (chapter C extraction).
//!
//! Everything here is data plus decisions: the registry and switch-marker
//! state machine, the checkpoint verb, the front-door mutations, path
//! resolution, and command construction. The runtime orchestration that
//! drives them — teardown, wipe, reconcile — lives in `app-core`'s `control`.

pub mod checkpoint;
pub mod docopen;
pub mod gitinit;
pub mod manage;
pub mod reveal;
pub mod vault;
