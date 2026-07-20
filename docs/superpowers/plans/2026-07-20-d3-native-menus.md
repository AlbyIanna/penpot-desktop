# D3 — Native Menu Bar, Shortcuts, Open Recent: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A real native menu bar wired to real commands, with each file opening in its own window.

**Architecture:** Follows the `tray/model.rs` precedent exactly: a **pure, toolkit-agnostic menu model** (plain Rust structs, zero Tauri types, fully unit-tested) plus a **dumb translation layer** that turns it into `tauri::menu::*`. Menus cannot be clicked in CI, so all branching lives in the model and the gate asserts the model's shape, that every command it names really exists, and that the translation layer has no orphaned or dead items. Window-per-file requires replacing the six hardcoded `"main"` window lookups with a window registry, which is also what the Window menu enumerates.

**Tech Stack:** Tauri 2.11.5 (`tauri::menu::{Menu, MenuItem, Submenu, PredefinedMenuItem}`, `AppHandle::set_menu`), Rust, macOS `osascript` for native pickers, bash + python3 for the gate.

## Global Constraints

- **Core invariant (P0):** delete the entire database, restart, and every project/file rebuilds from the folder tree with no data loss. The folder tree is the source of truth; the DB is a disposable cache.
- **Invariant 3:** the SPA stays byte-untouched — no serve-time patching of upstream JS/CSS, no injected scripts, nothing under `runtime/frontend/`. Only URLs reach the canvas.
- **The dashboard is not the front door:** never navigate to `/dashboard`, `/settings` or `/auth`.
- **No orphaned or dead menu items.** Every item in the model maps to a command that exists and works. This is a gate assertion, not a guideline.
- **Preferences (⌘,) is deliberately NOT in D3** — it does not exist until D4, and a dead item would violate the rule above. This is a recorded deviation from PLAN4's D3 accelerator list.
- **macOS reality:** the menu bar is app-wide (`Window::set_menu` is unsupported on macOS); it must be rebuilt via `AppHandle::set_menu` when the window set changes, exactly as the tray already rebuilds itself on every status change.
- **D3 dedicated ports:** proxy 9050, backend 6512, postgres 5585, valkey 6528.
- Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`; never a bare `#<number>` in commit or PR text.
- `just d3` chained into `just e2e`; green twice.

## File Structure

| File | Responsibility |
|---|---|
| `apps/desktop/src/windows.rs` | **New.** The window registry: which windows are open, which file each shows, which is key. Replaces every hardcoded `"main"` lookup. |
| `apps/desktop/src/recent.rs` | **New.** The "recently opened" store (Open Recent). Distinct from the index's `last_synced_at`, which is a sync fact, not a user action. |
| `apps/desktop/src/menubar/model.rs` | **New.** The pure menu model + `build_menu_model`. All branching lives here. Zero Tauri types. |
| `apps/desktop/src/menubar/mod.rs` | **New.** The dumb translation to `tauri::menu::*` + `AppHandle::set_menu` + event dispatch. |
| `apps/desktop/src/dialog.rs` | Modify: add `choose_file` and `save_file` beside the existing `choose_folder`. |
| `apps/desktop/src/main.rs` | Modify: use the registry instead of `"main"`; install and rebuild the menu. |
| `scripts/d3-menus.sh`, `scripts/d3_menus_helper.py` | **New.** The gate. |
| `justfile` | Modify: `d3` recipe + chain into `e2e`. |
| `docs/milestones/d3/README.md` + `img/` | **New.** The milestone doc. |

---

### Task 1: The window registry

**Files:**
- Create: `apps/desktop/src/windows.rs`
- Modify: `apps/desktop/src/main.rs`, `apps/desktop/src/lib.rs` (add `pub mod windows;`)

