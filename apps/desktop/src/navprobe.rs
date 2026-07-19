//! D0 spike: our own navigation test page.
//!
//! Isolates the mechanism question — "does the webview report a same-document
//! hash change?" — on a page WE control. Penpot's SPA is never involved, so
//! invariant 3 (the SPA stays byte-untouched) is not in tension here.
//!
//! Not reachable in normal use: nothing links to `/__navprobe`.

use axum::{response::Html, routing::get, Router};

/// The probe page, compiled in (mirrors the `include_str!` pattern used by
/// `home.rs` / `http.rs` for `/__home` and `/__search`).
pub const PROBE_PAGE_HTML: &str = include_str!("navprobe.html");

async fn probe_page() -> Html<&'static str> {
    Html(PROBE_PAGE_HTML)
}

/// Router merged into the proxy's extra router.
pub fn router() -> Router {
    Router::new().route("/__navprobe", get(probe_page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_declares_all_three_navigation_cases() {
        assert!(PROBE_PAGE_HTML.contains("location.hash"), "hash-change case");
        assert!(PROBE_PAGE_HTML.contains("history.pushState"), "pushState case");
        assert!(PROBE_PAGE_HTML.contains("location.assign"), "full-navigation case");
    }

    #[test]
    fn page_marks_completion_in_the_title() {
        assert!(PROBE_PAGE_HTML.contains("navprobe:"), "completion marker");
    }
}
