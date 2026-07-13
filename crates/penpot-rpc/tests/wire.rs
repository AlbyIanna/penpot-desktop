//! Wire-shape tests for the Penpot RPC client, asserting the exact shapes
//! documented in docs/m0/rpc-endpoints.md against an in-process mock server:
//! content types, camelCase JSON field casing, kebab-case multipart field
//! names, auth-token cookie capture, `Authorization: Token` header, and SSE
//! stream handling for export/import.

use penpot_rpc::{Auth, Error, PenpotClient};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const PROFILE_ID: &str = "e4ebd8e6-e0d6-8139-8008-51ec952603ac";
const TEAM_ID: &str = "e4ebd8e6-e0d6-8139-8008-51ec952e5c36";
const PROJECT_ID: &str = "e4ebd8e6-e0d6-8139-8008-51ec9531fcd2";
const FILE_ID: &str = "3a4be581-6d37-8010-8008-51ee126e1fb4";

/// Profile body as returned by login/get-profile/register (camelCase keys,
/// shape from rpc-endpoints.md §login-with-password), plus extra fields the
/// client must tolerate.
fn profile_body() -> serde_json::Value {
    json!({
        "id": PROFILE_ID,
        "email": "m0@local.test",
        "fullname": "M0 Spike",
        "defaultTeamId": TEAM_ID,
        "defaultProjectId": PROJECT_ID,
        "isActive": true,
        "isAdmin": false,
        "isBlocked": false,
        "props": {"someUiState": true}
    })
}

/// Matcher asserting the raw request body contains a byte substring —
/// used to check kebab-case multipart field names.
struct BodyContains(&'static str);

impl Match for BodyContains {
    fn matches(&self, request: &Request) -> bool {
        request
            .body
            .windows(self.0.len())
            .any(|w| w == self.0.as_bytes())
    }
}

/// Matcher asserting the raw request body does NOT contain a byte substring.
struct BodyLacks(&'static str);

impl Match for BodyLacks {
    fn matches(&self, request: &Request) -> bool {
        !request
            .body
            .windows(self.0.len())
            .any(|w| w == self.0.as_bytes())
    }
}

/// Matcher asserting the Content-Type starts with the given prefix
/// (multipart content types carry a random boundary suffix).
struct ContentTypePrefix(&'static str);

impl Match for ContentTypePrefix {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with(self.0))
    }
}

fn sse_response(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream;charset=UTF-8")
        .set_body_raw(body.as_bytes().to_vec(), "text/event-stream;charset=UTF-8")
}

// ---------------------------------------------------------------------
// login-with-password
// ---------------------------------------------------------------------

#[tokio::test]
async fn login_sends_camel_case_json_and_captures_auth_token_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/login-with-password"))
        .and(header("content-type", "application/json"))
        .and(header("accept", "application/json"))
        // Exact JSON body per rpc-endpoints.md §login-with-password.
        .and(body_json(json!({"email": "m0@local.test", "password": "m0-spike-password"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "set-cookie",
                    "auth-token=eyJhbGciOiJBMjU2S1ci.fake; Path=/; HttpOnly; \
                     Expires=Mon, 20 Jul 2026 00:00:00 GMT; \
                     Comment=Renewal at: Mon, 13 Jul 2026 06:00:00 GMT; SameSite=Lax",
                )
                .set_body_json(profile_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let outcome = client
        .login_with_password("m0@local.test", "m0-spike-password")
        .await
        .unwrap();

    // Cookie value captured verbatim, attributes stripped.
    assert_eq!(outcome.auth_token, "eyJhbGciOiJBMjU2S1ci.fake");
    // camelCase response fields parsed.
    assert_eq!(outcome.profile.id, PROFILE_ID);
    assert_eq!(outcome.profile.email, "m0@local.test");
    assert_eq!(outcome.profile.fullname, "M0 Spike");
    assert_eq!(outcome.profile.default_team_id.as_deref(), Some(TEAM_ID));
    assert_eq!(
        outcome.profile.default_project_id.as_deref(),
        Some(PROJECT_ID)
    );
    assert!(outcome.profile.is_active);
    assert!(!outcome.profile.is_admin);
    assert!(!outcome.profile.is_blocked);
}

#[tokio::test]
async fn login_wrong_credentials_is_400_with_code_not_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/login-with-password"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "validation",
            "code": "wrong-credentials"
        })))
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let err = client
        .login_with_password("m0@local.test", "nope")
        .await
        .unwrap_err();
    match &err {
        Error::Rpc {
            status,
            error_type,
            code,
            ..
        } => {
            assert_eq!(*status, 400);
            assert_eq!(error_type.as_deref(), Some("validation"));
            assert_eq!(code.as_deref(), Some("wrong-credentials"));
        }
        other => panic!("expected Error::Rpc, got {other:?}"),
    }
    assert_eq!(err.rpc_code(), Some("wrong-credentials"));
}