**Interfaces:**
- Produces:
  - `pub const HOME_LABEL: &str = "main";`
  - `pub fn file_window_label(file_id: &str) -> String` — deterministic, `file-<file_id>`
  - `pub struct OpenWindow { pub label: String, pub file_id: Option<String>, pub title: String }`
  - `pub struct WindowRegistry` with `pub fn new() -> Self`, `pub fn insert(&self, w: OpenWindow)`, `pub fn remove(&self, label: &str)`, `pub fn list(&self) -> Vec<OpenWindow>` (stable order: home first, then files by title), `pub fn label_for_file(&self, file_id: &str) -> Option<String>`, `pub fn set_key(&self, label: &str)`, `pub fn key(&self) -> Option<OpenWindow>`
- Internally `Arc<Mutex<…>>` so it can be cloned into Tauri callbacks and read from the menu builder.

**Why this task is first:** the Window menu has nothing to list without it, and six places currently assume a single window labelled `"main"` — `main.rs:62` (single-instance refocus), `:76` (construction), `:92` (the `on_navigation` redirect), `:196` (N5 vault switch), `:291` (post-boot navigation) and `:319` (boot-failure title). Every one must go through the registry. Leaving any of them hardcoded means that code path silently targets the wrong window once a second window exists.

- [ ] **Step 1: Write the failing tests**

```rust
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
    fn inserting_the_same_label_twice_replaces_rather_than_duplicates() {
        let r = WindowRegistry::new();
        let label = file_window_label("f1");
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "Old".into() });
        r.insert(OpenWindow { label: label.clone(), file_id: Some("f1".into()), title: "New".into() });
        assert_eq!(r.list().len(), 1);
        assert_eq!(r.list()[0].title, "New");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p penpot-desktop windows::`
Expected: FAIL — `cannot find function file_window_label`.

- [ ] **Step 3: Implement**

Write `apps/desktop/src/windows.rs` with a module doc explaining WHY it exists (window-per-file; the six former `"main"` assumptions; the Window menu needs an enumerable set). Use `Arc<Mutex<Inner>>` with a `BTreeMap<String, OpenWindow>` plus an `Option<String>` key label. `list()` sorts home first, then by title.

- [ ] **Step 4: Replace every hardcoded `"main"`**

Read `apps/desktop/src/main.rs` and route all six sites through `windows::HOME_LABEL` or the registry. Do NOT change behaviour yet — the home window is still labelled `HOME_LABEL`, so this step is a pure refactor and everything must still work exactly as before.

- [ ] **Step 5: Run tests + build**

Run: `cargo test -p penpot-desktop` and `cargo build -p penpot-desktop`. Expected: green, and no remaining bare `"main"` string in `main.rs` (`grep -n '"main"' apps/desktop/src/main.rs` should return nothing).

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src/windows.rs apps/desktop/src/main.rs apps/desktop/src/lib.rs
git commit -m "D3: a window registry, replacing six hardcoded \"main\" lookups

Window-per-file needs to know which windows exist, which file each shows and
which is key -- and the Window menu has nothing to list without it. Six places
assumed a single window labelled \"main\": the single-instance refocus, window
construction, the navigation redirect, the vault switch, the post-boot
navigation and the boot-failure title. Each would silently target the wrong
window once a second one exists.

Pure refactor: the home window keeps the same label, so behaviour is unchanged.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Open a file in its own window

**Files:**
- Modify: `apps/desktop/src/windows.rs`, `apps/desktop/src/main.rs`

**Interfaces:**
- Consumes: `WindowRegistry` (Task 1); `vault_index::workspace_deep_link(team_id, file_id, page_id) -> String` (`crates/vault-index/src/query.rs:32`).
- Produces: `pub fn open_file_window(app: &tauri::AppHandle, reg: &WindowRegistry, proxy_url: &str, team_id: &str, file_id: &str, page_id: Option<&str>, title: &str) -> tauri::Result<()>` — focuses the existing window if the file is already open, otherwise builds a new one.

**Behaviour that matters:** opening a file that is already open must **focus** its window, not create a second one for the same file. The window title is the file's name (that is the point of window-per-file). New windows must attach the same `on_navigation` policy the home window has — otherwise `#/dashboard` would be reachable from a file window, silently undoing D1/D2's closure.

- [ ] **Step 1: Write the failing test**

