//! Local HTTP proxy replacing Penpot's upstream nginx (Milestone M1).
//!
//! Penpot's stock deployment puts an nginx in front of the backend; this crate
//! reimplements the parts of that nginx config the desktop app needs
//! (reference behavior characterized live in `docs/m0/asset-serving.md`):
//!
//! - **Static SPA** — serves the upstream frontend build from
//!   [`ProxyConfig::static_dir`], with an `index.html` fallback so SPA routes
//!   (e.g. `/#/workspace/...`, `/dashboard`) resolve.
//! - **`/api/**`** — transparent reverse proxy to the Penpot backend
//!   ([`ProxyConfig::backend_addr`]): method, headers (incl. `Cookie` /
//!   `Authorization`) and bodies are streamed through unchanged in both
//!   directions. Exception: `/api/export` belongs to the separate
//!   `penpot-exporter` service — when [`ProxyConfig::exporter_addr`] is set
//!   (M5, exporter child running) it is proxied there (cookie passthrough
//!   included, exactly like upstream nginx's `location /api/export`);
//!   otherwise it answers `502 Bad Gateway` with a clear message instead of
//!   hanging.
//! - **`/ws/notifications`** — WebSocket proxy: the HTTP/1.1 upgrade is
//!   forwarded to the backend and, once both sides switch protocols, bytes are
//!   tunneled verbatim in both directions (no frame re-encoding).
//! - **`/assets/**`** — X-Accel-Redirect handling (PLAN.md risk 6). The
//!   request is forwarded to the backend *with* auth headers; when the backend
//!   answers `204 No Content` + `x-accel-redirect: <accel_prefix><relpath>`
//!   (the `fs` storage backend always does), the proxy strips
//!   [`ProxyConfig::accel_prefix`], resolves `<relpath>` under
//!   [`ProxyConfig::storage_dir`] (rejecting path traversal with `403`), and
//!   serves the file itself with the 204's `content-type` and `cache-control`
//!   copied onto the response, a correct `Content-Length`,
//!   `Accept-Ranges: bytes`, and single-range `Range` support (`206` /
//!   `416`). Non-204 backend responses (`404`, `401` for private buckets)
//!   pass through unchanged.
//! - **`/internal/assets/**`** is *not* externally routable (`404`), matching
//!   nginx's `internal` location.
//!
//! # Public API
//!
//! ```no_run
//! use proxy::{Proxy, ProxyConfig};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let config = ProxyConfig {
//!     listen_addr: "127.0.0.1:8686".parse()?,   // default
//!     backend_addr: "127.0.0.1:6161".parse()?,  // default
//!     static_dir: "/path/to/penpot-frontend".into(),
//!     storage_dir: "/path/to/assets".into(),    // PENPOT_OBJECTS_STORAGE_FS_DIRECTORY
//!     accel_prefix: "/internal/assets/".into(), // default; = PENPOT_ASSETS_PATH
//!     exporter_addr: None,                      // default; Some(addr) proxies /api/export
//!     html_csp: None,                           // default; Some(v) adds a CSP header on text/html
//! };
//! let proxy = Proxy::bind(config).await?;      // binds the listener
//! let addr = proxy.local_addr();               // real port (use port 0 for ephemeral)
//! proxy.serve().await?;                        // runs until the task is dropped/aborted
//! # Ok(()) }
//! ```
//!
//! For coordinated shutdown use [`Proxy::serve_with_shutdown`] with any
//! future (e.g. a `tokio::sync::oneshot` receiver).

use std::future::Future;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, StatusCode, Uri};
use http_body_util::Empty;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::io::ReaderStream;
use tower_http::services::{ServeDir, ServeFile};

