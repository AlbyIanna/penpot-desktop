//! Wire types for the Penpot RPC surface used by M1/M2.
//!
//! JSON RPC bodies/responses are camelCase (verified in M0 —
//! docs/m0/rpc-endpoints.md). Unknown fields are ignored on purpose:
//! Penpot returns many more profile fields than we consume.

use serde::Deserialize;

/// Profile object as returned by `login-with-password`, `get-profile` and
/// `register-profile`. Shape verified in M0 (rpc-endpoints.md §login-with-password):
/// it already contains everything discovery needs (`defaultTeamId`,
/// `defaultProjectId`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub email: String,
    pub fullname: String,
    #[serde(default)]
    pub default_team_id: Option<String>,
    #[serde(default)]
    pub default_project_id: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub is_blocked: bool,
}

/// Response of `prepare-register-profile`: an intermediate registration token
/// to pass to `register-profile`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PrepareRegister {
    pub token: String,
}

/// Result of `login-with-password`: the profile body plus the `auth-token`
/// session cookie captured from `Set-Cookie` (a JWE; attributes observed in
/// M0: `Path=/; HttpOnly; SameSite=Lax`, 7-day expiry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    pub profile: Profile,
    /// Raw value of the `auth-token` cookie.
    pub auth_token: String,
}

/// Result of `register-profile`. With email verification disabled (the M1
/// single-user setup) the backend activates the profile and attaches the
/// session cookie directly; `auth_token` carries it when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterOutcome {
    pub profile: Profile,
    /// Raw value of the `auth-token` cookie, when the backend attached one.
    pub auth_token: Option<String>,
}

/// Response of `create-access-token` (rpc-endpoints.md §create-access-token).
/// The `token` value is only returned at creation time.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessToken {
    pub id: String,
    pub profile_id: String,
    pub name: String,
    pub token: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Project summary as returned by `get-projects` and `create-project`
/// (rpc-endpoints.md §get-projects).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: String,
    pub team_id: String,
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    /// Soft-deletion marker. **`delete-project` answers 204 but is a soft
    /// delete** (verified live on 2.16.2): the project keeps appearing in
    /// `get-projects` with `deletedAt` set to the scheduled GC time (~7 days
    /// out). Poll consumers must treat `deleted_at.is_some()` as "gone".
    /// (Deleted *files*, by contrast, disappear from `get-project-files`
    /// immediately.)
    #[serde(default)]
    pub deleted_at: Option<String>,
}

/// File summary as returned by `get-project-files` (no `data`).
///
/// This is the sync daemon's **poll surface**: `revn` + `modified_at` are the
/// change-detection fields (rpc-endpoints.md §get-project-files). Beware that
/// `revn` is not monotonic across in-place imports — it is set to whatever the
/// imported binfile carries.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileSummary {
    pub id: String,
    pub name: String,
    pub project_id: String,
    pub revn: i64,
    #[serde(default)]
    pub vern: i64,
    pub modified_at: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub is_shared: bool,
    /// Soft-deletion marker, exposed for symmetry with [`ProjectInfo`].
    /// Deleted files were observed to vanish from `get-project-files`
    /// immediately, so this is expected to stay `None` in practice.
    #[serde(default)]
    pub deleted_at: Option<String>,
}

/// File object as returned by `create-file`: metadata plus the full `data`
/// map (pages, pagesIndex, ...). Verified live: `revn` starts at 0 and `data`
/// already contains one page.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CreatedFile {
    pub id: String,
    pub name: String,
    pub project_id: String,
    pub revn: i64,
    #[serde(default)]
    pub vern: i64,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl CreatedFile {
    /// Id of the first page (`data.pages[0]`) — `update-file` changes that
    /// touch page content need it as `pageId`.
    pub fn first_page_id(&self) -> Option<&str> {
        self.data.get("pages")?.as_array()?.first()?.as_str()
    }
}

/// Response of `update-file` (rpc-endpoints.md §update-file).
///
/// `revn` is the file revision **before** this update; `lagged` contains all
/// stored change entries with revn greater than the one you sent — including
/// your own (which lands at `revn + 1`). An up-to-date client sees exactly one
/// lagged entry. The server never conflict-errors on a stale revn; inspecting
/// `lagged` is the client's job.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFileOutcome {
    pub revn: i64,
    #[serde(default)]
    pub lagged: Vec<serde_json::Value>,
}

/// Result of `export-binfile`: the artifact URI carried by the final SSE
/// `end` event. The URI host is `PENPOT_PUBLIC_URI` and must be fetched
/// **with auth** (cookie or token both work — M0 verified); the backend
/// itself answers such GETs with 204 + `x-accel-redirect`, so the download
/// is meant to go through the frontend proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedBinfile {
    /// e.g. `http://localhost:9001/assets/by-id/<asset-uuid>`
    pub uri: String,
}