Add to `windows.rs` tests:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p penpot-desktop windows::` — Expected: FAIL, `cannot find function reuse_or_create`.

- [ ] **Step 3: Implement**

```rust
/// Whether opening a file should focus an existing window or create one.
/// Split out from the Tauri call so the decision is unit-testable.
#[derive(Debug, PartialEq, Eq)]
pub enum Reuse {
    Focus(String),
    Create(String),
}

pub fn reuse_or_create(file_id: &str, reg: &WindowRegistry) -> Reuse {
    match reg.label_for_file(file_id) {
        Some(label) => Reuse::Focus(label),
        None => Reuse::Create(file_window_label(file_id)),
    }
}
```

Then write `open_file_window` in `main.rs` (or `windows.rs` if it can avoid pulling Tauri into the pure module — prefer keeping `windows.rs` Tauri-free and putting the builder call in `main.rs`). It must:
- consult `reuse_or_create`
- on `Focus`: `show()`, `unminimize()`, `set_focus()`
- on `Create`: `WebviewWindowBuilder::new(app, label, WebviewUrl::External(deep_link))` with `.title(title)`, the same `.on_navigation(...)` policy closure as the home window, and register it in the registry on success
- register an `on_window_event` handler that removes the window from the registry when it is destroyed, and calls the menu rebuild from Task 6

- [ ] **Step 4: Run tests + build**, then **Step 5: Commit** with a message explaining the focus-don't-duplicate rule and that file windows carry the same navigation policy so the dashboard stays closed from every window.

---

### Task 3: The recently-opened store

**Files:**
- Create: `apps/desktop/src/recent.rs`
- Modify: `apps/desktop/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct RecentEntry { pub file_id: String, pub title: String, pub page_id: Option<String>, pub opened_at: String }`
  - `pub fn record_open(data_dir: &Path, entry: RecentEntry) -> anyhow::Result<()>`
  - `pub fn list_recent(data_dir: &Path, limit: usize) -> Vec<RecentEntry>`
  - `pub const RECENT_FILE_NAME: &str = "recent-files.json";`
  - `pub const RECENT_LIMIT: usize = 10;`

**Why a new store:** nothing today records "the user opened this". The index's `Sort::Recency` orders by `last_synced_at`, which is when the sync daemon last wrote the file — a different fact. Reusing it would make Open Recent list files the user never opened.

**Where it lives:** the app **data dir**, not the vault. It is per-machine UI state, not user work, and must not travel with a cloned vault — the same reasoning as E7's consent ledger.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, at: &str) -> RecentEntry {
        RecentEntry { file_id: id.into(), title: id.to_uppercase(), page_id: None, opened_at: at.into() }
    }

    #[test]
    fn most_recent_first() {
        let tmp = tempfile::tempdir().unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T10:00:00Z")).unwrap();
        record_open(tmp.path(), entry("b", "2026-07-20T11:00:00Z")).unwrap();
        let ids: Vec<String> = list_recent(tmp.path(), 10).into_iter().map(|e| e.file_id).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn reopening_moves_to_front_without_duplicating() {
        let tmp = tempfile::tempdir().unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T10:00:00Z")).unwrap();
        record_open(tmp.path(), entry("b", "2026-07-20T11:00:00Z")).unwrap();
        record_open(tmp.path(), entry("a", "2026-07-20T12:00:00Z")).unwrap();
        let ids: Vec<String> = list_recent(tmp.path(), 10).into_iter().map(|e| e.file_id).collect();
        assert_eq!(ids, vec!["a", "b"], "reopen must move to front, not duplicate");
    }

    #[test]
    fn the_list_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(RECENT_LIMIT + 5) {
            record_open(tmp.path(), entry(&format!("f{i}"), &format!("2026-07-20T10:{i:02}:00Z"))).unwrap();
        }
        assert_eq!(list_recent(tmp.path(), RECENT_LIMIT).len(), RECENT_LIMIT);
    }

    #[test]
    fn a_missing_or_corrupt_store_is_an_empty_list_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_recent(tmp.path(), 10).is_empty());
        std::fs::write(tmp.path().join(RECENT_FILE_NAME), b"{ this is not json").unwrap();
        assert!(list_recent(tmp.path(), 10).is_empty(), "corrupt store must degrade, not panic");
    }
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p penpot-desktop recent::`

