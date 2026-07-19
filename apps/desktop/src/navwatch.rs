//! D0 spike + D1 hardening: invariant-clean observation of webview navigation.
//!
//! The webview reports navigations to us through Tauri's `on_navigation`
//! callback. Reading that callback is OUR code observing OUR window — it does
//! not patch, inject into, or drive Penpot's SPA, so invariant 3 holds.
//!
//! Two different policies apply to two different route classes (D1 decision,
//! `.superpowers/sdd/task-6-report.md`):
//!   * `#/auth/*` — cancelled UNCONDITIONALLY, no env var required. There is
//!     no second account to log into or register in a single-user offline
//!     app, so these routes are always closed. This is safe for the real
//!     login path: `/__bootstrap` (`apps/desktop/src/lib.rs::bootstrap_login`)
//!     logs in server-side via RPC and 302s straight to `/__home`, never
//!     through an `#/auth/...` fragment in the webview — so unconditionally
//!     cancelling `#/auth` cannot cancel login itself. (This supersedes the
//!     original D0 WARNING that D2 must verify this before enabling it; D1
//!     verified it by inspection of the bootstrap route above.)
//!   * `#/dashboard`, `#/settings` — measurement-only by default, exactly as
//!     D0 shipped them; still gated behind `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`.
//!     Their native replacement does not exist yet (D2/D3 build it), so
//!     closing them now would remove the only way to browse files.
//!
//! Env switches:
//!   * `PENPOT_LOCAL_NAVWATCH_LOG=<path>`  — append observations as JSONL.
//!   * `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`  — cancel `#/dashboard`/`#/settings`
//!     too, on top of the always-on `#/auth` cancellation above.

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
const ALWAYS_CANCELLED_PREFIX: &str = "#/auth";

/// Hash routes cancelled ONLY when `PENPOT_LOCAL_NAVWATCH_REDIRECT=1` — their
/// native replacement doesn't exist yet, so they stay open (measurement-only)
/// by default. Do NOT add to this list without a native replacement shipping
/// alongside — closing these is what removes the only way to browse files.
const GATED_WEB_ROUTE_PREFIXES: [&str; 2] = ["#/dashboard", "#/settings"];

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

    // `#/auth/*` is closed unconditionally — this class doesn't wait on the
    // redirect env var at all.
    if matches_prefix(&frag, ALWAYS_CANCELLED_PREFIX) {
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

    /// `#/dashboard` and `#/settings` are UNCHANGED from D0: allowed by
    /// default (no native replacement exists yet — D2/D3 build it), cancelled
    /// only when the env var opts in.
    #[test]
    fn dashboard_and_settings_are_allowed_with_redirect_disabled() {
        for u in [
            "http://localhost:9034/#/dashboard",
            "http://localhost:9034/#/dashboard/team/abc",
            "http://localhost:9034/#/settings",
            "http://localhost:9034/#/settings/profile",
        ] {
            assert_eq!(decide(u, false), Decision::Allow, "{u} must be allowed by default");
        }
    }

    #[test]
    fn dashboard_and_settings_redirect_to_home_when_enabled() {
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
    fn redirect_disabled_still_allows_dashboard_and_settings() {
        // Default production behaviour for the gated class: observe only,
        // change nothing. (#/auth is covered separately above — it is NOT
        // part of "everything" any more.)
        assert_eq!(decide("http://localhost:9034/#/dashboard", false), Decision::Allow);
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
