//! Live end-to-end test of M5 OS-side create/rename/move (`#[ignore]`).
//!
//! Needs a running Penpot stack (headless bin is fine) and:
//!
//! - `SYNC_DAEMON_LIVE_BASE_URL` — the **proxy** base URL.
//! - `SYNC_DAEMON_LIVE_TOKEN` — a personal access token
//!   (`<data_dir>/credentials.json`, key `access_token`).
//!
//! Run: `cargo test -p sync-daemon --test live_m5 -- --ignored --nocapture`
//!
//! Scenarios (each prints its observed latency):
//!
//! 1. `mv` a `.penpot` dir to a new name while the daemon runs → the DB file
//!    is RENAMED (same file id, no reimport: no new file appears, revn is
//!    untouched, and after the follow-up name-refresh export the DB
//!    `(revn, modifiedAt)` freezes — an import would keep bumping it).
//! 2. `mv` the dir into another project's folder → `move-files` (same id,
//!    `modifiedAt` untouched), manifest re-keyed.
//! 3. `mv` the dir into a brand-new folder → the project is created and the
//!    file moves there (still the same id).
//! 4. `mv` a whole project folder → `rename-project` (project identity via
//!    the manifest's projectId mapping), all entries re-keyed, no churn.
//! 5. Copy a dir into a NEW folder (unknown content) → import-as-new under a
//!    freshly created project (the M3 arm, extended to new folders).
//! 6. (separate test) rename while the daemon is STOPPED, then boot → startup
//!    reconciliation re-keys (same id), the old path is NOT re-exported as an
//!    orphan and no import-as-new duplicate appears.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use penpot_rpc::{Auth, PenpotClient};
use serde_json::{json, Value};
use sync_daemon::SyncConfig;

const ROOT_FRAME: &str = "00000000-0000-0000-0000-000000000000";

fn client() -> PenpotClient {
    let base = std::env::var("SYNC_DAEMON_LIVE_BASE_URL")
        .expect("set SYNC_DAEMON_LIVE_BASE_URL to the proxy base URL");
    let token =
        std::env::var("SYNC_DAEMON_LIVE_TOKEN").expect("set SYNC_DAEMON_LIVE_TOKEN to a token");
    PenpotClient::new(base).with_auth(Auth::Token(token))
}

fn uuid_v4() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
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

async fn wait_until<F: FnMut() -> bool>(what: &str, timeout: Duration, mut f: F) -> Duration {
    let start = Instant::now();
    loop {
        if f() {
            return start.elapsed();
        }
        assert!(start.elapsed() < timeout, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn tree_contains(root: &Path, needle: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { return false };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                if std::fs::read_to_string(&path).unwrap_or_default().contains(needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn find_conflict_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries {
            let e = e.unwrap();
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(".conflict-") && name.ends_with(".penpot") {
                out.push(e.path());
            } else if !name.ends_with(".penpot") {
                stack.push(e.path());
            }
        }
    }
    out
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dst = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), &dst).unwrap();
        }
    }
}

async fn file_state(
    rpc: &PenpotClient,
    project_id: &str,
    file_id: &str,
) -> Option<penpot_rpc::FileSummary> {
    rpc.get_project_files(project_id)
        .await
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file_id)
}

