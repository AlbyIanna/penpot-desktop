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
