//! Direction B front end: filesystem watcher (`notify`) + per-file-dir
//! debounce.
//!
//! Raw events are mapped to the owning `<project>/<name>.penpot` directory
//! ([`map_event_path`]) and coalesced per file dir with a quiescence-based
//! debounce ([`FsDebounce`]): every event re-arms the timer, so an event
//! storm (editor save dances, `git checkout` replacing hundreds of files)
//! fires exactly once, after the storm has settled.
//!
//! Ignored outright (returning `None` from the mapping):
//! - anything under a dot-component (`.git/`, the `.penpot-sync.json`
//!   manifest and its `.penpot-sync.json.tmp-*` siblings),
//! - our own swap machinery (`*.penpot.tmp-*` staging and `*.penpot.old-*`
//!   backup dirs),
//! - conflict copies (`*.conflict-<ts>.penpot`) — never watched, never
//!   synced, never auto-deleted.
//!
//! Loop prevention lives one level up: Direction A records the new
//! `lastSyncedHash` in the manifest BEFORE its dir swap lands, so when the
//! swap's events fire here and the debounce elapses, the engine finds the
//! tree's semantic hash already in the ledger and skips silently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::paths;

/// Map a raw watcher event path to the sync-root-relative path of the
/// `.penpot` file dir that owns it. `None` = not ours / deliberately ignored
/// (see module docs). Pure — exhaustively unit-tested.
pub(crate) fn map_event_path(sync_root: &Path, event_path: &Path) -> Option<String> {
    let rel = event_path.strip_prefix(sync_root).ok()?;
    let mut acc = String::new();
    for comp in rel.components() {
        let name = comp.as_os_str().to_string_lossy();
        if name.starts_with('.') {
            return None; // dot dirs, the manifest, manifest tmp files
        }
        if name.contains(".penpot.tmp-") || name.contains(".penpot.old-") {
            return None; // our own swap staging/backup dirs
        }
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(&name);
        if name.ends_with(paths::PENPOT_DIR_SUFFIX) {
            if paths::is_conflict_dir_name(&name) {
                return None; // conflict copies are never watched
            }
            return Some(acc);
        }
    }
    None // event outside any .penpot dir (project dirs, stray files, root)
}

/// Is this a *structural* event: a path inside the root that is NOT inside
/// (or itself) any `.penpot` dir, the manifest, our swap machinery or a dot
/// dir? Project-folder renames/moves surface ONLY as such events on macOS
/// (FSEvents fires for the renamed directory itself, never for its
/// children), so the engine reacts to them with a debounced re-key sweep
/// (M5). Pure — unit-tested below.
pub(crate) fn is_structural_event(sync_root: &Path, event_path: &Path) -> bool {
    let Ok(rel) = event_path.strip_prefix(sync_root) else {
        return false;
    };
    let mut any = false;
    for comp in rel.components() {
        let name = comp.as_os_str().to_string_lossy();
        if name.starts_with('.')
            || name.contains(".penpot.tmp-")
            || name.contains(".penpot.old-")
            || name.ends_with(paths::PENPOT_DIR_SUFFIX)
        {
            return false; // ignored or owned by map_event_path
        }
        any = true;
    }
    any // the sync root itself is not structural
}

