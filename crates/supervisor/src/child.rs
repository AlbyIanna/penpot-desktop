//! Generic supervised child process: spawn → readiness probe → watch → crash
//! restart with exponential backoff → graceful stop (SIGTERM, then SIGKILL
//! after a grace period).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::probe;
use crate::{Notifier, RestartPolicy, Service, SupervisorEvent};

/// Readiness probe attached to a supervised child.
#[derive(Debug, Clone)]
pub(crate) enum Probe {
    /// RESP `PING` → `+PONG` (valkey).
    ValkeyPing { port: u16 },
    /// `GET <path>` returns 200 (penpot backend `/readyz`).
    HttpOk { port: u16, path: String },
}

impl Probe {
    async fn check(&self) -> Result<(), String> {
        match self {
            Probe::ValkeyPing { port } => probe::valkey_ping(*port).await,
            Probe::HttpOk { port, path } => probe::http_ok(*port, path).await,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChildSpec {
    pub service: Service,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub probe: Probe,
    pub ready_timeout: Duration,
}

/// Readiness as observed by `Supervisor::start`.
#[derive(Debug, Clone)]
pub(crate) enum ReadyState {
    Pending,
    Ready,
    /// Terminal: the service gave up (retries exhausted or unable to spawn).
    Failed(String),
}

/// Handle to a supervised service task.
pub(crate) struct ServiceHandle {
    service: Service,
    pid: Arc<Mutex<Option<u32>>>,
    shutdown_tx: watch::Sender<bool>,
    ready_rx: watch::Receiver<ReadyState>,
    task: Option<JoinHandle<()>>,
    grace: Duration,
}

impl ServiceHandle {
    pub(crate) fn spawn(
        spec: ChildSpec,
        policy: RestartPolicy,
        grace: Duration,
        notifier: Notifier,
    ) -> Self {
        let service = spec.service;
        let pid = Arc::new(Mutex::new(None));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = watch::channel(ReadyState::Pending);
        let task = tokio::spawn(supervise(
            spec,
            policy,
            grace,
            notifier,
            Arc::clone(&pid),
            shutdown_rx,
            ready_tx,
        ));
        ServiceHandle { service, pid, shutdown_tx, ready_rx, task: Some(task), grace }
    }

    /// Wait until the service is ready for the first time (or has given up).
    pub(crate) async fn wait_ready(&self) -> Result<(), String> {
        let mut rx = self.ready_rx.clone();
        crate::await_ready(&mut rx).await
    }

    pub(crate) fn current_pid(&self) -> Option<u32> {
        *self.pid.lock().expect("pid mutex")
    }

    /// Shared live view of the current pid (updates across crash respawns);
    /// used by the orphan-watchdog pid feeder.
    #[cfg(unix)]
    pub(crate) fn pid_slot(&self) -> Arc<Mutex<Option<u32>>> {
        Arc::clone(&self.pid)
    }

    /// Orderly stop: signal the supervision task, which SIGTERMs the child and
    /// SIGKILLs it after the grace period. Falls back to a hard kill if the
    /// task itself does not wind down.
    pub(crate) async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(self.grace + Duration::from_secs(5), task)
                .await
                .is_err()
            {
                warn!(service = %self.service, "supervision task stuck; hard-killing");
                abort.abort();
                self.kill_pid_now();
            }
        }
    }

    /// Last-resort synchronous kill (used from `Drop`): abort the supervision
    /// task (its `Child` has `kill_on_drop`) and SIGKILL by pid.
    pub(crate) fn kill_now(mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.kill_pid_now();
    }

    fn kill_pid_now(&self) {
        if let Some(pid) = self.current_pid() {
            kill_signal(pid, KillSignal::Kill);
        }
    }
}

enum KillSignal {
    Term,
    Kill,
}

#[cfg(unix)]
fn kill_signal(pid: u32, signal: KillSignal) {
    let signum = match signal {
        KillSignal::Term => libc::SIGTERM,
        KillSignal::Kill => libc::SIGKILL,
    };
    // Safety: plain kill(2) on a pid we spawned; worst case it no longer
    // exists and kill returns ESRCH.
    unsafe {
        libc::kill(pid as i32, signum);
    }
}

#[cfg(not(unix))]
fn kill_signal(_pid: u32, _signal: KillSignal) {
    // No SIGTERM equivalent; the caller escalates to Child::kill.
}

/// Wait until the shutdown flag flips to true (or the sender is dropped,
/// which we also treat as shutdown so children never outlive the supervisor).
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// SIGTERM the child, wait up to `grace`, then SIGKILL.
async fn terminate(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        kill_signal(pid, KillSignal::Term);
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
        warn!("child ignored SIGTERM for {grace:?}; sending SIGKILL");
    }
    let _ = child.kill().await; // SIGKILL + reap
}

