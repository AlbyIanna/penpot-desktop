//! Tier-2 and Tier-3 verbs: the ones that need the running app.
//!
//! Every verb here is a client of routes the app **already serves**. None of
//! them links the library and does the work itself, and that is the whole
//! design: the chapter-C spike (`docs/spikes/cli-verbs.md`, RC-7) constructed a
//! `packages::PackagesState` in a second process against a live vault and ran
//! the reconcilers alongside the app's own, writing `lock.json` and the consent
//! ledger with **nothing detecting it**. There is no advisory file locking
//! anywhere in this workspace (RC-3), so a second in-process writer does not
//! produce an error — it produces silent divergence. Loopback is what makes the
//! app the single writer.
//!
//! The CLI never self-boots either. [`crate::attach::attach`] fails, naming
//! `penpot daemon`, when no instance is running or when `runtime.json` names a
//! dead pid; [`route_for`] calls it BEFORE building anything, so that failure
//! always arrives before the first byte goes on the wire.
//!
//! # Success and failure
//!
//! **Pretty JSON on stdout on success; any non-2xx is an `Err`, which `main`
//! reports on stderr and exits 1 for.** Same as every Tier-1 verb and the
//! no-app attach path, so a caller never has to know a verb's tier to know how
//! it reports failure, and `penpot boards | jq` never gets a failure object
//! piped into it as though it were a board listing. See [`outcome`], which is
//! the whole decision, pure and unit-tested.
//!
//! # What this surface does not guarantee
//!
//! **`sync pause` is not a barrier.** `POST /__api/prefs` persists
//! `syncEnabled` and calls the bare `SyncControl::pause()` flag flip. It does
//! NOT call `pause_and_wait_idle()` — the barrier version, whose only caller in
//! the workspace is `manage::delete_file`. A poll cycle already in flight keeps
//! writing regardless of the flag, so **a `pause` that returns is not proof the
//! daemon is quiescent** (spike RC-5). A script that pauses and then
//! immediately touches the vault races the daemon. Two further consequences of
//! the same route: the pause is *sticky* — it survives a reboot, because it is
//! a saved preference, not a session flag — and it is applied to whichever
//! stack is live, so it does not follow a vault switch.
//!
//! **The write verbs read-modify-write.** `POST /__api/prefs` takes the FULL
//! preferences object (every field carries `serde(default)`, so a partial body
//! would silently reset the fields it omitted — the spike's `{"syncEnabled":
//! false}` quietly re-enabled plugins and CSP). So [`sync_set_paused`] and
//! [`prefs_set`] GET the current object, change one field, and POST it back.
//! That is the contract the Preferences page keeps too, but it is still a
//! read-modify-write from a second process: a GUI preferences change landing
//! between the two calls is lost, last-writer-wins (RC-3). It is narrow, it is
//! not fixable from this side, and it is not a reason to write the file
//! directly — that would lose the live apply as well.
//!
//! # `export` is deliberately absent
//!
//! There is no `Verb::Export`, and the omission is not an oversight. Our
//! per-board render pipeline (`crates/board-export`) registers no routes at
//! all: it is a poller keyed off `.penpot-sync.json` hash movement, enabled at
//! boot from `exports_enabled` and live-toggled through
//! `VaultRunner::set_renders_enabled`. There is no "export now" surface to
//! call. (The proxy's `/api/export*` routes are the passthrough to upstream
//! Penpot's own exporter service, not a trigger for ours; and
//! `/__api/vault/thumb` serves an ALREADY-RENDERED thumbnail — a read of prior
//! output, not a trigger, so it is not a partial answer either.) Reaching
//! `export` the way the spike did — hand-assembling `PenpotClient` +
//! `ExporterClient` in this process — is exactly the boundary above. Shipping
//! it against `POST /__api/prefs {"exportsEnabled":true}` would be worse than
//! omitting it: that returns immediately while a caller typing `penpot export`
//! expects rendered files to exist. What the verb needs is a handler-free verb
//! core in `board-export` plus a route the app serves — the RC-6 "promote verb
//! cores" work this chapter deliberately avoided by routing Tier-2/3 over
//! loopback.

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::attach::{attach, Surface};

