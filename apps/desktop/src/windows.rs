//! D3 window registry: tracks which windows are open, which file (if any)
//! each one shows, and which window is "key" (last focused / frontmost).
//!
//! D3 makes Penpot Local window-per-file: every open file gets its own Tauri
//! window, titled with the filename, instead of one window that navigates
//! in place. That single decision breaks six places in `main.rs` that used
//! to assume exactly one window labelled `"main"`: the single-instance
//! refocus callback, the window construction itself, the `on_navigation`
//! redirect, the N5 vault-switch renavigation, the post-boot navigation, and
//! the boot-failure title. Once a second window can exist, every one of
//! those would otherwise silently target the wrong window. Routing them all
//! through [`HOME_LABEL`] here — rather than leaving the literal scattered
//! across the file — makes "the home window's label" a single fact instead
//! of six copies that could drift.
//!
//! The Window menu (a later D3 task) also needs something to enumerate and
//! click to bring a specific file's window forward — this registry is that
//! list. It is deliberately free of Tauri types (`AppHandle`, `WebviewWindow`,
//! …) so it stays unit-testable without a Tauri runtime, mirroring
//! `tray::model`'s split: the pure model lives here, and Tauri wiring
//! (subscribing to window-created/closed/focused events, actually building
//! windows) is a dumb translation layer on top, added by the task that grows
//! this into a live window-per-file app. `Arc<Mutex<…>>` makes a `clone()`
//! cheap and `Send + Sync`, so the same registry can be captured into Tauri
//! callbacks (which run on various threads) and read from the menu builder.
//!
//! The N4 palette overlay (`overlay.rs`, label `"palette"`) is a second
//! window that already exists today, proving multi-window construction works
//! in this app — but it is a utility popup, not a file window, and does not
//! register here: `list()` backs the Window menu's "which files are open"
//! surface, and the palette showing up in it (with no file, an odd title,
//! and a lifecycle driven by a global shortcut instead of File > Open) would
//! be noise, not signal.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The home window's Tauri label. Unchanged from the pre-D3 single-window
/// app — this task only routes existing lookups through the constant, it
/// does not rename the window, so app behaviour is byte-identical.
pub const HOME_LABEL: &str = "main";

/// Deterministic per-file window label. Same `file_id` always yields the
/// same label, so [`WindowRegistry::label_for_file`] can find an
/// already-open file's window instead of a caller opening a second one.
pub fn file_window_label(file_id: &str) -> String {
    format!("file-{file_id}")
}

/// A single open window: its Tauri label, the file it shows (`None` for the
/// home window), and its title (the Window menu's display string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWindow {
    pub label: String,
    pub file_id: Option<String>,
    pub title: String,
}

/// The mutable state behind the `Mutex`, kept private so every access goes
/// through the registry's methods (in particular: `remove` clearing a
/// dangling `key`, which callers must not be able to bypass).
struct Inner {
    /// Keyed by label; a `BTreeMap` gives cheap "does this label already
    /// exist" replacement semantics for `insert` for free.
    windows: BTreeMap<String, OpenWindow>,
    /// The label of the key (frontmost/last-focused) window, if any.
    key: Option<String>,
}

/// Registry of open windows. Cheap to `clone()` (an `Arc` bump) and safe to
/// share across threads — see the module doc for why.
#[derive(Clone)]
pub struct WindowRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl WindowRegistry {
    pub fn new() -> Self {
        WindowRegistry {
            inner: Arc::new(Mutex::new(Inner {
                windows: BTreeMap::new(),
                key: None,
            })),
        }
    }

    /// Record a window as open. Inserting a label that is already present
    /// replaces its entry rather than duplicating it — callers re-announcing
    /// a window (e.g. after a title change) must not fork the registry's
    /// idea of what's open.
    pub fn insert(&self, w: OpenWindow) {
        let mut inner = lock_recovering(&self.inner);
        inner.windows.insert(w.label.clone(), w);
    }

    /// Forget a closed window. Also clears `key` if it pointed at this
    /// label — a stale key aimed at a closed window would misdirect every
    /// one of the six call sites this registry exists to fix.
    pub fn remove(&self, label: &str) {
        let mut inner = lock_recovering(&self.inner);
        inner.windows.remove(label);
        if inner.key.as_deref() == Some(label) {
            inner.key = None;
        }
    }

    /// All open windows in Window-menu order: the home window first, then
    /// files alphabetically by title.
    pub fn list(&self) -> Vec<OpenWindow> {
        let inner = lock_recovering(&self.inner);
        let mut out: Vec<OpenWindow> = inner.windows.values().cloned().collect();
        out.sort_by(|a, b| {
            let a_is_home = a.label == HOME_LABEL;
            let b_is_home = b.label == HOME_LABEL;
            match (a_is_home, b_is_home) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.title.cmp(&b.title),
            }
        });
        out
    }

    /// The label of the window already showing `file_id`, if any — so a
    /// second "open this file" request focuses the existing window instead
    /// of opening a duplicate.
    pub fn label_for_file(&self, file_id: &str) -> Option<String> {
        let inner = lock_recovering(&self.inner);
        inner
            .windows
            .values()
            .find(|w| w.file_id.as_deref() == Some(file_id))
            .map(|w| w.label.clone())
    }

    /// Mark `label` as the key (frontmost/last-focused) window.
    pub fn set_key(&self, label: &str) {
        let mut inner = lock_recovering(&self.inner);
        inner.key = Some(label.to_string());
    }

    /// The key window's record, if it is still open.
    pub fn key(&self) -> Option<OpenWindow> {
        let inner = lock_recovering(&self.inner);
        inner
            .key
            .as_ref()
            .and_then(|label| inner.windows.get(label).cloned())
    }
}

