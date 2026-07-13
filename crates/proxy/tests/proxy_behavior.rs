//! End-to-end tests for the proxy against an in-process mock backend that
//! emits the M0-documented Penpot behaviors (docs/m0/asset-serving.md):
//! 204 + x-accel-redirect for fs-backed assets, 404/401 passthrough, and a
//! websocket endpoint. Assertions are on the proxy's observable responses.

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use proxy::{Proxy, ProxyConfig};

/// 75 deterministic bytes standing in for the PNG from the M0 spike.
fn asset_bytes() -> Vec<u8> {
    (0u8..75).collect()
}

// --------------------------------------------------------------------------
// Mock backend (Penpot 2.16.2 behaviors per docs/m0/asset-serving.md)
// --------------------------------------------------------------------------

async fn mock_rpc_echo(method: Method, headers: HeaderMap, body: Bytes) -> Response {
    let echo = serde_json::json!({
        "method": method.as_str(),
        "cookie": headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        "authorization": headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()),
        "body": String::from_utf8_lossy(&body),
    });
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::HeaderName::from_static("x-mock-backend"), "1"),
        ],
        echo.to_string(),
    )
        .into_response()
}

async fn mock_asset(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    match id.as_str() {
        // fs storage backend: 204 No Content + x-accel-redirect (never bytes)
        "ok" => (
            StatusCode::NO_CONTENT,
            [
                (
                    header::HeaderName::from_static("x-accel-redirect"),
                    "/internal/assets/9b/6d/4690443a40fc88d6941beb488226",
                ),
                (header::CONTENT_TYPE, "image/png"),
                // upstream quirk: value is milliseconds; copied verbatim
                (header::CACHE_CONTROL, "max-age=86400000"),
            ],
        )
            .into_response(),
        // a malicious/buggy accel path attempting traversal
        "evil" => (
            StatusCode::NO_CONTENT,
            [
                (
                    header::HeaderName::from_static("x-accel-redirect"),
                    "/internal/assets/../../etc/passwd",
                ),
                (header::CONTENT_TYPE, "image/png"),
            ],
        )
            .into_response(),
        // accel path outside the configured prefix
        "outside" => (
            StatusCode::NO_CONTENT,
            [(
                header::HeaderName::from_static("x-accel-redirect"),
                "/elsewhere/foo",
            )],
        )
            .into_response(),
        // accel path pointing at a storage object that doesn't exist on disk
        "ghost" => (
            StatusCode::NO_CONTENT,
            [
                (
                    header::HeaderName::from_static("x-accel-redirect"),
                    "/internal/assets/de/ad/beef",
                ),
                (header::CONTENT_TYPE, "image/png"),
            ],
        )
            .into_response(),
        // private bucket without credentials
        "private" => (StatusCode::UNAUTHORIZED, "\"authentication-required\"").into_response(),
        // unknown storage object
        _ => (StatusCode::NOT_FOUND, "object not found").into_response(),
    }
}

async fn mock_ws(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_owned();
    ws.on_upgrade(move |socket| mock_ws_echo(socket, cookie))
}

