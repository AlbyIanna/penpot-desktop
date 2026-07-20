# D4 — Native Preferences + Native Dialogs: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Preferences window that replaces Penpot's `/settings`, where every setting genuinely takes effect — including the ones that are baked in at boot.

**Architecture:** Preferences persist as `preferences.json` in the app **data dir** (the convention `vaults.json`, `recent-files.json` and `plugin-consent.json` already established: per-machine state that must never travel with a cloned vault). The window is our own page at `/__preferences`, served by the same router as `/__home` and opened with `WebviewWindowBuilder`, matching `/__palette`. Settings split in two: **live** ones (sync on/off, vault switch, disabling the exporter) apply immediately; **boot-time** ones (plugins, CSP, enabling the exporter) apply by **rebooting the supervised stack in place**, reusing the proven N5 stop/boot machinery rather than asking the user to quit.

**Tech Stack:** Rust, axum, Tauri 2.11.5, vanilla HTML/JS (no framework, no build step), bash + python3 for the gate.

## Global Constraints

- **Core invariant (P0):** delete the entire database, restart, and every project/file rebuilds from the folder tree with no data loss. The folder tree is the source of truth; the DB is a disposable cache.
- **Zero cross-vault spill (P0):** switching vaults never surfaces, imports, or writes a file from another vault. A Preferences-initiated switch must go through `VaultRunner::switch_to` — the same path the N5 gate proves — not a reimplementation.
- **Invariant 3:** the SPA stays byte-untouched — no serve-time patching of upstream JS/CSS, no injected scripts, nothing under `runtime/frontend/`.
- **No orphaned or dead menu items** (D3's rule, still enforced by `just d3`). D4 finally adds the Preferences item and `CmdOrCtrl+,` — and must flip D3's `preferences_is_absent_in_d3` test rather than delete it.
- **Preferences live in the DATA dir, never the vault.** They are per-machine, and a cloned vault must not carry them.
- **D4 dedicated ports:** proxy 9054, backend 6516, postgres 5589, valkey 6532.
- Commits end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`; never a bare `#<number>` in commit or PR text.
- `just d4` chained into `just e2e`; green twice.

## File Structure

| File | Responsibility |
|---|---|
| `apps/desktop/src/prefs.rs` | **New.** The typed preferences model + atomic load/save. Pure, no Tauri. |
| `apps/desktop/src/prefs_http.rs` | **New.** `/__preferences` page + its JSON routes. |
| `apps/desktop/src/preferences.html` | **New.** The page itself — vanilla, matching `home.html`'s idiom. |
| `apps/desktop/src/control.rs` | Modify: `reboot_in_place` beside `switch_to`. |
| `apps/desktop/src/lib.rs` | Modify: read prefs at boot; expose a live "stop renders" control. |
| `apps/desktop/src/menubar/model.rs` | Modify: add Preferences + `CmdOrCtrl+,`. |
| `apps/desktop/src/navwatch.rs` | Modify: `#/settings` now cancelled by default — its replacement exists. |
| `scripts/d4-preferences.sh`, `scripts/d4_prefs_helper.py` | **New.** The gate. |
| `docs/milestones/d4/README.md` | **New.** The milestone doc. |

---

### Task 1: The preferences store

**Files:** Create `apps/desktop/src/prefs.rs`; modify `apps/desktop/src/lib.rs` (`pub mod prefs;`).

**Interfaces — Produces:**
- `pub const PREFS_FILE_NAME: &str = "preferences.json";`
- ```rust
  pub struct Preferences {
      pub sync_enabled: bool,      // default true
      pub exports_enabled: bool,   // default true  (thumbnails/exporter)
      pub plugins_enabled: bool,   // default true
      pub csp_enabled: bool,       // default true
  }
  ```
  with `Default`, `serde(default)` on every field, and `#[serde(rename_all = "camelCase")]`.
- `pub fn load(data_dir: &Path) -> Preferences` — never fails; a missing or corrupt file yields defaults.
- `pub fn save(data_dir: &Path, prefs: &Preferences) -> anyhow::Result<()>` — atomic (tmp + rename), mirroring `vault.rs`'s `write_json_atomic`.
- `pub fn needs_reboot(old: &Preferences, new: &Preferences) -> bool` — true when a **boot-time** field changed: `plugins_enabled`, `csp_enabled`, or `exports_enabled` going **false → true**.

**Why `needs_reboot` is a function and not a UI decision:** which settings can apply live is a property of the system, not of the screen. Putting it here makes it unit-testable and keeps the page dumb.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_everything_on() {
        let p = Preferences::default();
        assert!(p.sync_enabled && p.exports_enabled && p.plugins_enabled && p.csp_enabled);
    }

    #[test]
    fn a_missing_or_corrupt_file_yields_defaults_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()), Preferences::default());
        std::fs::write(tmp.path().join(PREFS_FILE_NAME), b"{not json").unwrap();
        assert_eq!(load(tmp.path()), Preferences::default(),
                   "a corrupt prefs file must not stop the app booting");
    }

    #[test]
    fn round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Preferences { sync_enabled: false, exports_enabled: false, ..Default::default() };
        save(tmp.path(), &p).unwrap();
        assert_eq!(load(tmp.path()), p);
    }

    #[test]
    fn an_unknown_future_field_does_not_break_loading() {
        // A newer build's prefs file must not brick an older one.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(PREFS_FILE_NAME),
                       br#"{"syncEnabled":false,"somethingNew":42}"#).unwrap();
        assert!(!load(tmp.path()).sync_enabled);
    }

    #[test]
    fn only_boot_time_changes_need_a_reboot() {
        let base = Preferences::default();
        // Live: sync toggles without a reboot.
        assert!(!needs_reboot(&base, &Preferences { sync_enabled: false, ..base.clone() }));
        // Live: turning renders OFF just stops the poll loop.
        assert!(!needs_reboot(&base, &Preferences { exports_enabled: false, ..base.clone() }));
        // Boot-time: turning renders back ON needs the supervisor to spawn the child.
        let off = Preferences { exports_enabled: false, ..base.clone() };
        assert!(needs_reboot(&off, &base));
        // Boot-time: both of these are baked into config.js / the CSP header.
        assert!(needs_reboot(&base, &Preferences { plugins_enabled: false, ..base.clone() }));
        assert!(needs_reboot(&base, &Preferences { csp_enabled: false, ..base.clone() }));
    }

    #[test]
    fn no_change_never_needs_a_reboot() {
        let p = Preferences::default();
        assert!(!needs_reboot(&p, &p));
    }
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p penpot-desktop prefs::`
- [ ] **Step 3: Implement.** Derive `PartialEq, Clone, Debug`. Document on each field whether it is live or boot-time and WHY, citing the mechanism (`config.js` is read once at script load; the supervisor has no hot-add).
- [ ] **Step 4: Run tests. Step 5: Commit.**

---

### Task 2: Make the live settings actually live

**Files:** Modify `apps/desktop/src/lib.rs`, `apps/desktop/src/control.rs`.

**Interfaces — Produces:**
- `RunningApp` / `VaultRunner` gain `pub fn set_renders_enabled(&self, on: bool) -> bool` — stops the board-export poll loop when turned off; returns whether the request could be honoured (turning it **on** when the exporter child was never spawned returns `false`, which is what tells the caller a reboot is needed).
- Boot applies persisted preferences: after the sync daemon spawns, if `!prefs.sync_enabled` then pause it; if `!prefs.exports_enabled` then do not start board-export.

**Why:** today `SyncControl` always starts unpaused (`crates/sync-daemon/src/lib.rs`), so "sync off" would silently turn itself back on at every boot **and at every vault switch** — which calls `boot()` again. A preference that forgets itself is worse than no preference.

`board_export::BoardExportHandle::stop(self)` already exists and is called during shutdown; this task exposes it on the running app so Preferences can call it without tearing down the stack.

- [ ] **Step 1: Write the failing test**

Test the pure decision, not the Tauri plumbing:

```rust
    #[test]
    fn persisted_sync_off_is_reapplied_at_boot() {
        // The boot path must consult prefs; encode that as a small pure
        // helper so it is testable without booting a stack.
        assert!(should_pause_sync_at_boot(&Preferences { sync_enabled: false, ..Default::default() }));
        assert!(!should_pause_sync_at_boot(&Preferences::default()));
    }
