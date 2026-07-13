//! Live integration tests against a real Penpot stack (see tests/README.md).
//!
//! All tests are `#[ignore]`d: they need a running stack and two env vars:
//!
//! - `PENPOT_RPC_LIVE_BASE_URL` — the **proxy** base URL (e.g.
//!   `http://localhost:8890`). It must be the proxy, not the bare backend:
//!   `export-binfile` artifact URIs point at `PENPOT_PUBLIC_URI` and only the
//!   proxy fulfils `/assets/**` downloads (the backend alone answers
//!   204 + x-accel-redirect).
//! - `PENPOT_RPC_LIVE_TOKEN` — a personal access token (the headless bin
//!   persists one in `<data_dir>/credentials.json`, key `access_token`).
//!
//! Run with: `cargo test -p penpot-rpc --test live -- --ignored`
//!
//! Each test provisions its own project (unique name), asserts, and deletes
//! the project afterwards, so tests are parallel-safe and re-runnable.

use penpot_rpc::{Auth, PenpotClient};
use serde_json::{json, Value};

const ROOT_FRAME: &str = "00000000-0000-0000-0000-000000000000";

fn client() -> PenpotClient {
    let base = std::env::var("PENPOT_RPC_LIVE_BASE_URL")
        .expect("set PENPOT_RPC_LIVE_BASE_URL to the proxy base URL");
    let token =
        std::env::var("PENPOT_RPC_LIVE_TOKEN").expect("set PENPOT_RPC_LIVE_TOKEN to a token");
    PenpotClient::new(base).with_auth(Auth::Token(token))
}

/// Default team id from the profile (the M1 single-user always has one).
async fn default_team_id(client: &PenpotClient) -> String {
    client
        .get_profile()
        .await
        .expect("get-profile")
        .default_team_id
        .expect("profile has no defaultTeamId")
}