- [ ] **Step 3: Implement.** JSON array, newest first, deduped by `file_id`, capped at `RECENT_LIMIT`, written atomically. `list_recent` never returns an error — a missing or corrupt file is an empty list, because a broken UI-state file must never block opening the app.

- [ ] **Step 4: Run tests. Step 5: Commit.**

---

### Task 4: Native file and save pickers

**Files:**
- Modify: `apps/desktop/src/dialog.rs`

**Interfaces:**
- Consumes: the existing `choose_folder` (`apps/desktop/src/dialog.rs:53-77`) — read it and mirror its structure, its macOS-only behaviour, and its `Option` return convention exactly.
- Produces:
  - `pub fn choose_file(prompt: &str, extensions: &[&str]) -> Option<PathBuf>`
  - `pub fn save_file(prompt: &str, default_name: &str) -> Option<PathBuf>`
  - Pure, testable command builders alongside them, following `reveal.rs`'s precedent: `pub fn choose_file_script(prompt: &str, extensions: &[&str]) -> String` and `pub fn save_file_script(prompt: &str, default_name: &str) -> String`.

**Why builders:** a dialog cannot be driven headlessly, so — exactly as `reveal.rs` does — all logic lives in pure functions that construct the `osascript` invocation, and only `Command::spawn` is untestable.

