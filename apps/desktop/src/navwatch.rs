//! D0 spike + D1 hardening: invariant-clean observation of webview navigation.
//!
//! The webview reports navigations to us through Tauri's `on_navigation`
//! callback. Reading that callback is OUR code observing OUR window — it does
//! not patch, inject into, or drive Penpot's SPA, so invariant 3 holds.
//!
//! Two different policies apply to two different route classes (D1 decision,
//! `.superpowers/sdd/task-6-report.md`; updated by D2 and D4):
//!   * `#/auth/*`, `#/dashboard`, `#/settings` — cancelled UNCONDITIONALLY, no
//!     env var required. `#/auth` is closed because there is no second
//!     account to log into or register in a single-user offline app; this is
//!     safe for the real login path: `/__bootstrap`
//!     (`apps/desktop/src/lib.rs::bootstrap_login`) logs in server-side via
//!     RPC and 302s straight to `/__home`, never through an `#/auth/...`
//!     fragment in the webview — so unconditionally cancelling `#/auth`
//!     cannot cancel login itself. (This supersedes the original D0 WARNING
//!     that D2 must verify this before enabling it; D1 verified it by
//!     inspection of the bootstrap route above.) `#/dashboard` joined this
//!     class in D2: its replacement, `/__home` with create/rename/duplicate/
//!     move/delete, shipped earlier in this milestone, so the surface could
//!     close by default. `#/settings` joins it in D4, for the identical
//!     reason: its native replacement, the Preferences window
//!     (`apps/desktop/src/prefs_http.rs`, `/__preferences`), now exists.
//!     D1/D2/D3 all left it open specifically because that replacement
//!     didn't exist yet — see `settings_is_present_and_cancelled_in_d4`'s doc
//!     for the history this test pins.
//!
//! The gated class below is currently EMPTY (D4 was the last web route in
//! it) — kept as a mechanism, not deleted, in case a future web route needs
//! the same staged-rollout treatment `#/settings` had through D1-D3.
//!
//! Env switches:
//!   * `PENPOT_LOCAL_NAVWATCH_LOG=<path>`  — append observations as JSONL.
//!   * `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`  — cancel any route in the (now
//!     empty) gated class, on top of the always-on cancellation above.

use std::io::Write;
use std::path::PathBuf;

/// Env var: path to the JSONL observation log. Absent ⇒ no recording.
pub const ENV_LOG: &str = "PENPOT_LOCAL_NAVWATCH_LOG";
/// Env var: `1` enables the cancel+redirect policy for the (currently empty)
/// gated web-route class — see [`GATED_WEB_ROUTE_PREFIXES`]. `#/dashboard`
/// and `#/settings` both graduated out of this class (D2, D4) into the
/// always-unconditional one, so this flag no longer affects either.
pub const ENV_REDIRECT: &str = "PENPOT_LOCAL_NAVWATCH_REDIRECT";

/// Where a web route should send the user instead.
pub const HOME_PATH: &str = "/__home";

/// Hash routes cancelled UNCONDITIONALLY, no env var required — see the
/// module doc comment above for why this is safe for the real login path.
/// `#/dashboard` joined this class in D2 (`/__home` with create/rename/
/// duplicate/move/delete shipped as its replacement); `#/settings` joins in
/// D4 (`/__preferences`, `prefs_http.rs`, shipped as ITS replacement).
const ALWAYS_CANCELLED_PREFIXES: [&str; 3] = ["#/auth", "#/dashboard", "#/settings"];

/// Hash routes cancelled ONLY when `PENPOT_LOCAL_NAVWATCH_REDIRECT=1` — their
/// native replacement doesn't exist yet, so they stay open (measurement-only)
/// by default. Do NOT add to this list without a native replacement shipping
/// alongside — closing a route here is what removes the only way to reach
/// whatever it does. Empty as of D4: `#/settings` (the last entry) moved to
/// [`ALWAYS_CANCELLED_PREFIXES`] once its replacement shipped — the array (and
/// the `redirect_enabled` gate below) stay in place as a mechanism for
/// whatever future web route needs the same staged rollout.
const GATED_WEB_ROUTE_PREFIXES: [&str; 0] = [];

/// What the navigation handler should do with a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the navigation proceed untouched.
    Allow,
    /// Cancel it and send the webview to this path instead.
    CancelAndRedirect(String),
}

/// Boundary-checked prefix match: a prefix only matches on an exact hit or a
/// `/` boundary, so `#/dashboardXYZ` is NOT treated as `#/dashboard` (a plain
/// `starts_with` would false-positive on any route that merely shares a
/// prefix string, e.g. a future `#/dashboard-export` route).
fn matches_prefix(frag: &str, prefix: &str) -> bool {
    frag == prefix || frag.starts_with(&format!("{prefix}/"))
}