/// RFC-4122-shaped v4 uuid without pulling in the `uuid` crate. Uniqueness
/// comes from nanosecond time + pid + an atomic counter — plenty for test
/// session/shape ids.
fn uuid_v4() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let a = (nanos as u64) ^ ((std::process::id() as u64) << 32);
    let b = ((nanos >> 64) as u64)
        .wrapping_add((nanos as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(CTR.fetch_add(1, Ordering::Relaxed).wrapping_mul(0xD1B5_4A32_D192_ED03));
    format!(
        "{:08x}-{:04x}-4{:03x}-{:1x}{:03x}-{:012x}",
        (a >> 32) & 0xffff_ffff,
        (a >> 16) & 0xffff,
        a & 0x0fff,
        0x8 | ((b >> 60) & 0x3),
        (b >> 48) & 0x0fff,
        b & 0xffff_ffff_ffff,
    )
}

/// The verified `add-obj` rectangle recipe from docs/m0/rpc-endpoints.md
/// §update-file: `selrect`/`points`/`transform`/`transformInverse` must be
/// supplied by the client.
fn add_rect_change(shape_id: &str, page_id: &str, name: &str, x: f64, y: f64) -> Value {
    let (w, h) = (200.0, 150.0);
    json!({
        "type": "add-obj",
        "id": shape_id,
        "pageId": page_id,
        "frameId": ROOT_FRAME,
        "parentId": ROOT_FRAME,
        "obj": {
            "id": shape_id,
            "type": "rect",
            "name": name,
            "x": x, "y": y, "width": w, "height": h,
            "rotation": 0,
            "selrect": {"x": x, "y": y, "width": w, "height": h,
                         "x1": x, "y1": y, "x2": x + w, "y2": y + h},
            "points": [{"x": x, "y": y}, {"x": x + w, "y": y},
                        {"x": x + w, "y": y + h}, {"x": x, "y": y + h}],
            "transform": {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0},
            "transformInverse": {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0},
            "parentId": ROOT_FRAME,
            "frameId": ROOT_FRAME,
            "fills": [{"fillColor": "#B1B2B5", "fillOpacity": 1}],
            "strokes": []
        }
    })
}

/// Does `get-file`'s data contain the shape on the given page?
fn has_shape(file: &Value, page_id: &str, shape_id: &str) -> bool {
    file.pointer(&format!("/data/pagesIndex/{page_id}/objects/{shape_id}"))
        .is_some()
}

// ---------------------------------------------------------------------
// Poll surface: get-projects / get-project-files (revn + modifiedAt)
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs a live stack (PENPOT_RPC_LIVE_BASE_URL / PENPOT_RPC_LIVE_TOKEN)"]
async fn live_poll_surface_projects_and_files() {
    let client = client();
    let team_id = default_team_id(&client).await;

    // The default "Drafts" project always exists (and is never soft-deleted).
    let projects = client.get_projects(&team_id).await.expect("get-projects");
    assert!(
        projects
            .iter()
            .any(|p| p.is_default && p.deleted_at.is_none()),
        "no live default project in {projects:?}"
    );

    let project = client
        .create_project(&team_id, &format!("rpc-live-poll-{}", uuid_v4()))
        .await
        .expect("create-project");
    assert_eq!(project.team_id, team_id);
    assert!(!project.is_default);

    // A created project shows up in get-projects, not soft-deleted.
    let projects = client.get_projects(&team_id).await.expect("get-projects");
    assert!(projects
        .iter()
        .any(|p| p.id == project.id && p.deleted_at.is_none()));

    // Fresh project has no files; then exactly the one we create.
    assert!(client
        .get_project_files(&project.id)
        .await
        .expect("get-project-files")
        .is_empty());

    let file = client
        .create_file(&project.id, "poll-me")
        .await
        .expect("create-file");
    assert_eq!(file.revn, 0);
    assert!(file.first_page_id().is_some(), "create-file data has a page");

    let files = client
        .get_project_files(&project.id)
        .await
        .expect("get-project-files");
    assert_eq!(files.len(), 1);
    let summary = &files[0];
    // The poll surface the M2 daemon depends on: revn + modifiedAt.
    assert_eq!(summary.id, file.id);
    assert_eq!(summary.revn, 0);
    assert_eq!(summary.vern, 0);
    assert!(
        summary.modified_at.contains('T'),
        "modifiedAt is a timestamp: {}",
        summary.modified_at
    );

    // delete-project is a SOFT delete: 204, but the project keeps appearing
    // in get-projects with `deletedAt` set (~7 days out, the GC deadline).
    // The M2 daemon must filter on deleted_at.is_none().
    client.delete_project(&project.id).await.expect("delete-project");
    let projects = client.get_projects(&team_id).await.expect("get-projects");
    let deleted = projects.iter().find(|p| p.id == project.id);
    assert!(
        deleted.is_none_or(|p| p.deleted_at.is_some()),
        "deleted project came back alive: {deleted:?}"
    );
}

// ---------------------------------------------------------------------
// update-file (add-obj) + revn semantics + delete-file
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs a live stack (PENPOT_RPC_LIVE_BASE_URL / PENPOT_RPC_LIVE_TOKEN)"]
async fn live_update_file_add_obj_and_delete_file() {
    let client = client();
    let team_id = default_team_id(&client).await;
    let project = client
        .create_project(&team_id, &format!("rpc-live-update-{}", uuid_v4()))
        .await
        .expect("create-project");
    let file = client
        .create_file(&project.id, "update-me")
        .await
        .expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();

    let shape_id = uuid_v4();
    let outcome = client
        .update_file(
            &file.id,
            &uuid_v4(),
            file.revn,
            file.vern,
            &[add_rect_change(&shape_id, &page_id, "Live Rect", 100.0, 100.0)],
        )
        .await
        .expect("update-file");
    // Response revn is the revision BEFORE the update; an up-to-date client
    // sees exactly one lagged entry (its own change).
    assert_eq!(outcome.revn, 0);
    assert_eq!(outcome.lagged.len(), 1, "lagged: {:?}", outcome.lagged);

    // The file is now at revn 1 and contains the shape.
    let files = client
        .get_project_files(&project.id)
        .await
        .expect("get-project-files");
    assert_eq!(files[0].revn, 1);
    let full = client.get_file(&file.id).await.expect("get-file");
    assert!(
        has_shape(&full, &page_id, &shape_id),
        "shape {shape_id} missing from page {page_id}"
    );

    // delete-file answers 204 and the file disappears from the summaries.
    client.delete_file(&file.id).await.expect("delete-file");
    assert!(client
        .get_project_files(&project.id)
        .await
        .expect("get-project-files")
        .is_empty());

    client.delete_project(&project.id).await.expect("delete-project");
}

// ---------------------------------------------------------------------
// export-binfile → download through the proxy → import as NEW file
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs a live stack (PENPOT_RPC_LIVE_BASE_URL / PENPOT_RPC_LIVE_TOKEN)"]
async fn live_export_download_and_import_as_new() {
    let client = client();
    let team_id = default_team_id(&client).await;
    let project = client
        .create_project(&team_id, &format!("rpc-live-export-{}", uuid_v4()))
        .await
        .expect("create-project");
    let file = client
        .create_file(&project.id, "export-me")
        .await
        .expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape_id = uuid_v4();
    client
        .update_file(
            &file.id,
            &uuid_v4(),
            0,
            0,
            &[add_rect_change(&shape_id, &page_id, "Exported Rect", 50.0, 60.0)],
        )
        .await
        .expect("update-file");

    // Export: SSE parse → artifact URI on the PUBLIC URI (the proxy).
    let exported = client
        .export_binfile(&file.id, false, false)
        .await
        .expect("export-binfile");
    assert!(
        exported.uri.contains("/assets/by-id/"),
        "unexpected artifact URI: {}",
        exported.uri
    );

    // Download through the proxy with token auth.
    let zip = client
        .download_exported_binfile(&exported.uri)
        .await
        .expect("download exported binfile");
    assert!(zip.len() > 200, "suspiciously small zip: {} bytes", zip.len());
    assert_eq!(&zip[..4], b"PK\x03\x04", "not a ZIP container");
    // binfile-v3 zips carry a manifest.json entry.
    assert!(
        zip.windows(b"manifest.json".len()).any(|w| w == b"manifest.json"),
        "zip has no manifest.json entry"
    );

    // Import as NEW: a fresh file id is minted.
    let ids = client
        .import_binfile("ignored-for-v3", &project.id, None, zip)
        .await
        .expect("import-binfile (as new)");
    assert_eq!(ids.len(), 1, "import end event ids: {ids:?}");
    let new_id = &ids[0];
    assert_ne!(new_id, &file.id, "as-new import must mint a new file id");

    // Both files are now in the project; the imported copy has the shape too.
    let files = client
        .get_project_files(&project.id)
        .await
        .expect("get-project-files");
    assert_eq!(files.len(), 2, "files: {files:?}");
    assert!(files.iter().any(|f| &f.id == new_id));
    let imported = client.get_file(new_id).await.expect("get-file imported");
    // Ids inside the binfile are remapped on as-new import, so look for any
    // rect with our shape name instead of the original uuid.
    let found = imported
        .pointer("/data/pagesIndex")
        .and_then(Value::as_object)
        .is_some_and(|pages| {
            pages.values().any(|page| {
                page.pointer("/objects")
                    .and_then(Value::as_object)
                    .is_some_and(|objs| {
                        objs.values()
                            .any(|o| o.get("name").and_then(Value::as_str) == Some("Exported Rect"))
                    })
            })
        });
    assert!(found, "imported copy lost the exported shape");

    client.delete_project(&project.id).await.expect("delete-project");
}

// ---------------------------------------------------------------------
// import-binfile IN-PLACE: same id, content + revn roll back to the zip
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs a live stack (PENPOT_RPC_LIVE_BASE_URL / PENPOT_RPC_LIVE_TOKEN)"]
async fn live_import_in_place_replaces_content_and_resets_revn() {
    let client = client();
    let team_id = default_team_id(&client).await;
    let project = client
        .create_project(&team_id, &format!("rpc-live-inplace-{}", uuid_v4()))
        .await
        .expect("create-project");
    let file = client
        .create_file(&project.id, "inplace-me")
        .await
        .expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();

    // revn 0 → 1: rect A. Export this state.
    let shape_a = uuid_v4();
    client
        .update_file(
            &file.id,
            &uuid_v4(),
            0,
            0,
            &[add_rect_change(&shape_a, &page_id, "Rect A", 10.0, 10.0)],
        )
        .await
        .expect("update-file A");
    let exported = client
        .export_binfile(&file.id, false, false)
        .await
        .expect("export-binfile");
    let zip = client
        .download_exported_binfile(&exported.uri)
        .await
        .expect("download");

    // revn 1 → 2: rect B (this edit must be rolled back by the import).
    let shape_b = uuid_v4();
    client
        .update_file(
            &file.id,
            &uuid_v4(),
            1,
            0,
            &[add_rect_change(&shape_b, &page_id, "Rect B", 400.0, 10.0)],
        )
        .await
        .expect("update-file B");
    let files = client.get_project_files(&project.id).await.expect("files");
    assert_eq!(files[0].revn, 2);

    // In-place import of the older zip: same file id comes back.
    let ids = client
        .import_binfile("inplace-me", &project.id, Some(&file.id), zip)
        .await
        .expect("import-binfile (in place)");
    assert_eq!(ids, vec![file.id.clone()], "in-place import must keep the id");

    // Content rolled back: rect A present, rect B gone. In-place preserves
    // UUIDs, so we can address shapes by their original ids.
    let full = client.get_file(&file.id).await.expect("get-file");
    assert!(has_shape(&full, &page_id, &shape_a), "rect A lost by in-place import");
    assert!(!has_shape(&full, &page_id, &shape_b), "rect B survived the rollback");

    // revn is NOT monotonic across in-place imports: it resets to the value
    // stored in the binfile (2 → 1 here).
    let files = client.get_project_files(&project.id).await.expect("files");
    assert_eq!(files[0].revn, 1, "revn should roll back to the zip's revn");

    client.delete_project(&project.id).await.expect("delete-project");
}
