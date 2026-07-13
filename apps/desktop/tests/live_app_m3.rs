//! Live end-to-end test of the APP-LEVEL M3 wiring (`#[ignore]`): boots the
//! full stack via `penpot_desktop::boot()` (the exact code path the GUI and
//! headless bins use) and drives the sync daemon through the SAME accessors
//! the tray uses (`RunningApp::sync_status` / `RunningApp::sync_control`).
//!
//! Scenario:
//! 1. boot on dedicated ports + fresh temp dirs → daemon status/control are
//!    exposed;
//! 2. Direction B: external disk edit → visible in `get-file` within seconds;
//! 3. Direction A: DB edit → exported to disk; idle window proves no
//!    import bounce (loop prevention);
//! 4. pause via a direct `SyncControl` call → disk edits accumulate but
//!    nothing syncs; resume → the catch-up rescan applies them;
//! 5. clean `shutdown()`.
//!
//! Run (ports are baked into the env the test sets itself):
//! `cargo test -p penpot-desktop --test live_app_m3 -- --ignored --nocapture`
//!
//! Requires runtime/ artifacts (scripts/fetch-penpot.sh), JDK, valkey — the
//! same prerequisites as scripts/m1-smoke.sh. First boot on a fresh data dir
//! downloads embedded postgres binaries.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use penpot_rpc::{Auth, PenpotClient};
use serde_json::{json, Value};

const ROOT_FRAME: &str = "00000000-0000-0000-0000-000000000000";

// Dedicated live-test ports (see PLAN/env conventions; unique to this test).
const PROXY_PORT: &str = "8898";
const BACKEND_PORT: &str = "6373";
const POSTGRES_PORT: &str = "5447";
const VALKEY_PORT: &str = "6392";

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
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

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

fn tree_contains(root: &Path, needle: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { return false };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json")
                && std::fs::read_to_string(&path).unwrap_or_default().contains(needle)
            {
                return true;
            }
        }
    }
    false
}