/// Pure policy: map a URL to a decision. Kept free of Tauri types so it is
/// unit-testable without a window.
pub fn decide(url: &str, redirect_enabled: bool) -> Decision {
    // Penpot routes on the FRAGMENT, so the decision is made on the part after
    // '#'. Anything without a fragment (our own /__ pages, /__bootstrap) is ours.
    let Some((_, frag)) = url.split_once('#') else {
        return Decision::Allow;
    };
    let frag = format!("#{frag}");

    // `#/auth/*`, `#/dashboard` and `#/settings` are closed unconditionally
    // — this class doesn't wait on the redirect env var at all.
    if ALWAYS_CANCELLED_PREFIXES.iter().any(|p| matches_prefix(&frag, p)) {
        return Decision::CancelAndRedirect(HOME_PATH.to_string());
    }

    if !redirect_enabled {
        return Decision::Allow;
    }

    if GATED_WEB_ROUTE_PREFIXES.iter().any(|p| matches_prefix(&frag, p)) {
        return Decision::CancelAndRedirect(HOME_PATH.to_string());
    }
    Decision::Allow
}

/// Observation sink. Cheap to clone; safe to call when disabled.
#[derive(Debug, Clone, Default)]
pub struct NavWatch {
    log_path: Option<PathBuf>,
    redirect: bool,
}

impl NavWatch {
    /// Read both switches from the environment.
    pub fn from_env() -> Self {
        NavWatch {
            log_path: std::env::var(ENV_LOG).ok().filter(|s| !s.is_empty()).map(PathBuf::from),
            redirect: std::env::var(ENV_REDIRECT).ok().as_deref() == Some("1"),
        }
    }

    pub fn redirect_enabled(&self) -> bool {
        self.redirect
    }

