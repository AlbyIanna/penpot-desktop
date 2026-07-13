//! Live end-to-end test of M3 two-way sync + conflicts (`#[ignore]`).
//!
//! Needs a running Penpot stack (headless bin is fine) and:
//!
//! - `SYNC_DAEMON_LIVE_BASE_URL` — the **proxy** base URL (export artifact
//!   URIs only resolve through the proxy).
//! - `SYNC_DAEMON_LIVE_TOKEN` — a personal access token
//!   (`<data_dir>/credentials.json`, key `access_token`).
//!
//! Run: `cargo test -p sync-daemon --test live_m3 -- --ignored --nocapture`
//!
//! The test spawns ITS OWN daemon on a fresh temp designs root (the headless
//! app's built-in daemon syncs the same team into a different root — that is
//! benign: it only ever exports there). One sequential scenario:
//!
//! 1. Direction A baseline: create project+file+rect via RPC → dir appears.
//! 2. Direction B: edit a shape name on disk → appears in the DB (get-file)
//!    within seconds; latency printed.
//! 3. Loop prevention: idle window → DB `(revn, modifiedAt)` and the disk
//!    stat fingerprint both stay frozen (no export/import ping-pong).
//! 4. Direction A live: update-file → exported to disk; another idle window
//!    proves the export did not bounce back as an import.
//! 5. Conflict: pause, edit disk AND DB, resume → conflict copy dir with the
//!    DB version, disk version lands in the DB, manifest consistent, status
//!    = Conflict{copy_path}.
//! 6. git-checkout-style revert: restore a saved older tree over the dir →
//!    the old content reappears in the DB (exit criterion 1).
//! 7. Import-as-new: an unknown `.penpot` dir appears → new DB file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use penpot_rpc::{Auth, PenpotClient};
use serde_json::{json, Value};
use sync_daemon::{FileState, SyncConfig};

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

/// Recursive (relpath → (len, mtime)) fingerprint of a directory.
fn stat_fingerprint(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = entry.metadata().unwrap();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            out.insert(rel, (meta.len(), meta.modified().unwrap()));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

/// Replace `needle` with `replacement` in every `.json` under `root`.
fn edit_tree_json(root: &Path, needle: &str, replacement: &str) -> usize {
    let mut hits = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                let s = std::fs::read_to_string(&path).unwrap();
                if s.contains(needle) {
                    std::fs::write(&path, s.replace(needle, replacement)).unwrap();
                    hits += 1;
                }
            }
        }
    }
    hits
}

/// Copy a directory tree (used to snapshot/restore, git-checkout style).
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

fn find_conflict_dirs(project_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(project_dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            (name.contains(".conflict-") && name.ends_with(".penpot")).then(|| e.path())
        })
        .collect()
}