fn shape_name(file: &Value, page_id: &str, shape_id: &str) -> Option<String> {
    file.pointer(&format!("/data/pagesIndex/{page_id}/objects/{shape_id}/name"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The workspace target/debug dir (test exe lives in target/debug/deps/).
fn target_debug_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent()?.parent().map(Path::to_path_buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs runtime/ artifacts + JDK + valkey; boots a full live stack"]
async fn live_app_boot_tray_contract_and_pause_resume() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();

    let data_dir = tempfile::tempdir().unwrap();
    let designs_dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded at this point in the test; the only test in
    // the binary, so no env races with other tests.
    std::env::set_var("PENPOT_LOCAL_DATA_DIR", data_dir.path());
    std::env::set_var("PENPOT_LOCAL_DESIGNS_DIR", designs_dir.path());
    std::env::set_var("PENPOT_LOCAL_PROXY_PORT", PROXY_PORT);
    std::env::set_var("PENPOT_LOCAL_BACKEND_PORT", BACKEND_PORT);
    std::env::set_var("PENPOT_LOCAL_POSTGRES_PORT", POSTGRES_PORT);
    std::env::set_var("PENPOT_LOCAL_VALKEY_PORT", VALKEY_PORT);
    // The test exe lives in target/debug/deps/, so the supervisor's
    // "sibling of current_exe" watchdog resolution misses target/debug/
    // — point PENPOT_WATCHDOG_BIN there when the bin exists.
    if let Some(debug_dir) = target_debug_dir() {
        let watchdog = debug_dir.join("penpot-watchdog");
        if watchdog.is_file() {
            std::env::set_var("PENPOT_WATCHDOG_BIN", &watchdog);
        }
    }

    let config = penpot_desktop::AppConfig::resolve().expect("config");
    let app = penpot_desktop::boot(config).await.expect("boot");

    // --- the tray contract: daemon status + control are exposed ----------
    let status = app.sync_status().expect("sync daemon must expose status");
    let control = app.sync_control().expect("sync daemon must expose control");
    assert!(!status.borrow().paused);

    // --- seed: project + file + rect via RPC (through the proxy) ---------
    let token = app.credentials.access_token.clone().expect("token");
    let rpc = PenpotClient::new(&app.proxy_url).with_auth(Auth::Token(token));
    let team_id = app.profile.default_team_id.clone().expect("team id");
    let proj_name = format!("M3 App {}", &uuid_v4()[..8]);
    let project = rpc.create_project(&team_id, &proj_name).await.expect("create-project");
    let file = rpc.create_file(&project.id, "appfile").await.expect("create-file");
    let page_id = file.first_page_id().expect("page id").to_string();
    let shape_a = uuid_v4();
    let session = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        file.revn,
        file.vern,
        &[add_rect_change(&shape_a, &page_id, "app-orig", 10.0, 10.0)],
    )
    .await
    .expect("update-file");

    let file_dir = designs_dir.path().join(&proj_name).join("appfile.penpot");
    let took = wait_until("initial export on disk", Duration::from_secs(60), || {
        tree_contains(&file_dir, "app-orig")
    })
    .await;
    println!("LATENCY app-daemon initial export: {took:?}");
    // Let the ledger/status settle (the export briefly flips Pending while
    // the watcher hash-verifies our own write).
    tokio::time::sleep(Duration::from_secs(4)).await;

    // --- Direction B through the app's daemon ----------------------------
    let hits = edit_tree_json(&file_dir, "\"name\": \"app-orig\"", "\"name\": \"app-disk-edit\"");
    assert!(hits > 0);
    let t0 = Instant::now();
    let took = wait_until("external edit → DB", Duration::from_secs(30), || {
        let f = futures_block(rpc.get_file(&file.id)).expect("get-file");
        shape_name(&f, &page_id, &shape_a).as_deref() == Some("app-disk-edit")
    })
    .await;
    println!("LATENCY direction-B external edit → get-file: {took:?} ({:?})", t0.elapsed());

    // --- Direction A + export no-bounce ----------------------------------
    tokio::time::sleep(Duration::from_secs(3)).await;
    let db = futures_block(rpc.get_project_files(&project.id))
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    let shape_b = uuid_v4();
    rpc.update_file(
        &file.id,
        &session,
        db.revn,
        db.vern,
        &[add_rect_change(&shape_b, &page_id, "app-db-edit", 300.0, 10.0)],
    )
    .await
    .expect("update-file (rect B)");
    let took = wait_until("DB edit exported to disk", Duration::from_secs(30), || {
        tree_contains(&file_dir, "app-db-edit")
    })
    .await;
    println!("LATENCY direction-A DB edit → on disk: {took:?}");
    tokio::time::sleep(Duration::from_secs(6)).await; // > fs debounce + import
    let settled = futures_block(rpc.get_project_files(&project.id))
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let later = futures_block(rpc.get_project_files(&project.id))
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    assert_eq!(
        (settled.revn, &settled.modified_at),
        (later.revn, &later.modified_at),
        "IMPORT BOUNCE: our own export was re-imported"
    );
    println!("LOOP-PREVENTION: no import bounce after a Direction A export");

    // --- pause via a DIRECT SyncControl call ------------------------------
    control.pause();
    wait_until("paused visible in status snapshot", Duration::from_secs(5), || {
        status.borrow().paused
    })
    .await;
    let db_at_pause = futures_block(rpc.get_project_files(&project.id))
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    // Edits accumulate on disk while paused…
    let hits =
        edit_tree_json(&file_dir, "\"name\": \"app-disk-edit\"", "\"name\": \"paused-edit-1\"");
    assert!(hits > 0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let hits =
        edit_tree_json(&file_dir, "\"name\": \"paused-edit-1\"", "\"name\": \"paused-edit-2\"");
    assert!(hits > 0);
    // …and the paused daemon must sync NOTHING (give it several fs-debounce
    // + poll windows to prove the point).
    tokio::time::sleep(Duration::from_secs(8)).await;
    let db_still = futures_block(rpc.get_project_files(&project.id))
        .expect("get-project-files")
        .into_iter()
        .find(|f| f.id == file.id)
        .unwrap();
    assert_eq!(
        (db_at_pause.revn, &db_at_pause.modified_at),
        (db_still.revn, &db_still.modified_at),
        "paused daemon imported a disk edit"
    );
    let f = futures_block(rpc.get_file(&file.id)).expect("get-file");
    assert_eq!(
        shape_name(&f, &page_id, &shape_a).as_deref(),
        Some("app-disk-edit"),
        "paused daemon must not have applied the disk edits"
    );
    println!("PAUSE: 2 disk edits accumulated over ~11s, DB provably untouched");

    // --- resume → catch-up rescan applies the accumulated state ----------
    control.resume();
    let took = wait_until("resume catch-up applies disk edits", Duration::from_secs(30), || {
        let f = futures_block(rpc.get_file(&file.id)).expect("get-file");
        shape_name(&f, &page_id, &shape_a).as_deref() == Some("paused-edit-2")
    })
    .await;
    println!("LATENCY resume → catch-up import visible in get-file: {took:?}");
    // No conflict copy: the DB never moved while paused.
    let project_dir = designs_dir.path().join(&proj_name);
    let conflicts: Vec<_> = std::fs::read_dir(&project_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".conflict-")
        })
        .collect();
    assert!(conflicts.is_empty(), "clean pause/resume must not create conflict copies");

    // --- clean shutdown ----------------------------------------------------
    let _ = rpc.delete_project(&project.id).await;
    app.shutdown().await;
    println!("SHUTDOWN clean");
}

/// Await an RPC future from inside a sync closure (`wait_until` passes a
/// `FnMut() -> bool`). Runs on the current runtime via `block_in_place`.
fn futures_block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