/// A Tier-2/3 verb, reduced to the one request it makes.
///
/// Tier is recorded per verb ([`Verb::tier`]) because it is the honest answer
/// to "what has to be up": Tier 2 needs the running app but the work itself is
/// disk-only, Tier 3 additionally needs the supervised backend. Both attach the
/// same way — the distinction is about what failure to expect, not about how to
/// reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Live sync state: `syncPaused` and `lastOp`, which exist only in the
    /// running daemon (the Tier-1 `sync status` reads the manifest and can
    /// report neither).
    SyncStatus,
    /// Pause the sync daemon. See the module doc: NOT a barrier.
    SyncPause,
    /// Resume the sync daemon.
    SyncResume,
    /// The persisted preferences plus the live facts alongside them.
    PrefsGet,
    /// Save the full preferences object and apply what can apply live.
    PrefsSet,
    /// `git clone` a package repo into `.penpot-packages/<id>`.
    PkgAdd,
    /// Import a fetched package's `.penpot` tree and lock it.
    PkgInstall,
    /// The lighttable board listing.
    Boards,
    /// Full-text search over the vault index.
    Search,
    /// Reveal a vault-relative path in the OS file manager.
    Reveal,
    /// Import a builtin template as a new file.
    NewFromTemplate,
    /// Switch the active vault (tears the stack down and boots the target).
    VaultSwitch,
    /// Reboot the supervised stack in place.
    Reboot,
}

impl Verb {
    /// Every verb, once, so the tables in the tests below cannot silently stop
    /// covering one.
    ///
    /// What actually stops a new variant from defaulting to something wrong is
    /// the absence of `_` arms in [`route`](Verb::route),
    /// [`surface`](Verb::surface), [`method`](Verb::method) and
    /// [`tier`](Verb::tier): adding `Verb::Foo` fails to COMPILE until all four
    /// state its answer. `ALL` is the weaker, second line — it de-duplicates the
    /// three hand-written verb lists the tests used to carry, so they can no
    /// longer drift apart from each other. Its own completeness is not
    /// compiler-checked (nothing forces a new variant into this array), which is
    /// precisely why the four exhaustive matches must keep their `_`-free form.
    pub const ALL: [Verb; 13] = [
        Verb::SyncStatus,
        Verb::SyncPause,
        Verb::SyncResume,
        Verb::PrefsGet,
        Verb::PrefsSet,
        Verb::PkgAdd,
        Verb::PkgInstall,
        Verb::Boards,
        Verb::Search,
        Verb::Reveal,
        Verb::NewFromTemplate,
        Verb::VaultSwitch,
        Verb::Reboot,
    ];