```

- [ ] **Step 2: Run to verify it fails. Step 3: Implement. Step 4: Run tests + `cargo build`. Step 5: Commit.**

---

### Task 3: Reboot the stack in place

**Files:** Modify `apps/desktop/src/control.rs`.

**Interfaces — Produces:** `pub async fn reboot_in_place(&self) -> anyhow::Result<()>` on `VaultRunner`.

**Read `switch_to` first** (`apps/desktop/src/control.rs:160-222`) and reuse its machinery. A reboot is the same sequence **minus the vault change**: stop the current stack, then `boot()` again against the SAME vault with freshly-read preferences.

**What must NOT happen:** `switch_to` wipes the disposable DB and index (`vault::reset_disposable_state`) because it is changing vaults. A reboot in place **must not** wipe anything — the vault is unchanged, and wiping would force a full re-import of every file for a settings change. If reuse makes that hard to guarantee, factor the shared stop/boot part out rather than copying it.

**The switch marker:** `switch_to` writes a crash-safety marker so an interrupted switch is recoverable. Decide deliberately whether a reboot needs the same protection and say why in your report — an interrupted reboot leaves the same vault with no stack, which is a different (and milder) failure than an interrupted switch.

- [ ] **Step 1: Write the failing test.** Assert the reboot path does not call the disposable-state reset. If that is only observable by boot, factor the decision into a small pure function (e.g. `fn wipes_disposable_state(op: StackOp) -> bool`) and test that.
- [ ] **Step 2-4: implement, test, commit.**

---

### Task 4: The Preferences page and its routes

**Files:** Create `apps/desktop/src/prefs_http.rs`, `apps/desktop/src/preferences.html`; modify `apps/desktop/src/lib.rs`.

**Interfaces — Produces:**
- `GET /__preferences` → the page
- `GET /__api/prefs` → `{preferences: {...}, vault: {path, name}, syncPaused: bool, rendersRunning: bool}`
- `POST /__api/prefs` `{...preferences}` → `{ok: true, needsReboot: bool}` — saves, applies whatever is live, and reports whether a reboot is required for the rest
- `POST /__api/prefs/reboot` → reboots the stack in place, then `{ok: true}`
- `POST /__api/prefs/vault` `{path}` → the N5 switch, delegating to `VaultRunner::switch_to`

**Follow `home.rs` exactly** for the router shape and `include_str!` page serving, and `home.html` for the page idiom: vanilla JS, no framework, no build step, CSS custom properties. This is the fourth page in that family — match it, do not invent a new style.

**The page must be honest about reboots.** When a change needs one, say so and let the user choose — an "Apply & Restart" button — rather than silently rebooting the stack under an open workspace.

- [ ] **Step 1:** Routes with a test for the save round-trip and the `needsReboot` flag.
- [ ] **Step 2:** The page.
- [ ] **Step 3:** Verify by hand against a live stack, then commit.

---

### Task 5: Preferences joins the menu

**Files:** Modify `apps/desktop/src/menubar/model.rs`, `apps/desktop/src/menubar/mod.rs`.

D3 deliberately omitted Preferences and pinned its absence with `preferences_is_absent_in_d3`. **Flip that test rather than deleting it** — rename it to assert Preferences is now PRESENT, carries `CmdOrCtrl+,`, and sits in the application section where macOS users expect it.

Add `Command::Preferences`, dispatched to open the Preferences window (reuse-if-open by label, like `/__palette`). The dispatch `match` is exhaustive with no wildcard, so the compiler will demand this.

- [ ] Steps: test first, implement, verify `just d3` still passes (it asserts menu-model test names), commit.

---

### Task 6: Close `#/settings`

