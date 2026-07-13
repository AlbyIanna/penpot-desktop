//! SIGKILL orphan watchdog.
//!
//! Problem (docs/milestones/m2.md "Implications for M3"): SIGTERM/Ctrl+C
//! shutdown is clean, but SIGKILL of the app orphans postgres+valkey+java,
//! which keep holding their ports — and with two-way sync an orphaned backend
//! that keeps accepting writes is a data-consistency hazard, not just a port
//! squat.
//!
//! # Mechanism
//!
//! The supervisor spawns ONE tiny watchdog child (`penpot-watchdog`, a second
//! bin in this crate) holding the read end of a pipe whose write end lives in
//! the app process (the watchdog's piped stdin — Rust's std creates those
//! pipe fds with `FD_CLOEXEC`, so the penpot children spawned afterwards do
//! NOT inherit the write end; the pipe's lifetime is exactly the app
//! process's lifetime).
//!
//! The supervisor writes a line-oriented protocol:
//!
//! ```text
//! grace <ms>            # SIGTERM→SIGKILL grace period (optional, default 5000)
//! pgdata <path>         # PGDATA path for the cmdline fallback (optional)
//! pids <p1> <p2> ...    # the CURRENT child pid set; re-sent on every respawn
//! bye                   # clean shutdown: exit WITHOUT killing anything
//! ```
//!
//! When the pipe hits EOF *without* a preceding `bye` — the parent died for
//! ANY reason, including SIGKILL — the watchdog SIGTERMs the last-known pids,
//! waits the grace period, SIGKILLs survivors, and finally SIGKILLs any
//! process whose command line contains the pgdata path (postgres is started
//! detached via `pg_ctl` and re-parents itself, so a stale pid alone is not
//! enough), then exits. On `bye` it exits immediately, touching nothing.
//!
//! The watchdog runs in its own process group (so a terminal Ctrl+C does not
//! take it down with the app) and ignores SIGINT/SIGTERM/SIGHUP — the only
//! ways it exits are `bye` and pipe EOF.
//!
//! # Locating the watchdog binary
//!
//! In order of precedence:
//! 1. the `PENPOT_WATCHDOG_BIN` environment variable,
//! 2. [`crate::SupervisorConfig::watchdog_bin`],
//! 3. a sibling of the current executable named `penpot-watchdog`.
//!
//! The sibling rule covers both layouts:
//! - **cargo dev**: all workspace bins land in the same `target/<profile>/`
//!   dir — note that `cargo run -p penpot-desktop` alone does NOT build a
//!   dependency crate's bins, so dev workflows must `cargo build --workspace`
//!   (or at least `cargo build -p supervisor --bin penpot-watchdog`) first;
//!   if the binary is missing the supervisor logs a loud warning and boots
//!   without the watchdog rather than failing.
//! - **packaged**: bundle `penpot-watchdog` next to the app executable
//!   (e.g. `Contents/MacOS/` in an app bundle), or point
//!   `PENPOT_WATCHDOG_BIN` at it.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

/// Binary name searched for next to the current executable.
pub const WATCHDOG_BIN_NAME: &str = "penpot-watchdog";
/// Env var overriding the watchdog binary location.
pub const WATCHDOG_BIN_ENV: &str = "PENPOT_WATCHDOG_BIN";
/// Default SIGTERM→SIGKILL grace period.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Protocol (pure; unit-tested)
// ---------------------------------------------------------------------------

/// One parsed protocol line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Replaces (not extends) the known child pid set.
    Pids(Vec<u32>),
    /// PGDATA path for the command-line fallback kill.
    Pgdata(String),
    /// Grace period in milliseconds.
    GraceMs(u64),
    /// Clean shutdown: exit without killing anything.
    Bye,
}

