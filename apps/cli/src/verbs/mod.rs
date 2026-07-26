//! CLI verbs, grouped by tier (`docs/spikes/cli-verbs.md`).
//!
//! Only Tier 1 exists yet (chapter C task 10): verbs that read the vault tree
//! and the app data dir with no app and no backend running. Tiers 2 and 3
//! need the running app and talk to it over loopback via [`crate::attach`] —
//! never in-process — and are Task 11's.

pub mod tier1;