    /// Append one observation as a JSON line. Best-effort: a logging failure
    /// must never affect navigation.
    pub fn record(&self, source: &str, url: &str) {
        let Some(path) = &self.log_path else { return };
        let line = serde_json::json!({ "source": source, "url": url }).to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE D1 CONTRACT'S CENTRAL CLAIM: `#/auth/*` is cancelled even with
    /// the redirect env var DISABLED — this is the fix for the "policy is
    /// off by default" finding. Nothing in the product sets
    /// PENPOT_LOCAL_NAVWATCH_REDIRECT, so if this regressed to needing
    /// `redirect_enabled=true`, the shipped app would once again leave
    /// `#/auth/register` reachable.
    #[test]
    fn auth_family_is_cancelled_even_with_redirect_disabled() {
        for u in [
            "http://localhost:9034/#/auth",
            "http://localhost:9034/#/auth/login",
            "http://localhost:9034/#/auth/register",
        ] {
            assert_eq!(
                decide(u, false),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must be redirected unconditionally (redirect_enabled=false)"
            );
        }
    }

    /// The `#/auth` family must still be cancelled when the env var IS on —
    /// enabling the gated policy must not accidentally disable the
    /// unconditional one.
    #[test]
    fn auth_family_is_cancelled_with_redirect_enabled_too() {
        for u in [
            "http://localhost:9034/#/auth",
            "http://localhost:9034/#/auth/login",
            "http://localhost:9034/#/auth/register",
        ] {
            assert_eq!(
                decide(u, true),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must still be redirected (redirect_enabled=true)"
            );
        }
    }

    /// Flips `settings_is_allowed_with_redirect_disabled`: `#/settings` moved
    /// to the unconditional-cancel class in D4 (like `#/dashboard` did in
    /// D2 — see `dashboard_is_cancelled_even_with_redirect_disabled`), now
    /// that its native replacement (D4's Preferences window) exists.
    #[test]
    fn settings_is_cancelled_with_redirect_disabled_in_d4() {
        for u in [
            "http://localhost:9034/#/settings",
            "http://localhost:9034/#/settings/profile",
        ] {
            assert_eq!(
                decide(u, false),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must be redirected unconditionally (redirect_enabled=false)"
            );
        }
    }

    #[test]
    fn dashboard_and_settings_redirect_to_home_when_enabled() {
        // `#/dashboard` redirects unconditionally as of D2 and `#/settings`
        // as of D4, so this also holds with the env var on — both are in the
        // unconditional class now, independent of `redirect_enabled`.
        for u in [
            "http://localhost:9034/#/dashboard",
            "http://localhost:9034/#/dashboard/team/abc",
            "http://localhost:9034/#/settings/profile",
        ] {
            assert_eq!(
                decide(u, true),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must be redirected"
            );
        }
    }

    #[test]
    fn workspace_and_our_surfaces_are_never_redirected() {
        for redirect_enabled in [false, true] {
            for u in [
                "http://localhost:9034/#/workspace/p/f",
                "http://localhost:9034/__home",
                "http://localhost:9034/__search",
                "http://localhost:9034/__bootstrap",
            ] {
                assert_eq!(
                    decide(u, redirect_enabled),
                    Decision::Allow,
                    "{u} must be allowed (redirect_enabled={redirect_enabled})"
                );
            }
        }
    }

    #[test]
    fn dashboard_is_cancelled_even_with_redirect_disabled() {
        // D2: the replacement (/__home with create/rename/move/delete) now
        // exists, so the dashboard closes by default like the auth family.
        for url in [
            "http://localhost:9048/#/dashboard",
            "http://localhost:9048/#/dashboard/recent?team-id=abc",
            "http://localhost:9048/#/dashboard/fonts?team-id=abc",
        ] {
            match decide(url, false) {
                Decision::CancelAndRedirect(to) => assert!(to.ends_with(HOME_PATH), "{url} -> {to}"),
                other => panic!("{url} was not cancelled with redirect disabled: {other:?}"),
            }
        }
    }

    /// Flips `settings_is_unchanged_by_d2` — THE central pin for this task.
    /// D1/D2/D3 all left `#/settings` open by default specifically because
    /// its native replacement didn't exist yet (closing a surface before its
    /// replacement exists is exactly the mistake D1 avoided for `#/auth`).
    /// That replacement — D4's Preferences window (`/__preferences`,
    /// `apps/desktop/src/prefs_http.rs`) — now exists, so `#/settings`
    /// closes unconditionally, exactly like `#/auth` and `#/dashboard`: the
    /// `redirect_enabled` flag no longer makes any difference to it.
    #[test]
    fn settings_is_present_and_cancelled_in_d4() {
        for redirect_enabled in [false, true] {
            assert!(
                matches!(
                    decide("http://localhost:9048/#/settings/profile", redirect_enabled),
                    Decision::CancelAndRedirect(_)
                ),
                "#/settings must be cancelled regardless of redirect_enabled={redirect_enabled}"
            );
        }
    }

    #[test]
    fn dashboard_prefix_boundary_still_holds() {
        // "#/dashboardx" must not be treated as the dashboard.
        assert!(matches!(decide("http://localhost:9048/#/dashboardx", false), Decision::Allow));
    }

    /// Flips `redirect_disabled_still_allows_settings`: `#/settings` is no
    /// longer part of the gated class at all as of D4 — `#/auth`,
    /// `#/dashboard` and `#/settings` are ALL unconditional now, and the
    /// gated class ([`GATED_WEB_ROUTE_PREFIXES`]) is empty.
    #[test]
    fn redirect_disabled_still_cancels_settings() {
        assert_eq!(
            decide("http://localhost:9034/#/settings", false),
            Decision::CancelAndRedirect("/__home".to_string())
        );
    }

    #[test]
    fn prefix_match_has_boundary_check() {
        // A route that merely shares a string prefix with a web route must
        // NOT be treated as that web route — only an exact match or a `/`
        // boundary counts. Checked against both redirect states since
        // `#/auth`'s boundary check is independent of the env var.
        for redirect_enabled in [false, true] {
            for u in [
                "http://localhost:9034/#/dashboardXYZ",
                "http://localhost:9034/#/authenticate",
                "http://localhost:9034/#/authoring",
                // D4: the same boundary discipline must hold for the newly
                // unconditional `#/settings` — a naive `starts_with` would
                // wrongly cancel this.
                "http://localhost:9034/#/settingsXYZ",
            ] {
                assert_eq!(
                    decide(u, redirect_enabled),
                    Decision::Allow,
                    "{u} must NOT be redirected (no boundary match, redirect_enabled={redirect_enabled})"
                );
            }
        }
    }

    #[test]
    fn prefix_match_still_redirects_exact_and_subpath() {
        // The boundary fix must not regress the real cases: an exact prefix
        // match, or a prefix followed by a `/` subpath, still redirects.
        for u in [
            "http://localhost:9034/#/dashboard",
            "http://localhost:9034/#/dashboard/team/x",
            "http://localhost:9034/#/auth/login",
            "http://localhost:9034/#/settings",
            "http://localhost:9034/#/settings/profile",
        ] {
            assert_eq!(
                decide(u, true),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must still be redirected"
            );
        }
    }
}
