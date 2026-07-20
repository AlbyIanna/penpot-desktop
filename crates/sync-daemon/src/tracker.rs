//! Change detection + per-file debounce over the DB poll surface.
//!
//! A file *changed* iff its [`SyncKey`] (revn, modifiedAt, projectId, name)
//! differs from the last synced state. `projectId` and `name` are in the key
//! alongside the content-version fields because they determine the file's
//! on-disk PATH (its parent directory and its dir name) — `move-files` and
//! `rename-file` change one of them WITHOUT bumping `revn` or `modifiedAt`
//! (only the content-version fields), so without this a move/rename would be
//! silently invisible to the tracker and the directory-relocation logic in
//! `Engine::export_file` would never run. A detected change arms a per-file
//! debounce timer; a *further* change (a different [`SyncKey`]) resets the
//! timer; seeing the same key again lets the timer keep running. When the key
//! returns to the synced state before the timer fires, the pending entry is
//! cancelled.
//!
//! Files that vanish from a (successful!) DB listing are reported to the
//! caller and forgotten here, but nothing on disk is ever touched — the disk
//! is the source of truth, deletion in the DB is not deletion of user data.
//! (Failed listings never reach this type; the poll cycle is skipped.)

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::DbFileState;

#[derive(Debug)]
struct Pending {
    state: DbFileState,
    deadline: Instant,
}

/// The subset of [`DbFileState`] that change detection keys on. `revn` and
/// `modified_at` track content changes; `project_id` and `name` are included
/// too because, together, they determine where the file lives on disk — a
/// `move-files` or `rename-file` call changes one of THOSE without touching
/// `revn`/`modified_at`, and a move/rename that isn't detected here never
/// reaches the directory-relocation logic in `Engine::export_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncKey {
    revn: i64,
    modified_at: String,
    project_id: String,
    name: String,
}

impl From<&DbFileState> for SyncKey {
    fn from(st: &DbFileState) -> Self {
        SyncKey {
            revn: st.revn,
            modified_at: st.modified_at.clone(),
            project_id: st.project_id.clone(),
            name: st.name.clone(),
        }
    }
}

/// See module docs. Uses `tokio::time::Instant` so tests can drive it with
/// `tokio::time::pause`/`advance`.
#[derive(Debug, Default)]
pub(crate) struct ChangeTracker {
    /// fileId → [`SyncKey`] at last completed sync.
    last_synced: HashMap<String, SyncKey>,
    pending: HashMap<String, Pending>,
}

