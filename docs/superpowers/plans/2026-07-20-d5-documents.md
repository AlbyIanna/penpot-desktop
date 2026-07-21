# D5 — OS Integration: Documents, Windows, Drag-and-Drop: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The app behaves like a document-based app — a `.penpot` opens in its own window from a CLI argument, a second launch, a drag-drop, or (packaged) a Finder double-click.

**Architecture:** Every way of "opening a document" funnels through one path: a filesystem path → a **file-id** (via the manifest's `entry_by_path`) → `open_file_window`. A path outside the vault is offered for import (copy in, let the daemon import it, open once it has an id). The macOS Finder path arrives as `RunEvent::Opened` (an Apple event, **not argv** — proven by the D5a spike); the CLI and second-launch paths arrive as argv. A control-server route exposes the open-window set so the gate can observe that a document actually opened.

**Tech Stack:** Rust, Tauri 2.11.5 (`RunEvent::Opened`, `WindowEvent::DragDrop`, `bundle.macOS.infoPlist`), `crates/sync-core` (manifest, zip_dir), bash + python3 for the gate.

## Global Constraints

- **Core invariant (P0):** delete the entire database, restart, and every project/file rebuilds from the folder tree with no data loss. The folder tree is the source of truth; the DB is a disposable cache.
- **Zero cross-vault spill (P0):** opening or importing a document never surfaces or writes a file from another vault. A document is resolved against the *active* vault only.
- **Invariant 3:** the SPA stays byte-untouched — no injected scripts, nothing under `runtime/frontend/`. Drag-drop is caught natively via `WindowEvent::DragDrop`, never by a content script in the SPA.
- **The dashboard is not the front door:** file windows carry the same navigation policy as every other window (D3 established this — reuse it).
- **Cooperate with the M5 single-instance guard:** a second launch must **forward the document** to the running app, never boot a second supervised stack.
- **D5a spike findings are binding** (`docs/spikes/finder-document-association.md`): the Finder double-click path works **only from a signed, installed `.app`**, so it is verified by the spike + a manual packaged-build check, NOT by the headless gate. The document path arrives via `RunEvent::Opened`, not argv.
- **External-file policy (product-owner decision):** a `.penpot` from outside the vault is **offered for import** (copy into the vault, daemon imports it, window opens once it has a file-id) — never opened in place, never silently written.
- **D5 dedicated ports:** proxy 9056, backend 6518, postgres 5591, valkey 6534; control 9057.
- Commits end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`; never a bare `#<number>` in commit or PR text.
- `just d5` chained into `just e2e`; green twice.

## File Structure

| File | Responsibility |
|---|---|
| `apps/desktop/src/docopen.rs` | **New.** The pure path→resolution logic. No Tauri, no I/O beyond a manifest handed in. |
| `apps/desktop/src/menubar/mod.rs` | Modify: `open_document` orchestration (resolve → open in-vault / offer import); the `RunEvent::Opened` + drag-drop handlers call it. |
| `apps/desktop/src/main.rs` | Modify: first-launch CLI arg; single-instance callback forwards the document; `RunEvent::Opened` arm; drag-drop event arm. |
| `apps/desktop/src/control.rs` | Modify: `GET /windows` exposing the open-window set for the gate. |
| `apps/desktop/tauri.conf.json` / a new `apps/desktop/Info.plist` | Modify/Create: the `.penpot` package document-type declaration. |
| `scripts/d5-documents.sh`, `scripts/d5_docs_helper.py` | **New.** The gate. |
| `docs/milestones/d5/README.md` | **New.** The milestone doc. |

---

### Task 1: The path → resolution logic

**Files:** Create `apps/desktop/src/docopen.rs`; modify `apps/desktop/src/lib.rs` (`pub mod docopen;`).

**Interfaces — Consumes:** `sync_core::manifest::Manifest` and its `entry_by_path(rel_path: &str) -> Option<(&str, &ManifestEntry)>` (path is **vault-relative**, `/`-separated). `ManifestEntry.path`.
**Produces:**
- ```rust
  pub enum Resolved {
      InVault { file_id: String, title: String },
      External { path: PathBuf },
      NotAPenpotDir { reason: String },
  }
  ```
- `pub fn resolve(raw_path: &Path, vault_root: &Path, manifest: &Manifest) -> Resolved`
- `pub fn display_title(rel_path: &str) -> String` — `<project>/<name>.penpot` → `<name>` (mirror the existing `file_display_name` in `menubar/mod.rs` — read it and match its output exactly).

**Rules the resolver enforces (each is a test):**
- A path that is not a directory whose name ends in `.penpot` → `NotAPenpotDir`.
- A `.penpot` dir **inside** `vault_root` and **present in the manifest** → `InVault{file_id, title}`.
- A `.penpot` dir inside the vault but **not yet in the manifest** (freshly created on disk, daemon hasn't imported it) → treat as `External`? No — it is inside the vault, so it belongs to this vault; return a distinct state the caller can poll on. Model this as `InVault` only when the id is known; when it's a vault-internal path with no manifest entry yet, return `External{path}` is WRONG (it's already in the vault). Add a fourth variant `PendingImport { rel_path: String }` for "inside the vault, not yet an id" so the caller polls rather than copies.
- A `.penpot` dir **outside** `vault_root` → `External{path}`.
- Path normalization: resolve symlinks / `..` before the `strip_prefix(vault_root)` check, so `vault/../vault/x.penpot` cannot masquerade as external and a `..`-laden path cannot escape. Use `std::fs::canonicalize` on both sides where the path exists.

- [ ] **Step 1: Write the failing tests** — one per rule above, plus:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::manifest::{Manifest, ManifestEntry};

    fn vault_with(root: &std::path::Path, rel: &str, id: &str) -> Manifest {
        std::fs::create_dir_all(root.join(rel)).unwrap();
        let mut m = Manifest::default();
        m.files.insert(id.into(), ManifestEntry {
            path: rel.into(), project_id: "p".into(), project_name: "P".into(),
            revn: 1, db_modified_at: String::new(), last_synced_hash: "h".into(),
            last_synced_at: "2026-07-20T00:00:00Z".into(),
        });
        m
    }

    #[test]
    fn a_known_in_vault_penpot_resolves_to_its_file_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let m = vault_with(root, "Proj/Home.penpot", "fid1");
        match resolve(&root.join("Proj/Home.penpot"), root, &m) {
            Resolved::InVault { file_id, title } => { assert_eq!(file_id, "fid1"); assert_eq!(title, "Home"); }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_non_penpot_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        assert!(matches!(resolve(&tmp.path().join("notes"), tmp.path(), &Manifest::default()),
                         Resolved::NotAPenpotDir { .. }));
    }

    #[test]
    fn an_external_penpot_is_flagged_for_import() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("Loose.penpot")).unwrap();
        assert!(matches!(resolve(&outside.path().join("Loose.penpot"), vault.path(), &Manifest::default()),
                         Resolved::External { .. }));
    }

    #[test]
    fn a_vault_internal_penpot_with_no_manifest_entry_yet_is_pending_not_external() {
        // Freshly created on disk; the daemon has not imported it. It is NOT
        // external — copying it in would duplicate it. The caller polls.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Proj/New.penpot")).unwrap();
        assert!(matches!(resolve(&tmp.path().join("Proj/New.penpot"), tmp.path(), &Manifest::default()),
                         Resolved::PendingImport { .. }));
    }

    #[test]
    fn dotdot_cannot_make_an_in_vault_path_look_external() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let m = vault_with(root, "Proj/Home.penpot", "fid1");
        let sneaky = root.join("Proj").join("..").join("Proj").join("Home.penpot");
        assert!(matches!(resolve(&sneaky, root, &m), Resolved::InVault { .. }));
    }
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p penpot-desktop docopen::`
- [ ] **Step 3: Implement** with the `PendingImport` variant added to the enum.
- [ ] **Step 4: Run tests. Step 5: Commit.**

---

### Task 2: Expose the open-window set for the gate

**Files:** Modify `apps/desktop/src/control.rs`.

**Why first:** the gate must observe that a document actually opened, and D3's own gate hit the wall that `WindowRegistry` is not reachable from a shell. The data already exists (`WindowRegistry::list()` returns `{label, file_id, title}`); this exposes it over the existing localhost control server.

**Interfaces — Produces:** `GET /windows` on the control port → `{"windows":[{"label","fileId":<opt>,"title"},...]}`.

Read how the control router is built (`control.rs`, the `/health`/`/active`/`/list` routes) and add `/windows` the same way. The router needs read access to the `WindowRegistry`; thread it in the same way the other state reaches those handlers. The control server is localhost-only and only bound when `PENPOT_LOCAL_CONTROL_PORT` is set (test-only) — confirm that stays true.

- [ ] **Step 1:** a test that the route serializes a registry snapshot to the documented shape (construct a `WindowRegistry`, insert two windows, assert the JSON).
- [ ] **Step 2-4:** implement, test, commit.

---

### Task 3: Open an in-vault document — orchestration + the argv paths

**Files:** Modify `apps/desktop/src/menubar/mod.rs`, `apps/desktop/src/main.rs`.

**Interfaces — Produces:** `pub fn open_document(app, ctx: &MenuCtx, raw_path: &Path)` in `menubar/mod.rs` — resolves via `docopen::resolve` against the active vault's manifest, and for `InVault` calls the existing `open_file_window(app, ctx, &file_id, None, &title)`. `External` and `PendingImport` are handled in Tasks 5 (and a poll) — in THIS task, log them clearly and do nothing destructive; the later tasks fill them in.

**The three argv arrival paths, all funnelling into `open_document`:**

1. **First launch with a path** (`penpot-desktop /path/to/x.penpot`): today `main.rs` ignores argv entirely (survey §4). Read `std::env::args()` in `.setup()` **after boot has published the vault facts** (the same ordering `open_file_window` already requires — it no-ops before boot), and call `open_document` for a `.penpot` argument.
2. **Second launch** (`tauri_plugin_single_instance`): today the callback discards `_argv` and only refocuses the home window (survey §3, `main.rs:78-91`). Change it to parse a `.penpot` path out of argv and call `open_document`, so a second launch **forwards the document** instead of only focusing. It must still refocus when there is no document. Retrieve the `MenuCtx` via Tauri managed state (`app.manage(ctx)` in setup; `handle.try_state()` in the callback) — by the time a second launch fires, the first instance has finished setup, so the state is present.
3. **`RunEvent::Opened { urls }`** (macOS Finder/`open`, the spike's path): add an arm to the existing `app.run(|handle, event| …)` loop (`main.rs:443`). Each url is a `file://` URL — convert to a path and call `open_document`. This is macOS-gated (`#[cfg(target_os = "macos")]`).