    /// The path this verb requests, on [`Verb::surface`].
    ///
    /// Several verbs deliberately share a path: `sync status` and `prefs get`
    /// are the same GET, and `sync pause`, `sync resume` and `prefs set` are
    /// the same POST with a different body. That is a fact about the app's
    /// surface — `/__api/prefs` is the only place the live sync flag can be
    /// read or written — not a modelling shortcut, and it is why `route()`
    /// alone never identifies a verb.
    pub fn route(&self) -> &'static str {
        match self {
            Verb::SyncStatus | Verb::SyncPause | Verb::SyncResume => "/__api/prefs",
            Verb::PrefsGet | Verb::PrefsSet => "/__api/prefs",
            Verb::PkgAdd => "/__api/packages/fetch",
            Verb::PkgInstall => "/__api/packages/install",
            Verb::Boards => "/__api/vault/boards",
            Verb::Search => "/__api/vault/search",
            Verb::Reveal => "/__api/vault/reveal",
            Verb::NewFromTemplate => "/__api/templates/new",
            Verb::VaultSwitch => "/open",
            Verb::Reboot => "/__api/prefs/reboot",
        }
    }

    /// Which listener serves [`Verb::route`]. Only `vault switch` is on the
    /// control surface — see [`Surface`] for why it has to be.
    ///
    /// **No `_` arm, on purpose** (and the same goes for [`method`](Verb::method)
    /// and [`tier`](Verb::tier)). With one, a future `Verb::Foo` compiled
    /// straight into `Surface::Proxy`/`POST`/tier-2 without anybody choosing
    /// that — and a wrong surface here is the failure mode this file's own test
    /// `vault_switch_is_the_only_control_surface_verb` describes: it "would look
    /// right and fail only under a real switch", because a request sent to the
    /// proxy dies with the proxy the switch tears down. Listing every variant
    /// turns that into a compile error at the moment the variant is added.
    pub fn surface(&self) -> Surface {
        match self {
            Verb::VaultSwitch => Surface::Control,
            Verb::SyncStatus
            | Verb::SyncPause
            | Verb::SyncResume
            | Verb::PrefsGet
            | Verb::PrefsSet
            | Verb::PkgAdd
            | Verb::PkgInstall
            | Verb::Boards
            | Verb::Search
            | Verb::Reveal
            | Verb::NewFromTemplate
            | Verb::Reboot => Surface::Proxy,
        }
    }

    /// The HTTP method [`Verb::route`] is registered with. Exhaustive — see
    /// [`surface`](Verb::surface).
    pub fn method(&self) -> Method {
        match self {
            Verb::SyncStatus | Verb::PrefsGet | Verb::Boards | Verb::Search | Verb::Reveal => {
                Method::GET
            }
            Verb::SyncPause
            | Verb::SyncResume
            | Verb::PrefsSet
            | Verb::PkgAdd
            | Verb::PkgInstall
            | Verb::NewFromTemplate
            | Verb::VaultSwitch
            | Verb::Reboot => Method::POST,
        }
    }

    /// 2 = needs the running app; 3 = needs the supervised backend too.
    /// Straight from the spike's Q2 table. Exhaustive — see
    /// [`surface`](Verb::surface).
    pub fn tier(&self) -> u8 {
        match self {
            Verb::PkgInstall | Verb::NewFromTemplate | Verb::VaultSwitch | Verb::Reboot => 3,
            Verb::SyncStatus
            | Verb::SyncPause
            | Verb::SyncResume
            | Verb::PrefsGet
            | Verb::PrefsSet
            | Verb::PkgAdd
            | Verb::Boards
            | Verb::Search
            | Verb::Reveal => 2,
        }
    }
}

/// Attach to the running app and build `verb`'s request against it.
///
/// The attach happens FIRST, so "no instance running" surfaces as an error
/// naming `penpot daemon` before any network call is attempted — the CLI's most
/// common failure must never read as a connection bug.
pub fn route_for(data_dir: &Path, verb: Verb) -> Result<reqwest::RequestBuilder> {
    let attached = attach(data_dir)?;
    Ok(attached.request_on(verb.surface(), verb.method(), verb.route()))
}

/// Turn a response into a verb result. Pure, so the decision that governs the
/// process's exit status is testable without a socket.
///
/// **Contract: JSON on stdout only on success; any non-2xx is an `Err`.** A
/// failure therefore exits 1, with the server's own explanation on stderr.
///
/// The alternative — always print JSON and set the exit code separately — was
/// considered and rejected. Returning `Err` is what the rest of this CLI
/// already does (every Tier-1 verb propagates one, and so does the no-app
/// attach path), so a caller does not have to know which tier a verb is in to
/// know how it reports failure. It also keeps stdout clean: `penpot boards |
/// jq` never has a failure object piped into it as if it were a board listing.
///
/// The status AND the parsed body both go into the message, because these
/// routes answer with a JSON `{"ok":false,"error":…}` worth reading — losing it
/// would trade a silent success for an uninformative failure.
fn outcome(verb: Verb, status: reqwest::StatusCode, body: &str) -> Result<Value> {
    let parsed: Value =
        serde_json::from_str(body).unwrap_or_else(|_| json!({ "raw": body.to_string() }));
    if status.is_success() {
        return Ok(parsed);
    }
    anyhow::bail!("{verb:?} failed: HTTP {status}\n{parsed:#}")
}