/// Whether opening a file should focus an existing window or create one.
/// Split out from the Tauri call (`open_file_window` in `main.rs`) so the
/// decision — the behaviour that matters most for D3 — is unit-testable
/// without a Tauri runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reuse {
    /// A window for this file is already open; focus this label instead of
    /// creating a duplicate.
    Focus(String),
    /// No window for this file exists yet; the caller should build one at
    /// this label.
    Create(String),
}

/// Decide how to handle "open this file": reuse the window already showing
/// it, or create a new one at its deterministic label.
pub fn reuse_or_create(file_id: &str, reg: &WindowRegistry) -> Reuse {
    match reg.label_for_file(file_id) {
        Some(label) => Reuse::Focus(label),
        None => Reuse::Create(file_window_label(file_id)),
    }
}

/// Lock `inner`, recovering from poisoning instead of propagating the panic.
///
/// The registry is about to sit behind the menu builder and every window
/// lookup; a poisoned `Mutex` would make `.expect()` panic on every later
/// call, wedging the menu bar permanently for the rest of the app's life —
/// far worse than the panic that poisoned it in the first place. Recovery is
/// safe here because the registry holds no cross-field invariant a panic
/// mid-update could leave broken: it's a plain map of open windows plus one
/// optional key, and `BTreeMap`/`Option` mutations that panic (e.g. an OOM in
/// `insert`) leave the map in *some* valid state, just possibly missing the
/// one update that was in flight — recovering and carrying on is strictly
/// better than every subsequent call panicking too.
fn lock_recovering(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Default for WindowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_window_labels_are_deterministic_and_distinct() {
        assert_eq!(file_window_label("abc"), file_window_label("abc"));
        assert_ne!(file_window_label("abc"), file_window_label("abd"));
        assert_ne!(file_window_label("abc"), HOME_LABEL);
    }

    #[test]
    fn registry_lists_home_first_then_files_by_title() {
        let r = WindowRegistry::new();
        r.insert(OpenWindow { label: file_window_label("f2"), file_id: Some("f2".into()), title: "Zebra".into() });
        r.insert(OpenWindow { label: HOME_LABEL.into(), file_id: None, title: "Penpot Local".into() });
        r.insert(OpenWindow { label: file_window_label("f1"), file_id: Some("f1".into()), title: "Alpha".into() });
        let titles: Vec<String> = r.list().into_iter().map(|w| w.title).collect();
        assert_eq!(titles, vec!["Penpot Local", "Alpha", "Zebra"]);
    }

    #[test]
    fn a_file_already_open_is_found_by_id_so_it_is_not_opened_twice() {
        let r = WindowRegistry::new();
        let label = file_window_label("f1");
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "Alpha".into() });
        assert_eq!(r.label_for_file("f1"), Some(label));
        assert_eq!(r.label_for_file("nope"), None);
    }

    #[test]
    fn removing_a_window_drops_it_and_clears_key_if_it_was_key() {
        let r = WindowRegistry::new();
        let label = file_window_label("f1");
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "Alpha".into() });
        r.set_key(&label);
        assert_eq!(r.key().map(|w| w.label), Some(label.clone()));
        r.remove(&label);
        assert!(r.list().is_empty());
        assert!(r.key().is_none(), "key must not dangle at a closed window");
    }

    #[test]
    fn registry_survives_a_panic_while_the_lock_was_held() {
        let r = WindowRegistry::new();
        r.insert(OpenWindow { label: HOME_LABEL.into(), file_id: None, title: "Penpot Local".into() });

        // Poison the mutex: panic on a thread while a clone of `r` holds the
        // lock inside `insert`. `Mutex` is not re-entrant so the only way to
        // panic mid-lock from a single thread is via a closure invoked while
        // the guard is alive — `catch_unwind` around that closure emulates
        // exactly the "a callback panicked while the registry was locked"
        // scenario this fix targets.
        let r2 = r.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock_recovering(&r2.inner);
            panic!("simulated panic while the window registry lock is held");
        }));
        assert!(result.is_err(), "the closure should have panicked");

        // The registry must still work afterwards: a poisoned-but-recovered
        // lock, not a permanently wedged one.
        r.insert(OpenWindow {
            label: file_window_label("f1"),
            file_id: Some("f1".into()),
            title: "Alpha".into(),
        });
        let titles: Vec<String> = r.list().into_iter().map(|w| w.title).collect();
        assert_eq!(titles, vec!["Penpot Local", "Alpha"]);
    }

    #[test]
    fn inserting_the_same_label_twice_replaces_rather_than_duplicates() {
        let r = WindowRegistry::new();
        let label = file_window_label("f1");
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "Old".into() });
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "New".into() });
        assert_eq!(r.list().len(), 1);
        assert_eq!(r.list()[0].title, "New");
    }

    #[test]
    fn opening_an_already_open_file_reuses_its_window() {
        // The decision the GUI path depends on, extracted so it is testable
        // without a Tauri runtime.
        let r = WindowRegistry::new();
        let label = file_window_label("f1");
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "Alpha".into() });
        assert_eq!(reuse_or_create("f1", &r), Reuse::Focus(label));
        assert_eq!(reuse_or_create("f2", &r), Reuse::Create(file_window_label("f2")));
    }
}
