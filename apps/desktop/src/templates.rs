//! N6 — offline template gallery + New-from-template (pillar 7).
//!
//! The "Arduino Examples menu" move: an OFFLINE gallery seeded from the 15
//! builtin-template binfiles the runtime bundle already ships at
//! `<runtime>/backend/builtin-templates/`. Nothing here reaches the network:
//! the catalog is enumerated from disk, and "New file from template" is a real
//! RPC import into the ACTIVE vault's default project — the sync daemon then
//! materializes the `.penpot` tree on disk (folder = source of truth). Templates
//! are the first foothold of the package ecosystem
//! (`docs/ecosystem-concept.md`): "surface, don't apply" — nothing is applied
//! silently, the user explicitly creates a NEW file.
//!
//! Routes (own), served same-origin through the proxy's extra router (auth
//! cookie + RPC for free), plain HTML/vanilla JS like `/__home` & `/__palette`:
//! - `GET  /__templates` — the gallery page.
//! - `GET  /__api/templates` — the shippable catalog (JSON).
//! - `POST /__api/templates/new {templateId, name?}` — import-as-new into the
//!   default project, settle the file to a round-trip fixpoint, and return the
//!   new file's `/#/workspace?…` deep link.
//!
//! ## Format recipe (verified in the N6 spike; do not re-litigate)
//! The 15 binfiles come in two on-disk formats, classified by magic bytes,
//! which only affects the IMPORT-AS-NEW `version` field:
//! - **v3-zip** (`PK\x03\x04`, 4 templates): `import-binfile` WITHOUT a version
//!   field (auto-detect).
//! - **legacy binfile-v1** (`0x010B1A86`, 11 templates): `import-binfile` WITH
//!   `version=1` (REQUIRED — omitting it → HTTP 500). Once imported the file is
//!   a normal v3 tree in the DB, so in-place re-imports are version-less again.
//!
//! ## Settle-until-fixpoint (the P0-critical part)
//! The daemon materializes the `.penpot` tree by exporting the DB. That first
//! on-disk export MUST be its own round-trip fixpoint — export → normalize →
//! in-place re-import → re-export yields an identical semantic hash — otherwise
//! the core invariant (delete the DB, rebuild from disk) surfaces a spurious
//! change on the FIRST rebuild.
//!
//! Whether the first export is already a fixpoint is NOT a function of the
//! origin format: legacy origins get deterministic migrations on their first
//! in-place re-import, and some v3 origins (`tokens-starter-kit`,
//! `penpot-design-system`) ship cached thumbnails + orphaned media that Penpot
//! GCs on the next in-place re-import. So we do NOT format-gate the settle. We
//! SETTLE UNTIL FIXPOINT: after import-as-new we run export → normalize →
//! in-place re-import → re-export cycles until two consecutive exports have an
//! equal semantic hash (per `scripts/roundtrip.py`), capped at
//! [`MAX_SETTLE_CYCLES`] with a hard error if it never converges. A clean file
//! converges after one cycle; migration/GC-heavy files after two. Because the
//! semantic hash is computed with the exact `sync-core` normalizer the daemon
//! uses, "two consecutive exports equal" proves the DB's current export is a
//! fixpoint — so the tree the daemon later writes surfaces no spurious change.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::StatusCode;
use penpot_rpc::{Auth, PenpotClient};
use serde::{Deserialize, Serialize};
use serde_json::json;

const TEMPLATES_PAGE_HTML: &str = include_str!("templates.html");

/// The builtin-templates subdirectory inside the resolved runtime dir's
/// `backend/` tree (they ship inside the backend extraction — N2 bundle).
pub const BUILTIN_TEMPLATES_REL: &str = "backend/builtin-templates";

// ---------------------------------------------------------------------------
// Catalog (pure, unit-tested)
// ---------------------------------------------------------------------------

/// On-disk binfile format, classified by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemplateFormat {
    /// binfile-v3 ZIP archive (magic `PK\x03\x04`). Import version-less.
    V3Zip,
    /// legacy binfile-v1 (magic `0x010B1A86`). Import with `version=1`.
    LegacyV1,
}

impl TemplateFormat {
    /// The `version` multipart field for import-as-new: `None` for v3
    /// (auto-detect), `Some(1)` for legacy binfile-v1.
    pub fn import_version(self) -> Option<u8> {
        match self {
            TemplateFormat::V3Zip => None,
            TemplateFormat::LegacyV1 => Some(1),
        }
    }