enum ReadyOutcome {
    Ready,
    Exited(String),
    ProbeTimeout,
    ShuttingDown,
}

async fn readiness_phase(
    child: &mut Child,
    probe: &Probe,
    ready_timeout: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> ReadyOutcome {
    let deadline = Instant::now() + ready_timeout;
    loop {
        if Instant::now() >= deadline {
            return ReadyOutcome::ProbeTimeout;
        }
        tokio::select! {
            _ = wait_shutdown(shutdown_rx) => return ReadyOutcome::ShuttingDown,
            status = child.wait() => {
                let status = status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| format!("wait failed: {e}"));
                return ReadyOutcome::Exited(format!("exited during startup: {status}"));
            }
            result = probe.check() => {
                match result {
                    Ok(()) => return ReadyOutcome::Ready,
                    Err(reason) => {
                        debug!(%reason, "probe not ready yet");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// The supervision loop for one service.
#[allow(clippy::too_many_lines)]
async fn supervise(
    spec: ChildSpec,
    policy: RestartPolicy,
    grace: Duration,
    notifier: Notifier,
    pid_slot: Arc<Mutex<Option<u32>>>,
    mut shutdown_rx: watch::Receiver<bool>,
    ready_tx: watch::Sender<ReadyState>,
) {
    let service = spec.service;
    let mut attempts: u32 = 0;
    let mut ever_ready = false;

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        // ---- spawn -------------------------------------------------------
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &spec.envs {
            command.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        let failure_reason: String;
        match command.spawn() {
            Ok(mut child) => {
                *pid_slot.lock().expect("pid mutex") = child.id();
                notifier.emit(SupervisorEvent::Starting { service });

                // ---- readiness -------------------------------------------
                match readiness_phase(&mut child, &spec.probe, spec.ready_timeout, &mut shutdown_rx)
                    .await
                {
                    ReadyOutcome::Ready => {
                        let ready_at = Instant::now();
                        if ever_ready {
                            notifier.emit(SupervisorEvent::Restarted { service });
                        } else {
                            ever_ready = true;
                            notifier.emit(SupervisorEvent::Ready { service });
                        }
                        let _ = ready_tx.send(ReadyState::Ready);

                        // ---- steady state --------------------------------
                        tokio::select! {
                            _ = wait_shutdown(&mut shutdown_rx) => {
                                terminate(&mut child, grace).await;
                                *pid_slot.lock().expect("pid mutex") = None;
                                notifier.emit(SupervisorEvent::Stopped { service });
                                return;
                            }
                            status = child.wait() => {
                                let status = status
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|e| format!("wait failed: {e}"));
                                if ready_at.elapsed() >= policy.stable_after {
                                    attempts = 0; // was stable; fresh retry budget
                                }
                                failure_reason = format!("exited unexpectedly: {status}");
                            }
                        }
                    }
                    ReadyOutcome::ShuttingDown => {
                        terminate(&mut child, grace).await;
                        *pid_slot.lock().expect("pid mutex") = None;
                        notifier.emit(SupervisorEvent::Stopped { service });
                        return;
                    }
                    ReadyOutcome::Exited(reason) => {
                        failure_reason = reason;
                    }
                    ReadyOutcome::ProbeTimeout => {
                        terminate(&mut child, grace).await;
                        failure_reason =
                            format!("readiness probe timed out after {:?}", spec.ready_timeout);
                    }
                }
            }
            Err(error) => {
                failure_reason = format!("failed to spawn {}: {error}", spec.program.display());
            }
        }

        // ---- crash path -------------------------------------------------
        *pid_slot.lock().expect("pid mutex") = None;
        let _ = ready_tx.send(ReadyState::Pending);
        attempts += 1;
        if attempts > policy.max_retries {
            notifier.emit(SupervisorEvent::Crashed { service, attempt: attempts, restarting: false });
            notifier.emit(SupervisorEvent::GaveUp { service });
            let _ = ready_tx.send(ReadyState::Failed(failure_reason));
            return;
        }
        notifier.emit(SupervisorEvent::Crashed { service, attempt: attempts, restarting: true });
        tokio::select! {
            _ = tokio::time::sleep(policy.backoff(attempts)) => {}
            _ = wait_shutdown(&mut shutdown_rx) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Real-process integration tests (valkey-server). These spawn the actual
// binary; they are skipped (with a loud message) only when valkey-server is
// not installed on the machine.
// ---------------------------------------------------------------------------
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::probe::valkey_ping;

    const VALKEY_CANDIDATES: &[&str] = &[
        "/opt/homebrew/bin/valkey-server",
        "/usr/local/bin/valkey-server",
        "/usr/bin/valkey-server",
    ];

    fn valkey_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("VALKEY_SERVER") {
            return Some(PathBuf::from(path));
        }
        VALKEY_CANDIDATES
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind 127.0.0.1:0")
            .local_addr()
            .expect("local addr")
            .port()
    }

    fn process_alive(pid: u32) -> bool {
        // kill(pid, 0) probes existence without sending a signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn valkey_spec(program: PathBuf, port: u16, dir: &std::path::Path) -> ChildSpec {
        ChildSpec {
            service: Service::Valkey,
            program,
            args: vec![
                "--port".into(),
                port.to_string(),
                "--bind".into(),
                "127.0.0.1".into(),
                "--save".into(),
                String::new(),
                "--appendonly".into(),
                "no".into(),
                "--daemonize".into(),
                "no".into(),
                "--dir".into(),
                dir.to_string_lossy().into_owned(),
            ],
            envs: Vec::new(),
            cwd: Some(dir.to_path_buf()),
            probe: Probe::ValkeyPing { port },
            ready_timeout: Duration::from_secs(10),
        }
    }

    fn recording_notifier() -> (Notifier, Arc<Mutex<Vec<SupervisorEvent>>>) {
        let events: Arc<Mutex<Vec<SupervisorEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let hook: crate::NotifyHook = Arc::new(move |event| {
            sink.lock().expect("events mutex").push(event);
        });
        (Notifier(Some(hook)), events)
    }

    async fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Real integration: spawn valkey-server, readiness = RESP PING, orderly
    /// shutdown actually terminates the process.
    #[tokio::test]
    async fn valkey_spawn_ping_and_shutdown() {
        let Some(program) = valkey_path() else {
            eprintln!("SKIP: valkey-server not found; set VALKEY_SERVER to run this test");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let port = free_port();
        let (notifier, events) = recording_notifier();

        let handle = ServiceHandle::spawn(
            valkey_spec(program, port, dir.path()),
            RestartPolicy::default(),
            Duration::from_secs(5),
            notifier,
        );
        handle.wait_ready().await.expect("valkey should become ready");
        valkey_ping(port).await.expect("PING should succeed against real valkey");
        let pid = handle.current_pid().expect("pid recorded");
        assert!(process_alive(pid));

        handle.shutdown().await;
        wait_until("valkey process to die", Duration::from_secs(5), || !process_alive(pid)).await;
        assert!(valkey_ping(port).await.is_err(), "port should be closed after shutdown");

        let events = events.lock().expect("events mutex");
        assert!(matches!(events.first(), Some(SupervisorEvent::Starting { service: Service::Valkey })));
        assert!(events.iter().any(|e| matches!(e, SupervisorEvent::Ready { .. })));
        assert!(matches!(events.last(), Some(SupervisorEvent::Stopped { .. })));
    }

    /// Real integration: SIGKILL the running valkey; the supervisor must
    /// restart it (new pid, PING answers again) and emit Crashed + Restarted.
    #[tokio::test]
    async fn valkey_restarts_after_kill() {
        let Some(program) = valkey_path() else {
            eprintln!("SKIP: valkey-server not found; set VALKEY_SERVER to run this test");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let port = free_port();
        let (notifier, events) = recording_notifier();
        let policy = RestartPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            stable_after: Duration::from_secs(30),
        };

        let handle = ServiceHandle::spawn(
            valkey_spec(program, port, dir.path()),
            policy,
            Duration::from_secs(5),
            notifier,
        );
        handle.wait_ready().await.expect("valkey should become ready");
        let first_pid = handle.current_pid().expect("pid recorded");

        // Crash it.
        kill_signal(first_pid, KillSignal::Kill);

        // The supervisor should bring it back on a new pid.
        wait_until("valkey to be restarted with a new pid", Duration::from_secs(10), || {
            matches!(handle.current_pid(), Some(pid) if pid != first_pid)
        })
        .await;
        let second_pid = handle.current_pid().expect("new pid");
        assert_ne!(second_pid, first_pid);
        wait_until("PING to succeed after restart", Duration::from_secs(10), || {
            futures_ping_ok(port)
        })
        .await;
        // The Restarted event fires on the supervisor's own probe cycle,
        // which can lag our external ping by a beat.
        wait_until("Restarted event to be emitted", Duration::from_secs(10), || {
            events
                .lock()
                .expect("events mutex")
                .iter()
                .any(|e| matches!(e, SupervisorEvent::Restarted { service: Service::Valkey }))
        })
        .await;

        {
            let events = events.lock().expect("events mutex");
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    SupervisorEvent::Crashed { service: Service::Valkey, attempt: 1, restarting: true }
                )),
                "expected a Crashed(attempt=1, restarting) event, got {events:?}"
            );
        }

        handle.shutdown().await;
        wait_until("valkey process to die", Duration::from_secs(5), || !process_alive(second_pid))
            .await;
    }

    /// Synchronous ping helper for use inside `wait_until` closures.
    fn futures_ping_ok(port: u16) -> bool {
        use std::io::{Read, Write};
        let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return false;
        };
        if stream.write_all(b"*1\r\n$4\r\nPING\r\n").is_err() {
            return false;
        }
        let mut buf = [0u8; 16];
        matches!(stream.read(&mut buf), Ok(n) if buf[..n].starts_with(b"+PONG"))
    }

    /// Retries are bounded: a service that keeps dying reports GaveUp and the
    /// readiness future resolves to an error (no real valkey needed).
    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let (notifier, events) = recording_notifier();
        let policy = RestartPolicy {
            max_retries: 2,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            stable_after: Duration::from_secs(30),
        };
        let spec = ChildSpec {
            service: Service::Valkey,
            program: PathBuf::from("/usr/bin/true"), // exits immediately
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
            probe: Probe::ValkeyPing { port: free_port() }, // nothing listens
            ready_timeout: Duration::from_secs(2),
        };
        let handle =
            ServiceHandle::spawn(spec, policy, Duration::from_secs(1), notifier);
        let result = handle.wait_ready().await;
        assert!(result.is_err(), "must not become ready");

        let events = events.lock().expect("events mutex");
        assert!(matches!(events.last(), Some(SupervisorEvent::GaveUp { .. })));
        let crashes = events
            .iter()
            .filter(|e| matches!(e, SupervisorEvent::Crashed { .. }))
            .count();
        assert_eq!(crashes, 3, "2 restarts + the final non-restarting crash");
    }

    /// Spawn failure (missing binary) also flows through the retry/GaveUp path.
    #[tokio::test]
    async fn missing_binary_fails_readiness() {
        let (notifier, _events) = recording_notifier();
        let policy = RestartPolicy {
            max_retries: 1,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            stable_after: Duration::from_secs(30),
        };
        let spec = ChildSpec {
            service: Service::Backend,
            program: PathBuf::from("/nonexistent/java"),
            args: Vec::new(),
            envs: Vec::new(),
            cwd: None,
            probe: Probe::HttpOk { port: free_port(), path: "/readyz".into() },
            ready_timeout: Duration::from_secs(1),
        };
        let handle = ServiceHandle::spawn(spec, policy, Duration::from_secs(1), notifier);
        let reason = handle.wait_ready().await.expect_err("must fail");
        assert!(reason.contains("failed to spawn"), "reason: {reason}");
    }
}