**Title at open:** `open_file_window` already sets the window title from the passed `title`, which for a document is `docopen::display_title`. That satisfies "the title reflects the file at open time" — the rename-while-open case is Task 6.

- [ ] **Step 1:** a unit test of `open_document`'s routing decision on a resolver result (extract the decision so it is testable without a running app, mirroring D3's `reuse_or_create`).
- [ ] **Step 2-5:** implement the three arrival paths, build, verify by hand (`./target/debug/penpot-desktop /path/to/an/in-vault/x.penpot` opens that file's window), commit.

---

### Task 4: Drag-and-drop onto the window

**Files:** Modify `apps/desktop/src/menubar/mod.rs` (the `on_window_event` match already there).

`WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. })` — add an arm to the SAME `on_window_event` match `open_file_window` already wires for `Destroyed`/`Focused` (survey §6). For each dropped path, call `open_document`. This is native — no JS injected into the SPA, satisfying invariant 3. A drop delivers a `Vec<PathBuf>`; handle each entry, and since a `.penpot` is a directory, the resolver's `is_dir` check routes non-directories to `NotAPenpotDir`.

- [ ] Steps: implement, build, verify by hand (drag an in-vault `.penpot` onto a window → it opens), commit. (Drag-drop cannot be driven headlessly — the gate asserts the resolver + `open_document` decision instead, and this leg is verified manually like the Finder path.)

---

### Task 5: Offer to import an external `.penpot`

**Files:** Modify `apps/desktop/src/menubar/mod.rs`.

The product-owner decision: an `External` result prompts "Import into your vault?" (`dialog::native_confirm` — check the real dialog API in `dialog.rs`; if only info/error exist, add a confirm mirroring them). On yes:
1. Copy the `.penpot` directory into the active vault under a project folder (default project; decide the target and state it). Copy, do not move — the source is the user's, outside the vault.
2. The sync daemon's Direction B imports an unknown in-vault dir as a new file (survey §9) on its ~2s poll.
3. Poll the manifest (via `entry_by_path` on the new relative path) until a file-id appears, then `open_document`/`open_file_window`. Bound the poll with a timeout and surface a clear error on timeout — never hang.

`PendingImport` (a vault-internal path with no id yet, from Task 1) uses the **same poll-until-id** step, minus the copy.

- [ ] **Step 1:** a test of the target-path computation (external path → vault-relative destination), pure and deterministic.
- [ ] **Step 2-5:** implement copy + poll + open, build, verify by hand with a `.penpot` from outside the vault, commit.

**Zero-spill note:** the copy target is always inside the *active* vault, computed from the active `vault_root` — never another vault. The gate re-asserts no cross-vault spill after an import.

---

### Task 6: The window title tracks a rename

**Files:** Modify wherever the sync daemon's rename→disk relocation surfaces (D2 added directory relocation; find where a file's on-disk name changes) and `apps/desktop/src/windows.rs` / `menubar/mod.rs`.

