//! D0 spike + D1 hardening: invariant-clean observation of webview navigation.
//!
//! The webview reports navigations to us through Tauri's `on_navigation`
//! callback. Reading that callback is OUR code observing OUR window — it does
//! not patch, inject into, or drive Penpot's SPA, so invariant 3 holds.
//!
//! Two different policies apply to two different route classes (D1 decision,
//! `.superpowers/sdd/task-6-report.md`; updated by D2):
//!   * `#/auth/*`, `#/dashboard` — cancelled UNCONDITIONALLY, no env var
//!     required. `#/auth` is closed because there is no second account to
//!     log into or register in a single-user offline app; this is safe for
//!     the real login path: `/__bootstrap`
//!     (`apps/desktop/src/lib.rs::bootstrap_login`) logs in server-side via
//!     RPC and 302s straight to `/__home`, never through an `#/auth/...`
//!     fragment in the webview — so unconditionally cancelling `#/auth`
//!     cannot cancel login itself. (This supersedes the original D0 WARNING
//!     that D2 must verify this before enabling it; D1 verified it by
//!     inspection of the bootstrap route above.) `#/dashboard` joined this
//!     class in D2: its replacement, `/__home` with create/rename/duplicate/
//!     move/delete, shipped earlier in this milestone, so the surface can
//!     close by default.
//!   * `#/settings` — still measurement-only by default, exactly as D0
//!     shipped it; gated behind `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`. Its
//!     native replacement (D4's Preferences) does not exist yet, so closing
//!     it now would remove the only way to reach account/workspace settings
//!     — the same mistake D1 already avoided once.
//!
//! Env switches:
//!   * `PENPOT_LOCAL_NAVWATCH_LOG=<path>`  — append observations as JSONL.
//!   * `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`  — cancel `#/settings` too, on top
//!     of the always-on `#/auth`/`#/dashboard` cancellation above.

use std::io::Write;
use std::path::PathBuf;

/// Env var: path to the JSONL observation log. Absent ⇒ no recording.
pub const ENV_LOG: &str = "PENPOT_LOCAL_NAVWATCH_LOG";
/// Env var: `1` enables the cancel+redirect policy for `#/dashboard`/`#/settings`.
pub const ENV_REDIRECT: &str = "PENPOT_LOCAL_NAVWATCH_REDIRECT";

/// Where a web route should send the user instead.
pub const HOME_PATH: &str = "/__home";

/// Hash routes cancelled UNCONDITIONALLY, no env var required — see the
/// module doc comment above for why this is safe for the real login path.
/// `#/dashboard` joined this class in D2: its replacement (`/__home` with
/// create/rename/duplicate/move/delete) shipped earlier in this milestone.
const ALWAYS_CANCELLED_PREFIXES: [&str; 2] = ["#/auth", "#/dashboard"];

/// Hash routes cancelled ONLY when `PENPOT_LOCAL_NAVWATCH_REDIRECT=1` — their
/// native replacement doesn't exist yet, so they stay open (measurement-only)
/// by default. Do NOT add to this list without a native replacement shipping
/// alongside — closing these is what removes the only way to browse files.
/// `#/settings`'s replacement is D4's native Preferences.
const GATED_WEB_ROUTE_PREFIXES: [&str; 1] = ["#/settings"];

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

    // `#/auth/*` and `#/dashboard` are closed unconditionally — this class
    // doesn't wait on the redirect env var at all.
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

    /// `#/settings` is UNCHANGED from D0: allowed by default (its native
    /// replacement, D4's Preferences, does not exist yet), cancelled only
    /// when the env var opts in. `#/dashboard` moved to the unconditional
    /// class in D2 — see `dashboard_is_cancelled_even_with_redirect_disabled`.
    #[test]
    fn settings_is_allowed_with_redirect_disabled() {
        for u in [
            "http://localhost:9034/#/settings",
            "http://localhost:9034/#/settings/profile",
        ] {
            assert_eq!(decide(u, false), Decision::Allow, "{u} must be allowed by default");
        }
    }

    #[test]
    fn dashboard_and_settings_redirect_to_home_when_enabled() {
        // `#/dashboard` redirects unconditionally as of D2, so this also
        // holds with the env var on; `#/settings` still needs it enabled.
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

    #[test]
    fn settings_is_unchanged_by_d2() {
        // Its replacement is D4's native Preferences. Closing a surface before
        // its replacement exists is exactly the mistake D1 avoided.
        assert!(matches!(decide("http://localhost:9048/#/settings/profile", false), Decision::Allow));
        assert!(matches!(
            decide("http://localhost:9048/#/settings/profile", true),
            Decision::CancelAndRedirect(_)
        ));
    }

    #[test]
    fn dashboard_prefix_boundary_still_holds() {
        // "#/dashboardx" must not be treated as the dashboard.
        assert!(matches!(decide("http://localhost:9048/#/dashboardx", false), Decision::Allow));
    }

    #[test]
    fn redirect_disabled_still_allows_settings() {
        // Default production behaviour for the still-gated class: observe
        // only, change nothing. (#/auth and #/dashboard are covered
        // separately above — they are NOT part of the gated class any more.)
        assert_eq!(decide("http://localhost:9034/#/settings", false), Decision::Allow);
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
        ] {
            assert_eq!(
                decide(u, true),
                Decision::CancelAndRedirect("/__home".to_string()),
                "{u} must still be redirected"
            );
        }
    }
}