/// Parse one protocol line. Unknown/malformed lines yield `None` and are
/// ignored by the watchdog (forward compatibility).
pub fn parse_line(raw: &str) -> Option<Line> {
    let line = raw.trim();
    if line == "bye" {
        return Some(Line::Bye);
    }
    if line == "pids" {
        // An explicitly empty pid set is valid (all children currently down).
        return Some(Line::Pids(Vec::new()));
    }
    if let Some(rest) = line.strip_prefix("pids ") {
        let pids = rest
            .split_whitespace()
            .filter_map(|t| t.parse::<u32>().ok())
            .collect();
        return Some(Line::Pids(pids));
    }
    if let Some(rest) = line.strip_prefix("pgdata ") {
        let path = rest.trim();
        if path.is_empty() {
            return None;
        }
        return Some(Line::Pgdata(path.to_string()));
    }
    if let Some(rest) = line.strip_prefix("grace ") {
        return rest.trim().parse::<u64>().ok().map(Line::GraceMs);
    }
    None
}

/// Accumulated watchdog state (what to kill if the parent dies).
#[derive(Debug, Clone)]
pub struct WatchdogState {
    /// Last-known child pid set (each `pids` line replaces it wholesale).
    pub pids: Vec<u32>,
    /// PGDATA path for the ps-cmdline fallback.
    pub pgdata: Option<String>,
    /// SIGTERM→SIGKILL grace.
    pub grace: Duration,
}

impl Default for WatchdogState {
    fn default() -> Self {
        WatchdogState { pids: Vec::new(), pgdata: None, grace: DEFAULT_GRACE }
    }
}

impl WatchdogState {
    /// Apply one parsed line ([`Line::Bye`] is handled by the caller, not here).
    pub fn apply(&mut self, line: Line) {
        match line {
            Line::Pids(pids) => self.pids = pids,
            Line::Pgdata(path) => self.pgdata = Some(path),
            Line::GraceMs(ms) => self.grace = Duration::from_millis(ms),
            Line::Bye => {}
        }
    }
}

/// The pids actually safe to signal: dedup, never pid 0/1, never ourselves.
pub fn kill_targets(pids: &[u32], self_pid: u32) -> Vec<u32> {
    let mut out: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|&p| p > 1 && p != self_pid)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Parse `ps -axww -o pid=,command=` output and return the pids whose command