async fn mock_ws_echo(mut socket: WebSocket, cookie: String) {
    // first frame proves auth headers crossed the proxy
    let _ = socket.send(Message::Text(format!("cookie={cookie}").into())).await;
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(t) => {
                if socket
                    .send(Message::Text(format!("echo:{t}").into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn start_mock_backend() -> SocketAddr {
    let app = Router::new()
        .route("/api/rpc/command/ping", any(mock_rpc_echo))
        .route("/assets/by-file-media-id/{id}", get(mock_asset))
        .route("/ws/notifications", get(mock_ws));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// --------------------------------------------------------------------------
// Test stack: temp static dir + temp storage dir + mock backend + proxy
// --------------------------------------------------------------------------

struct Stack {
    proxy_addr: SocketAddr,
    _tmp: TempDir,
}

async fn start_stack() -> Stack {
    let tmp = TempDir::new().unwrap();

    let static_dir = tmp.path().join("dist");
    std::fs::create_dir_all(static_dir.join("js")).unwrap();
    std::fs::write(static_dir.join("index.html"), "<html>penpot-spa</html>").unwrap();
    std::fs::write(static_dir.join("js/app.js"), "console.log('app')").unwrap();

    let storage_dir = tmp.path().join("assets");
    std::fs::create_dir_all(storage_dir.join("9b/6d")).unwrap();
    std::fs::write(
        storage_dir.join("9b/6d/4690443a40fc88d6941beb488226"),
        asset_bytes(),
    )
    .unwrap();

    let backend_addr = start_mock_backend().await;

    let config = ProxyConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        backend_addr,
        static_dir,
        storage_dir,
        accel_prefix: "/internal/assets/".into(),
        exporter_addr: None,
    };
    let proxy = Proxy::bind(config).await.unwrap();
    let proxy_addr = proxy.local_addr();
    tokio::spawn(async move {
        proxy.serve().await.unwrap();
    });

    Stack { proxy_addr, _tmp: tmp }
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn send(
    _stack: &Stack,
    req: Request<Full<Bytes>>,
) -> (StatusCode, HeaderMap, Bytes) {
    let resp = client().request(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, bytes)
}

fn get_req(stack: &Stack, path: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .uri(format!("http://{}{}", stack.proxy_addr, path))
        .body(Full::new(Bytes::new()))
        .unwrap()
}

// --------------------------------------------------------------------------
// Static SPA
// --------------------------------------------------------------------------

#[tokio::test]
async fn spa_serves_index_and_static_files() {
    let stack = start_stack().await;

    let (status, _, body) = send(&stack, get_req(&stack, "/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"<html>penpot-spa</html>");

    let (status, headers, body) = send(&stack, get_req(&stack, "/js/app.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"console.log('app')");
    assert!(headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("javascript"));
}

#[tokio::test]
async fn spa_falls_back_to_index_for_client_routes() {
    let stack = start_stack().await;
    let (status, _, body) = send(&stack, get_req(&stack, "/dashboard/recent")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"<html>penpot-spa</html>");
}

// --------------------------------------------------------------------------
// /api reverse proxy
// --------------------------------------------------------------------------

#[tokio::test]
async fn api_preserves_method_headers_cookies_and_body() {
    let stack = start_stack().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "http://{}/api/rpc/command/ping",
            stack.proxy_addr
        ))
        .header(header::COOKIE, "auth-token=secret-session")
        .header(header::AUTHORIZATION, "Token pat-123")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from_static(b"{\"hello\":\"backend\"}")))
        .unwrap();
    let (status, headers, body) = send(&stack, req).await;

    assert_eq!(status, StatusCode::OK);
    // backend response headers pass through
    assert_eq!(headers.get("x-mock-backend").unwrap(), "1");
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/json");

    let echo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["cookie"], "auth-token=secret-session");
    assert_eq!(echo["authorization"], "Token pat-123");
    assert_eq!(echo["body"], "{\"hello\":\"backend\"}");
}

#[tokio::test]
async fn api_export_fails_gracefully_with_502() {
    let stack = start_stack().await;
    let (status, _, body) = send(&stack, get_req(&stack, "/api/export")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("exporter"));

    let (status, _, _) = send(&stack, get_req(&stack, "/api/export/some/sub")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn api_with_dead_backend_is_502_not_hang_or_panic() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("index.html"), "x").unwrap();
    // point at a port nothing listens on
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    let config = ProxyConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        backend_addr: dead_addr,
        static_dir: tmp.path().to_path_buf(),
        storage_dir: tmp.path().to_path_buf(),
        accel_prefix: "/internal/assets/".into(),
        exporter_addr: None,
    };
    let proxy = Proxy::bind(config).await.unwrap();
    let addr = proxy.local_addr();
    tokio::spawn(async move { proxy.serve().await.unwrap() });

    let req = Request::builder()
        .uri(format!("http://{addr}/api/rpc/command/ping"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// --------------------------------------------------------------------------
// /assets — X-Accel-Redirect
// --------------------------------------------------------------------------

#[tokio::test]
async fn asset_204_accel_is_resolved_and_served_from_storage() {
    let stack = start_stack().await;
    let (status, headers, body) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/ok")).await;

    // matches the nginx baseline response documented in asset-serving.md
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], &asset_bytes()[..]);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "max-age=86400000");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), "75");
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    // the internal path must not leak as a redirect the client has to follow
    assert!(headers.get(header::LOCATION).is_none());
}

#[tokio::test]
async fn asset_range_request_yields_206() {
    let stack = start_stack().await;
    let mut req = get_req(&stack, "/assets/by-file-media-id/ok");
    req.headers_mut()
        .insert(header::RANGE, "bytes=0-9".parse().unwrap());
    let (status, headers, body) = send(&stack, req).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], &asset_bytes()[..10]);
    assert_eq!(headers.get(header::CONTENT_RANGE).unwrap(), "bytes 0-9/75");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), "10");
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");

    // suffix range
    let mut req = get_req(&stack, "/assets/by-file-media-id/ok");
    req.headers_mut()
        .insert(header::RANGE, "bytes=-5".parse().unwrap());
    let (status, headers, body) = send(&stack, req).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], &asset_bytes()[70..]);
    assert_eq!(headers.get(header::CONTENT_RANGE).unwrap(), "bytes 70-74/75");

    // unsatisfiable range
    let mut req = get_req(&stack, "/assets/by-file-media-id/ok");
    req.headers_mut()
        .insert(header::RANGE, "bytes=500-".parse().unwrap());
    let (status, headers, _) = send(&stack, req).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers.get(header::CONTENT_RANGE).unwrap(), "bytes */75");
}