    fn label(self) -> &'static str {
        match self {
            TemplateFormat::V3Zip => "v3-zip",
            TemplateFormat::LegacyV1 => "legacy-v1",
        }
    }
}

/// Classify a binfile by its leading bytes. `None` = neither shippable format
/// (the file would be dropped from the catalog with a logged reason).
pub fn classify_magic(bytes: &[u8]) -> Option<TemplateFormat> {
    if bytes.len() >= 4 && &bytes[0..2] == b"PK" {
        // PK\x03\x04 (local file header) — a ZIP, i.e. binfile-v3.
        Some(TemplateFormat::V3Zip)
    } else if bytes.len() >= 4 && bytes[0..4] == [0x01, 0x0B, 0x1A, 0x86] {
        Some(TemplateFormat::LegacyV1)
    } else {
        None
    }
}

/// Derive a human display name from the template's file id (its filename),
/// e.g. `black-white-mobile-templates` → `Black White Mobile Templates`.
pub fn display_name_from_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One shippable catalog entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateEntry {
    /// Stable id = the binfile's filename (e.g. `plants-app`).
    pub id: String,
    /// Human display name.
    pub name: String,
    /// On-disk binfile format.
    pub format: TemplateFormat,
    /// File size in bytes (shown as a hint in the gallery).
    pub size_bytes: u64,
}

/// A dropped (unshippable) template with the reason it was excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTemplate {
    pub id: String,
    pub reason: String,
}

/// The enumerated catalog: shippable entries (sorted by name) + any drops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub entries: Vec<TemplateEntry>,
    pub dropped: Vec<DroppedTemplate>,
}

impl Catalog {
    pub fn find(&self, id: &str) -> Option<&TemplateEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

/// Enumerate the builtin-templates directory into a catalog. Reads only the
/// first 4 bytes of each file for classification (cheap, offline). Files that
/// classify as neither format are dropped with a logged reason; sub-directories
/// and dotfiles are ignored. Deterministic: entries are sorted by display name
/// then id.
pub fn enumerate_catalog(builtin_dir: &Path) -> Catalog {
    let mut entries = Vec::new();
    let mut dropped = Vec::new();

    let read_dir = match std::fs::read_dir(builtin_dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(
                dir = %builtin_dir.display(),
                error = %e,
                "builtin-templates dir not readable; template gallery will be empty"
            );
            return Catalog::default();
        }
    };