/// Send a built request and hand the response to [`outcome`].
async fn send(verb: Verb, req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("cannot reach the running app for `{:?}`", verb))?;
    let status = resp.status();
    let body = resp.text().await.context("cannot read the response body")?;
    outcome(verb, status, &body)
}

async fn get(data_dir: &Path, verb: Verb, query: &[(&str, String)]) -> Result<Value> {
    let req = route_for(data_dir, verb)?.query(query);
    send(verb, req).await
}

async fn post(data_dir: &Path, verb: Verb, body: Value) -> Result<Value> {
    let req = route_for(data_dir, verb)?.json(&body);
    send(verb, req).await
}

// ---------------------------------------------------------------------------
// Preferences and the sync daemon
// ---------------------------------------------------------------------------

/// `GET /__api/prefs` — the persisted preferences plus the live facts
/// (`syncPaused`, `rendersRunning`, the active vault, `lastOp`).
pub async fn prefs_get(data_dir: &Path) -> Result<Value> {
    get(data_dir, Verb::PrefsGet, &[]).await
}

/// Live sync state. Same route as [`prefs_get`], projected down to the fields
/// a caller asking about sync actually wants — the full preferences blob would
/// bury them.
pub async fn sync_status(data_dir: &Path) -> Result<Value> {
    let prefs = get(data_dir, Verb::SyncStatus, &[]).await?;
    Ok(json!({
        "syncPaused": prefs.get("syncPaused").cloned().unwrap_or(Value::Null),
        "syncEnabled": prefs.pointer("/preferences/syncEnabled").cloned().unwrap_or(Value::Null),
        "vault": prefs.get("vault").cloned().unwrap_or(Value::Null),
        "lastOp": prefs.get("lastOp").cloned().unwrap_or(Value::Null),
    }))
}