#[tokio::test]
async fn login_without_set_cookie_is_protocol_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/login-with-password"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_body()))
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let err = client
        .login_with_password("m0@local.test", "pw")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
}

// ---------------------------------------------------------------------
// get-profile (both auth styles; `{}` body required)
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_profile_with_token_auth_sends_token_header_and_empty_object_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/get-profile"))
        // Literal word `Token`, single space (rpc-endpoints.md §Authentication).
        .and(header("authorization", "Token tok-123"))
        .and(header("content-type", "application/json"))
        // Commands with no params still need a `{}` body.
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_body()))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri()).with_auth(Auth::Token("tok-123".into()));
    let profile = client.get_profile().await.unwrap();
    assert_eq!(profile.id, PROFILE_ID);
}

#[tokio::test]
async fn get_profile_with_cookie_auth_sends_auth_token_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/get-profile"))
        .and(header("cookie", "auth-token=jwe-cookie-value"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_body()))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        PenpotClient::new(server.uri()).with_auth(Auth::Cookie("jwe-cookie-value".into()));
    let profile = client.get_profile().await.unwrap();
    assert_eq!(profile.email, "m0@local.test");
}

// ---------------------------------------------------------------------
// prepare-register-profile / register-profile
// ---------------------------------------------------------------------

#[tokio::test]
async fn prepare_register_sends_fullname_email_password_and_returns_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/prepare-register-profile"))
        .and(header("content-type", "application/json"))
        // Schema from the 2.16.2 jar (app/rpc/commands/auth.clj), verified
        // live in M1: fullname is required HERE (it travels inside the
        // prepared token), not on register-profile.
        .and(body_json(json!({
            "fullname": "Local User",
            "email": "user@local.test",
            "password": "secret-pw"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "reg-token-1"})))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let prep = client
        .prepare_register_profile("user@local.test", "secret-pw", "Local User")
        .await
        .unwrap();
    assert_eq!(prep.token, "reg-token-1");
}

#[tokio::test]
async fn register_profile_sends_token_only_and_captures_session_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/register-profile"))
        .and(header("content-type", "application/json"))
        // Only `token` is accepted by the 2.16.2 schema.
        .and(body_json(json!({"token": "reg-token-1"})))
        .respond_with(
            ResponseTemplate::new(200)
                // Email verification disabled: backend logs the new user in.
                .insert_header(
                    "set-cookie",
                    "auth-token=fresh-session-jwe; Path=/; HttpOnly; SameSite=Lax",
                )
                .set_body_json(profile_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let outcome = client.register_profile("reg-token-1").await.unwrap();
    assert_eq!(outcome.auth_token.as_deref(), Some("fresh-session-jwe"));
    assert_eq!(outcome.profile.id, PROFILE_ID);
}

#[tokio::test]
async fn register_profile_without_cookie_yields_none_auth_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/register-profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_body()))
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let outcome = client.register_profile("t").await.unwrap();
    assert!(outcome.auth_token.is_none());
}