Today the title is set once at window creation (survey §7); a file renamed while its window is open keeps a stale title. "The window title tracks the open file" (exit criterion) means: when the open file's name changes, `window.set_title(new)` for the matching `file_window_label`.

The registry already keys windows by `file_id`; the title update needs a signal that "file `<id>` is now named `<x>`". Decide the simplest source — the status/manifest watch the home page already polls, or a dedicated hook — and wire it to `set_title` for the open window. Keep it cheap and do not add a new polling loop if an existing signal carries the rename.

- [ ] Steps: implement, a unit test of the "compute the new title for a renamed file" decision, verify by hand (open a file, rename it via the home page, watch the window title change), commit.

---

### Task 7: Register `.penpot` as a package document type (packaged config)

**Files:** Create `apps/desktop/Info.plist` (or the `bundle.macOS.infoPlist` target); modify `apps/desktop/tauri.conf.json` and/or `tauri.bundle.conf.json`.

Per the D5a spike: Tauri's `fileAssociations` cannot express `LSTypeIsPackage`, so use the `bundle.macOS.infoPlist` escape hatch with a hand-written plist declaring the `dev.albyianna.penpot-file` UTI (`UTTypeConformsTo` = `com.apple.package`, extension `penpot`) and a `CFBundleDocumentTypes` entry with `LSTypeIsPackage = true` and role `Editor` — exactly the plist the spike proved works.