fn shape_name(file: &Value, page_id: &str, shape_id: &str) -> Option<String> {
    file.pointer(&format!("/data/pagesIndex/{page_id}/objects/{shape_id}/name"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live stack (SYNC_DAEMON_LIVE_BASE_URL / SYNC_DAEMON_LIVE_TOKEN)"]
async fn live_m3_two_way_sync_and_conflicts() {
    let rpc = client();
    let team_id = rpc
        .get_profile()
        .await
        .expect("get-profile")
        .default_team_id
        .expect("no defaultTeamId");

    // Fresh, dedicated designs root for OUR daemon.
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();

    // ------------------------------------------------------------------
    // Baseline: a project + file + one rect, created BEFORE the daemon so
    // startup reconciliation exports it (in DB, not on disk → export).
    // ------------------------------------------------------------------
    let proj_name = format!("M3 Live {}", &uuid_v4()[..8]);
    let project = rpc.create_project(&team_id, &proj_name).await.expect("create-project");
    let file = rpc.create_file(&project.id, "m3live").await.expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape_a = uuid_v4();
    let session = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        file.revn,
        file.vern,
        &[add_rect_change(&shape_a, &page_id, "rename-me", 10.0, 10.0)],
    )
    .await
    .expect("update-file (rect A)");

    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));
    let control = daemon.control();
    let status = daemon.status();

    let file_dir = root.join(&proj_name).join("m3live.penpot");
    let rel_path = format!("{proj_name}/m3live.penpot");
    let took = wait_until("initial export on disk", Duration::from_secs(90), || {
        file_dir.join("manifest.json").is_file() && tree_contains(&file_dir, "rename-me")
    })
    .await;
    println!("LATENCY initial-export (reconcile): {took:?}");

    // Wait for the ledger to be seeded (status Synced).
    wait_until("status Synced after export", Duration::from_secs(30), || {
        status.borrow().files.get(&rel_path) == Some(&FileState::Synced)
    })
    .await;

    // ------------------------------------------------------------------
    // 2. Direction B: external edit → appears in Penpot within seconds.
    // ------------------------------------------------------------------
    // Small settle so the export's own watcher events drain (they are
    // hash-skipped, but keep the timing measurement clean).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let hits = edit_tree_json(&file_dir, "\"name\": \"rename-me\"", "\"name\": \"renamed-on-disk\"");
    assert!(hits > 0, "the shape name must exist in the exported tree");
    let t0 = Instant::now();
    let mut latency = None;
    while t0.elapsed() < Duration::from_secs(30) {
        let f = rpc.get_file(&file.id).await.expect("get-file");
        if shape_name(&f, &page_id, &shape_a).as_deref() == Some("renamed-on-disk") {
            latency = Some(t0.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let latency = latency.expect("external edit did not reach the DB within 30s");
    println!("LATENCY direction-B external edit → visible in get-file: {latency:?}");

    // ------------------------------------------------------------------
    // 3. Loop prevention: idle window — DB and disk both frozen.
    // ------------------------------------------------------------------
    // Let the post-import bookkeeping settle first.
    wait_until("status Synced after import", Duration::from_secs(30), || {
        status.borrow().files.get(&rel_path) == Some(&FileState::Synced)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let db_before = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let disk_before = stat_fingerprint(&file_dir);
    tokio::time::sleep(Duration::from_secs(10)).await; // ≥5 poll cycles + fs debounce
    let db_after = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let disk_after = stat_fingerprint(&file_dir);
    assert_eq!(
        (db_before.revn, &db_before.modified_at),
        (db_after.revn, &db_after.modified_at),
        "IMPORT BOUNCE: the DB moved during an idle window"
    );
    assert_eq!(disk_before, disk_after, "EXPORT BOUNCE: the disk moved during an idle window");
    println!("LOOP-PREVENTION idle window (10s): DB (revn, modifiedAt) and disk stat fingerprint both frozen");

    // ------------------------------------------------------------------
    // 4. Direction A: DB edit → exported to disk; export must not bounce.
    // ------------------------------------------------------------------
    let shape_b = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        db_after.revn,
        db_after.vern,
        &[add_rect_change(&shape_b, &page_id, "db-edit-rect", 300.0, 10.0)],
    )
    .await
    .expect("update-file (rect B)");
    let t0 = Instant::now();
    let took = wait_until("DB edit exported to disk", Duration::from_secs(30), || {
        tree_contains(&file_dir, "db-edit-rect")
    })
    .await;
    println!("LATENCY direction-A DB edit → on disk: {took:?} (t0 delta {:?})", t0.elapsed());
    // The export lands, the watcher sees it, and it must be hash-skipped:
    tokio::time::sleep(Duration::from_secs(6)).await; // > fs debounce + import time
    let db_now = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let export_settled_state = (db_now.revn, db_now.modified_at.clone());
    tokio::time::sleep(Duration::from_secs(6)).await;
    let db_later = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    assert_eq!(
        export_settled_state,
        (db_later.revn, db_later.modified_at.clone()),
        "IMPORT BOUNCE after an export: our own write was re-imported"
    );
    println!("LOOP-PREVENTION export path: no import bounce after Direction A export");

    // ------------------------------------------------------------------
    // 5. Conflict: pause → edit disk + edit DB → resume.
    // ------------------------------------------------------------------
    control.pause();
    wait_until("paused visible in status", Duration::from_secs(5), || {
        status.borrow().paused
    })
    .await;
    // Disk side: rename shape A again.
    let hits = edit_tree_json(&file_dir, "\"name\": \"renamed-on-disk\"", "\"name\": \"disk-wins\"");
    assert!(hits > 0);
    // DB side: a competing edit (a rect that will exist ONLY in the copy).
    let db_cur = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let shape_c = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        db_cur.revn,
        db_cur.vern,
        &[add_rect_change(&shape_c, &page_id, "db-only-rect", 300.0, 300.0)],
    )
    .await
    .expect("update-file (conflicting)");
    // Give the paused daemon a couple of poll intervals: it must do NOTHING.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        find_conflict_dirs(&root.join(&proj_name)).is_empty(),
        "paused daemon acted on a change"
    );
    assert!(tree_contains(&file_dir, "disk-wins"), "paused daemon touched the disk");

    control.resume();
    let took = wait_until("conflict copy appears", Duration::from_secs(60), || {
        !find_conflict_dirs(&root.join(&proj_name)).is_empty()
    })
    .await;
    println!("LATENCY conflict resolution after resume: {took:?}");
    let copies = find_conflict_dirs(&root.join(&proj_name));
    assert_eq!(copies.len(), 1, "exactly one conflict copy: {copies:?}");
    let copy = &copies[0];
    // The copy holds the DB version (the db-only rect)…
    assert!(tree_contains(copy, "db-only-rect"), "conflict copy must hold the DB version");
    // …and the DB now holds the disk version.
    let f = rpc.get_file(&file.id).await.expect("get-file");
    assert_eq!(
        shape_name(&f, &page_id, &shape_a).as_deref(),
        Some("disk-wins"),
        "the disk version must have been imported"
    );
    assert!(
        shape_name(&f, &page_id, &shape_c).is_none(),
        "the DB-only rect lives in the conflict copy, not the live file"
    );
    // The disk tree itself was never overwritten by the DB version.
    assert!(tree_contains(&file_dir, "disk-wins"));
    assert!(!tree_contains(&file_dir, "db-only-rect"));
    // Status surfaces the conflict with the copy path.
    let copy_rel = copy.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
    wait_until("conflict status", Duration::from_secs(10), || {
        matches!(
            status.borrow().files.get(&rel_path),
            Some(FileState::Conflict { copy_path }) if *copy_path == copy_rel
        )
    })
    .await;
    // Manifest consistent: lastSyncedHash == current disk semantic hash.
    let manifest = sync_core::Manifest::load(&root).unwrap().expect("manifest exists");
    let entry = manifest.files.get(&file.id).expect("entry kept under the same file id");
    assert_eq!(
        entry.last_synced_hash,
        sync_core::semantic_tree_hash(&file_dir).unwrap(),
        "manifest lastSyncedHash must equal the on-disk semantic hash"
    );
    println!("CONFLICT rule verified: copy at {copy_rel}, disk version in DB, manifest consistent");

    // And the conflict copy is left alone forever after (never re-synced):
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(find_conflict_dirs(&root.join(&proj_name)).len(), 1);

    // ------------------------------------------------------------------
    // 6. git-checkout-style revert (exit criterion 1): restore the earlier
    //    tree over the dir → old content reappears in Penpot.
    // ------------------------------------------------------------------
    // Snapshot the current tree, mutate it, wait for sync, then restore.
    let snapshot = root_dir.path().parent().unwrap().join(format!("m3-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&snapshot);
    copy_tree(&file_dir, &snapshot);
    let hits = edit_tree_json(&file_dir, "\"name\": \"disk-wins\"", "\"name\": \"newer-version\"");
    assert!(hits > 0);
    let t0 = Instant::now();
    let mut imported = false;
    while t0.elapsed() < Duration::from_secs(30) {
        let f = rpc.get_file(&file.id).await.unwrap();
        if shape_name(&f, &page_id, &shape_a).as_deref() == Some("newer-version") {
            imported = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(imported, "the newer version did not reach the DB");
    // "git checkout" the older version: replace the dir wholesale.
    std::fs::remove_dir_all(&file_dir).unwrap();
    copy_tree(&snapshot, &file_dir);
    let t0 = Instant::now();
    let mut revert_latency = None;
    while t0.elapsed() < Duration::from_secs(30) {
        let f = rpc.get_file(&file.id).await.unwrap();
        if shape_name(&f, &page_id, &shape_a).as_deref() == Some("disk-wins") {
            revert_latency = Some(t0.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let _ = std::fs::remove_dir_all(&snapshot);
    let revert_latency =
        revert_latency.expect("git-checkout-style revert did not reach the DB within 30s");
    println!("LATENCY git-checkout-style revert → visible in get-file: {revert_latency:?}");
    // No conflict copy for a clean revert (DB had not moved on its own).
    assert_eq!(
        find_conflict_dirs(&root.join(&proj_name)).len(),
        1,
        "a clean revert must not create conflict copies"
    );

    // ------------------------------------------------------------------
    // 7. Import-as-new: an unknown .penpot dir appears on disk.
    // ------------------------------------------------------------------
    let files_before = rpc.get_project_files(&project.id).await.unwrap().len();
    let new_dir = root.join(&proj_name).join("copy-as-new.penpot");
    copy_tree(&file_dir, &new_dir);
    let t0 = Instant::now();
    let mut ok = false;
    while t0.elapsed() < Duration::from_secs(45) {
        if rpc.get_project_files(&project.id).await.unwrap().len() > files_before {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(ok, "unknown .penpot dir was not imported as a new file");
    let manifest = sync_core::Manifest::load(&root).unwrap().unwrap();
    assert!(
        manifest
            .files
            .values()
            .any(|e| e.path == format!("{proj_name}/copy-as-new.penpot")),
        "manifest gained an entry for the new dir"
    );
    println!("IMPORT-AS-NEW verified");

    daemon.stop().await;
    // Cleanup DB side (best effort).
    let _ = rpc.delete_project(&project.id).await;
}

/// Startup reconciliation's both-changed arm: edit disk AND DB while no
/// daemon is running, then boot one — reconciliation itself must apply the
/// conflict rule (copy with the DB version, disk imported).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live stack (SYNC_DAEMON_LIVE_BASE_URL / SYNC_DAEMON_LIVE_TOKEN)"]
async fn live_m3_startup_reconciliation_conflict_arm() {
    let rpc = client();
    let team_id = rpc
        .get_profile()
        .await
        .expect("get-profile")
        .default_team_id
        .expect("no defaultTeamId");
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();

    let proj_name = format!("M3 Boot {}", &uuid_v4()[..8]);
    let project = rpc.create_project(&team_id, &proj_name).await.expect("create-project");
    let file = rpc.create_file(&project.id, "bootfile").await.expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape_a = uuid_v4();
    let session = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        file.revn,
        file.vern,
        &[add_rect_change(&shape_a, &page_id, "boot-orig", 10.0, 10.0)],
    )
    .await
    .expect("update-file");

    // Daemon #1: export, then stop.
    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));
    let file_dir = root.join(&proj_name).join("bootfile.penpot");
    wait_until("initial export", Duration::from_secs(90), || {
        tree_contains(&file_dir, "boot-orig")
    })
    .await;
    // Let the ledger settle before stopping.
    tokio::time::sleep(Duration::from_secs(3)).await;
    daemon.stop().await;

    // Offline: both sides change.
    let hits = edit_tree_json(&file_dir, "\"name\": \"boot-orig\"", "\"name\": \"boot-disk\"");
    assert!(hits > 0);
    let db_cur = rpc
        .get_project_files(&project.id)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let shape_b = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        db_cur.revn,
        db_cur.vern,
        &[add_rect_change(&shape_b, &page_id, "boot-db-only", 300.0, 10.0)],
    )
    .await
    .expect("update-file (offline DB edit)");

    // Daemon #2: reconciliation must fire the conflict rule.
    let daemon = sync_daemon::spawn(rpc.clone(), SyncConfig::new(&root, team_id.clone()));
    let status = daemon.status();
    wait_until("startup conflict copy", Duration::from_secs(60), || {
        !find_conflict_dirs(&root.join(&proj_name)).is_empty()
    })
    .await;
    let copies = find_conflict_dirs(&root.join(&proj_name));
    assert_eq!(copies.len(), 1, "exactly one conflict copy: {copies:?}");
    assert!(
        tree_contains(&copies[0], "boot-db-only"),
        "the copy must hold the DB version"
    );
    // Disk version reaches the DB.
    let t0 = Instant::now();
    let mut ok = false;
    while t0.elapsed() < Duration::from_secs(60) {
        let f = rpc.get_file(&file.id).await.expect("get-file");
        if shape_name(&f, &page_id, &shape_a).as_deref() == Some("boot-disk")
            && shape_name(&f, &page_id, &shape_b).is_none()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(ok, "the disk version did not land in the DB after startup conflict");
    // Disk untouched; manifest consistent; status surfaces the conflict.
    assert!(tree_contains(&file_dir, "boot-disk"));
    assert!(!tree_contains(&file_dir, "boot-db-only"));
    let manifest = sync_core::Manifest::load(&root).unwrap().unwrap();
    let entry = manifest.files.get(&file.id).expect("same file id");
    assert_eq!(
        entry.last_synced_hash,
        sync_core::semantic_tree_hash(&file_dir).unwrap()
    );
    let rel_path = format!("{proj_name}/bootfile.penpot");
    let copy_rel = copies[0].strip_prefix(&root).unwrap().to_string_lossy().into_owned();
    wait_until("conflict status after boot", Duration::from_secs(10), || {
        matches!(
            status.borrow().files.get(&rel_path),
            Some(FileState::Conflict { copy_path }) if *copy_path == copy_rel
        )
    })
    .await;
    println!("STARTUP CONFLICT ARM verified: copy at {copy_rel}");

    daemon.stop().await;
    let _ = rpc.delete_project(&project.id).await;
}
