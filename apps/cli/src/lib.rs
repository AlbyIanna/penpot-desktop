//! `penpot` — the CLI front-end (chapter C).
//!
//! INTERNAL surface: no stability contract, no versioned API, no end-user
//! documentation. Verbs fall in three tiers (see docs/spikes/cli-verbs.md):
//! Tier 1 runs in-process against the tree and data dir with nothing else
//! running; Tiers 2 and 3 need the running app and go over loopback, never by
//! linking the library — the spike proved an in-process caller becomes an
//! undetected concurrent writer to app-owned state.

pub mod attach;
pub mod verbs;