/// Start a recursive watcher on `root`, forwarding every event path into
/// `tx` (filtering/mapping happens on the async side, keeping the watcher
/// callback trivial). The returned watcher must be kept alive.
pub(crate) fn start(
    root: &Path,
    tx: UnboundedSender<PathBuf>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for path in event.paths {
                    let _ = tx.send(path); // receiver gone = daemon stopping
                }
            }
            Err(e) => tracing::warn!(error = %e, "fs watcher error"),
        })?;
    watcher.watch(root, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Per-file-dir debounce (quiescence-based: every event re-arms the timer).
/// Uses `tokio::time::Instant` so tests can drive it with
/// `tokio::time::pause`/`advance`.
#[derive(Debug, Default)]
pub(crate) struct FsDebounce {
    pending: HashMap<String, Instant>,
}

impl FsDebounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re)arm the timer for a file dir: it fires `debounce` after the LAST
    /// observed event.
    pub fn arm(&mut self, rel: String, now: Instant, debounce: Duration) {
        self.pending.insert(rel, now + debounce);
    }

    /// Drain every entry whose deadline has passed, sorted for deterministic
    /// processing.
    pub fn take_due(&mut self, now: Instant) -> Vec<String> {
        let mut due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(rel, _)| rel.clone())
            .collect();
        due.sort();
        for rel in &due {
            self.pending.remove(rel);
        }
        due
    }

    /// Drop everything (used on pause; resume rescans the whole root).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    const DEBOUNCE: Duration = Duration::from_secs(2);

    fn map(root: &str, path: &str) -> Option<String> {
        map_event_path(Path::new(root), Path::new(path))
    }

    #[test]
    fn events_inside_a_file_dir_map_to_it() {
        let r = "/designs";
        assert_eq!(
            map(r, "/designs/Client A/home.penpot/files/x.json"),
            Some("Client A/home.penpot".to_string())
        );
        assert_eq!(
            map(r, "/designs/Client A/home.penpot"),
            Some("Client A/home.penpot".to_string())
        );
        // Deeply nested payload path.
        assert_eq!(
            map(
                r,
                "/designs/a/b/deep.penpot/files/f/pages/p/shape.json"
            ),
            Some("a/b/deep.penpot".to_string())
        );
        // Root-level file dir.
        assert_eq!(
            map(r, "/designs/root.penpot/manifest.json"),
            Some("root.penpot".to_string())
        );
    }

    #[test]
    fn non_penpot_paths_are_ignored() {
        let r = "/designs";
        assert_eq!(map(r, "/designs"), None);
        assert_eq!(map(r, "/designs/Client A"), None);
        assert_eq!(map(r, "/designs/Client A/readme.txt"), None);
        assert_eq!(map(r, "/elsewhere/x.penpot/f.json"), None); // outside root
    }

    #[test]
    fn manifest_and_dot_paths_are_ignored() {
        let r = "/designs";
        assert_eq!(map(r, "/designs/.penpot-sync.json"), None);
        assert_eq!(
            map(r, "/designs/.penpot-sync.json.tmp-0123456789ab"),
            None
        );
        assert_eq!(map(r, "/designs/.git/objects/ab/cdef"), None);
        assert_eq!(map(r, "/designs/.hidden/x.penpot/f.json"), None);
    }

    #[test]
    fn swap_staging_and_backup_dirs_are_ignored() {
        let r = "/designs";
        assert_eq!(
            map(r, "/designs/Client/home.penpot.tmp-0123456789ab/files/x.json"),
            None
        );
        assert_eq!(
            map(r, "/designs/Client/home.penpot.old-0123456789ab"),
            None
        );
        // The staging dir of a conflict copy (crash window) is also ignored.
        assert_eq!(
            map(
                r,
                "/designs/C/h.conflict-2026-07-13T09-04-42Z.penpot.tmp-0123456789ab/x.json"
            ),
            None
        );
    }

    #[test]
    fn conflict_copies_are_never_watched() {
        let r = "/designs";
        assert_eq!(
            map(
                r,
                "/designs/Client/home.conflict-2026-07-13T09-04-42Z.penpot"
            ),
            None
        );
        assert_eq!(
            map(
                r,
                "/designs/Client/home.conflict-2026-07-13T09-04-42Z.penpot/files/x.json"
            ),
            None
        );
    }

    #[test]
    fn structural_events_are_non_penpot_paths_inside_the_root() {
        let r = Path::new("/designs");
        let s = |p: &str| is_structural_event(r, Path::new(p));
        // Project folder renamed/moved: THE case this exists for.
        assert!(s("/designs/Client B"));
        assert!(s("/designs/Client B/nested"));
        // Stray files in project folders are structural too (harmless: the
        // sweep is a no-op when nothing vanished).
        assert!(s("/designs/Client A/readme.txt"));
        // Not structural: the root itself, anything outside it…
        assert!(!s("/designs"));
        assert!(!s("/elsewhere/Client B"));
        // …anything already owned by map_event_path or ignored by it.
        assert!(!s("/designs/Client A/home.penpot"));
        assert!(!s("/designs/Client A/home.penpot/files/x.json"));
        assert!(!s("/designs/.penpot-sync.json"));
        assert!(!s("/designs/.git/objects/ab"));
        assert!(!s("/designs/Client/home.penpot.tmp-0123/x.json"));
        assert!(!s("/designs/Client/home.penpot.old-0123"));
        assert!(!s("/designs/Client/home.conflict-2026-07-13T09-04-42Z.penpot"));
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_fires_once_after_quiescence() {
        let mut d = FsDebounce::new();
        d.arm("a/x.penpot".into(), Instant::now(), DEBOUNCE);
        advance(Duration::from_millis(1999)).await;
        assert!(d.take_due(Instant::now()).is_empty());
        advance(Duration::from_millis(2)).await;
        assert_eq!(d.take_due(Instant::now()), vec!["a/x.penpot".to_string()]);
        // Drained: not due twice.
        assert!(d.take_due(Instant::now()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn event_storm_coalesces_and_resets_the_timer() {
        let mut d = FsDebounce::new();
        // 10 events, 500 ms apart — a git checkout storm.
        for _ in 0..10 {
            d.arm("a/x.penpot".into(), Instant::now(), DEBOUNCE);
            advance(Duration::from_millis(500)).await;
            assert!(
                d.take_due(Instant::now()).is_empty(),
                "must not fire mid-storm"
            );
        }
        // 2 s after the LAST event it fires exactly once.
        advance(Duration::from_millis(1501)).await;
        assert_eq!(d.take_due(Instant::now()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn independent_dirs_debounce_independently_and_fire_sorted() {
        let mut d = FsDebounce::new();
        d.arm("b/y.penpot".into(), Instant::now(), DEBOUNCE);
        d.arm("a/x.penpot".into(), Instant::now(), DEBOUNCE);
        advance(Duration::from_secs(1)).await;
        d.arm("c/z.penpot".into(), Instant::now(), DEBOUNCE);
        advance(Duration::from_millis(1001)).await;
        // a and b are due (armed 2.001 s ago); c is not (1.001 s ago).
        assert_eq!(
            d.take_due(Instant::now()),
            vec!["a/x.penpot".to_string(), "b/y.penpot".to_string()]
        );
        advance(Duration::from_secs(1)).await;
        assert_eq!(d.take_due(Instant::now()), vec!["c/z.penpot".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn clear_drops_pending() {
        let mut d = FsDebounce::new();
        d.arm("a/x.penpot".into(), Instant::now(), DEBOUNCE);
        d.clear();
        advance(Duration::from_secs(10)).await;
        assert!(d.take_due(Instant::now()).is_empty());
    }
}
