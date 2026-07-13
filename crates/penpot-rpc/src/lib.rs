//! Typed async client for the Penpot HTTP RPC API (Milestone M1+).
//!
//! Built on the wire shapes verified in M0 (docs/m0/rpc-endpoints.md):
//! - RPC shape: `POST <base>/api/rpc/command/<command-name>`.
//! - Plain JSON works both directions with `Content-Type: application/json`
//!   and `Accept: application/json`; JSON keys are **camelCase**.
//! - Multipart commands (`import-binfile`) are the exception: form field
//!   names are **kebab-case** (`project-id`, `file-id`).
//! - Commands that take no params still need a body (`{}`).
//! - Auth: session cookie `auth-token=<JWE>` (from `login-with-password`
//!   `Set-Cookie`) or header `Authorization: Token <token>` (from
//!   `create-access-token`; requires the `enable-access-tokens` flag).
//! - `export-binfile` / `import-binfile` answer with an SSE stream; the final
//!   `end` event carries a transit-encoded payload.
//!
//! `prepare-register-profile` / `register-profile` are not covered by the M0
//! doc; their shapes were extracted from the 2.16.2 jar
//! (`app/rpc/commands/auth.clj`) and verified against the live backend during
//! M1 integration: `prepare-register-profile` takes `fullname`+`email`+
//! `password` and returns `{token}`; `register-profile` takes just `{token}`
//! and (with email verification disabled) answers with the stripped profile
//! plus the `auth-token` session cookie.

mod error;
mod sse;
mod types;

pub use error::{Error, Result};
pub use sse::{parse_sse, SseEvent};
pub use types::{
    AccessToken, CreatedFile, ExportedBinfile, FileSummary, LoginOutcome, PrepareRegister,
    Profile, ProjectInfo, RegisterOutcome, UpdateFileOutcome,
};

use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE, SET_COOKIE};
use reqwest::multipart::{Form, Part};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

/// How the client authenticates against the backend. Both styles work for
/// every authenticated command (verified in M0).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auth {
    /// No credentials (only `login-with-password` and the register commands
    /// work in this state).
    #[default]
    None,
    /// Session cookie: sent as `Cookie: auth-token=<value>`.
    Cookie(String),
    /// Personal access token: sent as `Authorization: Token <value>`
    /// (literal word `Token`, single space).
    Token(String),
}

/// Async client for the Penpot RPC surface needed by M1/M2.
#[derive(Debug, Clone)]
pub struct PenpotClient {
    http: reqwest::Client,
    /// Backend base URL without trailing slash, e.g. `http://127.0.0.1:6161`.
    base_url: String,
    auth: Auth,
}

