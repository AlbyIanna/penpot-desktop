//! Attach-or-fail: find a running daemon, or explain how to start one.
//!
//! The CLI never boots a stack implicitly. The spike measured a second stack
//! dying on the postgres lock in ~2 s — the data dir is exclusive — and a warm
//! self-boot costs ~11 s. Booting from a verb would therefore either race the
//! running app or stall an interactive command; either way the core invariant
//! assumes ONE aggressive reconciliation at boot, not two concurrent ones.

use std::path::Path;

use anyhow::Result;

// `Debug` is required for `Result::unwrap_err`'s bound on `T` (the Ok side),
// which the first test below calls — not in the brief's snippet, but needed
// for it to compile; no behavior change.
#[derive(Debug)]
pub struct Attached {
    pub base: String,
    pub token: String,
}

impl Attached {
    /// Build a request to the running daemon, carrying the control token and
    /// deliberately no `Origin` — the server rejects anything browser-shaped.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header(app_core::runtime_record::TOKEN_HEADER, &self.token)
    }
}

/// `kill(pid, 0)` liveness probe: signal 0 performs the existence/permission
/// check without delivering anything. A zero return or `EPERM` (the process
/// exists but we lack permission to signal it) both mean "alive"; `ESRCH`
/// (anything else) means it is not. Same idiom already used twice in this
/// workspace — `supervisor::child::process_alive` and
/// `supervisor::watchdog::process_alive` — just not reusable here without
/// adding a dependency on `supervisor` for one line, so it is repeated
/// rather than imported. Deliberately does not try to detect pid reuse (a
/// recycled pid looking alive when it's actually a different process); the
/// per-boot control token is the backstop for that, per the task brief.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// No non-unix target is packaged yet; default to "alive" so an unimplemented
/// check never turns into a false-positive stale-record purge.
#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

pub fn attach(data_dir: &Path) -> Result<Attached> {
    let record = app_core::runtime_record::read_record(data_dir)?;
    if !pid_is_alive(record.pid) {
        anyhow::bail!(
            "found a stale {} for pid {} — no such process. The app was killed without \
             cleaning up. Start one with `penpot daemon`.",
            app_core::runtime_record::RECORD_FILE_NAME,
            record.pid,
        );
    }
    Ok(Attached {
        base: format!("http://127.0.0.1:{}", record.control_port),
        token: record.token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaching_with_no_running_instance_names_the_daemon_verb() {
        let dir = tempfile::tempdir().unwrap();
        let err = attach(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("penpot daemon"),
            "attach failure must tell the user how to fix it, got: {err}"
        );
    }

    #[test]
    fn attaching_finds_a_published_instance() {
        let dir = tempfile::tempdir().unwrap();
        app_core::runtime_record::write_record(
            dir.path(),
            &app_core::runtime_record::RuntimeRecord {
                pid: std::process::id(),
                proxy_port: 9062,
                control_port: 9063,
                token: "s3cret".to_string(),
            },
        )
        .unwrap();
        let attached = attach(dir.path()).unwrap();
        assert_eq!(attached.base, "http://127.0.0.1:9063");
        assert_eq!(attached.token, "s3cret");
    }

    #[test]
    fn a_request_carries_the_token_and_never_an_origin() {
        let dir = tempfile::tempdir().unwrap();
        app_core::runtime_record::write_record(
            dir.path(),
            &app_core::runtime_record::RuntimeRecord {
                pid: std::process::id(),
                proxy_port: 9062,
                control_port: 9063,
                token: "s3cret".to_string(),
            },
        )
        .unwrap();
        let built = attach(dir.path())
            .unwrap()
            .request(reqwest::Method::GET, "/active")
            .build()
            .unwrap();
        assert_eq!(
            built.headers().get(app_core::runtime_record::TOKEN_HEADER).unwrap(),
            "s3cret"
        );
        assert!(built.headers().get("origin").is_none());
    }

    #[test]
    fn the_current_processs_own_pid_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn a_pid_that_cannot_exist_is_not_alive() {
        // Spawn a trivial child and reap it: its pid is guaranteed dead
        // (and not yet recycled) the instant `wait()` returns, unlike a
        // guessed large pid number which some systems could still have in
        // use.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for `true` to exit");
        assert!(!pid_is_alive(pid));
    }

    /// The behaviour the brief singles out for TDD: a `runtime.json` whose
    /// `pid` is no longer alive (a `SIGKILL` mid-vault-switch — the
    /// project's own N5 gate does this on purpose — leaves exactly this
    /// residue) must be rejected, not attached to. Deleting the liveness
    /// check in `attach()` and leaving every other test in this module
    /// untouched still leaves all of THEM green — this is the one test that
    /// would catch it going missing.
    #[test]
    fn attach_rejects_a_stale_record_whose_pid_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let dead_pid = child.id();
        child.wait().expect("wait for `true` to exit");

        app_core::runtime_record::write_record(
            dir.path(),
            &app_core::runtime_record::RuntimeRecord {
                pid: dead_pid,
                proxy_port: 9062,
                control_port: 9063,
                token: "s3cret".to_string(),
            },
        )
        .unwrap();

        let err = attach(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("stale") && err.contains("penpot daemon"),
            "a dead-pid record must be rejected with a message naming the fix, got: {err}"
        );
    }
}