/// The full preferences object with one field replaced, ready to POST back.
/// Pulled out of [`prefs_set`] so the read-modify-write rule is testable
/// without a running app: the result must carry EVERY field the GET returned,
/// not just the changed one (see the module doc).
///
/// **`field` must already exist**, and that check is the whole reason this
/// returns a `Result` on the happy path. `Preferences` carries no
/// `deny_unknown_fields`, so an invented key survives the merge, rides the POST,
/// gets dropped by serde on the way in, and comes back `{"ok":true}`:
/// `penpot prefs-set plugnsEnabled false` printed `ok: true`, exited 0, and
/// changed nothing. A write verb reporting success for work it did not do is
/// worse than one that fails — a script cannot tell the difference. The live
/// GET's own keys are the authority on what is settable, so the error can name
/// them exactly rather than repeating a list that could go stale.
fn merged_preferences(current: &Value, field: &str, value: Value) -> Result<Value> {
    let mut prefs = current
        .get("preferences")
        .cloned()
        .context("GET /__api/prefs returned no `preferences` object")?;
    let obj = prefs
        .as_object_mut()
        .context("GET /__api/prefs returned a non-object `preferences`")?;
    anyhow::ensure!(
        obj.contains_key(field),
        "unknown preference `{field}` — the app would accept the write and \
         silently drop it. Valid fields: {}",
        obj.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    obj.insert(field.to_string(), value);
    Ok(prefs)
}

/// Set one preference field, preserving the rest.
pub async fn prefs_set(data_dir: &Path, field: &str, value: Value) -> Result<Value> {
    let current = prefs_get(data_dir).await?;
    let body = merged_preferences(&current, field, value)?;
    post(data_dir, Verb::PrefsSet, body).await
}

/// Pause or resume the sync daemon by flipping `syncEnabled`. Read the module
/// doc before relying on the return: this does not wait for the daemon to go
/// idle, and the change is persisted.
pub async fn sync_set_paused(data_dir: &Path, paused: bool) -> Result<Value> {
    let verb = if paused { Verb::SyncPause } else { Verb::SyncResume };
    let current = prefs_get(data_dir).await?;
    let body = merged_preferences(&current, "syncEnabled", json!(!paused))?;
    post(data_dir, verb, body).await
}

/// `POST /__api/prefs/reboot` — ack-then-detach: a `202` means the reboot was
/// accepted, not that it finished. Its outcome lands in `lastOp`, which
/// [`prefs_get`] reports.
pub async fn reboot(data_dir: &Path) -> Result<Value> {
    post(data_dir, Verb::Reboot, json!({})).await
}

// ---------------------------------------------------------------------------
// Packages and templates
// ---------------------------------------------------------------------------

/// `POST /__api/packages/fetch` — `git clone` into `.penpot-packages/<id>`.
/// The response carries the DERIVED id (from the URL basename, not the
/// package's own manifest — spike RC-13), which is what `pkg install` takes.
pub async fn pkg_add(data_dir: &Path, url: &str, id: Option<&str>) -> Result<Value> {
    let mut body = json!({ "url": url });
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    post(data_dir, Verb::PkgAdd, body).await
}

/// `POST /__api/packages/install` — import a fetched package and lock it.
pub async fn pkg_install(data_dir: &Path, id: &str, name: Option<&str>) -> Result<Value> {
    let mut body = json!({ "id": id });
    if let Some(name) = name {
        body["name"] = json!(name);
    }
    post(data_dir, Verb::PkgInstall, body).await
}

/// `POST /__api/templates/new` — import a builtin template as a new file.
pub async fn new_from_template(
    data_dir: &Path,
    template_id: &str,
    name: Option<&str>,
) -> Result<Value> {
    let mut body = json!({ "templateId": template_id });
    if let Some(name) = name {
        body["name"] = json!(name);
    }
    post(data_dir, Verb::NewFromTemplate, body).await
}

// ---------------------------------------------------------------------------
// Vault index and the vault itself
// ---------------------------------------------------------------------------

/// `GET /__api/vault/boards` — the lighttable listing.
pub async fn boards(data_dir: &Path, project: Option<&str>, sort: Option<&str>) -> Result<Value> {
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(project) = project {
        query.push(("project", project.to_string()));
    }
    if let Some(sort) = sort {
        query.push(("sort", sort.to_string()));
    }
    get(data_dir, Verb::Boards, &query).await
}

/// `GET /__api/vault/search` — full-text search over the vault index.
pub async fn search(data_dir: &Path, q: &str, kind: Option<&str>, limit: Option<usize>) -> Result<Value> {
    let mut query: Vec<(&str, String)> = vec![("q", q.to_string())];
    if let Some(kind) = kind {
        query.push(("kind", kind.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    get(data_dir, Verb::Search, &query).await
}

/// `GET /__api/vault/reveal` — reveal a vault-RELATIVE path in the OS file
/// manager. Fire-and-forget on the app's side, and a no-op on a headless
/// daemon; the route rejects absolute paths and anything with `..`.
pub async fn reveal(data_dir: &Path, path: &str) -> Result<Value> {
    get(data_dir, Verb::Reveal, &[("path", path.to_string())]).await
}

/// `POST /open` on the CONTROL surface — switch the active vault. This is the
/// only correct way for a second process to switch: writing `vaults.json`
/// directly leaves the running app's cached `active` stale, so the process and
/// the disk disagree and the NEXT app start silently opens a vault the user
/// never chose (spike RC-4, observed).
pub async fn vault_switch(data_dir: &Path, path: &str) -> Result<Value> {
    post(data_dir, Verb::VaultSwitch, json!({ "path": path })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier2_verb_with_no_instance_fails_naming_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let err = route_for(dir.path(), Verb::SyncStatus).unwrap_err().to_string();
        assert!(err.contains("penpot daemon"), "got: {err}");
    }

    #[test]
    fn each_verb_maps_to_its_own_route() {
        assert_eq!(Verb::SyncStatus.route(), "/__api/prefs");
        assert_eq!(Verb::VaultSwitch.route(), "/open");
        assert_ne!(Verb::SyncStatus.route(), Verb::VaultSwitch.route());
    }

    /// A one-shot loopback server standing in for one route, so the REAL
    /// `send` path — attach, build, dial, read status and body — is exercised
    /// without booting a stack. Answers exactly one request with the given
    /// status line and body, then closes.
    async fn one_shot_server(status_line: &'static str, body: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request head; we do not care what it says, only that
            // the client is talking to us.
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        });
        port
    }

    /// Point a `runtime.json` at `port` so `attach` succeeds and the verbs
    /// dial the stub instead of a real app.
    fn record_pointing_at(dir: &Path, port: u16) {
        app_core::runtime_record::write_record(
            dir,
            &app_core::runtime_record::RuntimeRecord {
                pid: std::process::id(),
                proxy_port: port,
                control_port: port,
                token: "s3cret".to_string(),
            },
        )
        .unwrap();
    }

    /// **The regression this file exists to prevent.** A 4xx from the app must
    /// be an `Err`, because `main` prints whatever `dispatch` returns and then
    /// exits 0. Before this was fixed, every 4xx and 5xx across all thirteen
    /// verbs exited **0** — so a gate leg written as `penpot boards || fail`
    /// could never fail, and would pass vacuously against a completely broken
    /// route. The Tier-1 verbs and the no-app attach path both exit 1; only
    /// this path swallowed it.
    ///
    /// The body below is verbatim what `home.rs`'s `reveal_action` returns for
    /// an absolute path, which is the cheapest real 4xx this surface has.
    #[tokio::test]
    async fn a_non_2xx_is_an_error_so_the_process_cannot_exit_0() {
        let dir = tempfile::tempdir().unwrap();
        let port = one_shot_server(
            "400 Bad Request",
            r#"{"ok":false,"error":"path must be a within-vault relative path"}"#,
        )
        .await;
        record_pointing_at(dir.path(), port);

        let err = reveal(dir.path(), "/absolute/path")
            .await
            .expect_err("a 400 must not be reported as success")
            .to_string();
        assert!(err.contains("400"), "the status must survive: {err}");
        assert!(
            err.contains("within-vault"),
            "the server's own explanation must survive into the message: {err}"
        );
    }

    /// The other half: a 2xx still returns the parsed body, unchanged. Without
    /// this, "make failures fail" could quietly turn into "make everything
    /// fail".
    #[tokio::test]
    async fn a_2xx_still_returns_the_parsed_body() {
        let dir = tempfile::tempdir().unwrap();
        let port = one_shot_server("200 OK", r#"{"count":2,"cards":[]}"#).await;
        record_pointing_at(dir.path(), port);

        let out = boards(dir.path(), None, None).await.unwrap();
        assert_eq!(out["count"], json!(2));
    }

    /// The same rule stated over the whole status space, cheaply. `202` is the
    /// one that would bite: `reboot` and `POST /__api/prefs/vault` ack and
    /// detach, so treating "not 200" as failure would make a working reboot
    /// report an error.
    #[test]
    fn only_a_non_2xx_is_a_failure() {
        use reqwest::StatusCode;
        for ok in [StatusCode::OK, StatusCode::ACCEPTED, StatusCode::NO_CONTENT] {
            assert!(
                outcome(Verb::Reboot, ok, r#"{"ok":true}"#).is_ok(),
                "{ok} must be a success"
            );
        }
        for bad in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
            // `boards`/`search` answer this while the index is still building.
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(
                outcome(Verb::Boards, bad, r#"{"ok":false}"#).is_err(),
                "{bad} must be a failure"
            );
        }
    }

    /// A failing route that answers with something other than JSON (an nginx
    /// error page, a proxy hiccup) must still fail, and must still show what it
    /// said — an empty or unparseable body is exactly when a caller most needs
    /// the text.
    #[test]
    fn a_non_json_failure_body_still_fails_and_still_reports_itself() {
        use reqwest::StatusCode;
        let err = outcome(Verb::Boards, StatusCode::BAD_GATEWAY, "<html>502</html>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("502"), "got: {err}");
        assert!(err.contains("<html>502</html>"), "the raw body must survive: {err}");
    }

    /// The message has to name the verb, because a shell gate running several
    /// verbs sees only stderr.
    #[test]
    fn a_failure_message_names_the_verb() {
        use reqwest::StatusCode;
        let err = outcome(Verb::PkgInstall, StatusCode::NOT_FOUND, r#"{"error":"nope"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("PkgInstall"), "got: {err}");
    }

    /// Every verb, in one table, so a new one cannot be added without stating
    /// its surface, method and tier — and so a typo in a path is a test
    /// failure rather than a 404 nobody sees until the app is running.
    ///
    /// Driven off [`Verb::ALL`], not off `expected`: iterating the expectations
    /// meant a verb missing from the table was simply never checked, and the
    /// test stayed green describing twelve of thirteen verbs. The length
    /// assertion closes the other direction (an entry for a verb that no longer
    /// exists).
    #[test]
    fn the_verb_table_is_what_it_claims_to_be() {
        let expected = [
            (Verb::SyncStatus, Surface::Proxy, Method::GET, "/__api/prefs", 2),
            (Verb::SyncPause, Surface::Proxy, Method::POST, "/__api/prefs", 2),
            (Verb::SyncResume, Surface::Proxy, Method::POST, "/__api/prefs", 2),
            (Verb::PrefsGet, Surface::Proxy, Method::GET, "/__api/prefs", 2),
            (Verb::PrefsSet, Surface::Proxy, Method::POST, "/__api/prefs", 2),
            (Verb::PkgAdd, Surface::Proxy, Method::POST, "/__api/packages/fetch", 2),
            (Verb::PkgInstall, Surface::Proxy, Method::POST, "/__api/packages/install", 3),
            (Verb::Boards, Surface::Proxy, Method::GET, "/__api/vault/boards", 2),
            (Verb::Search, Surface::Proxy, Method::GET, "/__api/vault/search", 2),
            (Verb::Reveal, Surface::Proxy, Method::GET, "/__api/vault/reveal", 2),
            (Verb::NewFromTemplate, Surface::Proxy, Method::POST, "/__api/templates/new", 3),
            (Verb::VaultSwitch, Surface::Control, Method::POST, "/open", 3),
            (Verb::Reboot, Surface::Proxy, Method::POST, "/__api/prefs/reboot", 3),
        ];
        assert_eq!(
            expected.len(),
            Verb::ALL.len(),
            "the table describes a verb that no longer exists"
        );
        for verb in Verb::ALL {
            let (_, surface, method, route, tier) = expected
                .iter()
                .find(|(v, ..)| *v == verb)
                .unwrap_or_else(|| panic!("{verb:?} is missing from the verb table"));
            assert_eq!(verb.surface(), *surface, "{verb:?} surface");
            assert_eq!(&verb.method(), method, "{verb:?} method");
            assert_eq!(verb.route(), *route, "{verb:?} route");
            assert_eq!(verb.tier(), *tier, "{verb:?} tier");
        }
    }

    /// `vault switch` is the one verb that MUST NOT ride the proxy: the switch
    /// tears the proxy down, so a request sent there dies before the route can
    /// answer. Routing it to the proxy would look right and fail only under a
    /// real switch.
    #[test]
    fn vault_switch_is_the_only_control_surface_verb() {
        let control: Vec<Verb> = Verb::ALL
            .into_iter()
            .filter(|v| v.surface() == Surface::Control)
            .collect();
        assert_eq!(control, vec![Verb::VaultSwitch]);
    }

    /// The prefs verbs share one route on purpose (see `Verb::route`'s doc).
    /// Asserted so the sharing reads as a decision rather than a collision
    /// somebody should "fix".
    #[test]
    fn the_prefs_verbs_deliberately_share_one_route() {
        assert_eq!(Verb::SyncStatus.route(), Verb::PrefsGet.route());
        assert_eq!(Verb::SyncPause.route(), Verb::PrefsSet.route());
        assert_eq!(Verb::SyncPause.route(), Verb::SyncResume.route());
        // ...and the method is what separates read from write.
        assert_ne!(Verb::PrefsGet.method(), Verb::PrefsSet.method());
    }

    /// The read-modify-write rule: a preferences POST must carry every field
    /// the GET returned. `Preferences` gives every field a `serde(default)`, so
    /// a body that omits one silently resets it — the exact bug the spike's
    /// `{"syncEnabled":false}` shortcut had.
    /// The fixture both preferences tests below run against: a realistic `GET
    /// /__api/prefs` body, with the four fields the app actually has.
    fn a_prefs_get_response() -> Value {
        json!({
            "preferences": {
                "syncEnabled": true,
                "exportsEnabled": true,
                "pluginsEnabled": false,
                "cspEnabled": false,
            },
            "syncPaused": false,
        })
    }

    #[test]
    fn a_preferences_write_preserves_every_untouched_field() {
        let current = a_prefs_get_response();
        let merged = merged_preferences(&current, "syncEnabled", json!(false)).unwrap();
        assert_eq!(merged["syncEnabled"], json!(false), "the changed field");
        assert_eq!(merged["pluginsEnabled"], json!(false), "must not revert to the default");
        assert_eq!(merged["cspEnabled"], json!(false), "must not revert to the default");
        assert_eq!(merged["exportsEnabled"], json!(true));
        assert_eq!(merged.as_object().unwrap().len(), 4, "no field invented, none dropped");
    }

    /// A misspelt field must FAIL, not report success. `Preferences` has no
    /// `deny_unknown_fields`, so before this check the route accepted the body,
    /// serde dropped the unknown key, and `penpot prefs-set plugnsEnabled false`
    /// printed `ok: true` and exited 0 having changed nothing — a write verb
    /// claiming work it did not do.
    #[test]
    fn a_preferences_write_refuses_a_field_the_app_does_not_have() {
        let err = merged_preferences(&a_prefs_get_response(), "plugnsEnabled", json!(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("plugnsEnabled"), "the typo must be quoted back: {err}");
        // The message has to be actionable: a user who typo'd needs the list,
        // and the list has to come from the live response, not a stale copy.
        for field in ["syncEnabled", "exportsEnabled", "pluginsEnabled", "cspEnabled"] {
            assert!(err.contains(field), "`{field}` must be offered: {err}");
        }
    }

    /// The other half: the four real fields must all still be settable, so the
    /// guard above cannot quietly become "prefs-set never works".
    #[test]
    fn every_real_preference_field_is_still_settable() {
        let current = a_prefs_get_response();
        for field in ["syncEnabled", "exportsEnabled", "pluginsEnabled", "cspEnabled"] {
            let merged = merged_preferences(&current, field, json!(true))
                .unwrap_or_else(|e| panic!("`{field}` must be settable: {e}"));
            assert_eq!(merged[field], json!(true));
            assert_eq!(merged.as_object().unwrap().len(), 4, "no field invented");
        }
    }

    #[test]
    fn a_preferences_write_refuses_a_response_without_preferences() {
        let err = merged_preferences(&json!({"syncPaused": false}), "syncEnabled", json!(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `preferences`"), "got: {err}");
    }

    /// Every Tier-2/3 verb, not just the one in the brief's test, must fail
    /// with the same actionable message when nothing is running — the attach
    /// is in `route_for`, so this holds by construction, and this test is what
    /// keeps it that way if a verb ever grows its own request-building path.
    #[test]
    fn no_verb_reaches_the_network_without_a_running_instance() {
        let dir = tempfile::tempdir().unwrap();
        for verb in Verb::ALL {
            let err = route_for(dir.path(), verb).unwrap_err().to_string();
            assert!(err.contains("penpot daemon"), "{verb:?} got: {err}");
        }
    }
}