**The merge risk the spike flagged:** Tauri also generates a `CFBundleDocumentTypes` block. Verify our custom plist and Tauri's generated one **merge** rather than one clobbering the other — inspect the built `.app`'s `Info.plist`. This is the one step that needs a packaged build (`scripts/build-dmg.sh`).

- [ ] **Step 1:** write the plist and wire it into the bundle config.
- [ ] **Step 2:** build the `.app` (`scripts/build-dmg.sh`), and assert the resulting `Contents/Info.plist` contains BOTH `LSTypeIsPackage` and the UTI declaration (a `PlistBuddy`/`plutil` check). This is a build-time assertion, committed as a small script the milestone doc references — not part of the headless `just d5` gate.
- [ ] **Step 3:** run `scripts/d5a-finder-spike.sh` semantics against the *real* built app if feasible (register it, probe `isPackage`/default-handler) — reusing the spike's probe. Record the result in the milestone doc. Commit.

---

### Task 8: The gate

**Files:** Create `scripts/d5-documents.sh`, `scripts/d5_docs_helper.py`; modify `justfile`.

**Model on `scripts/d4-preferences.sh`**: header block, `pass`/`fail`, PID-scoped cleanup, totals, non-zero exit, SKIPPED distinct from PASS. Ports 9056/6518/5591/6534, control 9057.

Assertions:

- [ ] **(a) Launch with a path argument opens that file.** Boot the GUI binary with an in-vault `.penpot` path as argv (the testable path — NOT Finder double-click, which the spike showed needs a signed installed app). Assert via `GET /windows` (Task 2) that a window for that file-id is open, with the file's title. **Prove you were looking:** first assert `/windows` is reachable and returns the home window, so an empty/broken response cannot read as "no document opened".
- [ ] **(b) The window title tracks the open file.** After opening, assert the window's title equals the file's name via `/windows`. If Task 6's rename-tracking is in, additionally rename the file and assert the title updates.
- [ ] **(c) A second launch forwards instead of double-booting.** With instance 1 running, launch instance 2 with a `.penpot` path. Assert: instance 2 exits promptly, its log never shows `READY`/supervisor-boot lines, the D5 ports are **not** doubled (reuse `m5-features.sh`/`d4`'s `ports_all_free` on a second port set — nothing bound them), instance 1 still answers, and `/windows` on instance 1 now shows the forwarded document open. This is the M5-cooperation exit criterion.
- [ ] **(d) Import-external keeps zero cross-vault spill.** Drive the offer-to-import path (or its underlying copy+poll) for a `.penpot` outside the vault, and assert the imported file appears in THIS vault only — reuse the N5 spill assertions.
- [ ] **(e) The resolver + drag-drop + Finder legs that cannot be driven headlessly are SKIPPED with reasons**, kept out of the pass count, with the underlying logic covered by the unit tests required by name (the D1/D2/D3 precedent — require the specific `docopen::tests::*` names).
- [ ] **Step: chain into `just e2e`, verify (`bash -n`, `py_compile`, `just --list`), commit.**

---

### Task 9: The milestone document

**Files:** Create `docs/milestones/d5/README.md`.

- [ ] Follow `docs/milestones/d4/README.md`'s shape. Cover: the one funnel (path → file-id → window) and its four arrival paths; the D5a spike's GO-with-caveat (Finder needs a signed installed app; the path arrives as an Apple event, not argv); the offer-to-import decision; and **known limits stated not buried** — Finder double-click is spike-verified + manual-build-checked, not gate-asserted; drag-drop and Finder are macOS-only; the import poll has a timeout; whatever the gate could not assert headlessly.
- [ ] For visuals: capture our own web surfaces with `scripts/shots.sh` if useful, and describe the native document behaviour in text + the spike's probe output. **Do not use `screencapture` for native chrome** — it captures a screen region, not a window, and can grab unrelated on-screen content.
- [ ] Commit.

---

## Self-Review

**1. Spec coverage.** PLAN4's D5 asks for: open a `.penpot` from Finder / CLI argument / URL scheme ✅ (Tasks 3, 7 — Finder via the spike-proven package config, CLI via argv; URL scheme is not separately required and is out of scope, noted below); one window per file with filename in the title ✅ (Task 3, D3's window-per-file); multi-window ✅ (already, D3); drag a `.penpot` onto the app ✅ (Task 4); cooperate with the single-instance guard — forward the document ✅ (Task 3 path 2, Task 8c). Exit criteria: launch with a path argument opens the file ✅ (8a), window title tracks ✅ (6, 8b), second launch forwards without double-booting ✅ (8c), green twice ✅.

**2. Placeholders.** None. Tasks 3, 5, 6, 7 carry "read the real code/API first" instructions naming exact files and the interfaces to match.

**3. Type consistency.** `Resolved` (with `InVault`/`External`/`PendingImport`/`NotAPenpotDir`) defined in Task 1 and consumed by Tasks 3, 4, 5. `open_document` defined in Task 3 and called by Tasks 4, 5. `display_title` defined in Task 1, used in Task 3. The `GET /windows` shape from Task 2 is what Task 8 asserts against.

**Deliberate scope notes:**
- **URL scheme** (`penpot://`) is listed in PLAN4's "file association / CLI argument / URL scheme" as *alternatives*; the CLI + Apple-event paths satisfy "open from Finder". A custom URL scheme adds a second registration surface with no offline use case, so it is out of scope for D5 — recorded, not smuggled.
- **Finder double-click is not in the headless gate**, per the D5a spike (needs a signed, installed app). Task 7 verifies it against a real build; the gate proves the CLI-argument equivalent. This is the honest split, not a coverage gap hidden.

**Known risk carried into execution:** the offer-to-import poll depends on the sync daemon's Direction B importing an unknown in-vault directory within the poll timeout. If a large `.penpot` takes longer than the timeout to import, the window won't open automatically — Task 5 must surface that as a clear "still importing, it will appear on your home shortly" message, not a silent failure or a hang, and the milestone doc must state the bound.