impl ChangeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest one successful poll snapshot. Returns the ids of files that
    /// vanished from the DB listing (they are forgotten here; the caller only
    /// logs — never deletes).
    pub fn observe(
        &mut self,
        now: Instant,
        debounce: Duration,
        files: &HashMap<String, DbFileState>,
    ) -> Vec<String> {
        let mut vanished: Vec<String> = self
            .last_synced
            .keys()
            .chain(self.pending.keys())
            .filter(|id| !files.contains_key(*id))
            .cloned()
            .collect();
        vanished.sort();
        vanished.dedup();
        for id in &vanished {
            self.last_synced.remove(id);
            self.pending.remove(id);
        }

        for (id, st) in files {
            let key = SyncKey::from(st);
            let unchanged = self.last_synced.get(id).is_some_and(|k| *k == key);
            if unchanged {
                // Back to (or still at) the synced state: cancel any timer.
                self.pending.remove(id);
                continue;
            }
            match self.pending.get_mut(id) {
                // Same observed change as before: timer keeps running
                // (refresh cosmetic fields like names).
                Some(p) if SyncKey::from(&p.state) == key => {
                    p.state = st.clone();
                }
                // New change (or a further change): (re)arm the timer.
                _ => {
                    self.pending.insert(
                        id.clone(),
                        Pending {
                            state: st.clone(),
                            deadline: now + debounce,
                        },
                    );
                }
            }
        }
        vanished
    }

    /// Drain every pending entry whose debounce deadline has passed, sorted
    /// by fileId for deterministic processing.
    pub fn take_due(&mut self, now: Instant) -> Vec<DbFileState> {
        let mut due_ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        due_ids.sort();
        due_ids
            .into_iter()
            .map(|id| self.pending.remove(&id).expect("id just listed").state)
            .collect()
    }

    /// Record a completed sync (export/import landed, or verified no-op):
    /// the given state becomes the new baseline.
    pub fn mark_synced(&mut self, state: &DbFileState) {
        self.pending.remove(&state.id);
        self.last_synced
            .insert(state.id.clone(), SyncKey::from(state));
    }

    /// Re-queue a drained entry after a failed export so it is retried.
    pub fn reschedule(&mut self, state: DbFileState, deadline: Instant) {
        self.pending.insert(
            state.id.clone(),
            Pending { state, deadline },
        );
    }

    /// Ids currently waiting out their debounce (sorted). Used to surface
    /// `Pending` states in the status snapshot.
    pub fn pending_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.pending.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    const DEBOUNCE: Duration = Duration::from_secs(3);

    fn state(id: &str, revn: i64, modified_at: &str) -> DbFileState {
        DbFileState {
            id: id.to_string(),
            name: format!("{id}-name"),
            project_id: "p1".to_string(),
            project_name: "Project".to_string(),
            revn,
            modified_at: modified_at.to_string(),
        }
    }

    fn snap(states: &[DbFileState]) -> HashMap<String, DbFileState> {
        states.iter().map(|s| (s.id.clone(), s.clone())).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_file_counts_as_changed_and_fires_after_debounce() {
        let mut t = ChangeTracker::new();
        let s = state("f1", 0, "t0");
        t.observe(Instant::now(), DEBOUNCE, &snap(std::slice::from_ref(&s)));
        // Not due before the debounce elapses.
        assert!(t.take_due(Instant::now()).is_empty());
        advance(Duration::from_millis(2999)).await;
        assert!(t.take_due(Instant::now()).is_empty());
        advance(Duration::from_millis(2)).await;
        let due = t.take_due(Instant::now());
        assert_eq!(due, vec![s]);
        // Drained: not due twice.
        assert!(t.take_due(Instant::now()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn revn_or_modified_at_change_detected_synced_state_is_quiet() {
        let mut t = ChangeTracker::new();
        let s = state("f1", 1, "t1");
        t.mark_synced(&s);
        // Same pair → no pending.
        t.observe(Instant::now(), DEBOUNCE, &snap(std::slice::from_ref(&s)));
        assert!(t.pending_ids().is_empty());
        // revn change alone.
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 2, "t1")]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
        t.mark_synced(&state("f1", 2, "t1"));
        // modifiedAt change alone.
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 2, "t2")]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
        // revn moved BACKWARD (in-place import) — still a change.
        t.mark_synced(&state("f1", 2, "t2"));
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t3")]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn project_id_change_alone_is_detected_move_files_does_not_bump_revn() {
        let mut t = ChangeTracker::new();
        let s = state("f1", 1, "t1");
        t.mark_synced(&s);
        // Same revn/modifiedAt, only project_id differs (a `move-files` call).
        let mut moved = s.clone();
        moved.project_id = "p2".to_string();
        t.observe(Instant::now(), DEBOUNCE, &snap(&[moved.clone()]));
        assert_eq!(
            t.pending_ids(),
            vec!["f1".to_string()],
            "a project move must not be invisible just because revn/modifiedAt didn't bump"
        );
        advance(Duration::from_millis(3001)).await;
        let due = t.take_due(Instant::now());
        assert_eq!(due, vec![moved]);
    }

    #[tokio::test(start_paused = true)]
    async fn name_change_alone_is_detected_rename_does_not_bump_revn() {
        let mut t = ChangeTracker::new();
        let s = state("f1", 1, "t1");
        t.mark_synced(&s);
        // Same revn/modifiedAt, only name differs.
        let mut renamed = s.clone();
        renamed.name = "renamed".to_string();
        t.observe(Instant::now(), DEBOUNCE, &snap(&[renamed]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_changed_stays_quiet_across_consecutive_polls_after_sync() {
        // Regression guard: once a sync is recorded, the SAME state observed
        // again (and again) must never be treated as pending — otherwise the
        // daemon would re-export every file on every poll.
        let mut t = ChangeTracker::new();
        let s = state("f1", 1, "t1");
        t.observe(Instant::now(), DEBOUNCE, &snap(std::slice::from_ref(&s)));
        advance(Duration::from_millis(3001)).await;
        let due = t.take_due(Instant::now());
        assert_eq!(due, vec![s.clone()]);
        t.mark_synced(&due[0]);
        assert!(t.pending_ids().is_empty());

        // First poll after the sync: still quiet.
        t.observe(Instant::now(), DEBOUNCE, &snap(std::slice::from_ref(&s)));
        assert!(t.pending_ids().is_empty());
        // Second consecutive poll: still quiet (the no-churn regression).
        t.observe(Instant::now(), DEBOUNCE, &snap(std::slice::from_ref(&s)));
        assert!(t.pending_ids().is_empty());
        advance(Duration::from_secs(10)).await;
        assert!(t.take_due(Instant::now()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn further_change_resets_the_timer_same_change_does_not() {
        let mut t = ChangeTracker::new();
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t1")]));
        advance(Duration::from_secs(2)).await;
        // Same (revn, modifiedAt) re-observed: timer keeps running…
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t1")]));
        advance(Duration::from_millis(1001)).await; // 3.001s since arming
        assert_eq!(t.take_due(Instant::now()).len(), 1);

        // …but a FURTHER change resets it.
        t.mark_synced(&state("f1", 1, "t1"));
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 2, "t2")]));
        advance(Duration::from_secs(2)).await;
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 3, "t3")]));
        advance(Duration::from_millis(1001)).await; // 3.001s after FIRST change
        assert!(t.take_due(Instant::now()).is_empty(), "timer was reset");
        advance(Duration::from_secs(2)).await; // 3.001s after second change
        let due = t.take_due(Instant::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].revn, 3, "fires with the LATEST observed state");
    }

    #[tokio::test(start_paused = true)]
    async fn change_that_reverts_to_synced_state_is_cancelled() {
        let mut t = ChangeTracker::new();
        t.mark_synced(&state("f1", 1, "t1"));
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 2, "t2")]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
        // Back to the synced pair (e.g. our own in-place import echo).
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t1")]));
        assert!(t.pending_ids().is_empty());
        advance(Duration::from_secs(10)).await;
        assert!(t.take_due(Instant::now()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn vanished_files_are_reported_and_forgotten_never_exported() {
        let mut t = ChangeTracker::new();
        t.mark_synced(&state("f1", 1, "t1"));
        t.observe(
            Instant::now(),
            DEBOUNCE,
            &snap(&[state("f1", 1, "t1"), state("f2", 0, "t0")]),
        );
        // Next successful listing has neither file: both vanish (one from
        // last_synced, one from pending).
        let vanished = t.observe(Instant::now(), DEBOUNCE, &snap(&[]));
        assert_eq!(vanished, vec!["f1".to_string(), "f2".to_string()]);
        advance(Duration::from_secs(10)).await;
        assert!(t.take_due(Instant::now()).is_empty());
        // Reappearing counts as a fresh change.
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t1")]));
        assert_eq!(t.pending_ids(), vec!["f1".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn reschedule_retries_after_failure() {
        let mut t = ChangeTracker::new();
        t.observe(Instant::now(), DEBOUNCE, &snap(&[state("f1", 1, "t1")]));
        advance(Duration::from_millis(3001)).await;
        let due = t.take_due(Instant::now());
        assert_eq!(due.len(), 1);
        // Export failed → put it back with a fresh deadline.
        t.reschedule(due[0].clone(), Instant::now() + DEBOUNCE);
        assert!(t.take_due(Instant::now()).is_empty());
        advance(Duration::from_millis(3001)).await;
        assert_eq!(t.take_due(Instant::now()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_due_files_are_sorted_by_id() {
        let mut t = ChangeTracker::new();
        t.observe(
            Instant::now(),
            DEBOUNCE,
            &snap(&[state("b", 1, "t"), state("a", 1, "t"), state("c", 1, "t")]),
        );
        advance(Duration::from_millis(3001)).await;
        let ids: Vec<String> = t.take_due(Instant::now()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
