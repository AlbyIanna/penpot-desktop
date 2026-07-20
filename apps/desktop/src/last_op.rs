//! `last-op.json` — D4: the outcome of the most recently FINISHED detached
//! vault-switch/reboot task, persisted where it outlives the very stack it
//! reports on.
//!
//! `prefs_http::PrefsState` used to keep this in memory (`last_op:
//! StdMutex<Option<LastOpStatus>>`). That cannot work: a successful switch or
//! reboot tears down the entire supervised stack — including the router
//! holding that state — and `lib.rs::boot` constructs a FRESH
//! `prefs_http::router` (and therefore a fresh, empty `PrefsState`) every time
//! it runs. `lastOp` could only ever be observed for a failure that happened
//! before teardown; a SUCCESSFUL switch/reboot always reset it back to
//! `None`, so a client polling `GET /__api/prefs` for the outcome of a
//! successful operation would spin until its own timeout. Moving the record
//! to a file in the app **data dir** (never the vault — same reasoning as
//! `prefs.rs`, `vault.rs`'s registry, and `recent.rs`: per-machine state, not
//! user work, must not travel with a cloned vault) fixes this: the data dir
//! is the one thing stable across both a reboot and a vault switch, so a
//! record written here survives the stack being replaced.
//!
//! **Write ordering.** [`record`] is called by the detached task in
//! `prefs_http.rs` AFTER `VaultRunner::switch_to` / `reboot_in_place`
//! resolves — never before. By the time either of those returns `Ok`, the new
//! stack's proxy has already finished `Proxy::bind_with_router(..).await` in
//! `lib.rs::boot` — the TCP bind is itself awaited synchronously there;
//! `serve_with_shutdown` merely starts accepting on an already-bound socket
//! in a spawned task. So a client that observes a fresh record on disk can
//! immediately reach the new stack's `GET /__api/prefs` instead of racing its
//! boot — the file becoming visible IS the "new stack is up" signal, there is
//! no earlier window where the record exists but the stack doesn't. On
//! failure the same ordering holds trivially: the record is written once the
//! attempt is fully resolved, whatever state that leaves the stack in.
//!
//! [`load`] never fails, same posture as [`crate::prefs::load`]: a missing or
//! corrupt file reads as "nothing recorded yet" rather than an error — a
//! broken piece of UI-state must never stop `GET /__api/prefs` from
//! answering.
//!
//! **Identity.** A poller (`scripts/d4_prefs_helper.py`) needs to tell a NEW
//! record apart from a stale one already sitting on disk from an earlier
//! operation — that's why a bare `{op, ok}` isn't enough baseline/target
//! comparison on its own once the record survives a process. [`seq`] is a
//! counter persisted IN the record itself: read-modify-write, the next value
//! is the previous one plus one, starting at 1 if nothing is on disk yet.
//! That means two operations that finish with an identical-looking
//! `{op, ok, error}` — even within the same wall-clock second, where `at`'s
//! one-second resolution alone could collide — still produce distinguishable
//! records. The poller's contract: capture `seq` (or `None`) as a baseline
//! before kicking off an operation, then wait for the persisted `seq` to
//! differ from that baseline.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name of the last-op store, at the root of the app's DATA dir (NOT the
/// vault).
pub const LAST_OP_FILE_NAME: &str = "last-op.json";

/// Outcome of the most recently finished detached switch/reboot. Same shape
/// (`op`, `ok`, `error`, `at`) the in-memory `LastOpStatus` this replaces had,
/// plus `seq` — see the module doc's "Identity" section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastOp {
    /// `"vaultSwitch"` or `"reboot"`.
    pub op: String,
    pub ok: bool,
    pub error: Option<String>,
    /// UTC, same stamp format `vault::SwitchMarker` uses elsewhere in this
    /// codebase — not parsed by anything here, just for a human (or the
    /// gate) glancing at `GET /__api/prefs` to see how stale it is.
    pub at: String,
    /// Monotonically increasing across writes to this file — see the module
    /// doc's "Identity" section for why a timestamp alone isn't enough.
    pub seq: u64,
}

fn store_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(LAST_OP_FILE_NAME)
}