    for dent in read_dir.flatten() {
        let path = dent.path();
        let Some(id) = path.file_name().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let meta = match dent.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue, // dirs / unreadable → skip silently
        };
        let mut head = [0u8; 4];
        let n = read_head(&path, &mut head);
        match classify_magic(&head[..n]) {
            Some(format) => entries.push(TemplateEntry {
                name: display_name_from_id(&id),
                format,
                size_bytes: meta.len(),
                id,
            }),
            None => {
                let reason = format!(
                    "unrecognized magic bytes {:02x?} (not binfile-v3 `PK\\x03\\x04` nor legacy-v1 `0x010B1A86`)",
                    &head[..n]
                );
                tracing::warn!(template = %id, reason = %reason, "dropping unshippable builtin template");
                dropped.push(DroppedTemplate { id, reason });
            }
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    dropped.sort_by(|a, b| a.id.cmp(&b.id));
    Catalog { entries, dropped }
}

/// Read up to `buf.len()` leading bytes of a file, returning how many were
/// read (0 on any error — treated as an unclassifiable file).
fn read_head(path: &Path, buf: &mut [u8]) -> usize {
    use std::io::Read;
    std::fs::File::open(path)
        .and_then(|mut f| f.read(buf))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

/// Router state: where the templates live + how to reach the backend for the
/// import. Cloned per boot (rebuilt on a vault switch, so `team_id` is fresh).
pub struct TemplatesState {
    /// `<runtime>/backend/builtin-templates`.
    pub builtin_dir: PathBuf,
    /// Backend RPC base URL (loopback).
    pub backend_base: String,
    /// Provisioned access token for `Authorization: Token …` (None → the
    /// new-from-template action is unavailable, but the gallery still lists).
    pub token: Option<String>,
    /// The single team's id (deep-link `team-id` + import target team).
    pub team_id: String,
}

/// Build the N6 template routes for the proxy's extra router.
pub fn router(state: Arc<TemplatesState>) -> Router {
    Router::new()
        .route("/__templates", get(gallery_page))
        .route("/__api/templates", get(list_templates))
        .route("/__api/templates/new", post(new_from_template))
        .with_state(state)
}

async fn gallery_page() -> Html<&'static str> {
    Html(TEMPLATES_PAGE_HTML)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    count: usize,
    templates: Vec<TemplateEntry>,
}

async fn list_templates(State(state): State<Arc<TemplatesState>>) -> Response {
    let dir = state.builtin_dir.clone();
    let catalog = tokio::task::spawn_blocking(move || enumerate_catalog(&dir))
        .await
        .unwrap_or_default();
    Json(ListResponse {
        count: catalog.entries.len(),
        templates: catalog.entries,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewFromTemplateReq {
    template_id: String,
    #[serde(default)]
    name: Option<String>,
}

/// The successful new-from-template payload.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewFromTemplateResp {
    ok: bool,
    file_id: String,
    page_id: Option<String>,
    name: String,
    format: TemplateFormat,
    /// Whether the tree changed during settling (a real migration/GC happened,
    /// i.e. more than the single proving cycle a clean file needs).
    settled: bool,
    /// Number of in-place re-import cycles run to reach the round-trip fixpoint.
    settle_cycles: usize,
    /// The exact `/#/workspace?team-id&file-id&page-id` deep link.
    deep_link: String,
}

/// Maximum accepted display-name length. Mirrors Penpot 2.16.2's import-binfile
/// `name` schema, `[:string {:max 250}]` (verified against the runtime jar). The
/// backend counts via Java `String.length()` (UTF-16 code units) and rejects an
/// over-long name with an opaque 5xx (a 300-char name surfaced as HTTP 502); we
/// cap here and return a clean 4xx instead. Derived default names (from the
/// template id) are always short, so this only ever trips a user-supplied
/// `name`.
const MAX_DISPLAY_NAME_CHARS: usize = 250;

/// Hard cap on settle cycles (each an in-place re-import) before we give up on
/// reaching a round-trip fixpoint. A clean file converges after 1 cycle,
/// migration/GC-heavy files after 2; 3 leaves headroom without looping forever.
const MAX_SETTLE_CYCLES: usize = 3;

/// Validate a display name's length. `Err` carries a user-facing 4xx message.
/// Counts UTF-16 code units to match the backend's Java-`String.length()` schema
/// exactly, so this accepts a name iff the backend would (no 5xx slips through).
fn validate_display_name(name: &str) -> Result<(), String> {
    let len = name.encode_utf16().count();
    if len > MAX_DISPLAY_NAME_CHARS {
        return Err(format!(
            "name too long: {len} characters (max {MAX_DISPLAY_NAME_CHARS})"
        ));
    }
    Ok(())
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

async fn new_from_template(
    State(state): State<Arc<TemplatesState>>,
    Json(req): Json<NewFromTemplateReq>,
) -> Response {
    // Validate the id against the enumerated catalog (never trust the client
    // path — the id must be an EXACT catalog entry, so no traversal possible).
    let builtin_dir = state.builtin_dir.clone();
    let want = req.template_id.clone();
    let catalog = tokio::task::spawn_blocking(move || enumerate_catalog(&builtin_dir))
        .await
        .unwrap_or_default();
    let Some(entry) = catalog.find(&want).cloned() else {
        return bad_request(format!("unknown templateId {:?}", req.template_id));
    };

    let Some(token) = state.token.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "no access token provisioned; cannot import"})),
        )
            .into_response();
    };

    let display = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| entry.name.clone());

    if let Err(msg) = validate_display_name(&display) {
        return bad_request(msg);
    }

    let bytes = match std::fs::read(state.builtin_dir.join(&entry.id)) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("reading template failed: {e}")})),
            )
                .into_response()
        }
    };

    let client = PenpotClient::new(&state.backend_base).with_auth(Auth::Token(token));
    match import_new_from_template(&client, &state.team_id, &entry, &display, bytes).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => {
            tracing::error!(template = %entry.id, error = %e, "new-from-template failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"ok": false, "error": format!("import failed: {e}")})),
            )
                .into_response()
        }
    }
}

/// Resolve the target project id for a new file: the team's default ("Drafts")
/// project, falling back to the first live project.
async fn default_project_id(client: &PenpotClient, team_id: &str) -> anyhow::Result<String> {
    let projects = client.get_projects(team_id).await?;
    let live = || projects.iter().filter(|p| p.deleted_at.is_none());
    if let Some(p) = live().find(|p| p.is_default) {
        return Ok(p.id.clone());
    }
    live()
        .map(|p| p.id.clone())
        .next()
        .ok_or_else(|| anyhow::anyhow!("no live project to import into for team {team_id}"))
}