**Escaping is the risk here.** These strings are interpolated into AppleScript. A prompt or filename containing a double quote or backslash must not break out of the string literal. Escape them, and test it.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn choose_file_script_escapes_quotes_in_the_prompt() {
        let s = choose_file_script("say \"hi\"", &["penpot"]);
        assert!(!s.contains("say \"hi\""), "unescaped quote breaks out of the AppleScript literal");
        assert!(s.contains("\\\"hi\\\""), "expected escaped quotes in: {s}");
    }

    #[test]
    fn save_file_script_escapes_backslashes_and_quotes_in_the_name() {
        let s = save_file_script("Export", r#"we"ird\name"#);
        assert!(s.contains(r#"\""#));
        assert!(s.contains(r"\\"));
    }

    #[test]
    fn choose_file_script_mentions_every_extension() {
        let s = choose_file_script("Open", &["penpot", "zip"]);
        assert!(s.contains("penpot") && s.contains("zip"), "{s}");
    }
```

- [ ] **Step 2: Run to verify they fail. Step 3: Implement. Step 4: Run tests.**

On non-macOS, both functions return `None`, matching `choose_folder`. Document that limitation in the module doc rather than pretending it is cross-platform.

- [ ] **Step 5: Commit.**

---

### Task 5: The pure menu model

**Files:**
- Create: `apps/desktop/src/menubar/model.rs`
- Modify: `apps/desktop/src/lib.rs`

**Interfaces:**
- Consumes: `WindowRegistry::list()` (Task 1), `recent::list_recent` (Task 3).
- Produces:
  - `pub enum Command { NewFile, NewProject, OpenFile, OpenRecent(String), OpenVault, Import, Export, RevealInFinder, ShowHome, ShowSearch, ShowPalette, ShowPackages, ShowTemplates, FocusWindow(String), About, KnownLimits }`
  - `pub struct Item { pub id: String, pub label: String, pub accelerator: Option<String>, pub enabled: bool, pub command: Option<Command> }`
  - `pub enum Entry { Item(Item), Separator, Predefined(&'static str) }`
  - `pub struct MenuSection { pub title: String, pub entries: Vec<Entry> }`
  - `pub struct MenuModel { pub sections: Vec<MenuSection> }`
  - `pub fn build_menu_model(windows: &[OpenWindow], recent: &[RecentEntry], key_has_file: bool) -> MenuModel`
  - `pub fn command_for_id(model: &MenuModel, id: &str) -> Option<Command>`

**This module must not reference Tauri at all.** That is what makes the gate possible — read `apps/desktop/src/tray/model.rs` first and follow its structure and doc-comment style.

**The menu:**
- **File** — New File (`CmdOrCtrl+N`), New Project, separator, Open… (`CmdOrCtrl+O`), Open Recent ▸, Open Vault…, separator, Import…, Export…, separator, Reveal in Finder
- **Edit** — the predefined items only (undo/redo/cut/copy/paste/select-all), which delegate to the webview
- **View** — Home, Search (`CmdOrCtrl+F`), Palette, Packages, Templates
- **Window** — one item per open window, the key window marked; predefined minimize/zoom/close
- **Help** — About, Known Limits

**No Preferences and no ⌘, in D3** — it does not exist until D4, and shipping a dead item would break the rule the gate enforces.

**Context-sensitivity that must be modelled:** Export… and Reveal in Finder only make sense when the key window is a file. `key_has_file: false` ⇒ those items are `enabled: false`. Open Recent with an empty store shows a single disabled "No recent files" item rather than an empty submenu.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn win(label: &str, file: Option<&str>, title: &str) -> OpenWindow {
        OpenWindow { label: label.into(), file_id: file.map(Into::into), title: title.into() }
    }

    fn all_items(m: &MenuModel) -> Vec<&Item> {
        m.sections.iter().flat_map(|s| s.entries.iter()).filter_map(|e| match e {
            Entry::Item(i) => Some(i), _ => None,
        }).collect()
    }

    #[test]
    fn the_top_level_sections_are_the_five_we_promise() {
        let m = build_menu_model(&[], &[], false);
        let titles: Vec<&str> = m.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["File", "Edit", "View", "Window", "Help"]);
    }

    #[test]
    fn the_promised_accelerators_are_present_and_correct() {
        let m = build_menu_model(&[], &[], false);
        let acc = |id: &str| all_items(&m).into_iter().find(|i| i.id == id)
            .unwrap_or_else(|| panic!("no item {id}")).accelerator.clone();
        assert_eq!(acc("file.new-file"), Some("CmdOrCtrl+N".into()));
        assert_eq!(acc("file.open"), Some("CmdOrCtrl+O".into()));
        assert_eq!(acc("view.search"), Some("CmdOrCtrl+F".into()));
    }

    #[test]
    fn preferences_is_absent_in_d3() {
        // D4 adds it together with the window it opens. A dead item would
        // break the no-orphaned-items rule the gate enforces.
        let m = build_menu_model(&[], &[], false);
        assert!(all_items(&m).iter().all(|i| !i.id.contains("preferences")));
        assert!(all_items(&m).iter().all(|i| i.accelerator.as_deref() != Some("CmdOrCtrl+,")));
    }

    #[test]
    fn every_enabled_item_carries_a_command() {
        // The no-dead-items rule, as an assertion.
        let m = build_menu_model(&[win("main", None, "Penpot Local")], &[], false);
        for i in all_items(&m) {
            if i.enabled {
                assert!(i.command.is_some(), "enabled item {} has no command", i.id);
            }
        }
    }

    #[test]
    fn item_ids_are_unique() {
        // Duplicate ids would make command_for_id ambiguous and silently
        // dispatch the wrong action.
        let m = build_menu_model(&[win("main", None, "Home"), win("file-f1", Some("f1"), "Alpha")],
                                 &[RecentEntry { file_id: "f1".into(), title: "Alpha".into(), page_id: None, opened_at: "x".into() }],
                                 true);
        let mut ids: Vec<&str> = all_items(&m).iter().map(|i| i.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate menu item ids");
    }

    #[test]
    fn file_only_actions_are_disabled_when_the_key_window_is_not_a_file() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], false);
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m).into_iter().find(|i| i.id == id).unwrap();
            assert!(!i.enabled, "{id} should be disabled with no file window key");
        }
        let m2 = build_menu_model(&[win("file-f1", Some("f1"), "Alpha")], &[], true);
        for id in ["file.export", "file.reveal"] {
            let i = all_items(&m2).into_iter().find(|i| i.id == id).unwrap();
            assert!(i.enabled, "{id} should be enabled when a file window is key");
        }
    }

    #[test]
    fn empty_recent_shows_one_disabled_placeholder_not_an_empty_submenu() {
        let m = build_menu_model(&[], &[], false);
        let recents: Vec<&Item> = all_items(&m).into_iter().filter(|i| i.id.starts_with("file.recent")).collect();
        assert_eq!(recents.len(), 1);
        assert!(!recents[0].enabled);
        assert!(recents[0].command.is_none());
    }

    #[test]
    fn the_window_menu_lists_every_open_window() {
        let wins = [win("main", None, "Penpot Local"), win("file-f1", Some("f1"), "Alpha")];
        let m = build_menu_model(&wins, &[], true);
        let window_section = m.sections.iter().find(|s| s.title == "Window").unwrap();
        let labels: Vec<&str> = window_section.entries.iter().filter_map(|e| match e {
            Entry::Item(i) => Some(i.label.as_str()), _ => None,
        }).collect();
        assert!(labels.contains(&"Penpot Local") && labels.contains(&"Alpha"), "{labels:?}");
    }

    #[test]
    fn command_for_id_round_trips_every_enabled_item() {
        let m = build_menu_model(&[win("main", None, "Home")], &[], false);
        for i in all_items(&m) {
            if i.enabled {
                assert!(command_for_id(&m, &i.id).is_some(), "no command resolved for {}", i.id);
            }
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail. Step 3: Implement. Step 4: Run tests. Step 5: Commit.**

---

### Task 6: The translation layer and event dispatch

**Files:**
- Create: `apps/desktop/src/menubar/mod.rs`
- Modify: `apps/desktop/src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `pub fn install(app: &AppHandle, ctx: &MenuCtx) -> tauri::Result<()>` and `pub fn rebuild(app: &AppHandle, ctx: &MenuCtx) -> tauri::Result<()>`, where `MenuCtx` carries the registry, data dir, proxy url and team id.

**Read `apps/desktop/src/tray/mod.rs` first** — it is the same job for the tray and this must mirror it: a dumb translation of the model with no branching of its own.

**macOS behaviour that dictates the design:** the menu bar is app-wide, so it is set with `AppHandle::set_menu` and **rebuilt whenever the window set or the key window changes** — exactly as the tray rebuilds on every status change. Call `rebuild` when a window opens, closes, or gains focus.

- [ ] **Step 1: Wire it**

`install` builds the model, translates it (`Submenu` per section, `MenuItem::with_id` carrying the model's id and accelerator, `PredefinedMenuItem` for Edit and the Window minimize/zoom/close), and calls `app.set_menu(menu)`. Register `Builder::on_menu_event` once, app-global — per-window menu events do not exist on macOS. Dispatch by looking the id up with `command_for_id` and running the matching command. Every `Command` variant must be handled; use an exhaustive `match` with no `_` arm so a newly added command fails to compile until it is wired.

- [ ] **Step 2: Assert the translation is complete**

Add a test that every `Entry::Item` in the model has a non-empty id and that the dispatcher's match covers every `Command` variant. The compiler enforces the second half if you avoid a wildcard arm — say so in a comment so nobody adds one.

- [ ] **Step 3: Verify by hand**

Build and launch the GUI: `cargo build -p penpot-desktop && ./target/debug/penpot-desktop`. Confirm the menu bar appears with the five menus, ⌘N creates a file, ⌘O opens the picker, ⌘F opens search, opening two files gives two windows both listed under Window, and closing one removes it from that menu.

- [ ] **Step 4: Commit.**

---

### Task 7: The gate

**Files:**
- Create: `scripts/d3-menus.sh`, `scripts/d3_menus_helper.py`
- Modify: `justfile`

**Model it on `scripts/d2-home.sh`**: same header block, `pass`/`fail`, PID-scoped cleanup, totals, non-zero exit. D3 ports: proxy 9050, backend 6512, postgres 5585, valkey 6528.

The gate asserts:

- [ ] **Step 1: The menu model's shape** — run the Rust unit tests as a gate leg (`cargo test -p penpot-desktop menubar:: windows:: recent::`) and require the specific test names that pin the contract, not merely "the suite passed" (the D1/D2 precedent — a renamed or weakened test could otherwise leave this green).

- [ ] **Step 2: Every command behind the menu actually works, headlessly.** For each command the model names, exercise its underlying route or function against a live stack: New File and New Project via `/__api/vault/manage/*`; Home/Search/Palette/Packages/Templates by fetching each page and asserting it renders (HTTP 200 and a non-trivial body, not just a status code); Reveal via its pure command builder; Open Vault via the control server. **Print which commands are covered and which are not** — a silent gap here is the "no dead items" rule going unenforced.

- [ ] **Step 3: No orphaned items in either direction.** Assert every enabled model item resolves to a command (the model test), and that no command exists that no menu item reaches. Both directions, because an unreachable command is dead code and a command-less item is a dead menu entry.

- [ ] **Step 4: Open Recent is real.** Open a file, assert it appears in the recent store; open a second, assert ordering; reopen the first, assert it moves to the front without duplicating.

- [ ] **Step 5: Window-per-file, asserted where it can be.** The registry's behaviour is unit-tested; the GUI half (two windows, two titles) needs a GUI session, so follow the D0/D2 precedent: a clearly-marked GUI leg that states its requirement rather than silently skipping. **A skipped leg must not read as a pass** — print it as SKIPPED with the reason, and fail if it was expected to run.

- [ ] **Step 6: Chain into `just e2e`, verify** (`bash -n`, `python3 -m py_compile`, `just --list`), **and commit.**

---

### Task 8: The milestone document

**Files:** Create `docs/milestones/d3/README.md` and `img/`.

- [ ] **Step 1: Capture** the menu bar open (a screenshot of native chrome needs a real GUI session — take it manually, as agreed for native-chrome captures) and two file windows side by side.

- [ ] **Step 2: Write it**, following `docs/milestones/d2/README.md`'s shape: what changed, before/after, a diagram of model → translation → dispatch, then **known limits stated not buried**, covering at least: Preferences is absent until D4; the pickers are macOS-only; the Window menu is app-wide because macOS gives no per-window menu; whatever the GUI leg could not assert headlessly.

- [ ] **Step 3: Commit.**

---

## Self-Review

**1. Spec coverage.** PLAN4's D3 asks for: File menu ✅ (Task 5), Edit delegating to the webview ✅ (5, predefined items), View ✅ (5), Window ✅ (5 + Task 1's registry), Help ✅ (5), accelerators ✅ (5 — minus ⌘, by explicit decision), the gate asserting the model and that every action's command works headlessly with no orphaned items ✅ (Task 7), green twice ✅ (7). Window-per-file, resolved as open question 1, is Tasks 1-2.

**2. Placeholders.** None. Tasks 2, 6 and 7 carry "read the real code first" notes; those are verification instructions naming exact files, not deferred decisions.

**3. Type consistency.** `OpenWindow` defined in Task 1 and consumed by Tasks 2, 5, 6. `RecentEntry` defined in Task 3 and consumed by Task 5. `Command`/`Item`/`MenuModel` defined in Task 5 and consumed by Task 6. `build_menu_model(windows, recent, key_has_file)` has the same signature everywhere it appears.

**Deliberate deviations from PLAN4, both recorded in the Global Constraints:** no Preferences item and no ⌘, in D3 (D4 owns it, and a dead item would break the gate's central rule); the file pickers are macOS-only, mirroring the existing `choose_folder`.

**Known risk carried into execution:** whether menu-item accelerators collide with the N4 global shortcut registered via `tauri_plugin_global_shortcut` (`apps/desktop/src/overlay.rs:47`) is untested. They are different mechanisms — a global shortcut fires app-wide even unfocused, a menu accelerator only when the app is key — so a clash would surface as one of them not firing. Task 6's manual verification must exercise the palette shortcut and ⌘F in the same session, and the milestone doc must record the result rather than assuming they coexist.
