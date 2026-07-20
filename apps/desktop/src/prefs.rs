//! `preferences.json` — the native Preferences window's backing store (D4).
//!
//! Lives in the app **data dir**, never the vault: it's per-machine state
//! about which local services are turned on, not user work, so it must not
//! travel with a cloned vault — the same reasoning as the Open-Recent list
//! (`recent.rs`) and E7's consent ledger (`crates/sync-core/src/consent.rs`).
//!
//! [`load`] never fails: a missing or corrupt file degrades to
//! [`Preferences::default`], same posture as `recent.rs`'s `list_recent` — a
//! broken piece of UI-state must never stop the app from booting.
//!
//! [`needs_reboot`] is a pure function, not a UI decision: which settings can
//! apply live is a property of the running system, not of the Preferences
//! screen. Two boot-time mechanisms make each field what it is:
//! - `config.js` (the frontend flags string, including `enable-plugins`) is
//!   read ONCE by the SPA's `<script>` tag at page load — there is no live
//!   channel to push a changed flag into an already-loaded page, so
//!   `plugins_enabled` can only take effect on a fresh boot, in either
//!   direction.
//! - the proxy's CSP response header value is chosen once, at `boot()`
//!   (`resolve_html_csp`) and wired into the router at bind time — changing
//!   it needs a fresh proxy bind, so `csp_enabled` is boot-time too.
//! - the supervisor has no hot-add: it spawns the exporter child (if any)
//!   exactly once, at `boot()`, from `AppConfig.exporter`. Turning
//!   `exports_enabled` OFF is LIVE — `RunningApp::set_renders_enabled` just
//!   stops the board-export poll loop in place, the child keeps running idle.
//!   Turning it back ON needs the supervisor to spawn a child that was never
//!   started, which only happens at boot — so OFF→ON is boot-time, ON→OFF is
//!   not.
//! - `sync_enabled` is never boot-time in either direction: the sync
//!   daemon's pause/resume flag (`sync_daemon::SyncControl`) is a runtime
//!   toggle with no boot involved at all.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name of the preferences store, at the root of the app's DATA dir
/// (NOT the vault).
pub const PREFS_FILE_NAME: &str = "preferences.json";

fn default_true() -> bool {
    true
}

/// The native Preferences window's backing store. Every field defaults to
/// `true` (nothing is disabled out of the box); every field carries its own
/// `serde(default)` so a file written by an older build (missing a newer
/// field) or a newer build (carrying a field this build doesn't know about
/// yet — ordinary struct deserialization already ignores unknown fields)
/// loads cleanly instead of failing. See the module docs for which fields
/// are LIVE vs BOOT-TIME and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    /// LIVE. `sync_daemon::SyncControl::pause`/`resume` is a runtime flag.
    #[serde(default = "default_true")]
    pub sync_enabled: bool,
    /// LIVE going OFF, BOOT-TIME going ON. See the module docs — the
    /// supervisor cannot hot-add the exporter child.
    #[serde(default = "default_true")]
    pub exports_enabled: bool,
    /// BOOT-TIME (either direction). Baked into `config.js`, read once at
    /// SPA script load.
    #[serde(default = "default_true")]
    pub plugins_enabled: bool,
    /// BOOT-TIME (either direction). Baked into the proxy's CSP response
    /// header at bind time.
    #[serde(default = "default_true")]
    pub csp_enabled: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            sync_enabled: true,
            exports_enabled: true,
            plugins_enabled: true,
            csp_enabled: true,
        }
    }
}

fn store_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(PREFS_FILE_NAME)
}

/// Load preferences from `data_dir`. Never fails: a missing file, an
/// unreadable file, or corrupt/invalid JSON all degrade to
/// [`Preferences::default`] — a broken UI-state file must never stop the
/// app booting.
pub fn load(data_dir: &Path) -> Preferences {
    std::fs::read(store_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Save preferences to `data_dir`, atomically (tmp sibling + fsync + rename)
/// — same shape as `recent.rs`'s `atomic_write` / `vault.rs`'s
/// `write_json_atomic` — so a crash mid-write never leaves a half-written,
/// corrupt file for [`load`] to silently paper over.
pub fn save(data_dir: &Path, prefs: &Preferences) -> anyhow::Result<()> {
    let path = store_path(data_dir);
    let body = serde_json::to_vec_pretty(prefs)?;
    atomic_write(&path, &body)
}

/// Whether going from `old` to `new` requires rebooting the supervised stack
/// in place rather than applying live. True exactly when a BOOT-TIME change
/// (see the field docs on [`Preferences`]) happened:
/// - `plugins_enabled` or `csp_enabled` changed at all, in either direction;
/// - `exports_enabled` went `false -> true` (the supervisor cannot hot-add
///   the exporter child mid-run; `true -> false` is live —
///   `RunningApp::set_renders_enabled` handles it without a reboot).
///
/// `sync_enabled` never contributes — it has no boot-time mode at all.
pub fn needs_reboot(old: &Preferences, new: &Preferences) -> bool {
    old.plugins_enabled != new.plugins_enabled
        || old.csp_enabled != new.csp_enabled
        || (!old.exports_enabled && new.exports_enabled)
}

/// Write `bytes` to `path` atomically: write a sibling `.tmp` file, fsync it,
/// then rename over `path`. Same shape as `recent.rs`'s crate-private helper
/// of the same name — not shared because it isn't exported past that
/// module's boundary, and duplicating four lines is cheaper than a new
/// shared crate for it.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_everything_on() {
        let p = Preferences::default();
        assert!(p.sync_enabled && p.exports_enabled && p.plugins_enabled && p.csp_enabled);
    }

    #[test]
    fn a_missing_or_corrupt_file_yields_defaults_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()), Preferences::default());
        std::fs::write(tmp.path().join(PREFS_FILE_NAME), b"{not json").unwrap();
        assert_eq!(load(tmp.path()), Preferences::default(),
                   "a corrupt prefs file must not stop the app booting");
    }

    #[test]
    fn round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Preferences { sync_enabled: false, exports_enabled: false, ..Default::default() };
        save(tmp.path(), &p).unwrap();
        assert_eq!(load(tmp.path()), p);
    }

    #[test]
    fn an_unknown_future_field_does_not_break_loading() {
        // A newer build's prefs file must not brick an older one.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(PREFS_FILE_NAME),
                       br#"{"syncEnabled":false,"somethingNew":42}"#).unwrap();
        assert!(!load(tmp.path()).sync_enabled);
    }

    #[test]
    fn only_boot_time_changes_need_a_reboot() {
        let base = Preferences::default();
        // Live: sync toggles without a reboot.
        assert!(!needs_reboot(&base, &Preferences { sync_enabled: false, ..base.clone() }));
        // Live: turning renders OFF just stops the poll loop.
        assert!(!needs_reboot(&base, &Preferences { exports_enabled: false, ..base.clone() }));
        // Boot-time: turning renders back ON needs the supervisor to spawn the child.
        let off = Preferences { exports_enabled: false, ..base.clone() };
        assert!(needs_reboot(&off, &base));
        // Boot-time: both of these are baked into config.js / the CSP header.
        assert!(needs_reboot(&base, &Preferences { plugins_enabled: false, ..base.clone() }));
        assert!(needs_reboot(&base, &Preferences { csp_enabled: false, ..base.clone() }));
    }

    #[test]
    fn no_change_never_needs_a_reboot() {
        let p = Preferences::default();
        assert!(!needs_reboot(&p, &p));
    }
}
