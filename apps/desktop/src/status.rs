//! M3 sync-status wiring between the sync daemon and the tray UI.
//!
//! The snapshot/state types are the **real daemon's** (re-exported from
//! `sync_daemon`); the tray consumes
//!
//! - a `tokio::sync::watch::Receiver<SyncStatusSnapshot>`, and
//! - an `Arc<dyn SyncControl>` (`pause()` / `resume()`).
//!
//! Because the daemon only exists *after* the async boot sequence finishes
//! (supervisor → provisioning → proxy → daemon) while the tray must be
//! created in Tauri's `setup` on the main thread, [`DaemonStatusBridge`]
//! late-binds the two: the tray subscribes to the bridge immediately, and
//! once boot completes the bridge attaches to the real daemon, forwarding
//! every snapshot and replaying any pause the user requested during boot.
//! [`MockStatusSource`] remains for tests and the opt-in tray demo.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

pub use board_export::ExportStatusSnapshot;
pub use sync_daemon::{FileState, SyncStatusSnapshot};

/// Control handle the UI uses to pause/resume syncing. Implemented by
/// [`DaemonStatusBridge`] (production) and [`MockStatusSource`]'s control
/// (tests/demo); the real `sync_daemon::SyncControl` implements it too.
pub trait SyncControl: Send + Sync {
    fn pause(&self);
    fn resume(&self);
}

impl SyncControl for sync_daemon::SyncControl {
    fn pause(&self) {
        sync_daemon::SyncControl::pause(self);
    }
    fn resume(&self) {
        sync_daemon::SyncControl::resume(self);
    }
}

// ---------------------------------------------------------------------------
// DaemonStatusBridge — tray-before-daemon late binding
// ---------------------------------------------------------------------------

/// Bridges the tray (created at `setup`, before boot) to the sync daemon
/// (created at the end of boot). Owns its own watch channel; [`attach`]
/// starts a forwarder task that mirrors the daemon's snapshots into it.
/// Pause/resume presses that happen before attach are remembered and
/// replayed onto the daemon control at attach time, so a pause during boot
/// is honored.
///
/// [`attach`]: DaemonStatusBridge::attach
pub struct DaemonStatusBridge {
    tx: watch::Sender<SyncStatusSnapshot>,
    control: Mutex<Option<sync_daemon::SyncControl>>,
    want_paused: AtomicBool,
}

impl DaemonStatusBridge {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = watch::channel(SyncStatusSnapshot::default());
        Arc::new(DaemonStatusBridge {
            tx,
            control: Mutex::new(None),
            want_paused: AtomicBool::new(false),
        })
    }

    /// The receiver half the tray consumes.
    pub fn subscribe(&self) -> watch::Receiver<SyncStatusSnapshot> {
        self.tx.subscribe()
    }

    /// The control handle the tray's pause/resume toggle drives.
    pub fn control(self: &Arc<Self>) -> Arc<dyn SyncControl> {
        self.clone()
    }

    /// Bind to the real daemon: replay any pre-boot pause request, then
    /// forward every daemon snapshot into the bridge channel. Must be called
    /// from within a tokio runtime (the boot task).
    pub fn attach(
        self: &Arc<Self>,
        mut rx: watch::Receiver<SyncStatusSnapshot>,
        control: sync_daemon::SyncControl,
    ) {
        if self.want_paused.load(Ordering::SeqCst) {
            control.pause();
        }
        *self.control.lock().expect("bridge control mutex") = Some(control);
        let bridge = self.clone();
        tokio::spawn(async move {
            // Publish the daemon's current snapshot immediately, then mirror
            // every change until the daemon (or the tray) goes away.
            loop {
                let snapshot = rx.borrow_and_update().clone();
                let _ = bridge.tx.send(snapshot);
                if rx.changed().await.is_err() {
                    tracing::warn!("sync-daemon status channel closed; tray frozen");
                    break;
                }
            }
        });
    }
}

impl SyncControl for DaemonStatusBridge {
    fn pause(&self) {
        self.want_paused.store(true, Ordering::SeqCst);
        match &*self.control.lock().expect("bridge control mutex") {
            Some(control) => control.pause(),
            // Not attached yet: reflect the request in the tray immediately;
            // it is replayed onto the daemon at attach time.
            None => self.tx.send_modify(|s| s.paused = true),
        }
    }

    fn resume(&self) {
        self.want_paused.store(false, Ordering::SeqCst);
        match &*self.control.lock().expect("bridge control mutex") {
            Some(control) => control.resume(),
            None => self.tx.send_modify(|s| s.paused = false),
        }
    }
}

// ---------------------------------------------------------------------------
// ExportStatusBridge — same late binding for the board-export service (M5)
// ---------------------------------------------------------------------------

/// Bridges the tray's "Exports:" line (subscribed at `setup`) to the
/// board-export service (spawned at the end of boot). Read-only counterpart
/// of [`DaemonStatusBridge`]: no control surface, just snapshot forwarding.
pub struct ExportStatusBridge {
    tx: watch::Sender<ExportStatusSnapshot>,
}

