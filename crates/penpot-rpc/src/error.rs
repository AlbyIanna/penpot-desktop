//! Error type for the Penpot RPC client.

/// Errors produced by [`crate::PenpotClient`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure (connection refused, timeout, invalid URL, ...).
    #[error("http transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The backend answered with a non-2xx status. Penpot validation errors
    /// are HTTP 400 JSON bodies with `type`/`code`/`explain` (verified in M0);
    /// wrong credentials are `400` with `code: wrong-credentials`, not 401.
    #[error("penpot rpc error (http {status}) type={error_type:?} code={code:?}")]
    Rpc {
        status: u16,
        /// The `type` field of the error body, when the body was JSON.
        error_type: Option<String>,
        /// The `code` field of the error body, when the body was JSON.
        code: Option<String>,
        /// Full response body (JSON value, or a JSON string of the raw text).
        body: serde_json::Value,
    },

    /// A 2xx response that does not match the wire shape documented in
    /// docs/m0/rpc-endpoints.md (missing cookie, malformed SSE, bad JSON, ...).
    #[error("unexpected response from penpot: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The Penpot error `code` (e.g. `wrong-credentials`), if this is an RPC error.
    pub fn rpc_code(&self) -> Option<&str> {
        match self {
            Error::Rpc { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}
