//! Integration tests for the SIGKILL orphan watchdog — the REAL mechanism
//! (real pipe, real processes, real signals), no penpot stack:
//!
//! `watchdog-fake-parent` plays the app: it spawns dummy `sleep` children in
//! their own process groups plus (optionally) a fake postmaster whose command
//! line contains a pgdata path, spawns the real `penpot-watchdog` over a
//! piped stdin, and feeds it pids. The test then either SIGKILLs the fake
//! parent (dummies must die within the grace period, pgdata dummy via the
//! ps-cmdline fallback) or lets it send `bye` (dummies must SURVIVE).

#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use supervisor::watchdog::process_alive;

const WATCHDOG_BIN: &str = env!("CARGO_BIN_EXE_penpot-watchdog");
const FAKE_PARENT_BIN: &str = env!("CARGO_BIN_EXE_watchdog-fake-parent");

/// Fake parent feeds `grace 2000`; give kills a comfortable margin on top.
const GRACE: Duration = Duration::from_millis(2000);
const MARGIN: Duration = Duration::from_secs(6);

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_pid_file(path: &Path) -> Option<HashMap<String, u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ')?;
        map.insert(key.to_string(), value.trim().parse().ok()?);
    }
    Some(map)
}

fn sigkill(pid: u32) {
    let _ = Command::new("/bin/kill").args(["-9", &pid.to_string()]).status();
}

/// Parent SIGKILLed → pipe EOF → watchdog SIGTERMs the tracked pids, kills
/// survivors after grace, and reaps the pgdata-cmdline process it was never
/// given a pid for. Then it exits itself.
#[test]
fn sigkill_parent_kills_tracked_pids_and_pgdata_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Unique path so the ps-cmdline match cannot hit unrelated processes.
    let pgdata = dir.path().join(format!("pgdata-{}", std::process::id()));
    let outfile = dir.path().join("pids.txt");

    let mut parent = Command::new(FAKE_PARENT_BIN)
        .arg(WATCHDOG_BIN)
        .arg(&outfile)
        .arg("kill")
        .arg(&pgdata)
        .spawn()
        .expect("spawn fake parent");

    wait_until("fake parent to write the pid file", Duration::from_secs(10), || {
        outfile.is_file() && read_pid_file(&outfile).is_some()
    });
    let pids = read_pid_file(&outfile).expect("pid file");
    let (s1, s2) = (pids["s1"], pids["s2"]);
    let pg = pids["pg"];
    let watchdog = pids["watchdog"];
    assert!(process_alive(s1) && process_alive(s2), "dummies must be running");
    assert!(process_alive(pg), "pgdata dummy must be running");
    assert!(process_alive(watchdog), "watchdog must be running");

    // The parent-death trigger: SIGKILL, no chance to clean up.
    parent.kill().expect("SIGKILL fake parent");
    parent.wait().expect("reap fake parent");

    wait_until("tracked dummies to die within grace", GRACE + MARGIN, || {
        !process_alive(s1) && !process_alive(s2)
    });
    wait_until("pgdata dummy to die via ps-cmdline fallback", GRACE + MARGIN, || {
        !process_alive(pg)
    });
    wait_until("watchdog to exit after the kill sequence", GRACE + MARGIN, || {
        !process_alive(watchdog)
    });
}

/// Clean shutdown: the parent sends `bye` — the watchdog must exit WITHOUT
/// killing the (deliberately still-running) dummies.
#[test]
fn bye_makes_watchdog_exit_without_killing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outfile = dir.path().join("pids.txt");

    let mut parent = Command::new(FAKE_PARENT_BIN)
        .arg(WATCHDOG_BIN)
        .arg(&outfile)
        .arg("bye")
        .spawn()
        .expect("spawn fake parent");

    wait_until("fake parent to write the pid file", Duration::from_secs(10), || {
        outfile.is_file() && read_pid_file(&outfile).is_some()
    });
    let pids = read_pid_file(&outfile).expect("pid file");
    let (s1, s2) = (pids["s1"], pids["s2"]);
    let watchdog = pids["watchdog"];
    assert!(process_alive(s1) && process_alive(s2), "dummies must be running");

    // The fake parent sends bye, waits for the watchdog to exit, then exits.
    let status = parent.wait().expect("fake parent");
    assert!(status.success(), "fake parent should exit cleanly, got {status}");
    wait_until("watchdog to exit on bye", Duration::from_secs(5), || !process_alive(watchdog));

    // Long enough for a (wrong) grace-then-kill sequence to have fired.
    std::thread::sleep(GRACE + Duration::from_millis(500));
    assert!(
        process_alive(s1) && process_alive(s2),
        "bye must leave running children untouched"
    );

    // Cleanup: the dummies are ours to reap now.
    sigkill(s1);
    sigkill(s2);
}