/// The core new-from-template mechanism (import-as-new + settle-until-fixpoint),
/// returning the deep-link payload. The sync daemon materializes the `.penpot`
/// tree on disk on its own poll cycle; because the DB is already settled to a
/// round-trip fixpoint here, the tree it writes surfaces no spurious change.
async fn import_new_from_template(
    client: &PenpotClient,
    team_id: &str,
    entry: &TemplateEntry,
    display: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<NewFromTemplateResp> {
    let project_id = default_project_id(client, team_id).await?;

    // 1. import-as-new (no file-id → mints a fresh id, remaps internal ids).
    //    The `version` field depends on the origin format (legacy needs 1).
    let ids = client
        .import_binfile_versioned(display, &project_id, None, bytes, entry.format.import_version())
        .await?;
    let file_id = ids
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("import-binfile returned no file id"))?;
    tracing::info!(
        template = %entry.id, format = entry.format.label(), file = %file_id,
        "imported template as new file"
    );

    // 2. Settle until the DB's export is a round-trip fixpoint (see module docs).
    let settle_cycles = settle_to_fixpoint(client, &file_id, &project_id, display).await?;
    tracing::info!(
        template = %entry.id, file = %file_id, cycles = settle_cycles,
        "settled template to round-trip fixpoint"
    );

    // 3. First page id for the deep link.
    let page_id = first_page_id(client, &file_id).await;
    let deep_link = vault_index::workspace_deep_link(team_id, &file_id, page_id.as_deref());

    Ok(NewFromTemplateResp {
        ok: true,
        file_id,
        page_id,
        name: display.to_string(),
        format: entry.format,
        settled: settle_cycles > 1,
        settle_cycles,
        deep_link,
    })
}

/// Result of unzipping + normalizing one export: its semantic tree hash and the
/// deterministically re-zipped normalized tree (the exact bytes the daemon
/// would write to disk, ready to re-import in place). Computed off the async
/// runtime because it walks/parses the whole tree (thousands of files for the
/// heavy templates).
fn unzip_normalize_hash(zip: Vec<u8>) -> anyhow::Result<(String, Vec<u8>)> {
    let dir = tempfile::tempdir()?;
    sync_core::unzip_to(&zip, dir.path())?;
    sync_core::normalize_tree(dir.path())?;
    let hash = sync_core::semantic_tree_hash(dir.path())?;
    let norm_zip = sync_core::zip_dir(dir.path())?;
    Ok((hash, norm_zip))
}

/// Run export → normalize → in-place re-import → re-export cycles until two
/// consecutive exports have an equal semantic hash — i.e. the DB's current
/// export is its own round-trip fixpoint (per `scripts/roundtrip.py` semantics).
/// Returns the number of in-place re-import cycles run (>= 1). Errors if it does
/// not converge within [`MAX_SETTLE_CYCLES`]. Version-less imports throughout:
/// the exported tree is always a v3 zip regardless of origin format.
async fn settle_to_fixpoint(
    client: &PenpotClient,
    file_id: &str,
    project_id: &str,
    display: &str,
) -> anyhow::Result<usize> {
    let mut prev_hash: Option<String> = None;
    let mut cycles = 0usize;
    loop {
        // Export the CURRENT DB state (embed_assets=true matches the daemon).
        let exported = client.export_binfile(file_id, false, true).await?;
        let zip = client.download_exported_binfile(&exported.uri).await?;
        let (hash, norm_zip) =
            tokio::task::spawn_blocking(move || unzip_normalize_hash(zip)).await??;

        if prev_hash.as_deref() == Some(hash.as_str()) {
            // Two consecutive exports equal → the current export is a fixpoint.
            return Ok(cycles);
        }
        if cycles >= MAX_SETTLE_CYCLES {
            anyhow::bail!(
                "template {file_id} did not reach a round-trip fixpoint after \
                 {MAX_SETTLE_CYCLES} settle cycles (last semantic hash {hash})"
            );
        }

        // Not yet stable: re-import the normalized export in place and loop.
        client
            .import_binfile(display, project_id, Some(file_id), norm_zip)
            .await?;
        prev_hash = Some(hash);
        cycles += 1;
    }
}