/// Configuration for the local proxy. All fields are public; construct with
/// struct literal syntax or start from [`ProxyConfig::new`] and override.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to listen on. Default `127.0.0.1:8686`. Use port `0` for an
    /// OS-assigned port (read it back via [`Proxy::local_addr`]).
    pub listen_addr: SocketAddr,
    /// Penpot backend address. Default `127.0.0.1:6161`.
    pub backend_addr: SocketAddr,
    /// Directory containing the static frontend build (upstream
    /// `penpotapp/frontend` `/var/www/app`).
    pub static_dir: PathBuf,
    /// Objects-storage directory (`PENPOT_OBJECTS_STORAGE_FS_DIRECTORY`);
    /// `x-accel-redirect` paths resolve under this directory.
    pub storage_dir: PathBuf,
    /// The internal-redirect prefix the backend puts in `x-accel-redirect`
    /// (`PENPOT_ASSETS_PATH`). Default `/internal/assets/`. A trailing `/`
    /// is appended if missing.
    pub accel_prefix: String,
    /// `penpot-exporter` service address. `None` (the default) keeps the
    /// `502` stub on `/api/export`; `Some(addr)` reverse-proxies
    /// `/api/export/**` there (the upstream nginx `location /api/export`
    /// equivalent — body streamed, cookies passed through).
    pub exporter_addr: Option<SocketAddr>,
    /// E7 (CSP-GO): when set, this exact value is added as a
    /// `Content-Security-Policy` RESPONSE HEADER on every `text/html`
    /// response the proxy serves (the SPA document, `/__home`, plugin UI
    /// pages...). Penpot evaluates plugin code in a SES Compartment inside
    /// the SPA page context, so the SPA DOCUMENT's CSP is what governs plugin
    /// egress (proven live in the E7 spike: `connect-src 'self'` blocks an
    /// off-origin `fetch()` while the plugin still loads). A pure proxy-layer
    /// header — the served bytes are untouched (invariant 3). `None` adds
    /// nothing (the desktop boot defaults it ON; gates opt out per leg).
    pub html_csp: Option<String>,
}

/// Default proxy listen port (see docs/milestones/m1.md port conventions).
pub const DEFAULT_LISTEN_PORT: u16 = 8686;
/// Default Penpot backend port (see docs/milestones/m1.md port conventions).
pub const DEFAULT_BACKEND_PORT: u16 = 6161;
/// Penpot's default `PENPOT_ASSETS_PATH`.
pub const DEFAULT_ACCEL_PREFIX: &str = "/internal/assets/";

impl ProxyConfig {
    /// Config with default ports/prefix and the two required directories.
    pub fn new(static_dir: impl Into<PathBuf>, storage_dir: impl Into<PathBuf>) -> Self {
        Self {
            listen_addr: SocketAddr::from(([127, 0, 0, 1], DEFAULT_LISTEN_PORT)),
            backend_addr: SocketAddr::from(([127, 0, 0, 1], DEFAULT_BACKEND_PORT)),
            static_dir: static_dir.into(),
            storage_dir: storage_dir.into(),
            accel_prefix: DEFAULT_ACCEL_PREFIX.to_owned(),
            exporter_addr: None,
            html_csp: None,
        }
    }
}

/// A bound-but-not-yet-serving proxy. Created by [`Proxy::bind`]; consumed by
/// [`Proxy::serve`] / [`Proxy::serve_with_shutdown`].
pub struct Proxy {
    listener: TcpListener,
    router: Router,
    local_addr: SocketAddr,
}

impl Proxy {
    /// Bind the listener and build the router. Fails if the address is taken.
    pub async fn bind(config: ProxyConfig) -> anyhow::Result<Self> {
        Self::bind_with_router(config, Router::new()).await
    }