impl ExportStatusBridge {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = watch::channel(ExportStatusSnapshot::default());
        Arc::new(ExportStatusBridge { tx })
    }

    /// The receiver half the tray consumes.
    pub fn subscribe(&self) -> watch::Receiver<ExportStatusSnapshot> {
        self.tx.subscribe()
    }

    /// Forward every service snapshot into the bridge channel. Must be
    /// called from within a tokio runtime (the boot task).
    pub fn attach(self: &Arc<Self>, mut rx: watch::Receiver<ExportStatusSnapshot>) {
        let bridge = self.clone();
        tokio::spawn(async move {
            loop {
                let snapshot = rx.borrow_and_update().clone();
                let _ = bridge.tx.send(snapshot);
                if rx.changed().await.is_err() {
                    tracing::warn!("board-export status channel closed; tray exports line frozen");
                    break;
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Mock source (tests + the PENPOT_LOCAL_TRAY_DEMO menu-QA loop)
// ---------------------------------------------------------------------------

/// Local mock of the daemon's status API: owns the watch sender, implements
/// [`SyncControl`], and can play a scripted sequence of realistic transitions
/// so the tray can be exercised without a running stack.
pub struct MockStatusSource {
    tx: Arc<watch::Sender<SyncStatusSnapshot>>,
}

struct MockControl {
    tx: Arc<watch::Sender<SyncStatusSnapshot>>,
}

impl SyncControl for MockControl {
    fn pause(&self) {
        self.tx.send_modify(|s| s.paused = true);
    }
    fn resume(&self) {
        self.tx.send_modify(|s| s.paused = false);
    }
}

impl MockStatusSource {
    pub fn new(initial: SyncStatusSnapshot) -> Self {
        let (tx, _rx) = watch::channel(initial);
        MockStatusSource { tx: Arc::new(tx) }
    }

    /// The receiver half the tray consumes (same type the real daemon hands
    /// out).
    pub fn subscribe(&self) -> watch::Receiver<SyncStatusSnapshot> {
        self.tx.subscribe()
    }

    /// The control handle the tray's pause/resume toggle drives.
    pub fn control(&self) -> Arc<dyn SyncControl> {
        Arc::new(MockControl { tx: self.tx.clone() })
    }

    /// Mutate the current snapshot (test helper).
    pub fn update(&self, f: impl FnOnce(&mut SyncStatusSnapshot)) {
        self.tx.send_modify(f);
    }

    /// The scripted demo sequence, as a pure list of snapshots so tests can
    /// assert on it without timers: a file goes Pending → Exporting → Synced,
    /// another Importing → Synced, one hits a Conflict (with a plausible copy
    /// path), and one frame carries a per-file Error + daemon `last_error`.
    pub fn demo_frames() -> Vec<SyncStatusSnapshot> {
        const HOME: &str = "Client A/homepage.penpot";
        const BRAND: &str = "Client A/brand.penpot";
        const CAMPAIGN: &str = "Client B/campaign.penpot";
        let now = || Some(chrono::Utc::now().to_rfc3339());
        let base = |states: &[(&str, FileState)]| SyncStatusSnapshot {
            last_sync_at: now(),
            files: states
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            paused: false,
            last_error: None,
        };
        let all_synced = base(&[
            (HOME, FileState::Synced),
            (BRAND, FileState::Synced),
            (CAMPAIGN, FileState::Synced),
        ]);
        let conflict = FileState::Conflict {
            copy_path: "Client B/campaign.conflict-2026-07-13T12-00-00Z.penpot".into(),
        };
        let mut error_frame = base(&[
            (HOME, FileState::Synced),
            (
                BRAND,
                FileState::Error { message: "import-binfile failed: backend 502".into() },
            ),
            (CAMPAIGN, conflict.clone()),
        ]);
        error_frame.last_error = Some("import-binfile failed: backend 502".into());
        vec![
            all_synced.clone(),
            base(&[
                (HOME, FileState::Pending),
                (BRAND, FileState::Synced),
                (CAMPAIGN, FileState::Synced),
            ]),
            base(&[
                (HOME, FileState::Exporting),
                (BRAND, FileState::Synced),
                (CAMPAIGN, FileState::Synced),
            ]),
            all_synced.clone(),
            base(&[
                (HOME, FileState::Synced),
                (BRAND, FileState::Importing),
                (CAMPAIGN, FileState::Synced),
            ]),
            all_synced.clone(),
            base(&[
                (HOME, FileState::Synced),
                (BRAND, FileState::Synced),
                (CAMPAIGN, conflict.clone()),
            ]),
            error_frame,
            all_synced,
        ]
    }

    /// Play [`Self::demo_frames`] in a loop, one frame per `step`. While
    /// paused (via the control handle) the script holds its frame — only the
    /// `paused` flag itself is preserved across frames, mirroring how the
    /// real daemon keeps publishing state while paused.
    pub async fn play_demo(&self, step: Duration) {
        let frames = Self::demo_frames();
        let mut i = 0usize;
        loop {
            tokio::time::sleep(step).await;
            if self.tx.borrow().paused {
                continue;
            }
            let frame = frames[i % frames.len()].clone();
            i += 1;
            self.tx.send_modify(move |s| {
                let paused = s.paused;
                *s = frame;
                s.paused = paused;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_control_flips_paused_in_snapshot() {
        let mock = MockStatusSource::new(SyncStatusSnapshot::default());
        let rx = mock.subscribe();
        let control = mock.control();
        assert!(!rx.borrow().paused);
        control.pause();
        assert!(rx.borrow().paused);
        control.resume();
        assert!(!rx.borrow().paused);
    }

    #[test]
    fn demo_frames_cover_every_file_state_variant() {
        let frames = MockStatusSource::demo_frames();
        assert!(!frames.is_empty());
        let mut seen = [false; 6];
        for frame in &frames {
            for state in frame.files.values() {
                let idx = match state {
                    FileState::Synced => 0,
                    FileState::Pending => 1,
                    FileState::Importing => 2,
                    FileState::Exporting => 3,
                    FileState::Conflict { copy_path } => {
                        assert!(copy_path.contains(".conflict-"));
                        4
                    }
                    FileState::Error { message } => {
                        assert!(!message.is_empty());
                        5
                    }
                };
                seen[idx] = true;
            }
        }
        assert_eq!(seen, [true; 6], "demo script must exercise all states");
        assert!(frames.iter().any(|f| f.last_error.is_some()));
        assert!(frames.iter().all(|f| f.last_sync_at.is_some()));
    }

    #[tokio::test]
    async fn bridge_pause_before_attach_shows_in_the_tray_snapshot() {
        let bridge = DaemonStatusBridge::new();
        let rx = bridge.subscribe();
        let control = bridge.control();
        control.pause();
        assert!(rx.borrow().paused, "pre-attach pause must show in the tray");
        control.resume();
        assert!(!rx.borrow().paused);
    }

    /// Spawn a real (offline) daemon: the backend URL is unroutable so the
    /// engine just retries reconciliation in the background, but the status
    /// channel and control handle are the genuine articles.
    fn offline_daemon() -> sync_daemon::SyncDaemonHandle {
        let root = std::env::temp_dir().join(format!(
            "penpot-desktop-bridge-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let client = penpot_rpc::PenpotClient::new("http://127.0.0.1:9");
        sync_daemon::spawn(client, sync_daemon::SyncConfig::new(root, "team"))
    }

    #[tokio::test]
    async fn export_bridge_forwards_service_snapshots() {
        let bridge = ExportStatusBridge::new();
        let mut ui_rx = bridge.subscribe();
        assert_eq!(*ui_rx.borrow(), ExportStatusSnapshot::default());

        let (service_tx, service_rx) = watch::channel(ExportStatusSnapshot::default());
        bridge.attach(service_rx);
        service_tx.send_modify(|s| {
            s.files_up_to_date = 2;
            s.rendering = Some("proj/home.penpot".into());
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if ui_rx.borrow().files_up_to_date == 2 {
                    break;
                }
                ui_rx.changed().await.expect("bridge channel open");
            }
        })
        .await
        .expect("service snapshot must reach the tray channel");
        assert_eq!(
            ui_rx.borrow().rendering.as_deref(),
            Some("proj/home.penpot")
        );
    }

    #[tokio::test]
    async fn bridge_attach_forwards_daemon_snapshots_and_pause() {
        let daemon = offline_daemon();
        let bridge = DaemonStatusBridge::new();
        let mut rx = bridge.subscribe();
        let ui_control = bridge.control();

        // Pause BEFORE attach: must be replayed onto the daemon.
        ui_control.pause();
        assert!(rx.borrow().paused);

        bridge.attach(daemon.status(), daemon.control());
        assert!(
            daemon.control().is_paused(),
            "pre-attach pause must be replayed onto the real daemon"
        );

        // The daemon's own snapshot (paused=true) must flow to the tray side.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if rx.borrow().paused {
                    break;
                }
                rx.changed().await.expect("bridge channel open");
            }
        })
        .await
        .expect("daemon snapshot must reach the tray channel");

        // Post-attach resume goes straight to the daemon and flows back.
        ui_control.resume();
        assert!(!daemon.control().is_paused());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !rx.borrow().paused {
                    break;
                }
                rx.changed().await.expect("bridge channel open");
            }
        })
        .await
        .expect("resume must reach the tray channel");

        // Do NOT `daemon.stop().await` here: the offline engine sits inside
        // its RPC retry backoff (~90 s budget) and stop() politely waits for
        // it. Dropping the handle is fine — the test runtime tears the
        // spawned task down when the test ends.
        drop(daemon);
    }
}