/// Manifest entry (path, projectId) for a file id.
fn manifest_entry(root: &Path, file_id: &str) -> Option<(String, String)> {
    let m = sync_core::Manifest::load(root).unwrap()?;
    m.files
        .get(file_id)
        .map(|e| (e.path.clone(), e.project_id.clone()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live stack (SYNC_DAEMON_LIVE_BASE_URL / SYNC_DAEMON_LIVE_TOKEN)"]
async fn live_m5_os_side_rename_move_and_project_rename() {
    let rpc = client();
    let team_id = rpc
        .get_profile()
        .await
        .expect("get-profile")
        .default_team_id
        .expect("no defaultTeamId");
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();

    // Two projects with one file each, created BEFORE the daemon (initial
    // export via reconciliation), so both folders exist on disk.
    let proj_a_name = format!("M5 Ren A {}", &uuid_v4()[..8]);
    let proj_b_name = format!("M5 Ren B {}", &uuid_v4()[..8]);
    let proj_a = rpc.create_project(&team_id, &proj_a_name).await.expect("create A");
    let proj_b = rpc.create_project(&team_id, &proj_b_name).await.expect("create B");
    let file = rpc.create_file(&proj_a.id, "renme").await.expect("create-file");
    // Anchor file so proj B's folder exists on disk before the move test.
    let _anchor = rpc.create_file(&proj_b.id, "anchor").await.expect("create-file anchor");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape = uuid_v4();
    let session = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        file.revn,
        file.vern,
        &[add_rect_change(&shape, &page_id, "m5-shape", 10.0, 10.0)],
    )
    .await
    .expect("update-file");

    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));

    let dir_a = root.join(&proj_a_name);
    let dir_b = root.join(&proj_b_name);
    let old_dir = dir_a.join("renme.penpot");
    wait_until("initial exports on disk", Duration::from_secs(90), || {
        old_dir.join("manifest.json").is_file()
            && dir_b.join("anchor.penpot/manifest.json").is_file()
            && tree_contains(&old_dir, "m5-shape")
    })
    .await;
    // Let ledgers/status settle so the mv is the only pending change.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // ------------------------------------------------------------------
    // 1. RENAME while running: mv renme.penpot → renamed-on-os.penpot
    // ------------------------------------------------------------------
    let before = file_state(&rpc, &proj_a.id, &file.id).await.expect("listed");
    let new_dir = dir_a.join("renamed-on-os.penpot");
    std::fs::rename(&old_dir, &new_dir).expect("mv");
    let t0 = Instant::now();
    let mut latency = None;
    while t0.elapsed() < Duration::from_secs(30) {
        if let Some(f) = file_state(&rpc, &proj_a.id, &file.id).await {
            if f.name == "renamed-on-os" {
                latency = Some(t0.elapsed());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let latency = latency.expect("DB file was not renamed within 30s");
    println!("LATENCY os-rename → rename-file visible in DB: {latency:?}");

    // Same identity, no reimport: still exactly the files we created, revn
    // untouched, shape intact, manifest re-keyed under the SAME file id.
    let files_a = rpc.get_project_files(&proj_a.id).await.unwrap();
    assert_eq!(files_a.len(), 1, "no import-as-new duplicate: {files_a:?}");
    let renamed = &files_a[0];
    assert_eq!(renamed.id, file.id, "same file id (no reimport)");
    assert_eq!(renamed.revn, before.revn, "rename must not touch revn");
    assert_ne!(renamed.modified_at, before.modified_at, "rename bumps modifiedAt");
    wait_until("manifest re-keyed to the new path", Duration::from_secs(10), || {
        manifest_entry(&root, &file.id)
            .is_some_and(|(p, _)| p == format!("{proj_a_name}/renamed-on-os.penpot"))
    })
    .await;
    assert!(!old_dir.exists(), "old path must not be re-created");

    // The name-refresh export lands (the DB name is embedded in the JSON),
    // then everything freezes: an import bounce would keep moving modifiedAt.
    wait_until("name-refresh export on disk", Duration::from_secs(30), || {
        tree_contains(&new_dir, "renamed-on-os")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let settled = file_state(&rpc, &proj_a.id, &file.id).await.expect("listed");
    tokio::time::sleep(Duration::from_secs(8)).await; // ≥4 poll cycles
    let later = file_state(&rpc, &proj_a.id, &file.id).await.expect("listed");
    assert_eq!(
        (settled.revn, &settled.modified_at),
        (later.revn, &later.modified_at),
        "IMPORT BOUNCE after the rename"
    );
    assert!(find_conflict_dirs(&root).is_empty(), "a clean rename made a conflict copy");
    println!("RENAME verified: same id, revn untouched, no bounce, no conflict copy");

    // ------------------------------------------------------------------
    // 2. MOVE across projects while running: mv into proj B's folder.
    // ------------------------------------------------------------------
    let before_move = file_state(&rpc, &proj_a.id, &file.id).await.expect("listed");
    let moved_dir = dir_b.join("renamed-on-os.penpot");
    std::fs::rename(&new_dir, &moved_dir).expect("mv across projects");
    let t0 = Instant::now();
    let mut latency = None;
    while t0.elapsed() < Duration::from_secs(30) {
        if file_state(&rpc, &proj_b.id, &file.id).await.is_some() {
            latency = Some(t0.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let latency = latency.expect("move-files did not land within 30s");
    println!("LATENCY os-move → move-files visible in DB: {latency:?}");
    assert!(
        file_state(&rpc, &proj_a.id, &file.id).await.is_none(),
        "file must leave project A"
    );
    let moved = file_state(&rpc, &proj_b.id, &file.id).await.expect("in B");
    assert_eq!(moved.id, file.id, "same file id across the move");
    assert_eq!(
        moved.modified_at, before_move.modified_at,
        "move-files must not bump modifiedAt (no reimport happened)"
    );
    wait_until("manifest re-keyed to project B", Duration::from_secs(10), || {
        manifest_entry(&root, &file.id).is_some_and(|(p, pid)| {
            p == format!("{proj_b_name}/renamed-on-os.penpot") && pid == proj_b.id
        })
    })
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(find_conflict_dirs(&root).is_empty());
    assert!(!new_dir.exists(), "the A-side path must not be re-created");
    println!("MOVE verified: same id, project membership changed, no bounce");

    // ------------------------------------------------------------------
    // 3. MOVE into a brand-new folder → project is created on the fly.
    // ------------------------------------------------------------------
    let fresh_name = format!("M5 Fresh {}", &uuid_v4()[..8]);
    let fresh_dir = root.join(&fresh_name);
    std::fs::create_dir_all(&fresh_dir).unwrap();
    let fresh_file_dir = fresh_dir.join("renamed-on-os.penpot");
    std::fs::rename(&moved_dir, &fresh_file_dir).expect("mv to fresh folder");
    let t0 = Instant::now();
    let mut fresh_project_id = None;
    while t0.elapsed() < Duration::from_secs(30) {
        let projects = rpc.get_projects(&team_id).await.unwrap();
        if let Some(p) = projects
            .iter()
            .find(|p| p.name == fresh_name && p.deleted_at.is_none())
        {
            if file_state(&rpc, &p.id, &file.id).await.is_some() {
                fresh_project_id = Some(p.id.clone());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let fresh_project_id = fresh_project_id.expect("fresh project + moved file within 30s");
    println!("LATENCY os-move to a NEW folder → create-project + move-files: {:?}", t0.elapsed());
    assert!(
        file_state(&rpc, &proj_b.id, &file.id).await.is_none(),
        "file must leave project B"
    );
    wait_until("manifest re-keyed to the fresh project", Duration::from_secs(10), || {
        manifest_entry(&root, &file.id).is_some_and(|(p, pid)| {
            p == format!("{fresh_name}/renamed-on-os.penpot") && pid == fresh_project_id
        })
    })
    .await;
    println!("MOVE-TO-NEW-FOLDER verified: project created, same file id");

    // ------------------------------------------------------------------
    // 4. PROJECT FOLDER RENAME: mv the fresh folder wholesale.
    // ------------------------------------------------------------------
    tokio::time::sleep(Duration::from_secs(4)).await; // settle
    let renamed_folder_name = format!("{fresh_name} Renamed");
    std::fs::rename(&fresh_dir, root.join(&renamed_folder_name)).expect("mv project folder");
    let t0 = Instant::now();
    let mut ok = false;
    while t0.elapsed() < Duration::from_secs(30) {
        let projects = rpc.get_projects(&team_id).await.unwrap();
        if projects
            .iter()
            .any(|p| p.id == fresh_project_id && p.name == renamed_folder_name)
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(ok, "rename-project did not land within 30s");
    println!("LATENCY os-project-folder-rename → rename-project visible: {:?}", t0.elapsed());
    // Same project id (identity via the manifest's projectId mapping), file
    // untouched, manifest re-keyed.
    let f = file_state(&rpc, &fresh_project_id, &file.id).await.expect("still in the project");
    assert_eq!(f.id, file.id);
    wait_until("manifest re-keyed after the folder rename", Duration::from_secs(10), || {
        manifest_entry(&root, &file.id).is_some_and(|(p, pid)| {
            p == format!("{renamed_folder_name}/renamed-on-os.penpot") && pid == fresh_project_id
        })
    })
    .await;
    // No churn afterwards.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let projects = rpc.get_projects(&team_id).await.unwrap();
    assert_eq!(
        projects
            .iter()
            .filter(|p| p.deleted_at.is_none() && (p.name == fresh_name || p.name == renamed_folder_name))
            .count(),
        1,
        "no duplicate project was created by the folder rename"
    );
    assert!(find_conflict_dirs(&root).is_empty());
    println!("PROJECT-FOLDER-RENAME verified: rename-project, same ids, no duplicates");

    // ------------------------------------------------------------------
    // 5. NEW folder with a COPIED dir (unknown content) → import-as-new
    //    under a freshly created project (the safe-degradation arm).
    // ------------------------------------------------------------------
    let import_folder = format!("M5 NewProj {}", &uuid_v4()[..8]);
    let src = root.join(&renamed_folder_name).join("renamed-on-os.penpot");
    copy_tree(&src, &root.join(&import_folder).join("copied.penpot"));
    let t0 = Instant::now();
    let mut ok = false;
    while t0.elapsed() < Duration::from_secs(45) {
        let projects = rpc.get_projects(&team_id).await.unwrap();
        if let Some(p) = projects
            .iter()
            .find(|p| p.name == import_folder && p.deleted_at.is_none())
        {
            let files = rpc.get_project_files(&p.id).await.unwrap();
            if files.len() == 1 && files[0].id != file.id {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(ok, "new folder + copied dir was not imported as a new project/file");
    println!(
        "LATENCY new project folder + copy → create-project + import-as-new: {:?}",
        t0.elapsed()
    );
    // The original is untouched (the copy is a NEW id; content duplication of
    // an identical tree is ambiguous by design and must never re-key).
    assert!(
        file_state(&rpc, &fresh_project_id, &file.id).await.is_some(),
        "the original file must stay in its project"
    );

    daemon.stop().await;
    let _ = rpc.delete_project(&proj_a.id).await;
    let _ = rpc.delete_project(&proj_b.id).await;
    let _ = rpc.delete_project(&fresh_project_id).await;
}

/// Scenario 6: rename while the daemon is STOPPED, then boot — startup
/// reconciliation must re-key (same file id), mirror the rename into the DB,
/// NOT re-export the old path as an orphan and NOT import-as-new a duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live stack (SYNC_DAEMON_LIVE_BASE_URL / SYNC_DAEMON_LIVE_TOKEN)"]
async fn live_m5_offline_rename_reconciles_on_boot() {
    let rpc = client();
    let team_id = rpc
        .get_profile()
        .await
        .expect("get-profile")
        .default_team_id
        .expect("no defaultTeamId");
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();

    let proj_name = format!("M5 Boot {}", &uuid_v4()[..8]);
    let project = rpc.create_project(&team_id, &proj_name).await.expect("create-project");
    let file = rpc.create_file(&project.id, "offline-orig").await.expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape = uuid_v4();
    rpc.update_file(
        &file.id,
        &uuid_v4(),
        file.revn,
        file.vern,
        &[add_rect_change(&shape, &page_id, "offline-shape", 10.0, 10.0)],
    )
    .await
    .expect("update-file");

    // Daemon #1: initial export, then stop.
    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));
    let old_dir = root.join(&proj_name).join("offline-orig.penpot");
    wait_until("initial export", Duration::from_secs(90), || {
        tree_contains(&old_dir, "offline-shape")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await; // ledger settle
    daemon.stop().await;

    // Offline rename.
    let new_dir = root.join(&proj_name).join("offline-renamed.penpot");
    std::fs::rename(&old_dir, &new_dir).expect("offline mv");

    // Daemon #2: reconciliation must re-key, not duplicate.
    let t_boot = Instant::now();
    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));
    let took = wait_until("manifest re-key after boot", Duration::from_secs(60), || {
        manifest_entry(&root, &file.id)
            .is_some_and(|(p, _)| p == format!("{proj_name}/offline-renamed.penpot"))
    })
    .await;
    println!("LATENCY offline rename re-keyed at boot (manifest): {took:?} after spawn");
    let mut ok = false;
    while t_boot.elapsed() < Duration::from_secs(60) {
        if let Some(f) = file_state(&rpc, &project.id, &file.id).await {
            if f.name == "offline-renamed" {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(ok, "the DB file was not renamed after boot");
    println!("LATENCY offline rename visible in DB: {:?} after spawn", t_boot.elapsed());

    // No duplicate, no orphan re-export at the old path, shape intact.
    let files = rpc.get_project_files(&project.id).await.unwrap();
    assert_eq!(files.len(), 1, "no import-as-new duplicate: {files:?}");
    assert_eq!(files[0].id, file.id, "same file id (re-keyed, not reimported)");
    // Give any export in flight a moment, then check the old path stayed dead
    // and the new tree got the refreshed name.
    wait_until("name-refresh export", Duration::from_secs(30), || {
        tree_contains(&new_dir, "offline-renamed")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!old_dir.exists(), "the old path must NOT be re-exported as an orphan");
    assert!(tree_contains(&new_dir, "offline-shape"), "content intact");
    assert!(find_conflict_dirs(&root).is_empty(), "no conflict copies for a clean offline rename");

    daemon.stop().await;
    let _ = rpc.delete_project(&project.id).await;
}