    /// Like [`Proxy::bind`], but merges `extra` (app-provided routes such as
    /// `/__bootstrap` or an overridden `/js/config.js`) into the router.
    /// Extra routes take precedence over the static-SPA fallback but must not
    /// collide with the proxy's own routes (`/api`, `/assets`, ...).
    pub async fn bind_with_router(config: ProxyConfig, extra: Router) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(config.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        let mut accel_prefix = config.accel_prefix;
        if !accel_prefix.ends_with('/') {
            accel_prefix.push('/');
        }
        let state = AppState {
            client: Client::builder(TokioExecutor::new()).build_http(),
            backend_addr: config.backend_addr,
            storage_dir: config.storage_dir,
            accel_prefix,
            exporter_addr: config.exporter_addr,
        };
        let mut router = build_router(state, &config.static_dir).merge(extra);
        // E7: add the configured Content-Security-Policy response header on
        // text/html responses only (SPA document + any served HTML — the
        // contexts scripts execute in). Header-only — no served byte changes
        // (invariant 3). Layered AFTER the merge so it also covers the extra
        // router's pages (/__home, /__packages plugin UI documents...).
        if let Some(csp) = config.html_csp.clone() {
            router = router.layer(axum::middleware::map_response(
                move |mut res: Response| {
                    let csp = csp.clone();
                    async move {
                        let is_html = res
                            .headers()
                            .get(header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(|v| v.starts_with("text/html"))
                            .unwrap_or(false);
                        if is_html {
                            if let Ok(hv) = HeaderValue::from_str(&csp) {
                                res.headers_mut()
                                    .insert(header::CONTENT_SECURITY_POLICY, hv);
                            }
                        }
                        res
                    }
                },
            ));
        }
        tracing::info!(%local_addr, backend = %config.backend_addr, "proxy bound");
        Ok(Self { listener, router, local_addr })
    }

    /// The actual bound address (useful with an ephemeral `listen_addr` port).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve forever (until the enclosing task is aborted or the process exits).
    pub async fn serve(self) -> anyhow::Result<()> {
        axum::serve(self.listener, self.router).await?;
        Ok(())
    }

    /// Serve until `signal` resolves, then stop accepting and drain gracefully.
    pub async fn serve_with_shutdown(
        self,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(signal)
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    client: Client<HttpConnector, Body>,
    backend_addr: SocketAddr,
    storage_dir: PathBuf,
    accel_prefix: String,
    exporter_addr: Option<SocketAddr>,
}

fn build_router(state: AppState, static_dir: &Path) -> Router {
    let spa = ServeDir::new(static_dir)
        .fallback(ServeFile::new(static_dir.join("index.html")));
    Router::new()
        // /api/export is the penpot-exporter service: proxied when the
        // exporter child runs (M5), a clear 502 otherwise.
        .route("/api/export", any(export_proxy))
        .route("/api/export/{*rest}", any(export_proxy))
        .route("/api", any(api_proxy))
        .route("/api/{*rest}", any(api_proxy))
        .route("/ws/notifications", any(ws_proxy))
        .route("/assets", any(asset_proxy))
        .route("/assets/{*rest}", any(asset_proxy))
        // nginx marks this location `internal`; it must never be reachable
        // from outside (and must not fall through to the SPA handler).
        .route("/internal/assets", any(internal_blocked))
        .route("/internal/assets/{*rest}", any(internal_blocked))
        .fallback_service(spa)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// /api — plain reverse proxy
// ---------------------------------------------------------------------------

async fn api_proxy(State(state): State<AppState>, req: Request) -> Response {
    match forward_to_backend(&state, req).await {
        Ok(resp) => passthrough_response(resp),
        Err(resp) => resp,
    }
}

/// `/api/export` → the penpot-exporter service (when configured). The
/// exporter ignores the request path (only `/readyz` is special), so plain
/// forwarding with cookie passthrough is exactly what upstream nginx does.
async fn export_proxy(State(state): State<AppState>, req: Request) -> Response {
    let Some(exporter_addr) = state.exporter_addr else {
        return (
            StatusCode::BAD_GATEWAY,
            "export service unavailable: the penpot-exporter is not running in this build \
             (enable it with PENPOT_LOCAL_EXPORTS=1)",
        )
            .into_response();
    };
    match forward_to(&state, exporter_addr, req).await {
        Ok(resp) => passthrough_response(resp),
        Err(resp) => resp,
    }
}

async fn internal_blocked() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// Rewrite the request URI onto the backend and send it, streaming the body.
/// On failure returns a ready-made `502` response.
async fn forward_to_backend(
    state: &AppState,
    req: Request,
) -> Result<http::Response<hyper::body::Incoming>, Response> {
    forward_to(state, state.backend_addr, req).await
}

/// Rewrite the request URI onto `upstream` and send it, streaming the body.
/// On failure returns a ready-made `502` response.
async fn forward_to(
    state: &AppState,
    upstream: SocketAddr,
    mut req: Request,
) -> Result<http::Response<hyper::body::Incoming>, Response> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let uri: Uri = format!("http://{upstream}{path_and_query}")
        .parse()
        .map_err(|e| bad_gateway(format!("bad upstream uri: {e}")))?;
    *req.uri_mut() = uri;
    strip_hop_by_hop(req.headers_mut());
    state
        .client
        .request(req)
        .await
        .map_err(|e| bad_gateway(format!("upstream {upstream} unreachable: {e}")))
}

/// Convert a hyper response into an axum response, streaming the body.
fn passthrough_response(resp: http::Response<hyper::body::Incoming>) -> Response {
    let mut resp = resp.map(Body::new);
    // hyper re-frames the message itself; stale framing headers would lie.
    resp.headers_mut().remove(header::CONNECTION);
    resp.headers_mut().remove(header::TRANSFER_ENCODING);
    resp.headers_mut().remove("keep-alive");
    resp
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for h in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(&h);
    }
    headers.remove("keep-alive");
}

fn bad_gateway(msg: String) -> Response {
    tracing::warn!("proxy 502: {msg}");
    (StatusCode::BAD_GATEWAY, msg).into_response()
}

// ---------------------------------------------------------------------------
// /ws/notifications — WebSocket tunnel
// ---------------------------------------------------------------------------

/// Proxy a WebSocket upgrade: forward the client's handshake to the backend,
/// mirror the backend's `101` back to the client, then copy raw bytes in both
/// directions. No frame parsing — the tunnel is protocol-transparent, so
/// cookies, subprotocols and extensions all pass through untouched.
async fn ws_proxy(State(state): State<AppState>, req: Request) -> Response {
    let is_ws_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if !is_ws_upgrade {
        return (StatusCode::BAD_REQUEST, "expected a websocket upgrade request").into_response();
    }

    // Dial the backend and perform the same handshake there.
    let stream = match TcpStream::connect(state.backend_addr).await {
        Ok(s) => s,
        Err(e) => return bad_gateway(format!("backend unreachable: {e}")),
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(pair) => pair,
        Err(e) => return bad_gateway(format!("backend handshake failed: {e}")),
    };
    tokio::spawn(async move {
        // `with_upgrades` keeps the connection alive through the 101.
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!("ws backend connection ended: {e}");
        }
    });

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/ws/notifications".to_owned());
    let mut backend_req = http::Request::builder()
        .method(req.method().clone())
        .uri(path_and_query)
        .body(Empty::<Bytes>::new())
        .expect("static request parts are valid");
    *backend_req.headers_mut() = req.headers().clone();