#[tokio::test]
async fn asset_traversal_in_accel_path_is_rejected() {
    let stack = start_stack().await;
    let (status, _, body) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/evil")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // must not leak /etc/passwd contents
    assert!(!String::from_utf8_lossy(&body).contains("root:"));
}

#[tokio::test]
async fn asset_accel_outside_prefix_is_bad_gateway() {
    let stack = start_stack().await;
    let (status, _, _) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/outside")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn asset_missing_storage_file_is_404() {
    let stack = start_stack().await;
    let (status, _, _) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/ghost")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn asset_non_204_backend_responses_pass_through() {
    let stack = start_stack().await;

    let (status, _, body) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(&body[..], b"object not found");

    let (status, _, body) =
        send(&stack, get_req(&stack, "/assets/by-file-media-id/private")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(&body[..], b"\"authentication-required\"");
}

#[tokio::test]
async fn internal_assets_path_is_not_externally_routable() {
    let stack = start_stack().await;
    let (status, _, body) = send(
        &stack,
        get_req(&stack, "/internal/assets/9b/6d/4690443a40fc88d6941beb488226"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // must not serve the storage bytes, nor fall back to the SPA
    assert_ne!(&body[..], &asset_bytes()[..]);
    assert_ne!(&body[..], b"<html>penpot-spa</html>");
}

// --------------------------------------------------------------------------
// /ws/notifications
// --------------------------------------------------------------------------

#[tokio::test]
async fn websocket_is_proxied_both_directions_with_headers() {
    let stack = start_stack().await;

    let mut req = format!("ws://{}/ws/notifications", stack.proxy_addr)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert(header::COOKIE, "auth-token=ws-secret".parse().unwrap());

    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

    // backend → client: first frame carries the forwarded cookie header
    let first = ws.next().await.unwrap().unwrap();
    assert_eq!(first.into_text().unwrap().as_str(), "cookie=auth-token=ws-secret");

    // client → backend → client echo
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "file-change".into(),
    ))
    .await
    .unwrap();
    let echoed = ws.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "echo:file-change");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn ws_route_without_upgrade_is_bad_request() {
    let stack = start_stack().await;
    let (status, _, _) = send(&stack, get_req(&stack, "/ws/notifications")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --------------------------------------------------------------------------
// Shutdown
// --------------------------------------------------------------------------

#[tokio::test]
async fn serve_with_shutdown_stops_the_listener() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("index.html"), "x").unwrap();
    let config = ProxyConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        backend_addr: "127.0.0.1:1".parse().unwrap(),
        static_dir: tmp.path().to_path_buf(),
        storage_dir: tmp.path().to_path_buf(),
        accel_prefix: "/internal/assets/".into(),
        exporter_addr: None,
    };
    let proxy = Proxy::bind(config).await.unwrap();
    let addr = proxy.local_addr();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        proxy
            .serve_with_shutdown(async move {
                let _ = rx.await;
            })
            .await
    });

    // serving before shutdown
    let resp = client()
        .request(
            Request::builder()
                .uri(format!("http://{addr}/"))
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    tx.send(()).unwrap();
    handle.await.unwrap().unwrap();

    // listener is gone
    let err = client()
        .request(
            Request::builder()
                .uri(format!("http://{addr}/"))
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await;
    assert!(err.is_err());
}
