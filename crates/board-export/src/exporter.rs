//! HTTP client for the `penpot-exporter` service — the exact wire contract
//! verified in the M5 spike against `penpotapp/exporter:2.16.2`:
//!
//! - `POST <exporter>/api/export` with `Content-Type:
//!   application/transit+json` and **cookie auth** (`Cookie:
//!   auth-token=<session>`). Personal access tokens DO NOT work here: the
//!   exporter forwards the cookie to its headless browser for `render.html`,
//!   which only authenticates via the session cookie. A wrong/missing cookie
//!   surfaces as a 500 after a ~10 s in-browser locator timeout.
//! - Body: transit-json verbose, one export entry per request with
//!   `"~:wait": true` → the response is synchronous.
//!   `::suffix`/`::scale`/`::name` are spec-required on every entry and
//!   `::profile-id` at the top level (400 otherwise); uuids are `"~u<uuid>"`.
//! - Response: transit+json object whose `~:uri` member holds the artifact
//!   URI (`{"~#uri": "http://<public-uri>/assets/by-id/<id>"}`); GET that
//!   URI **with the same cookie** (401 without) → the rendered bytes,
//!   served through the app's own X-Accel `/assets` proxy path.

use serde_json::{json, Value};

/// Rendered output formats supported per board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Svg,
    Png,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Svg => "svg",
            Format::Png => "png",
        }
    }

    fn transit_type(self) -> &'static str {
        match self {
            Format::Svg => "~:svg",
            Format::Png => "~:png",
        }
    }
}

/// Errors from one render round-trip, classified for retry.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// Connection-level failure (exporter/proxy restarting) — transient.
    #[error("transport: {0}")]
    Transport(reqwest::Error),
    /// Non-2xx from the exporter or the artifact download. 5xx is transient
    /// (exporter busy/restarting, or an in-browser auth/render timeout);
    /// 4xx is permanent (bad request shape).
    #[error("http {status}: {body}")]
    Status { status: u16, body: String },
    /// Unexpected response shape — permanent.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl RenderError {
    pub fn is_transient(&self) -> bool {
        match self {
            RenderError::Transport(_) => true,
            RenderError::Status { status, .. } => *status >= 500,
            RenderError::Protocol(_) => false,
        }
    }
}

/// Build the transit-json body for a single-board export (spike-verified
/// shape). Pure → unit-tested against the spike's captured payload.
pub fn export_payload(
    profile_id: &str,
    file_id: &str,
    page_id: &str,
    object_id: &str,
    name: &str,
    format: Format,
) -> String {
    let body = json!({
        "~:cmd": "~:export-shapes",
        "~:profile-id": format!("~u{profile_id}"),
        "~:wait": true,
        "~:exports": [{
            "~:file-id": format!("~u{file_id}"),
            "~:page-id": format!("~u{page_id}"),
            "~:object-id": format!("~u{object_id}"),
            "~:type": format.transit_type(),
            "~:suffix": "",
            "~:scale": 1,
            "~:name": name,
        }],
    });
    body.to_string()
}

/// Extract the artifact URI from the exporter's transit+json response.
pub fn artifact_uri(response: &Value) -> Result<String, RenderError> {
    response["~:uri"]["~#uri"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            RenderError::Protocol(format!("no ~:uri/~#uri in exporter response: {response}"))
        })
}

/// Thin async client: one `render` = POST /api/export + artifact GET.
pub struct ExporterClient {
    http: reqwest::Client,
    /// e.g. `http://127.0.0.1:6467` (no trailing slash).
    base: String,
}

impl ExporterClient {
    pub fn new(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        ExporterClient {
            // Loopback-only traffic: keep env proxies out (same rationale as
            // PenpotClient — a configured corporate proxy must not hijack
            // localhost requests).
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("building a loopback-only reqwest client cannot fail"),
            base,
        }
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, RenderError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(RenderError::Status { status: status.as_u16(), body })
    }

    /// Render one board: returns the raw SVG/PNG bytes. `session_cookie` is
    /// the `auth-token` cookie VALUE (from `login-with-password`).
    pub async fn render(
        &self,
        session_cookie: &str,
        payload: &str,
    ) -> Result<Vec<u8>, RenderError> {
        let resp = self
            .http
            .post(format!("{}/api/export", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/transit+json")
            .header(reqwest::header::COOKIE, format!("auth-token={session_cookie}"))
            .body(payload.to_string())
            .send()
            .await
            .map_err(RenderError::Transport)?;
        let resp = Self::check(resp).await?;
        let body: Value = resp.json().await.map_err(RenderError::Transport)?;
        let uri = artifact_uri(&body)?;

        // Artifact download leg: same cookie (401 without), goes through the
        // app's proxy /assets X-Accel path.
        let resp = self
            .http
            .get(&uri)
            .header(reqwest::header::COOKIE, format!("auth-token={session_cookie}"))
            .send()
            .await
            .map_err(RenderError::Transport)?;
        let resp = Self::check(resp).await?;
        let bytes = resp.bytes().await.map_err(RenderError::Transport)?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_matches_the_spike_capture() {
        // Reference: scratchpad m5-spike/export-svg.transit.json (verified
        // live against exporter 2.16.2).
        let payload = export_payload(
            "40e3ee1b-f830-81ff-8008-528b0b80506d",
            "40e3ee1b-f830-81ff-8008-528b4cbec23b",
            "40e3ee1b-f830-81ff-8008-528b4cbec23c",
            "a226083b-baec-4e5d-aa68-1dd2c1548a92",
            "M5 Board",
            Format::Svg,
        );
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["~:cmd"], "~:export-shapes");
        assert_eq!(v["~:profile-id"], "~u40e3ee1b-f830-81ff-8008-528b0b80506d");
        assert_eq!(v["~:wait"], true);
        let e = &v["~:exports"][0];
        assert_eq!(e["~:file-id"], "~u40e3ee1b-f830-81ff-8008-528b4cbec23b");
        assert_eq!(e["~:page-id"], "~u40e3ee1b-f830-81ff-8008-528b4cbec23c");
        assert_eq!(e["~:object-id"], "~ua226083b-baec-4e5d-aa68-1dd2c1548a92");
        assert_eq!(e["~:type"], "~:svg");
        // Spec-required members (400 when missing — spike trap 6).
        assert_eq!(e["~:suffix"], "");
        assert_eq!(e["~:scale"], 1);
        assert_eq!(e["~:name"], "M5 Board");
        assert_eq!(v["~:exports"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn png_payload_type() {
        let payload = export_payload("p", "f", "pg", "o", "n", Format::Png);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["~:exports"][0]["~:type"], "~:png");
    }

    #[test]
    fn artifact_uri_parses_the_spike_response() {
        let resp: Value = serde_json::from_str(
            r#"{"~:mtype":"image/svg+xml","~:name":"M5 Board","~:filename":"M5 Board.svg","~:id":"~u0bbedaf9","~:uri":{"~#uri":"http://localhost:8910/assets/by-id/cb666ddc"}}"#,
        )
        .unwrap();
        assert_eq!(
            artifact_uri(&resp).unwrap(),
            "http://localhost:8910/assets/by-id/cb666ddc"
        );
        assert!(artifact_uri(&serde_json::json!({})).is_err());
    }

    #[test]
    fn transience_classification() {
        assert!(RenderError::Status { status: 500, body: String::new() }.is_transient());
        assert!(RenderError::Status { status: 502, body: String::new() }.is_transient());
        assert!(!RenderError::Status { status: 400, body: String::new() }.is_transient());
        assert!(!RenderError::Protocol("x".into()).is_transient());
    }
}