    let backend_resp = match sender.send_request(backend_req).await {
        Ok(r) => r,
        Err(e) => return bad_gateway(format!("backend rejected websocket handshake: {e}")),
    };

    if backend_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Backend refused the upgrade (e.g. 401) — pass its answer through.
        return passthrough_response(backend_resp);
    }

    // Mirror the backend's 101 (sec-websocket-accept etc.) to the client.
    let mut client_resp = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in backend_resp.headers() {
        client_resp = client_resp.header(name, value);
    }
    let client_resp = client_resp
        .body(Body::empty())
        .expect("mirrored 101 response is valid");

    // Once our 101 is flushed, both upgrades resolve; then it's a byte pipe.
    tokio::spawn(async move {
        let backend_io = match hyper::upgrade::on(backend_resp).await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!("ws backend upgrade failed: {e}");
                return;
            }
        };
        let client_io = match hyper::upgrade::on(req).await {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!("ws client upgrade failed: {e}");
                return;
            }
        };
        let mut backend_io = TokioIo::new(backend_io);
        let mut client_io = TokioIo::new(client_io);
        match tokio::io::copy_bidirectional(&mut client_io, &mut backend_io).await {
            Ok((tx, rx)) => tracing::debug!("ws tunnel closed ({tx} bytes up, {rx} bytes down)"),
            Err(e) => tracing::debug!("ws tunnel error: {e}"),
        }
    });

    client_resp
}