/// Load the last-op record from `data_dir`. Never fails: a missing file, an
/// unreadable file, or corrupt/invalid JSON all degrade to `None` — "nothing
/// recorded (yet)" — rather than an error, same posture as [`crate::prefs::load`].
pub fn load(data_dir: &Path) -> Option<LastOp> {
    std::fs::read(store_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Process-wide lock guarding the `load` → bump `seq` → `atomic_write`
/// sequence in [`record`]. `switch_to`/`reboot_in_place` are serialized by
/// `VaultRunner::switch_lock`, but that guard is already dropped by the time
/// the detached task in `prefs_http.rs` calls `record` — the write happens
/// AFTER the awaited future resolves, deliberately, so it can capture the
/// caller's `Result` (see this module's "Write ordering" doc). That leaves a
/// window where two detached tasks finishing back-to-back (a reboot and a
/// switch both in flight, or two switches from two callers) could both read
/// the same `seq` before either writes, then both write — the loser's
/// outcome silently vanishes. This mutex closes that window without
/// touching `switch_lock` itself, which exists for a different purpose
/// (serializing the stop→boot dance, not the bookkeeping after it).
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Record a detached switch/reboot's outcome, atomically. `seq` is derived
/// from whatever is currently on disk (corrupt/missing counts as "nothing
/// yet", i.e. the next `seq` is 1) so it keeps climbing across process
/// restarts, not just within one. The read-modify-write is additionally
/// serialized process-wide by [`RECORD_LOCK`] — see its doc for why that is
/// needed on top of `VaultRunner::switch_lock`.
pub fn record(data_dir: &Path, op: &str, result: Result<(), String>) -> anyhow::Result<()> {
    let (ok, error) = match result {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e)),
    };
    // Poisoning would only happen if a prior call panicked mid-write, which
    // would already have taken down the process; recovering the guard here
    // (rather than propagating the poison) keeps a single unrelated panic
    // from permanently wedging every future last-op record.
    let _guard = RECORD_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seq = load(data_dir).map(|prev| prev.seq + 1).unwrap_or(1);
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let record = LastOp { op: op.to_string(), ok, error, at, seq };
    atomic_write(&store_path(data_dir), &record)
}

/// Write `value` to `path` atomically: write a sibling `.tmp` file, fsync it,
/// then rename over `path`. Same shape as `prefs.rs`'s and `vault.rs`'s
/// helpers of the same purpose — not shared past their module boundary, same
/// reasoning as `prefs.rs`'s own copy: duplicating a few lines is cheaper
/// than a new shared crate for it.
fn atomic_write<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(value)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_nothing_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()), None);
    }

    #[test]
    fn corrupt_file_reads_as_nothing_recorded_rather_than_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LAST_OP_FILE_NAME), b"{not json").unwrap();
        assert_eq!(load(tmp.path()), None, "a corrupt last-op file must not break GET /__api/prefs");
    }

    #[test]
    fn round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "reboot", Ok(())).unwrap();
        let got = load(tmp.path()).unwrap();
        assert_eq!(got.op, "reboot");
        assert!(got.ok);
        assert_eq!(got.error, None);
        assert_eq!(got.seq, 1);
    }

    #[test]
    fn failure_outcome_round_trips_with_its_error() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "vaultSwitch", Err("boom".to_string())).unwrap();
        let got = load(tmp.path()).unwrap();
        assert_eq!(got.op, "vaultSwitch");
        assert!(!got.ok);
        assert_eq!(got.error.as_deref(), Some("boom"));
    }

    #[test]
    fn consecutive_operations_are_distinguishable_even_with_identical_op_and_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "vaultSwitch", Ok(())).unwrap();
        let first = load(tmp.path()).unwrap();
        record(tmp.path(), "vaultSwitch", Ok(())).unwrap();
        let second = load(tmp.path()).unwrap();
        assert_eq!(first.op, second.op);
        assert_eq!(first.ok, second.ok);
        assert_ne!(first.seq, second.seq, "seq must climb even for identical-looking consecutive ops");
        assert_eq!(second.seq, first.seq + 1);
    }

    #[test]
    fn seq_keeps_climbing_across_a_fresh_load_read_modify_write_cycle() {
        // Simulates what happens across a process restart (a switch/reboot):
        // each `record` call independently re-reads the file from disk rather
        // than trusting any in-memory counter.
        let tmp = tempfile::tempdir().unwrap();
        for i in 1..=5u64 {
            record(tmp.path(), "reboot", Ok(())).unwrap();
            assert_eq!(load(tmp.path()).unwrap().seq, i);
        }
    }

    #[test]
    fn concurrent_records_never_collide_on_seq() {
        // Simulates two detached tasks (e.g. a reboot and a switch, both
        // triggered by the page in quick succession) finishing at roughly
        // the same time and both calling `record`. `switch_lock` in
        // `control.rs` is already released by the time either caller gets
        // here — this test is what proves `RECORD_LOCK` picks up where that
        // guard leaves off, rather than trusting the ordering documented in
        // finding 5's fix without exercising it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        const N: u64 = 8;
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || record(&path, "reboot", Ok(())).unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Without serialization, several threads can race `load` before any
        // of them `atomic_write`s, so multiple end up computing the SAME
        // `seq` and the last writer clobbers the rest — the final on-disk
        // `seq` would land below `N`. With `RECORD_LOCK` in place, every
        // call's read-modify-write is atomic with respect to the others, so
        // `N` fully serialized increments must leave `seq == N` on disk.
        assert_eq!(
            load(&path).unwrap().seq,
            N,
            "concurrent record() calls must be serialized, not lose an outcome to a seq collision"
        );
    }

    #[test]
    fn a_corrupt_existing_file_does_not_block_recording_a_fresh_op() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LAST_OP_FILE_NAME), b"not json at all").unwrap();
        record(tmp.path(), "reboot", Ok(())).unwrap();
        let got = load(tmp.path()).unwrap();
        assert_eq!(got.seq, 1, "seq restarts at 1 when the prior file was unreadable");
    }
}