impl PenpotClient {
    /// Create an unauthenticated client for a backend base URL
    /// (e.g. `http://127.0.0.1:6161`).
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            // This client only ever talks to the local backend on loopback.
            // .no_proxy() keeps system/env proxy settings (http_proxy & co.)
            // from hijacking loopback RPC: reqwest honors them by default,
            // and on a machine with a corporate proxy configured the app
            // would otherwise fail single-user provisioning at first boot
            // (found by the M4 artifact test's poisoned-proxy run).
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("building a loopback-only reqwest client cannot fail"),
            base_url,
            auth: Auth::None,
        }
    }

    /// Builder-style auth configuration.
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    /// Replace the credentials used for subsequent calls.
    pub fn set_auth(&mut self, auth: Auth) {
        self.auth = auth;
    }

    /// Current credentials.
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Backend base URL (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn command_url(&self, command: &str) -> String {
        format!("{}/api/rpc/command/{command}", self.base_url)
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => req,
            Auth::Cookie(v) => req.header(COOKIE, format!("auth-token={v}")),
            Auth::Token(t) => req.header(AUTHORIZATION, format!("Token {t}")),
        }
    }

    /// POST a JSON RPC command and return the raw (already status-checked)
    /// response.
    async fn rpc_response(
        &self,
        command: &str,
        params: &impl Serialize,
    ) -> Result<reqwest::Response> {
        let resp = self
            .apply_auth(self.http.post(self.command_url(command)))
            .header(ACCEPT, "application/json")
            .json(params)
            .send()
            .await?;
        Self::check_status(resp).await
    }

    /// POST a JSON RPC command, parse the JSON response body.
    async fn rpc<T: DeserializeOwned>(&self, command: &str, params: &impl Serialize) -> Result<T> {
        let resp = self.rpc_response(command, params).await?;
        Ok(resp.json::<T>().await?)
    }

    /// Turn non-2xx responses into [`Error::Rpc`], surfacing Penpot's
    /// `type`/`code` fields (validation errors are HTTP 400 JSON; wrong
    /// credentials are 400 `code: wrong-credentials`, not 401).
    async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let body: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        let field = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Err(Error::Rpc {
            status: status.as_u16(),
            error_type: field("type"),
            code: field("code"),
            body,
        })
    }

    /// Extract the value of the `auth-token` cookie from `Set-Cookie` headers.
    fn auth_token_cookie(resp: &reqwest::Response) -> Option<String> {
        resp.headers().get_all(SET_COOKIE).iter().find_map(|hv| {
            let s = hv.to_str().ok()?;
            let (name_value, _) = s.split_once(';').unwrap_or((s, ""));
            let (name, value) = name_value.split_once('=')?;
            (name.trim() == "auth-token").then(|| value.to_string())
        })
    }

    // ------------------------------------------------------------------
    // Provisioning (single-user; email verification disabled in M1)
    // ------------------------------------------------------------------

    /// `prepare-register-profile` — first step of registration; returns an
    /// intermediate token to pass to [`Self::register_profile`]. Schema
    /// (2.16.2 `app/rpc/commands/auth.clj`): required `fullname`, `email`,
    /// `password` (the fullname travels inside the prepared token).
    pub async fn prepare_register_profile(
        &self,
        email: &str,
        password: &str,
        fullname: &str,
    ) -> Result<PrepareRegister> {
        self.rpc(
            "prepare-register-profile",
            &json!({ "fullname": fullname, "email": email, "password": password }),
        )
        .await
    }

    /// `register-profile` — completes registration; the only required param
    /// is the prepared `token`. With email verification disabled the backend
    /// activates the profile immediately, answers with the (stripped) profile
    /// object and attaches the `auth-token` session cookie, returned in
    /// [`RegisterOutcome::auth_token`].
    pub async fn register_profile(&self, prepare_token: &str) -> Result<RegisterOutcome> {
        let resp = self
            .rpc_response("register-profile", &json!({ "token": prepare_token }))
            .await?;
        let auth_token = Self::auth_token_cookie(&resp);
        let profile = resp.json::<Profile>().await?;
        Ok(RegisterOutcome {
            profile,
            auth_token,
        })
    }

    // ------------------------------------------------------------------
    // Auth
    // ------------------------------------------------------------------

    /// `login-with-password` — returns the profile body plus the `auth-token`
    /// cookie captured from `Set-Cookie`. Does **not** mutate this client's
    /// auth; call [`Self::set_auth`] with `Auth::Cookie(outcome.auth_token)`
    /// to adopt the session.
    pub async fn login_with_password(&self, email: &str, password: &str) -> Result<LoginOutcome> {
        let resp = self
            .rpc_response(
                "login-with-password",
                &json!({ "email": email, "password": password }),
            )
            .await?;
        let auth_token = Self::auth_token_cookie(&resp).ok_or_else(|| {
            Error::Protocol("login-with-password response had no auth-token Set-Cookie".into())
        })?;
        let profile = resp.json::<Profile>().await?;
        Ok(LoginOutcome {
            profile,
            auth_token,
        })
    }

    /// `get-profile` — works with either auth style. Body must be `{}`
    /// (an empty body fails — M0 verified).
    pub async fn get_profile(&self) -> Result<Profile> {
        self.rpc("get-profile", &json!({})).await
    }

    /// `create-access-token` — mints a personal access token. Works with the
    /// session cookie **and** with an existing access token. `expiration` is
    /// an optional duration string (e.g. `"3600s"`); omit for a non-expiring
    /// token. The `token` value is only returned at creation time.
    pub async fn create_access_token(
        &self,
        name: &str,
        expiration: Option<&str>,
    ) -> Result<AccessToken> {
        let mut params = json!({ "name": name });
        if let Some(exp) = expiration {
            params["expiration"] = json!(exp);
        }
        self.rpc("create-access-token", &params).await
    }

    // ------------------------------------------------------------------
    // Projects & files (the M2 sync daemon's poll/CRUD surface)
    // ------------------------------------------------------------------

    /// `get-projects` — all projects of a team. The default project is named
    /// "Drafts" with `isDefault: true`.
    pub async fn get_projects(&self, team_id: &str) -> Result<Vec<ProjectInfo>> {
        self.rpc("get-projects", &json!({ "teamId": team_id })).await
    }

    /// `get-project-files` — file summaries (no `data`) for a project.
    /// This is the sync daemon's poll surface: `revn` + `modifiedAt`.
    pub async fn get_project_files(&self, project_id: &str) -> Result<Vec<FileSummary>> {
        self.rpc("get-project-files", &json!({ "projectId": project_id }))
            .await
    }

    /// `create-project` — creates a project in a team and returns its summary.
    pub async fn create_project(&self, team_id: &str, name: &str) -> Result<ProjectInfo> {
        self.rpc("create-project", &json!({ "teamId": team_id, "name": name }))
            .await
    }

    /// `create-file` — creates an (empty, one-page) file in a project and
    /// returns the complete file object including `data`; keep
    /// [`CreatedFile::first_page_id`] around, `update-file` changes need it.
    pub async fn create_file(&self, project_id: &str, name: &str) -> Result<CreatedFile> {
        self.rpc("create-file", &json!({ "name": name, "projectId": project_id }))
            .await
    }

    /// `create-file` with a **client-chosen file uuid** (`id` is an optional
    /// schema param). Verified live on 2.16.2: the file is created under
    /// exactly that id, and a subsequent in-place [`Self::import_binfile`]
    /// onto it replaces the content — this create-then-import pair is the
    /// **resurrect recipe** the M2 core invariant relies on, because
    /// `import-binfile` with a `file-id` that does not currently exist in the
    /// DB fails (SSE `error` event `object-not-found`, verified live) rather
    /// than creating the file. Fails with HTTP 500 if the id already exists —
    /// including **soft-deleted** files (delete-file keeps the row ~7 days).
    pub async fn create_file_with_id(
        &self,
        project_id: &str,
        name: &str,
        file_id: &str,
    ) -> Result<CreatedFile> {
        self.rpc(
            "create-file",
            &json!({ "name": name, "projectId": project_id, "id": file_id }),
        )
        .await
    }

    /// `get-file` — the full file object including `data.pages` and
    /// `data.pagesIndex.<pageId>.objects`. Returned as raw JSON: the sync
    /// daemon only ever inspects it, the durable format is the binfile.
    pub async fn get_file(&self, file_id: &str) -> Result<serde_json::Value> {
        self.rpc("get-file", &json!({ "id": file_id })).await
    }

    /// `update-file` — apply a list of change objects (e.g. `add-obj`, see
    /// docs/m0/rpc-endpoints.md §update-file for the full verified recipe).
    ///
    /// `session_id` is any client-generated uuid v4 identifying this editing
    /// session. `revn`/`vern` are the values the client believes current
    /// (from [`Self::get_project_files`] or [`Self::get_file`]). A stale
    /// `revn` is **not** rejected — inspect [`UpdateFileOutcome::lagged`] to
    /// detect concurrent edits.
    pub async fn update_file(
        &self,
        file_id: &str,
        session_id: &str,
        revn: i64,
        vern: i64,
        changes: &[serde_json::Value],
    ) -> Result<UpdateFileOutcome> {
        self.rpc(
            "update-file",
            &json!({
                "id": file_id,
                "sessionId": session_id,
                "revn": revn,
                "vern": vern,
                "changes": changes,
            }),
        )
        .await
    }

    /// `delete-file` — answers `204 No Content` (verified in M0).
    pub async fn delete_file(&self, file_id: &str) -> Result<()> {
        self.rpc_response("delete-file", &json!({ "id": file_id }))
            .await?;
        Ok(())
    }

    /// `delete-project` — answers `204 No Content`, but is a **soft delete**
    /// (verified live on 2.16.2): the project keeps appearing in
    /// [`Self::get_projects`] with [`ProjectInfo::deleted_at`] set to the
    /// scheduled GC time (~7 days out). Filter on `deleted_at.is_none()`.
    pub async fn delete_project(&self, project_id: &str) -> Result<()> {
        self.rpc_response("delete-project", &json!({ "id": project_id }))
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Binfile export / import (implemented for M2; unit-tested only in M1)
    // ------------------------------------------------------------------

    /// `export-binfile` — always produces binfile-v3 on 2.16.2. The response
    /// is an SSE stream (`text/event-stream`); this method consumes it and
    /// returns the artifact URI from the final `end` event. Fetch that URI
    /// **with auth** (see [`Self::download_exported_binfile`]); its host is
    /// `PENPOT_PUBLIC_URI`, i.e. the frontend proxy, because the backend
    /// answers asset GETs with 204 + `x-accel-redirect`.
    pub async fn export_binfile(
        &self,
        file_id: &str,
        include_libraries: bool,
        embed_assets: bool,
    ) -> Result<ExportedBinfile> {
        let resp = self
            .rpc_response(
                "export-binfile",
                &json!({
                    "fileId": file_id,
                    "includeLibraries": include_libraries,
                    "embedAssets": embed_assets,
                }),
            )
            .await?;
        let body = resp.text().await?;
        let end = sse::find_end_event(&body)?;
        let uri = sse::decode_export_end(&end.data)?;
        Ok(ExportedBinfile { uri })
    }

    /// GET an export artifact URI with this client's auth (cookie and token
    /// both work — M0 verified) and return the ZIP bytes
    /// (`content-type: application/zip`).
    pub async fn download_exported_binfile(&self, uri: &str) -> Result<Vec<u8>> {
        let resp = self.apply_auth(self.http.get(uri)).send().await?;
        let resp = Self::check_status(resp).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `import-binfile` — multipart with **kebab-case** field names
    /// (camelCase fields fail with 400 params-validation):
    /// `name` (schema-required but ignored for v3 binfiles), `project-id`,
    /// optional `file-id` for in-place import, and `file` (the `.penpot` ZIP
    /// bytes as `application/zip`).
    ///
    /// Returns the created/updated file id(s) from the SSE `end` event.
    /// In-place import replaces content wholesale and resets `revn` to the
    /// value stored in the binfile — `revn` can move backwards.
    pub async fn import_binfile(
        &self,
        name: &str,
        project_id: &str,
        file_id: Option<&str>,
        penpot_zip: Vec<u8>,
    ) -> Result<Vec<String>> {
        let mut form = Form::new()
            .text("name", name.to_string())
            .text("project-id", project_id.to_string());
        if let Some(fid) = file_id {
            form = form.text("file-id", fid.to_string());
        }
        let part = Part::bytes(penpot_zip)
            .file_name("import.penpot")
            .mime_str("application/zip")
            .map_err(|e| Error::Protocol(format!("invalid mime for import part: {e}")))?;
        form = form.part("file", part);

        let resp = self
            .apply_auth(self.http.post(self.command_url("import-binfile")))
            .header(ACCEPT, "application/json")
            .multipart(form)
            .send()
            .await?;
        let resp = Self::check_status(resp).await?;
        let body = resp.text().await?;
        let end = sse::find_end_event(&body)?;
        sse::decode_import_end(&end.data)
    }
}