// ---------------------------------------------------------------------------
// /assets — X-Accel-Redirect handling
// ---------------------------------------------------------------------------

async fn asset_proxy(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let range = req.headers().get(header::RANGE).cloned();

    let backend_resp = match forward_to_backend(&state, req).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let Some(accel) = backend_resp.headers().get("x-accel-redirect").cloned() else {
        // No internal redirect (404 missing object, 401 private bucket, ...):
        // pass the backend response through unchanged.
        return passthrough_response(backend_resp);
    };

    // The fs storage backend answers 204 + x-accel-redirect + the metadata
    // headers nginx would copy onto the final response (docs/m0/asset-serving.md).
    let content_type = backend_resp.headers().get(header::CONTENT_TYPE).cloned();
    let cache_control = backend_resp.headers().get(header::CACHE_CONTROL).cloned();

    let Ok(accel_path) = accel.to_str() else {
        return bad_gateway("non-ascii x-accel-redirect from backend".to_owned());
    };
    let Some(rel) = accel_path.strip_prefix(state.accel_prefix.as_str()) else {
        return bad_gateway(format!(
            "x-accel-redirect {accel_path:?} outside configured prefix {:?}",
            state.accel_prefix
        ));
    };
    let Some(file_path) = safe_join(&state.storage_dir, rel) else {
        tracing::warn!("rejected traversal in x-accel-redirect: {accel_path:?}");
        return (StatusCode::FORBIDDEN, "invalid asset path").into_response();
    };

    serve_storage_file(&file_path, content_type, cache_control, &method, range.as_ref()).await
}

/// Join a relative accel path onto the storage root, refusing anything that
/// could escape it (`..`, absolute paths, drive prefixes, backslashes, NUL).
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return None;
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            // ParentDir / RootDir / Prefix all mean escape attempts.
            _ => return None,
        }
    }
    Some(out)
}

enum RangeSpec {
    /// No (usable) Range header — serve the whole file with 200.
    Full,
    /// `206 Partial Content` over the inclusive byte range.
    Partial(u64, u64),
    /// `416 Range Not Satisfiable`.
    Unsatisfiable,
}

