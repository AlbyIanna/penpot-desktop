//! D0 spike: invariant-clean observation of webview navigation.
//!
//! The webview reports navigations to us through Tauri's `on_navigation`
//! callback. Reading that callback is OUR code observing OUR window — it does
//! not patch, inject into, or drive Penpot's SPA, so invariant 3 holds.
//!
//! Both behaviours are env-gated and OFF by default, so merging this changes
//! nothing in a normal run:
//!   * `PENPOT_LOCAL_NAVWATCH_LOG=<path>`  — append observations as JSONL.
//!   * `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`  — cancel web routes and go `/__home`.

use std::io::Write;
use std::path::PathBuf;

/// Env var: path to the JSONL observation log. Absent ⇒ no recording.
pub const ENV_LOG: &str = "PENPOT_LOCAL_NAVWATCH_LOG";
/// Env var: `1` enables the cancel+redirect policy.
pub const ENV_REDIRECT: &str = "PENPOT_LOCAL_NAVWATCH_REDIRECT";

/// Where a web route should send the user instead.
pub const HOME_PATH: &str = "/__home";

/// Hash routes that belong to the logged-in web experience.
const WEB_ROUTE_PREFIXES: [&str; 3] = ["#/dashboard", "#/settings", "#/auth"];

/// What the navigation handler should do with a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the navigation proceed untouched.
    Allow,
    /// Cancel it and send the webview to this path instead.
    CancelAndRedirect(String),
}

/// Pure policy: map a URL to a decision. Kept free of Tauri types so it is
/// unit-testable without a window.
pub fn decide(url: &str, redirect_enabled: bool) -> Decision {
    if !redirect_enabled {
        return Decision::Allow;
    }
    // Penpot routes on the FRAGMENT, so the decision is made on the part after
    // '#'. Anything without a fragment (our own /__ pages, /__bootstrap) is ours.
    let Some((_, frag)) = url.split_once('#') else {
        return Decision::Allow;
    };
    let frag = format!("#{frag}");
    if WEB_ROUTE_PREFIXES.iter().any(|p| frag.starts_with(p)) {
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

    #[test]
    fn web_routes_redirect_to_home_when_enabled() {
        for u in [
            "http://localhost:9034/#/dashboard",
            "http://localhost:9034/#/dashboard/team/abc",
            "http://localhost:9034/#/settings/profile",
            "http://localhost:9034/#/auth/login",
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
        for u in [
            "http://localhost:9034/#/workspace/p/f",
            "http://localhost:9034/__home",
            "http://localhost:9034/__search",
            "http://localhost:9034/__bootstrap",
        ] {
            assert_eq!(decide(u, true), Decision::Allow, "{u} must be allowed");
        }
    }

    #[test]
    fn redirect_disabled_allows_everything() {
        // Default production behaviour: observe only, change nothing.
        assert_eq!(decide("http://localhost:9034/#/dashboard", false), Decision::Allow);
    }
}
