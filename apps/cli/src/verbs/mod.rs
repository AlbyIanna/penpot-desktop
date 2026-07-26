//! CLI verbs, grouped by tier (`docs/spikes/cli-verbs.md`).
//!
//! [`tier1`] runs in-process against the tree and the app data dir with no app
//! and no backend running (chapter C task 10). [`loopback`] is Tiers 2 and 3:
//! they need the running app and talk to it over loopback via
//! [`crate::attach`] — never in-process, because a second in-process writer to
//! app-owned state is undetectable in this workspace (see that module's doc).
//!
//! [`daemon`] is the odd one out and belongs to no tier: it is not a client of
//! a running app, it is how one starts. `bin/headless` — the entry point every
//! milestone gate launches — is a shim over it.

pub mod daemon;
pub mod loopback;
pub mod tier1;