// ---------------------------------------------------------------------
// create-access-token
// ---------------------------------------------------------------------

#[tokio::test]
async fn create_access_token_minimal_body_and_response_parse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/create-access-token"))
        // No expiration key at all when not requested.
        .and(body_json(json!({"name": "m1-desktop"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "11111111-2222-3333-4444-555555555555",
            "profileId": PROFILE_ID,
            "name": "m1-desktop",
            "token": "eyJhbGciOiJBMjU2S1ci.token",
            "createdAt": "2026-07-13T00:00:00Z",
            "updatedAt": "2026-07-13T00:00:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        PenpotClient::new(server.uri()).with_auth(Auth::Cookie("session-cookie".into()));
    let tok = client.create_access_token("m1-desktop", None).await.unwrap();
    assert_eq!(tok.id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(tok.profile_id, PROFILE_ID);
    assert_eq!(tok.name, "m1-desktop");
    assert_eq!(tok.token, "eyJhbGciOiJBMjU2S1ci.token");
    assert_eq!(tok.created_at.as_deref(), Some("2026-07-13T00:00:00Z"));
}

#[tokio::test]
async fn create_access_token_with_expiration_duration_string() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/create-access-token"))
        // Verified duration-string form (`"3600s"`), rpc-endpoints.md.
        .and(body_json(json!({"name": "short-lived", "expiration": "3600s"})))
        // Tokens can mint more tokens: token auth accepted here too.
        .and(header("authorization", "Token existing-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "aaaa",
            "profileId": PROFILE_ID,
            "name": "short-lived",
            "token": "new-token"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        PenpotClient::new(server.uri()).with_auth(Auth::Token("existing-token".into()));
    let tok = client
        .create_access_token("short-lived", Some("3600s"))
        .await
        .unwrap();
    assert_eq!(tok.token, "new-token");
    // Optional timestamps tolerated when absent.
    assert!(tok.created_at.is_none());
}

// ---------------------------------------------------------------------
// export-binfile (SSE)
// ---------------------------------------------------------------------

#[tokio::test]
async fn export_binfile_sends_camel_case_params_and_parses_sse_end_uri() {
    let server = MockServer::start().await;
    let sse_body = format!(
        "event: progress\n\
         data: {{\"~:section\":\"~:file\",\"~:id\":\"~u{FILE_ID}\",\"~:name\":\"m0-rpc-spike\"}}\n\
         \n\
         event: end\n\
         data: {{\"~#uri\":\"http://localhost:9001/assets/by-id/deadbeef-0000\"}}\n\
         \n"
    );
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/export-binfile"))
        .and(header("authorization", "Token tok-123"))
        // All three params required, camelCase (rpc-endpoints.md §export-binfile).
        .and(body_json(json!({
            "fileId": FILE_ID,
            "includeLibraries": false,
            "embedAssets": false
        })))
        .respond_with(sse_response(&sse_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri()).with_auth(Auth::Token("tok-123".into()));
    let exported = client.export_binfile(FILE_ID, false, false).await.unwrap();
    assert_eq!(
        exported.uri,
        "http://localhost:9001/assets/by-id/deadbeef-0000"
    );
}

#[tokio::test]
async fn download_exported_binfile_gets_uri_with_auth() {
    let server = MockServer::start().await;
    let zip_bytes = b"PK\x03\x04fake-zip".to_vec();
    Mock::given(method("GET"))
        .and(path("/assets/by-id/deadbeef-0000"))
        .and(header("authorization", "Token tok-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(zip_bytes.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri()).with_auth(Auth::Token("tok-123".into()));
    let uri = format!("{}/assets/by-id/deadbeef-0000", server.uri());
    let bytes = client.download_exported_binfile(&uri).await.unwrap();
    assert_eq!(bytes, zip_bytes);
}

// ---------------------------------------------------------------------
// import-binfile (kebab-case multipart, SSE response)
// ---------------------------------------------------------------------

const IMPORTED_ID: &str = "3a4be581-6d37-8010-8008-51eecd7dc111";

#[tokio::test]
async fn import_binfile_as_new_file_uses_kebab_case_multipart() {
    let server = MockServer::start().await;
    let sse_body = format!(
        "event: progress\ndata: {{\"~:section\":\"~:manifest\"}}\n\n\
         event: end\ndata: [\"~u{IMPORTED_ID}\"]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/import-binfile"))
        .and(header("authorization", "Token tok-123"))
        .and(ContentTypePrefix("multipart/form-data"))
        // kebab-case field names (camelCase fails with 400 params-validation).
        .and(BodyContains("name=\"name\""))
        .and(BodyContains("name=\"project-id\""))
        .and(BodyContains(PROJECT_ID))
        .and(BodyContains("name=\"file\""))
        // File part carries a filename and application/zip content type.
        .and(BodyContains("filename="))
        .and(BodyContains("Content-Type: application/zip"))
        // Never the camelCase form, and no file-id when importing as new.
        .and(BodyLacks("projectId"))
        .and(BodyLacks("name=\"file-id\""))
        .respond_with(sse_response(&sse_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri()).with_auth(Auth::Token("tok-123".into()));
    let ids = client
        .import_binfile("m0-imported-copy", PROJECT_ID, None, b"PK\x03\x04zip".to_vec())
        .await
        .unwrap();
    // A NEW file id is minted, decoded from the transit `~u` uuid.
    assert_eq!(ids, vec![IMPORTED_ID.to_string()]);
}

#[tokio::test]
async fn import_binfile_in_place_sends_file_id_field_and_returns_same_id() {
    let server = MockServer::start().await;
    let sse_body = format!("event: end\ndata: [\"~u{FILE_ID}\"]\n\n");
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/import-binfile"))
        .and(ContentTypePrefix("multipart/form-data"))
        .and(BodyContains("name=\"project-id\""))
        .and(BodyContains("name=\"file-id\""))
        .and(BodyContains(FILE_ID))
        .and(BodyLacks("fileId"))
        .respond_with(sse_response(&sse_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri()).with_auth(Auth::Cookie("sess".into()));
    let ids = client
        .import_binfile("m0-rpc-spike", PROJECT_ID, Some(FILE_ID), b"PK\x03\x04zip".to_vec())
        .await
        .unwrap();
    // In-place import echoes back the SAME file id that was passed.
    assert_eq!(ids, vec![FILE_ID.to_string()]);
}

#[tokio::test]
async fn import_binfile_params_validation_error_surfaces_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/import-binfile"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "validation",
            "code": "params-validation",
            "explain": "malli schema dump here"
        })))
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let err = client
        .import_binfile("x", PROJECT_ID, None, vec![1, 2, 3])
        .await
        .unwrap_err();
    assert_eq!(err.rpc_code(), Some("params-validation"));
}

// ---------------------------------------------------------------------
// Misc client behavior
// ---------------------------------------------------------------------

#[tokio::test]
async fn base_url_trailing_slash_is_normalized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/get-profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_body()))
        .expect(1)
        .mount(&server)
        .await;

    let client = PenpotClient::new(format!("{}/", server.uri()));
    client.get_profile().await.unwrap();
}

#[tokio::test]
async fn non_json_error_body_is_preserved_as_string() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/rpc/command/get-profile"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let client = PenpotClient::new(server.uri());
    let err = client.get_profile().await.unwrap_err();
    match err {
        Error::Rpc { status, body, .. } => {
            assert_eq!(status, 500);
            assert_eq!(body, serde_json::Value::String("boom".into()));
        }
        other => panic!("expected Error::Rpc, got {other:?}"),
    }
}