/// Parse a single-range `Range: bytes=...` header against a known length.
/// Multi-range and syntactically invalid headers are ignored (RFC 9110 allows
/// serving the full representation), out-of-bounds ranges yield 416.
fn parse_range(header_value: Option<&HeaderValue>, len: u64) -> RangeSpec {
    let Some(value) = header_value else {
        return RangeSpec::Full;
    };
    let Ok(s) = value.to_str() else {
        return RangeSpec::Full;
    };
    let Some(spec) = s.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full; // multi-range: not needed by the frontend
    }
    let Some((start_s, end_s)) = spec.trim().split_once('-') else {
        return RangeSpec::Full;
    };
    match (start_s.is_empty(), end_s.is_empty()) {
        (true, true) => RangeSpec::Full,
        // suffix form: last N bytes
        (true, false) => {
            let Ok(n) = end_s.parse::<u64>() else {
                return RangeSpec::Full;
            };
            if n == 0 || len == 0 {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Partial(len.saturating_sub(n), len - 1)
        }
        // open-ended form: from start to EOF
        (false, true) => {
            let Ok(start) = start_s.parse::<u64>() else {
                return RangeSpec::Full;
            };
            if start >= len {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Partial(start, len - 1)
        }
        (false, false) => {
            let (Ok(start), Ok(end)) = (start_s.parse::<u64>(), end_s.parse::<u64>()) else {
                return RangeSpec::Full;
            };
            if start > end {
                return RangeSpec::Full; // invalid — ignore the header
            }
            if start >= len {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Partial(start, end.min(len - 1))
        }
    }
}

/// Serve a storage file the way nginx's `internal` + `alias` location would:
/// content-type/cache-control from the backend's 204, real Content-Length,
/// `Accept-Ranges: bytes`, single-range 206 support, empty body for HEAD.
async fn serve_storage_file(
    path: &Path,
    content_type: Option<HeaderValue>,
    cache_control: Option<HeaderValue>,
    method: &Method,
    range: Option<&HeaderValue>,
) -> Response {
    let mut file = match File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, "asset not found in storage").into_response();
        }
        Err(e) => {
            tracing::error!("cannot open storage file {path:?}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::error!("cannot stat storage file {path:?}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(ct) = content_type {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    if let Some(cc) = cache_control {
        headers.insert(header::CACHE_CONTROL, cc);
    }

    let (status, start, count) = match parse_range(range, len) {
        RangeSpec::Full => (StatusCode::OK, 0, len),
        RangeSpec::Partial(start, end) => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{len}"))
                    .expect("numeric content-range is a valid header value"),
            );
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        RangeSpec::Unsatisfiable => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{len}"))
                    .expect("numeric content-range is a valid header value"),
            );
            let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            resp.headers_mut().extend(headers);
            return resp;
        }
    };
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(count));

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        if start > 0 {
            if let Err(e) = file.seek(SeekFrom::Start(start)).await {
                tracing::error!("cannot seek storage file {path:?}: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
        Body::from_stream(ReaderStream::new(file.take(count)))
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp.headers_mut().extend(headers);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_sharded_uuid_paths() {
        let root = Path::new("/storage");
        assert_eq!(
            safe_join(root, "9b/6d/4690443a40fc88d6941beb488226"),
            Some(PathBuf::from("/storage/9b/6d/4690443a40fc88d6941beb488226"))
        );
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let root = Path::new("/storage");
        assert_eq!(safe_join(root, "../../etc/passwd"), None);
        assert_eq!(safe_join(root, "a/../../etc/passwd"), None);
        assert_eq!(safe_join(root, "/etc/passwd"), None);
        assert_eq!(safe_join(root, ""), None);
        assert_eq!(safe_join(root, "a\\..\\b"), None);
        assert_eq!(safe_join(root, "a/\0/b"), None);
    }

    #[test]
    fn range_parsing() {
        let hv = |s: &str| HeaderValue::from_str(s).unwrap();
        assert!(matches!(parse_range(None, 100), RangeSpec::Full));
        assert!(matches!(
            parse_range(Some(&hv("bytes=0-9")), 100),
            RangeSpec::Partial(0, 9)
        ));
        assert!(matches!(
            parse_range(Some(&hv("bytes=90-")), 100),
            RangeSpec::Partial(90, 99)
        ));
        assert!(matches!(
            parse_range(Some(&hv("bytes=-10")), 100),
            RangeSpec::Partial(90, 99)
        ));
        // end clamped to len-1
        assert!(matches!(
            parse_range(Some(&hv("bytes=50-1000")), 100),
            RangeSpec::Partial(50, 99)
        ));
        assert!(matches!(
            parse_range(Some(&hv("bytes=100-")), 100),
            RangeSpec::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some(&hv("bytes=200-300")), 100),
            RangeSpec::Unsatisfiable
        ));
        // multi-range and garbage fall back to the full representation
        assert!(matches!(
            parse_range(Some(&hv("bytes=0-1,5-9")), 100),
            RangeSpec::Full
        ));
        assert!(matches!(parse_range(Some(&hv("items=0-9")), 100), RangeSpec::Full));
    }
}