/// Best-effort fetch of a file's first page id (`data.pages[0]`) for the deep
/// link. A missing page id just yields a workspace link without `page-id`.
async fn first_page_id(client: &PenpotClient, file_id: &str) -> Option<String> {
    let file = client.get_file(file_id).await.ok()?;
    file.get("data")?
        .get("pages")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_magic_recognizes_both_formats() {
        assert_eq!(classify_magic(b"PK\x03\x04rest"), Some(TemplateFormat::V3Zip));
        assert_eq!(
            classify_magic(&[0x01, 0x0B, 0x1A, 0x86, 0x00]),
            Some(TemplateFormat::LegacyV1)
        );
        // Too short / unknown magic → unclassifiable.
        assert_eq!(classify_magic(b"PK"), None);
        assert_eq!(classify_magic(&[0, 1, 2, 3]), None);
        assert_eq!(classify_magic(&[]), None);
    }

    #[test]
    fn import_recipe_matches_the_spike() {
        // Format only affects the import-as-new `version` field; the fixpoint
        // settle is format-agnostic (a round-trip check, not a format gate).
        assert_eq!(TemplateFormat::V3Zip.import_version(), None);
        assert_eq!(TemplateFormat::LegacyV1.import_version(), Some(1));
    }

    #[test]
    fn display_name_length_is_capped_with_a_clean_error() {
        // Names up to the cap are accepted. 'é' (U+00E9) is one UTF-16 unit, so
        // 250 of them is exactly at the backend's `[:string {:max 250}]` limit.
        assert!(validate_display_name("Plants App").is_ok());
        let at_cap: String = "a".repeat(MAX_DISPLAY_NAME_CHARS);
        assert!(validate_display_name(&at_cap).is_ok());
        let bmp_at_cap: String = "é".repeat(MAX_DISPLAY_NAME_CHARS);
        assert!(validate_display_name(&bmp_at_cap).is_ok());

        // One over the cap → Err with a "too long" message (→ 400, not 502).
        let too_long: String = "a".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        let err = validate_display_name(&too_long).unwrap_err();
        assert!(err.contains("too long"), "message was: {err}");
        let too_long_bmp: String = "é".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        assert!(validate_display_name(&too_long_bmp).is_err());

        // Astral chars are 2 UTF-16 units each (as Java counts them), so 126
        // of them = 252 units > 250 → rejected, exactly as the backend would.
        let astral: String = "😀".repeat(126);
        assert_eq!(astral.chars().count(), 126);
        assert!(validate_display_name(&astral).is_err());
    }

    #[test]
    fn display_name_title_cases_the_id() {
        assert_eq!(
            display_name_from_id("black-white-mobile-templates"),
            "Black White Mobile Templates"
        );
        assert_eq!(display_name_from_id("ux-notes"), "Ux Notes");
        assert_eq!(display_name_from_id("plants-app"), "Plants App");
        assert_eq!(display_name_from_id("welcome"), "Welcome");
        assert_eq!(display_name_from_id("tokens_starter_kit"), "Tokens Starter Kit");
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[test]
    fn enumerate_classifies_ships_and_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "plants-app", b"PK\x03\x04....payload");
        write(d, "welcome", &[0x01, 0x0B, 0x1A, 0x86, 0xff, 0xff]);
        write(d, "broken-thing", b"not a binfile at all");
        write(d, ".hidden", b"PK\x03\x04"); // dotfile ignored
        std::fs::create_dir(d.join("a-subdir")).unwrap(); // dir ignored

        let cat = enumerate_catalog(d);
        assert_eq!(cat.entries.len(), 2, "two shippable");
        // Sorted by display name: "Plants App" < "Welcome".
        assert_eq!(cat.entries[0].id, "plants-app");
        assert_eq!(cat.entries[0].format, TemplateFormat::V3Zip);
        assert_eq!(cat.entries[1].id, "welcome");
        assert_eq!(cat.entries[1].format, TemplateFormat::LegacyV1);
        // size_bytes reflects the actual file length.
        assert_eq!(cat.entries[0].size_bytes, b"PK\x03\x04....payload".len() as u64);

        // The unrecognized file is dropped with a reason (never silently).
        assert_eq!(cat.dropped.len(), 1);
        assert_eq!(cat.dropped[0].id, "broken-thing");
        assert!(cat.dropped[0].reason.contains("magic"));

        assert!(cat.find("plants-app").is_some());
        assert!(cat.find("nope").is_none());
    }

    #[test]
    fn enumerate_missing_dir_is_empty_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = enumerate_catalog(&tmp.path().join("does-not-exist"));
        assert!(cat.entries.is_empty());
        assert!(cat.dropped.is_empty());
    }
}