**Files:** Modify `apps/desktop/src/navwatch.rs`.

D1 left `#/settings` open and said so plainly; D2 and D3 kept it open because its replacement did not exist. It does now. Move `#/settings` into the unconditional-cancel class beside `#/auth` and `#/dashboard`, and update the tests that pin the old behaviour (`settings_is_unchanged_by_d2` — flip it, do not delete it).

- [ ] Steps: flip the tests, implement, run `cargo test -p penpot-desktop navwatch::`, commit.

---

### Task 7: The gate

**Files:** Create `scripts/d4-preferences.sh`, `scripts/d4_prefs_helper.py`; modify `justfile`.

**Model on `scripts/d3-menus.sh`**: header block with the port set, `pass`/`fail`, PID-scoped cleanup, totals, non-zero exit. Ports 9054/6516/5589/6532.

Assertions:

- [ ] **(a) Preferences persist across a restart.** Set a preference, restart the stack, read it back — from the file AND from `GET /__api/prefs`.
- [ ] **(b) A live setting actually takes effect.** Turn sync off via `POST /__api/prefs`, assert the daemon reports paused; turn it on, assert it resumes.
- [ ] **(c) The exporter toggle actually stops renders — the exit criterion.** With renders on, edit a file and wait for a render to appear under `<file>.exports/` (reuse `scripts/n2-thumbs.sh`'s `exports_check` helper approach — do not write a third hasher). Then turn renders OFF, edit again, and assert **no new render appears** within a comparable window. **Prove you were looking:** the "absence" leg must first demonstrate that the same edit DID produce a render while enabled, otherwise a broken exporter would pass this trivially.
- [ ] **(d) Sync-off survives a restart** — the failure this milestone must not ship: `SyncControl` starts unpaused on every boot, so without the re-apply, "sync off" silently turns itself back on.
- [ ] **(e) A Preferences-initiated vault switch keeps zero cross-vault spill.** Drive `POST /__api/prefs/vault` and assert the N5 guarantee: after the switch, the DB, `/__api/vault/boards` and the index hold ONLY the new vault's files. Reuse `scripts/n5_vaults_helper.py`'s assertions rather than writing new ones.
- [ ] **(f) A reboot-in-place does NOT wipe the vault's DB state.** Change a boot-time setting, reboot via `POST /__api/prefs/reboot`, and assert the files are still present with their ORIGINAL ids — a reboot that silently re-imported everything would be a regression against the core invariant's cost model.
- [ ] **Step: chain into `just e2e`, verify, commit.**

---

### Task 8: The milestone document

**Files:** Create `docs/milestones/d4/README.md`.

- [ ] Follow `docs/milestones/d3/README.md`'s shape. Native chrome cannot be screenshotted safely from an automated session — a screen capture takes a region of the display, not a window — so capture the Preferences **page** with `scripts/shots.sh` (it is our own web page) and describe the native window frame in text.
- [ ] **Known limits, stated not buried,** covering at least: which settings need a reboot and why (`config.js` is read once at script load; the supervisor cannot hot-add the exporter child); that a reboot under an open workspace is a real event the user opts into; anything the gate could not assert; and that the pickers remain macOS-only.

---

## Self-Review

**1. Spec coverage.** PLAN4's D4 asks for: vault location and switching through the N5 path ✅ (Tasks 4, 7e), sync on/off + status ✅ (2, 4, 7b, 7d), thumbnails/exporter toggle ✅ (2, 7c), plugin + CSP toggles ✅ (1, 3, 4), about/updates — **About already shipped in D3**; "updates" does not exist and is explicitly out of scope here (recorded as a deviation below). Native open/save dialogs ✅ — these shipped in D3 and are reused, not rebuilt. Exit criteria: persist across restart ✅ (7a), actually take effect ✅ (7b, 7c), vault switch keeps zero spill ✅ (7e), green twice ✅.

**2. Placeholders.** None. Tasks 3 and 4 carry "read the real code first" instructions naming exact files and line ranges.

**3. Type consistency.** `Preferences` and `needs_reboot` defined in Task 1 and used by 2, 3, 4, 7. `set_renders_enabled` defined in Task 2, called in Task 4. `reboot_in_place` defined in Task 3, called by the route in Task 4 and asserted in Task 7f.

**Deliberate deviation from PLAN4:** "about/updates" ships as About only. An update checker would mean contacting a server, which collides head-on with this chapter's own invariant that a normal session makes **zero non-loopback connection attempts**. That deserves its own decision rather than being smuggled in under a Preferences milestone; D6's residue audit is the right place to raise it.

**Known risk carried into execution:** rebooting the stack while a workspace window is open is the riskiest thing D4 does. The N5 vault switch already does exactly this and is proven by its gate, but that gate switches vaults from the home surface, not from an open file. Task 7f must assert the vault survives a reboot with original ids, and the milestone doc must state plainly whether an open workspace window survives it — measured, not assumed.