/// line contains `pgdata` (postgres re-parents/pgroups itself after `pg_ctl`
/// detaches it, so pid tracking alone can miss it). Excludes pid 0/1 and
/// `self_pid`.
pub fn pgdata_matches(ps_output: &str, pgdata: &str, self_pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for line in ps_output.lines() {
        let line = line.trim_start();
        let Some((pid_str, command)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        if pid <= 1 || pid == self_pid {
            continue;
        }
        if command.contains(pgdata) {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Unix runtime (used by the `penpot-watchdog` bin and the supervisor)
// ---------------------------------------------------------------------------

/// `kill(pid, 0)` liveness probe (EPERM also means "exists").
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn send_signal(pid: u32, signum: i32) {
    // Safety: plain kill(2); worst case ESRCH on an already-dead pid.
    unsafe {
        libc::kill(pid as i32, signum);
    }
}

/// Snapshot of all processes: `<pid> <command line>` per line.
#[cfg(unix)]
fn ps_snapshot() -> String {
    match std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) => {
            eprintln!("[penpot-watchdog] ps failed ({error}); skipping pgdata fallback");
            String::new()
        }
    }
}

/// The EOF path: SIGTERM the last-known pids, wait `grace`, SIGKILL
/// survivors, then SIGKILL anything whose command line contains the pgdata
/// path (multiple passes to catch stragglers). Blocking; called by the
/// watchdog bin right before it exits.
#[cfg(unix)]
pub fn run_kill_sequence(state: &WatchdogState) {
    use std::time::Instant;

    let self_pid = std::process::id();
    let targets = kill_targets(&state.pids, self_pid);
    eprintln!(
        "[penpot-watchdog] parent died (pipe EOF without bye); SIGTERM {targets:?}, grace {:?}",
        state.grace
    );
    for &pid in &targets {
        send_signal(pid, libc::SIGTERM);
    }

    let deadline = Instant::now() + state.grace;
    let mut survivors = targets;
    loop {
        survivors.retain(|&pid| process_alive(pid));
        if survivors.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!("[penpot-watchdog] grace elapsed; SIGKILL survivors {survivors:?}");
            for &pid in &survivors {
                send_signal(pid, libc::SIGKILL);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let Some(pgdata) = &state.pgdata {
        // postgres detaches from pg_ctl and its workers re-title themselves;
        // the postmaster's command line keeps `-D <pgdata>` though, so match
        // on the pgdata path. A few passes catch respawn/teardown races.
        for pass in 1..=3 {
            let extra = pgdata_matches(&ps_snapshot(), pgdata, self_pid);
            if extra.is_empty() {
                break;
            }
            eprintln!("[penpot-watchdog] pgdata fallback pass {pass}: SIGKILL {extra:?}");
            for &pid in &extra {
                send_signal(pid, libc::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    eprintln!("[penpot-watchdog] done");
}

/// Read the postmaster pid from `<pgdata>/postmaster.pid` (first line),
/// returning it only if that process is currently alive (guards against a
/// stale file after an unclean postgres death).
#[cfg(unix)]
pub fn read_postmaster_pid(pgdata: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pgdata.join("postmaster.pid")).ok()?;
    let pid = text.lines().next()?.trim().parse::<u32>().ok()?;
    process_alive(pid).then_some(pid)
}

// ---------------------------------------------------------------------------
// Parent-side handle (lives in the app process)
// ---------------------------------------------------------------------------

/// Parent-side handle over a spawned watchdog: owns the pipe's write end
/// (the watchdog's stdin) and the child handle.
///
/// Dropping the handle WITHOUT [`bye`](Self::bye) closes the pipe, which the
/// watchdog treats as parent death — that is deliberate: it makes the
/// watchdog a backstop for every non-`shutdown()` exit path (panic, abort,
/// `Drop`-only teardown).
#[cfg(unix)]
pub struct WatchdogHandle {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

#[cfg(unix)]
impl WatchdogHandle {
    /// Resolve the watchdog binary: `PENPOT_WATCHDOG_BIN` env → explicit
    /// config path → sibling of the current executable.
    pub fn locate_bin(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(WATCHDOG_BIN_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        if let Some(path) = explicit {
            if path.is_file() {
                return Some(path.to_path_buf());
            }
        }
        let exe = std::env::current_exe().ok()?;
        let sibling = exe.parent()?.join(WATCHDOG_BIN_NAME);
        sibling.is_file().then_some(sibling)
    }

    /// Spawn the watchdog (own process group, piped stdin) and send the
    /// initial `grace`/`pgdata` lines.
    pub fn spawn(
        bin: &Path,
        grace: Duration,
        pgdata: Option<&Path>,
    ) -> std::io::Result<WatchdogHandle> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let mut child = std::process::Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            // Own process group: a terminal Ctrl+C (SIGINT to the foreground
            // group) must not kill the watchdog before it can do its job.
            .process_group(0)
            .spawn()?;
        let stdin = child.stdin.take();
        let mut handle = WatchdogHandle { child, stdin };
        handle.send_line(&format!("grace {}", grace.as_millis()))?;
        if let Some(pgdata) = pgdata {
            handle.send_line(&format!("pgdata {}", pgdata.display()))?;
        }
        Ok(handle)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "watchdog stdin already closed")
        })?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    /// Send the current child pid set (replaces the previous set; call again
    /// on every respawn).
    pub fn send_pids(&mut self, pids: &[u32]) -> std::io::Result<()> {
        let joined = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(" ");
        self.send_line(&format!("pids {joined}"))
    }

    /// Clean shutdown: send `bye` (the watchdog exits WITHOUT killing
    /// anything), close the pipe, and wait up to `wait` for it to exit —
    /// SIGKILLing it if it somehow lingers (children are already stopped by
    /// the time this is called, so that is safe).
    pub fn bye(&mut self, wait: Duration) {
        use std::time::Instant;
        let _ = self.send_line("bye");
        drop(self.stdin.take());
        let deadline = Instant::now() + wait;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pids_lines() {
        assert_eq!(parse_line("pids 100 200 300"), Some(Line::Pids(vec![100, 200, 300])));
        assert_eq!(parse_line("pids 42"), Some(Line::Pids(vec![42])));
        // Explicitly empty set is valid (all children currently down).
        assert_eq!(parse_line("pids"), Some(Line::Pids(vec![])));
        assert_eq!(parse_line("pids "), Some(Line::Pids(vec![])));
        // Trailing newline / surrounding whitespace tolerated.
        assert_eq!(parse_line("  pids 7 8\n"), Some(Line::Pids(vec![7, 8])));
        // Invalid tokens are skipped, valid ones kept.
        assert_eq!(parse_line("pids 10 nope -3 20"), Some(Line::Pids(vec![10, 20])));
    }

    #[test]
    fn parses_pgdata_including_spaces_in_path() {
        assert_eq!(
            parse_line("pgdata /tmp/x/postgres/data"),
            Some(Line::Pgdata("/tmp/x/postgres/data".to_string()))
        );
        // macOS app-data paths contain spaces.
        assert_eq!(
            parse_line("pgdata /Users/u/Library/Application Support/penpot-local/postgres/data"),
            Some(Line::Pgdata(
                "/Users/u/Library/Application Support/penpot-local/postgres/data".to_string()
            ))
        );
        assert_eq!(parse_line("pgdata "), None);
        assert_eq!(parse_line("pgdata"), None);
    }

    #[test]
    fn parses_grace_and_bye() {
        assert_eq!(parse_line("grace 5000"), Some(Line::GraceMs(5000)));
        assert_eq!(parse_line("grace abc"), None);
        assert_eq!(parse_line("bye"), Some(Line::Bye));
        assert_eq!(parse_line("bye now"), None);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("kill everything"), None);
        assert_eq!(parse_line("pidsX 1 2"), None);
    }

    #[test]
    fn state_pids_replace_not_extend() {
        let mut state = WatchdogState::default();
        state.apply(Line::Pids(vec![10, 20]));
        assert_eq!(state.pids, vec![10, 20]);
        // A respawn re-sends the FULL set; old pids must be forgotten.
        state.apply(Line::Pids(vec![10, 30]));
        assert_eq!(state.pids, vec![10, 30]);
        state.apply(Line::Pids(vec![]));
        assert_eq!(state.pids, Vec::<u32>::new());
    }

    #[test]
    fn state_applies_grace_and_pgdata() {
        let mut state = WatchdogState::default();
        assert_eq!(state.grace, DEFAULT_GRACE);
        state.apply(Line::GraceMs(1234));
        assert_eq!(state.grace, Duration::from_millis(1234));
        state.apply(Line::Pgdata("/data/pg".into()));
        assert_eq!(state.pgdata.as_deref(), Some("/data/pg"));
    }

    #[test]
    fn kill_targets_excludes_self_init_and_dupes() {
        assert_eq!(kill_targets(&[0, 1, 42, 42, 7, 999], 999), vec![7, 42]);
        assert_eq!(kill_targets(&[], 999), Vec::<u32>::new());
        assert_eq!(kill_targets(&[999], 999), Vec::<u32>::new());
    }

    #[test]
    fn pgdata_matches_parses_ps_output() {
        let ps = "\
    1 /sbin/launchd
  501 /opt/pg/bin/postgres -D /tmp/data-abc/postgres/data
  502 postgres: checkpointer
  503 /usr/bin/java -jar penpot.jar
  600 grep /tmp/data-abc/postgres/data
  700 /opt/pg/bin/postgres -D /tmp/data-abc/postgres/data
";
        let hits = pgdata_matches(ps, "/tmp/data-abc/postgres/data", 600);
        // 501 + 700 match; 600 is excluded as self; 1 is init; workers (502)
        // do not carry the path and are covered by postmaster death instead.
        assert_eq!(hits, vec![501, 700]);
    }

    #[test]
    fn pgdata_matches_handles_paths_with_spaces() {
        let ps = "  90 /opt/pg/bin/postgres -D /Users/u/Application Support/pl/postgres/data\n";
        assert_eq!(
            pgdata_matches(ps, "/Users/u/Application Support/pl/postgres/data", 1),
            vec![90]
        );
    }

    #[test]
    fn pgdata_matches_empty_and_garbage_input() {
        assert_eq!(pgdata_matches("", "/data", 1), Vec::<u32>::new());
        assert_eq!(pgdata_matches("notapid /data\n", "/data", 1), Vec::<u32>::new());
    }
}
