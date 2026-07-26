# penpot-cli Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the ~11,400 GUI-free lines out of `apps/desktop` into six library crates, then add `penpot` as a third front-end over them.

**Architecture:** Chapter C (the command line), following the M/N/E/D chapter pattern. Phase 1 spiked the verb list against a live stack and threw the code away — **it is complete, and its findings revised this plan** (see "What Task 1 changed"). Phase 2 moves six crates in dependency order, one `git mv` plus Cargo wiring per task, with no behaviour change. Phase 3 builds the real CLI: Tier-1 verbs run in-process with no stack, Tier-2/3 verbs attach to a running daemon over loopback and never self-boot.

**Tech Stack:** Rust 2021, cargo workspace, axum 0.8, clap 4 (new, `apps/cli` only), tokio, anyhow. Gates are bash scripts under `scripts/`, chained from the `justfile`.

**Spec:** [`docs/superpowers/specs/2026-07-26-penpot-cli-design.md`](../specs/2026-07-26-penpot-cli-design.md)
**Task 1 findings (binding):** [`docs/spikes/cli-verbs.md`](../../spikes/cli-verbs.md)

## What Task 1 changed

The spike ran, and its crate-map verdict was **REVISED**. Three module assignments were wrong, two of them uncompilable:

- **`navprobe` → `crates/web-ui`.** `lib.rs:1074` does `.merge(navprobe::router())`. With `boot` in `app-core`, leaving `navprobe` in `apps/desktop` makes a library crate depend on the binary crate.
- **`status` → `crates/app-core`.** `lib.rs:1174` uses `status::MockStatusSource::demo_frames()`. Same illegal direction. Its other consumers (`tray/`, `main.rs`) reach it fine from `app-core`.
- **`prefs_http` → `crates/app-core`.** A real two-way cycle: `prefs_http.rs:78` has `use crate::control::RunnerSlot`, while `lib.rs:1064` calls `prefs_http::router(...)`. One direction is an import and the other a call site, which is why an import-only analysis missed it.

**Consequence: `app-core` now depends on `web-ui`, so `web-ui` is extracted BEFORE `app-core`** — the reverse of the original order.

Two further findings from the spike are folded into this plan rather than given their own tasks:

- **RC-6 (private verb handlers) does not apply.** Every mutating verb is a private axum handler, so a library-linking CLI would need each crate to grow a handler-free verb core. Because Tier-2/3 verbs go over loopback instead, they call the existing routes and need no new API. The Tier-1 readers are already `pub` (`vault::VaultRegistry::load`, `prefs::load`, `templates::enumerate_catalog`, `docopen::resolve`, `checkpoint::decide`, `packages::read_manifest`); whatever narrow gap remains is opened inside Task 10, driven by its failing test, not guessed at in advance.
- **RC-7 (silent concurrent writer) hardens the loopback rule.** The spike constructed `packages::PackagesState` in a second process against a live vault and ran the reconcilers alongside the app's own, writing `lock.json` and the consent ledger with nothing detecting it. Tier-2/3 verbs must therefore never link the library.

## Global Constraints

- **Behaviour is frozen in Phase 2.** Every task in Phase 2 is a move plus Cargo wiring. If a step appears to need a logic change, stop the task and land the change as its own commit before or after the move — never inside it.
- **Extraction order is load-bearing:** `installer` → `app-state` → `packages` → `vault-ops` → `web-ui` → `app-core`. `packages` and `vault-ops` are independent of each other; nothing else may be reordered.
- **The gate ladder is the oracle.** `cargo test --workspace` must be green before AND after every Phase 2 task, plus the live gates each task names. The GUI-session gates (`d2-home.sh`, `d3-menus.sh`, `d5-documents.sh`) cannot run headless — the controller runs those at the end of Phase 2 and Phase 3, never a subagent.
- **No new crate may depend on `tauri`.** Enforced by Task 8's gate.
- **Module names never change.** `docopen`, `windows`, `recent`, `reveal` keep their exact module paths inside their new crates, so the pinned test names in `scripts/d3-menus.sh` and `scripts/d5-documents.sh` stay valid — only the `-p <package>` flag changes.
- **`include_str!` resolves relative to its source file.** Four modules compile in a sibling HTML page and must move with it: `home.rs` + `home.html` + `palette.html`, `navprobe.rs` + `navprobe.html`, `prefs_http.rs` + `preferences.html`, `templates.rs` + `templates.html`.
- **`apps/desktop` needs `installer` as a DIRECT dependency** — `menubar/mod.rs:1033` and `:1043` call `installer::default_project_id` / `first_page_id`. Easy to miss, because the module map reads as if only `vault-ops` and `packages` use it.
- **`bin/headless` stays.** It keeps its path, its `READY <url>` stdout contract, and its `STOPPED` line. No gate script's launch command changes.
- **Axum routers stay with their domain.** `packages`, `templates`, `manage`, and `checkpoint` each build their own `Router`. `crates/web-ui` is the two *page* modules only.
- **The CLI never self-boots.** A Tier-2/3 verb with no app running exits with an error naming `penpot daemon`. The data dir is exclusive — the spike measured a second stack dying on the postgres lock in ~2s.
- **Crate manifests follow the existing pattern:** `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`, and a `description` that states what the crate owns (see `crates/vault-index/Cargo.toml`).
- **Each task's dependency list is a starting point, not a contract.** It was derived by scanning the moved modules for `<crate>::` paths, which cannot tell a normal dependency from a dev-only one, and cannot see a crate reached only through a re-export. Add whatever the compiler demands, remove whatever it says is unused, and say in the report which lines you changed and why. Task 3 hit this first: its snippet omitted `chrono`, which `last_op.rs` needs for `Utc::now()`.
- **Chapter C port set (verified free against every existing gate):** proxy `9062`, backend `6522`, postgres `5597`, valkey `6540`, control `9063`.

---

## File Structure

**New crates** (each gets `Cargo.toml` + `src/lib.rs` declaring its modules; the module files are `git mv`'d unchanged):

| # | Path | Owns | Lines | Depends on |
|---|---|---|---|---|
| 1 | `crates/installer/` | `installer` | 127 | `penpot-rpc`, `sync-core` |
| 2 | `crates/app-state/` | `prefs`, `recent`, `last_op` | 613 | — |
| 3 | `crates/packages/` | `packages`, `templates` (+`templates.html`) | 3,422 | `installer` |
| 4 | `crates/vault-ops/` | `vault`, `checkpoint`, `gitinit`, `manage`, `docopen`, `reveal` | 2,662 | `installer` |
| 5 | `crates/web-ui/` | `home` (+`home.html`, `palette.html`), `navprobe` (+`navprobe.html`) | 359 | `vault-ops` |
| 6 | `crates/app-core/` | `lib.rs`→`boot.rs`, `control`, `layout`, `preflight`, `windows`, `status`, `prefs_http` (+`preferences.html`) | 4,188 | `app-state`, `packages`, `vault-ops`, `web-ui` |
| — | `apps/cli/` | `main.rs`, `attach.rs`, `verbs/` — the `penpot` binary | new | all of the above, `clap` |

**Stays in `apps/desktop`** (5,155 lines): `main.rs`, `menubar/`, `tray/`, `overlay.rs`, `dialog.rs`, `navwatch.rs`, plus `bin/headless.rs`.

The six crate line counts plus 5,155 sum to 16,526 — the current size of `apps/desktop/src`.

**Modified:** root `Cargo.toml` (members) · `apps/desktop/Cargo.toml` · `apps/desktop/src/lib.rs` · `apps/desktop/src/bin/headless.rs` · `scripts/d3-menus.sh` · `scripts/d5-documents.sh` · `scripts/n5-vaults.sh` · `justfile`

---

## Phase 1 — Spike the verbs

### Task 1: Verb spike (throwaway) — ✅ COMPLETE (commit `3e3254c`)

Deliverable: [`docs/spikes/cli-verbs.md`](../../spikes/cli-verbs.md). Verdict: crate map REVISED (see "What Task 1 changed"). Verb list: all ten proposed verbs survived, six were added, none dropped; the offline/online split turned out to be three tiers. Do not re-run this task.

---

## Phase 2 — Extract the crates

Every Phase 2 task has the same shape: record the green baseline, create the crate, move the files, repoint callers, verify, commit. These are moves, so a green suite before and after is exactly the right oracle.

### Task 2: `crates/installer`

**Files:**
- Create: `crates/installer/Cargo.toml`, `crates/installer/src/lib.rs`
- Move: `apps/desktop/src/installer.rs` → `crates/installer/src/lib.rs`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/packages.rs`, `apps/desktop/src/templates.rs`, `apps/desktop/src/manage.rs`, `apps/desktop/src/menubar/mod.rs`

**Interfaces:**
- Consumes: nothing (leaf crate).
- Produces: `installer::MAX_SETTLE_CYCLES: usize`; `installer::default_project_id(&PenpotClient, &str) -> anyhow::Result<String>`; `installer::import_binfile_and_settle(&PenpotClient, &str, &str, Vec<u8>, Option<u8>) -> anyhow::Result<(String, usize)>`; `installer::settle_to_fixpoint(&PenpotClient, &str, &str, &str) -> anyhow::Result<usize>`; `installer::first_page_id(&PenpotClient, &str) -> Option<String>`. All `async` except `MAX_SETTLE_CYCLES`.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS. Write down the test count — Step 5 must match it. The current count is 600.

- [ ] **Step 2: Create the crate**

`crates/installer/Cargo.toml`:

```toml
[package]
name = "installer"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local shared import-as-new + settle-until-fixpoint mechanism (PLAN3 E2). Templates and design-data packages both import through here: import-as-new mints a fresh file id, then export -> normalize -> in-place re-import cycles run until two consecutive exports share a semantic hash, so the first on-disk export the sync daemon writes is already a round-trip fixpoint."

[dependencies]
tokio = { workspace = true }
anyhow = { workspace = true }
penpot-rpc = { path = "../penpot-rpc" }
sync-core = { path = "../sync-core" }
tempfile = "3"
```

- [ ] **Step 3: Move the file**

```bash
mkdir -p crates/installer/src
git mv apps/desktop/src/installer.rs crates/installer/src/lib.rs
```

- [ ] **Step 4: Wire it up**

Add `"crates/installer",` to the `members` list in the root `Cargo.toml`. Add `installer = { path = "../../crates/installer" }` to `apps/desktop/Cargo.toml`. Delete the `pub mod installer;` line from `apps/desktop/src/lib.rs`. Then repoint every caller:

```bash
grep -rn "crate::installer" apps/desktop/src
```

Rewrite each hit from `crate::installer` to `installer`. There are callers in `packages.rs`, `templates.rs`, `manage.rs`, and `menubar/mod.rs` — the last is the one the module map does not predict.

- [ ] **Step 5: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS with the same count as Step 1. A *lower* count means a test module was lost in the move — investigate before continuing.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/installer from apps/desktop"
```

### Task 3: `crates/app-state`

**Files:**
- Create: `crates/app-state/Cargo.toml`, `crates/app-state/src/lib.rs`
- Move: `apps/desktop/src/{prefs,recent,last_op}.rs` → `crates/app-state/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/control.rs`, `apps/desktop/src/prefs_http.rs`, `apps/desktop/src/menubar/mod.rs`, `apps/desktop/src/menubar/model.rs`, `scripts/d3-menus.sh`

**Interfaces:**
- Consumes: nothing.
- Produces: `app_state::prefs`, `app_state::recent`, `app_state::last_op`, every currently-public item unchanged — including `app_state::prefs::load(&Path) -> Preferences`. `last_op` keeps its internal dependency on `prefs`, still spelled `crate::prefs` within `app-state`.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Create the crate**

`crates/app-state/Cargo.toml`:

```toml
[package]
name = "app-state"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local per-machine app state: the preferences store (D4), the Open Recent list (D3), and the last-finished-operation record (D4). All three are small JSON files in the app data dir, never in the vault — they describe this machine's session, not the user's designs."

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

`crates/app-state/src/lib.rs`:

```rust
//! Per-machine app state (chapter C): three JSON stores in the app data dir.
//!
//! Extracted verbatim from `apps/desktop`. These files describe this machine's
//! session — preferences, recently opened files, the last finished operation —
//! and never live inside the user's vault.

pub mod last_op;
pub mod prefs;
pub mod recent;
```

- [ ] **Step 3: Move the files**

```bash
mkdir -p crates/app-state/src
git mv apps/desktop/src/prefs.rs crates/app-state/src/prefs.rs
git mv apps/desktop/src/recent.rs crates/app-state/src/recent.rs
git mv apps/desktop/src/last_op.rs crates/app-state/src/last_op.rs
```

- [ ] **Step 4: Wire it up**

Add `"crates/app-state",` to the root `members`. Add `app-state = { path = "../../crates/app-state" }` to `apps/desktop/Cargo.toml`. Delete the three `pub mod` lines from `apps/desktop/src/lib.rs`. Rewrite `crate::prefs` → `app_state::prefs`, `crate::recent` → `app_state::recent`, `crate::last_op` → `app_state::last_op` in `control.rs`, `prefs_http.rs`, `menubar/mod.rs`, and `menubar/model.rs`. Inside `last_op.rs`, leave `crate::prefs` alone — it now correctly means `app-state`'s own `prefs`.

- [ ] **Step 5: Repoint the D3 gate**

`scripts/d3-menus.sh:298` runs one command for four module filters, and `recent::` now lives in `app-state`. Replace the single invocation with two, both appending to the same log the `REQUIRED_TESTS` grep reads:

```bash
(cd "$ROOT" && cargo test -p penpot-desktop -- menubar:: windows:: reveal::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
(cd "$ROOT" && cargo test -p app-state -- recent::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
```

The five `recent::tests::*` entries in `REQUIRED_TESTS` keep their exact names and need no edit.

- [ ] **Step 6: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
cargo test -p app-state -- recent:: 2>&1 | tail -8
```

Expected: both PASS, and the second lists `most_recent_first`, `reopening_moves_to_front_without_duplicating`, `the_list_is_capped`, and `a_missing_or_corrupt_store_is_an_empty_list_not_an_error` by name.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/app-state (prefs, recent, last_op)"
```

### Task 4: `crates/packages`

**Files:**
- Create: `crates/packages/Cargo.toml`, `crates/packages/src/lib.rs`
- Move: `apps/desktop/src/{packages,templates}.rs` and `apps/desktop/src/templates.html` → `crates/packages/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, every caller of `crate::packages` / `crate::templates`

**Interfaces:**
- Consumes: `installer::*` from Task 2.
- Produces: `packages::packages` and `packages::templates` with their existing public items, including each module's axum `Router` builder, `packages::read_manifest`, and `templates::enumerate_catalog`.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Create the crate**

`crates/packages/Cargo.toml`:

```toml
[package]
name = "packages"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local package home, lockfile, and offline template gallery (PLAN3 E2/E4, N6). A package is a folder or git repo under <vault>/.penpot-packages/ that the sync daemon is blind to; installing one imports through the shared installer crate and pins it in lock.json so a DB wipe re-applies it deterministically. Templates are package type 0."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
installer = { path = "../installer" }
penpot-rpc = { path = "../penpot-rpc" }
sync-core = { path = "../sync-core" }
sync-daemon = { path = "../sync-daemon" }
vault-index = { path = "../vault-index" }
axum = "0.8"
http = "1"
tempfile = "3"
```

`crates/packages/src/lib.rs`:

```rust
//! Package home, lockfile, and template gallery (chapter C extraction).
//!
//! Both modules import through the shared [`installer`] crate: a template is
//! package type 0, and an E2 design-data package is the same mechanism pointed
//! at a `.penpot` source tree instead of shipped binfile bytes.

pub mod packages;
pub mod templates;
```

- [ ] **Step 3: Move the files, including the compiled-in page**

`templates.rs:63` does `include_str!("templates.html")`, which resolves relative to the source file — the page must travel with it or the crate will not compile:

```bash
mkdir -p crates/packages/src
git mv apps/desktop/src/packages.rs crates/packages/src/packages.rs
git mv apps/desktop/src/templates.rs crates/packages/src/templates.rs
git mv apps/desktop/src/templates.html crates/packages/src/templates.html
```

- [ ] **Step 4: Wire it up**

Add `"crates/packages",` to the root `members`, add `packages = { path = "../../crates/packages" }` to `apps/desktop/Cargo.toml`, delete the two `pub mod` lines from `apps/desktop/src/lib.rs`, then repoint callers:

```bash
grep -rn "crate::packages\|crate::templates" apps/desktop/src
```

Rewrite each hit to `packages::packages` / `packages::templates`.

- [ ] **Step 5: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
cargo tree -p packages -e normal | grep -c 'tauri v'
```

Expected: PASS, and `0`.

- [ ] **Step 6: Run the live package gates**

```bash
bash scripts/e2-packages.sh
bash scripts/n6-templates.sh
```

Expected: both all-green. These exercise the moved code against a real stack, including the `templates.html` route — the unit suite alone would not catch a lost page asset.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/packages (packages, templates)"
```

### Task 5: `crates/vault-ops`

**Files:**
- Create: `crates/vault-ops/Cargo.toml`, `crates/vault-ops/src/lib.rs`
- Move: `apps/desktop/src/{vault,checkpoint,gitinit,manage,docopen,reveal}.rs` → `crates/vault-ops/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/control.rs`, `apps/desktop/src/home.rs`, `apps/desktop/src/tray/mod.rs`, `apps/desktop/src/menubar/mod.rs`, `scripts/d3-menus.sh`, `scripts/d5-documents.sh`

**Interfaces:**
- Consumes: `installer::*` from Task 2.
- Produces: `vault_ops::{vault, checkpoint, gitinit, manage, docopen, reveal}`. `vault::{SwitchMarker, VaultRef, VaultRegistry}`, `vault::VaultRegistry::load`, `vault::reset_disposable_state`, `docopen::resolve`, and `checkpoint::decide` stay public — Task 7's `control.rs` and Task 10's CLI both depend on them.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Create the crate**

`crates/vault-ops/Cargo.toml`:

```toml
[package]
name = "vault-ops"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local verbs over a vault: the switchable-vault registry and its crash-recovery decision logic (N5), the git-native checkpoint verb (N4b), the git-versioning helper (M5), the front-door create/rename/duplicate/move/delete mutations (D2), document-open path resolution (D5), and reveal-in-file-manager command construction (M5). Pure data plus decisions; the runtime orchestration that consumes them lives in app-core."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
installer = { path = "../installer" }
penpot-rpc = { path = "../penpot-rpc" }
sync-core = { path = "../sync-core" }
sync-daemon = { path = "../sync-daemon" }
axum = "0.8"
http = "1"
rand = "0.8"
chrono = { version = "0.4", default-features = false, features = ["std", "clock"] }

[dev-dependencies]
# Test-only in all five modules that use it (each first `tempfile::` sits after
# the module's `#[cfg(test)]` line), so it belongs here — the convention every
# sibling crate follows except `installer`, whose usage IS production code.
tempfile = "3"
```

`crates/vault-ops/src/lib.rs`:

```rust
//! Verbs over a vault (chapter C extraction).
//!
//! Everything here is data plus decisions: the registry and switch-marker
//! state machine, the checkpoint verb, the front-door mutations, path
//! resolution, and command construction. The runtime orchestration that
//! drives them — teardown, wipe, reconcile — lives in `app-core`'s `control`.

pub mod checkpoint;
pub mod docopen;
pub mod gitinit;
pub mod manage;
pub mod reveal;
pub mod vault;
```

- [ ] **Step 3: Move the files**

```bash
mkdir -p crates/vault-ops/src
for m in vault checkpoint gitinit manage docopen reveal; do
  git mv "apps/desktop/src/$m.rs" "crates/vault-ops/src/$m.rs"
done
```

- [ ] **Step 4: Wire it up**

Add `"crates/vault-ops",` to the root `members`, add `vault-ops = { path = "../../crates/vault-ops" }` to `apps/desktop/Cargo.toml`, delete the six `pub mod` lines from `apps/desktop/src/lib.rs`, then repoint callers:

```bash
grep -rn "crate::vault\b\|crate::checkpoint\|crate::gitinit\|crate::manage\|crate::docopen\|crate::reveal" apps/desktop/src
```

Rewrite each to `vault_ops::<module>`. One doc comment needs a manual fix: `vault.rs:4` links `[`crate::control`]`, which no longer resolves — change it to plain text ``` `app-core`'s `control` ``` so `cargo doc` stays clean.

- [ ] **Step 5: Repoint the D3 and D5 gates**

In `scripts/d3-menus.sh`, `reveal::` moves out of `penpot-desktop`. Building on Task 3's edit, the block becomes:

```bash
(cd "$ROOT" && cargo test -p penpot-desktop -- menubar:: windows::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
(cd "$ROOT" && cargo test -p app-state -- recent::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
(cd "$ROOT" && cargo test -p vault-ops -- reveal::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
```

In `scripts/d5-documents.sh:294`, change the package only:

```bash
(cd "$ROOT" && cargo test -p vault-ops -- docopen::) >"$CARGO_TEST_LOG" 2>&1
```

All fourteen `docopen::tests::*` names and `reveal::tests::macos_reveal_uses_open_dash_r_on_the_item_itself` stay exactly as written.

- [ ] **Step 6: Verify the pinned names still report by name**

```bash
cargo test -p vault-ops -- docopen:: 2>&1 | grep -c '\.\.\. ok$'
```

Expected: at least `14`. This is the check that matters — the D5 gate greps for these names, so a silently-renamed module path is caught here rather than after a stack boot.

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/vault-ops; repoint the D3/D5 test pins"
```

### Task 6: `crates/web-ui`

**Files:**
- Create: `crates/web-ui/Cargo.toml`, `crates/web-ui/src/lib.rs`
- Move: `apps/desktop/src/home.rs`, `home.html`, `palette.html`, `navprobe.rs`, `navprobe.html` → `crates/web-ui/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`

**Interfaces:**
- Consumes: `vault_ops::reveal` from Task 5.
- Produces: `web_ui::home` (its router builder and `build_strip_model`) and `web_ui::navprobe` (`navprobe::router()`, `navprobe::PROBE_PAGE_HTML`). Task 7's `boot` calls both.

This task runs **before** `app-core`, not after: `boot` mounts both of these routers, so `app-core` depends on `web-ui`.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Create the crate**

`crates/web-ui/Cargo.toml`:

```toml
[package]
name = "web-ui"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local's served HTML pages: the N3 lighttable home (/__home, thumbnails, quick-open palette, activity/conflict strip) and the D0 navigation probe page. Framework-free pages compiled in with include_str! and served through the proxy's extra routes — the app's second front-end, beside the native chrome and the CLI."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
vault-ops = { path = "../vault-ops" }
sync-daemon = { path = "../sync-daemon" }
vault-index = { path = "../vault-index" }
axum = "0.8"
http = "1"

[dev-dependencies]
tempfile = "3"
```

`crates/web-ui/src/lib.rs`:

```rust
//! The served HTML pages (chapter C extraction).
//!
//! These modules were already thin clients over an in-process API before the
//! CLI existed — that is why the CLI is the app's THIRD front-end, not a new
//! layer beneath the other two. `boot` (in `app-core`) mounts both routers,
//! which is why this crate is extracted BEFORE `app-core`.

pub mod home;
pub mod navprobe;
```

- [ ] **Step 3: Move the files with their compiled-in pages**

`home.rs:38-39` do `include_str!("home.html")` and `include_str!("palette.html")`; `navprobe.rs:13` does `include_str!("navprobe.html")`. All three pages must travel with their module:

```bash
mkdir -p crates/web-ui/src
git mv apps/desktop/src/home.rs crates/web-ui/src/home.rs
git mv apps/desktop/src/home.html crates/web-ui/src/home.html
git mv apps/desktop/src/palette.html crates/web-ui/src/palette.html
git mv apps/desktop/src/navprobe.rs crates/web-ui/src/navprobe.rs
git mv apps/desktop/src/navprobe.html crates/web-ui/src/navprobe.html
```

- [ ] **Step 4: Wire it up**

Add `"crates/web-ui",` to the root `members`, add `web-ui = { path = "../../crates/web-ui" }` to `apps/desktop/Cargo.toml`, delete `pub mod home;` and `pub mod navprobe;` from `apps/desktop/src/lib.rs`. Inside the moved `home.rs`, rewrite `crate::reveal::reveal` to `vault_ops::reveal::reveal`. Then repoint the two call sites in `apps/desktop/src/lib.rs` (still the crate root at this point):

- `lib.rs:1074` — `.merge(navprobe::router())` becomes `.merge(web_ui::navprobe::router())`
- every `home::` reference becomes `web_ui::home::`

- [ ] **Step 5: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
bash scripts/n3-home.sh
bash scripts/n4-palette.sh
```

Expected: all green. `n3-home` drives `/__home` and its thumbnails; `n4-palette` drives the quick-open palette — together they prove both compiled-in pages survived the move.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/web-ui (home, navprobe) with their compiled-in pages"
```

### Task 7: `crates/app-core`

**Files:**
- Create: `crates/app-core/Cargo.toml`, `crates/app-core/src/lib.rs`, `crates/app-core/src/boot.rs`
- Move: `apps/desktop/src/{control,layout,preflight,windows,status,prefs_http}.rs` and `preferences.html` → `crates/app-core/src/`; the boot body of `apps/desktop/src/lib.rs` → `crates/app-core/src/boot.rs`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/main.rs`, `apps/desktop/src/bin/headless.rs`, `apps/desktop/src/tray/mod.rs`, `apps/desktop/src/tray/model.rs`, `apps/desktop/src/menubar/mod.rs`, `apps/desktop/src/menubar/model.rs`, `scripts/d3-menus.sh`

**Interfaces:**
- Consumes: `app_state::*` (Task 3), `packages::*` (Task 4), `vault_ops::*` (Task 5), `web_ui::*` (Task 6).
- Produces: `app_core::AppConfig` (with `AppConfig::resolve() -> anyhow::Result<AppConfig>`, fields `data_dir: PathBuf`, `runtime_dir: PathBuf`), `app_core::RunningApp`, `app_core::boot`, `app_core::Credentials`, `app_core::control::{VaultRunner, RunnerSlot, boot_active_vault, serve_control, control_port_from_env, CONTROL_PORT_ENV}`, `app_core::windows::{OpenWindow, WindowRegistry}`, `app_core::status::{DaemonStatusBridge, ExportStatusBridge, MockStatusSource}`, `app_core::prefs_http::router`, `app_core::layout::*`, `app_core::preflight::*`.

This is the largest move. `apps/desktop/src/lib.rs` is the crate root: its module declarations stay behind while its boot body leaves.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Create the crate**

`crates/app-core/Cargo.toml`:

```toml
[package]
name = "app-core"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local boot sequence and live-stack ownership (M1, N5, D4): resolve config -> pin PENPOT_SECRET_KEY -> supervise postgres/valkey/backend -> provision the single user -> serve the proxy, plus the VaultRunner that owns the running stack across vault switches and reboots-in-place, the Preferences control surface, the sync/export status bridges, the packaged runtime layout resolver, the non-BMP boot pre-flight, and the Tauri-free window registry."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
app-state = { path = "../app-state" }
packages = { path = "../packages" }
vault-ops = { path = "../vault-ops" }
web-ui = { path = "../web-ui" }
supervisor = { path = "../supervisor" }
proxy = { path = "../proxy" }
penpot-rpc = { path = "../penpot-rpc" }
sync-core = { path = "../sync-core" }
sync-daemon = { path = "../sync-daemon" }
board-export = { path = "../board-export" }
vault-index = { path = "../vault-index" }
axum = "0.8"
http = "1"
directories = "5"
rand = "0.8"
chrono = { version = "0.4", default-features = false, features = ["std", "clock"] }

[dev-dependencies]
tempfile = "3"
tower = { version = "0.5", features = ["util"] }
```

- [ ] **Step 3: Split `apps/desktop/src/lib.rs`**

Move everything below the `pub mod` block — `AppConfig`, `Credentials`, `RunningApp`, `boot`, the bootstrap route, `resolve_exporter_layout`, and the `#[cfg(test)] mod tests` — into `crates/app-core/src/boot.rs`. Then create `crates/app-core/src/lib.rs`:

```rust
//! Penpot Local core boot sequence (M1), extracted in chapter C.
//!
//! Shared by every front-end: the Tauri shell, the headless daemon, and the
//! CLI. Resolve configuration -> load-or-generate the pinned
//! `PENPOT_SECRET_KEY` -> start the supervisor (postgres -> valkey -> backend
//! JVM) -> first-boot single-user provisioning over RPC -> start the local
//! proxy with the `/__bootstrap` auto-login route -> expose readiness.
//!
//! `control` and `boot` are mutually recursive — `control::boot_active_vault`
//! calls `boot`, and `boot` takes a `control::RunnerSlot` — so they cannot be
//! separated without introducing a trait. They are one crate on purpose.

mod boot;

pub mod control;
pub mod layout;
pub mod preflight;
pub mod prefs_http;
pub mod status;
pub mod windows;

pub use boot::*;
```

`pub use boot::*;` keeps `app_core::AppConfig`, `app_core::RunningApp`, and `app_core::boot` at the paths the rest of the code already spells.

- [ ] **Step 4: Move the six modules and the Preferences page**

`prefs_http.rs:82` does `include_str!("preferences.html")`:

```bash
for m in control layout preflight windows status prefs_http; do
  git mv "apps/desktop/src/$m.rs" "crates/app-core/src/$m.rs"
done
git mv apps/desktop/src/preferences.html crates/app-core/src/preferences.html
```

- [ ] **Step 5: Wire it up**

Add `"crates/app-core",` to the root `members` and `app-core = { path = "../../crates/app-core" }` to `apps/desktop/Cargo.toml`.

Reduce `apps/desktop/src/lib.rs` to the GUI module declarations plus a re-export, so `main.rs` and the two integration tests under `apps/desktop/tests/` keep compiling unchanged:

```rust
//! Penpot Local desktop shell: the Tauri-facing half of the app.
//!
//! The boot sequence, the vault runner, and every GUI-free verb now live in
//! `app-core` and its sibling crates (chapter C). What remains here is the
//! native chrome — menus, tray, windows, overlay — plus its own pure model
//! layer, and the navigation policy that keeps the SPA inside the app.

pub mod dialog;
pub mod menubar;
pub mod navwatch;
pub mod overlay;
pub mod tray;

pub use app_core::{
    boot, control, layout, preflight, prefs_http, status, windows, AppConfig, RunningApp,
};
```

In `apps/desktop/src/bin/headless.rs`, change the import to `use app_core::{control, AppConfig};`.

Finally, close a broken doc link Task 5 left behind. `crates/vault-ops/src/vault.rs:356` links `` [`crate::AppConfig::storage_dir`] ``, which stopped resolving when `vault-ops` was extracted — Task 5 correctly left it alone, because the correct target did not exist yet. It does now: change it to `` [`app_core::AppConfig::storage_dir`] `` and add `app-core` to `crates/vault-ops/Cargo.toml`'s `[dev-dependencies]` **only if** rustdoc needs it to resolve. If resolving the link would require a normal dependency from `vault-ops` on `app-core`, do **not** add one — that is the cycle this whole chapter is built to avoid. Downgrade the link to plain text instead, exactly as Task 5 did for `vault.rs:4`, and say so in your report.

Verify with:

```bash
cargo doc -p vault-ops --no-deps 2>&1 | grep -c "unresolved link"
```

Expected: `0`.

- [ ] **Step 6: Repoint the D3 gate one last time**

`windows::` now lives in `app-core`:

```bash
(cd "$ROOT" && cargo test -p penpot-desktop -- menubar::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
(cd "$ROOT" && cargo test -p app-core -- windows::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
(cd "$ROOT" && cargo test -p app-state -- recent::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
(cd "$ROOT" && cargo test -p vault-ops -- reveal::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
```

- [ ] **Step 7: Verify the boot path end to end**

```bash
cargo test --workspace 2>&1 | tail -5
bash scripts/m1-smoke.sh
bash scripts/m2-invariant.sh
bash scripts/n5-vaults.sh
bash scripts/d4-preferences.sh
```

Expected: all green. `m1-smoke` proves the moved boot sequence still boots; `m2-invariant` is the core invariant; `n5-vaults` proves the moved `VaultRunner` still switches vaults with zero spill; `d4-preferences` proves `prefs_http` and its page survived.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/app-core (boot, control, layout, preflight, windows, status, prefs_http)"
```

### Task 8: Prove the extraction, and lock it

**Files:**
- Create: `scripts/c1-crates.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: every crate from Tasks 2–7.
- Produces: `just c1`.

This is the task that makes the extraction stick. Without it, the first convenient `use tauri::` puts the coupling back, spread across more files than before.

- [ ] **Step 1: Write the gate**

`scripts/c1-crates.sh`:

```bash
#!/usr/bin/env bash
# C1 extraction gate (chapter C). A FAST, PURE, STACK-FREE gate proving the
# library extraction is real rather than nominal:
#   (a) NO extracted crate depends on tauri, directly or transitively. Without
#       this the extraction regresses silently at the first convenient
#       `use tauri::`, leaving the old coupling spread over more files.
#   (b) Every extracted crate's tests RUN and pass in their new home, named by
#       crate — so a module that quietly failed to move cannot hide behind a
#       workspace-wide green.
#   (c) apps/desktop keeps only the GUI: its src/ holds no module the crate map
#       assigned elsewhere, and no orphaned HTML page whose module has left.
# No ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

EXTRACTED=(installer app-state packages vault-ops web-ui app-core)

echo "-- (a) no extracted crate depends on tauri"
for c in "${EXTRACTED[@]}"; do
    if (cd "$ROOT" && cargo tree -p "$c" -e normal 2>/dev/null | grep -q 'tauri v'); then
        fail "(a) $c depends on tauri — the extraction has regressed"
    else
        pass "(a) $c is tauri-free"
    fi
done

echo "-- (b) every extracted crate's tests pass in their new home"
for c in "${EXTRACTED[@]}"; do
    if (cd "$ROOT" && cargo test -p "$c") >"/tmp/c1-$c.log" 2>&1; then
        pass "(b) cargo test -p $c"
    else
        fail "(b) cargo test -p $c failed — see /tmp/c1-$c.log"
    fi
done

echo "-- (c) apps/desktop holds only GUI modules"
MOVED=(installer prefs recent last_op packages templates vault checkpoint
       gitinit manage docopen reveal home navprobe control layout preflight
       windows status prefs_http)
for m in "${MOVED[@]}"; do
    if [ -f "$ROOT/apps/desktop/src/$m.rs" ]; then
        fail "(c) apps/desktop/src/$m.rs still exists — it belongs in a crate"
    else
        pass "(c) $m.rs is out of apps/desktop"
    fi
done
for p in home palette navprobe preferences templates; do
    if [ -f "$ROOT/apps/desktop/src/$p.html" ]; then
        fail "(c) apps/desktop/src/$p.html is orphaned — its module moved without it"
    else
        pass "(c) $p.html moved with its module"
    fi
done

echo
echo "C1: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
```

- [ ] **Step 2: Run it and confirm it passes**

```bash
chmod +x scripts/c1-crates.sh && bash scripts/c1-crates.sh
```

Expected: `C1: 37 passed, 0 failed`.

- [ ] **Step 3: Prove the gate can fail**

Temporarily add `tauri = "2"` to `crates/app-state/Cargo.toml`, re-run, and confirm leg (a) reports FAIL for `app-state`. Then revert. A gate never seen red is not a gate.

```bash
bash scripts/c1-crates.sh; git checkout crates/app-state/Cargo.toml
```

Expected: the run reports `FAIL  (a) app-state depends on tauri`, then the revert restores green.

- [ ] **Step 4: Wire it into the justfile**

```make
# C1 extraction gate (chapter C): the six extracted crates are tauri-free,
# their tests pass in their new homes, and apps/desktop holds only GUI
# modules with no orphaned page assets. Fast, pure, no stack, no ports.
# Chained into `just e2e`.
c1:
    bash scripts/c1-crates.sh
```

Add `bash scripts/c1-crates.sh` to the `e2e` chain, immediately before `bash scripts/d1-offline.sh`.

- [ ] **Step 5: Repair the references the extraction invalidated**

Six tasks of moves left prose across the repo pointing at files that no longer exist. A controller sweep found every one and confirmed **none is an executed path** — the only real path assignment, `scripts/d6-residue.sh:63`'s `NAVWATCH_RS`, targets `navwatch.rs`, which stays in `apps/desktop`. So no gate is broken; what is broken is the documentation someone greps when deciding whether a gate still covers what they need. In this repo that documentation is load-bearing, so fix all of it:

| File:line | Says | Should say |
|---|---|---|
| `justfile:322` | `cargo test -p penpot-desktop -- docopen::` | `-p vault-ops` |
| `crates/sync-core/src/trash.rs:93` | `apps/desktop/src/home.rs::is_safe_vault_rel` | `crates/web-ui/src/home.rs::is_safe_vault_rel` |
| `crates/sync-daemon/src/paths.rs:32` | `apps/desktop/src/manage.rs` | `crates/vault-ops/src/manage.rs` |
| `scripts/e2_packages_helper.py:57` | `apps/desktop/src/installer.rs` | `crates/installer/src/lib.rs` |
| `scripts/d4-preferences.sh:108` | `apps/desktop/src/last_op.rs` | `crates/app-state/src/last_op.rs` |
| `scripts/d4_prefs_helper.py:4,35` | `apps/desktop/src/prefs_http.rs` | `crates/app-core/src/prefs_http.rs` |
| `scripts/d4_prefs_helper.py:45,241` | `apps/desktop/src/last_op.rs` | `crates/app-state/src/last_op.rs` |
| `scripts/designs-git-init.sh:23` | `apps/desktop/src/gitinit.rs` | `crates/vault-ops/src/gitinit.rs` |
| `scripts/d5_docs_helper.py:19` | `apps/desktop/src/control.rs` | `crates/app-core/src/control.rs` |
| `scripts/d0-navigation-spike.sh:6` | `apps/desktop/src/navprobe.{rs,html}` | `crates/web-ui/src/navprobe.{rs,html}` |
| `scripts/d6-residue.sh:148` | `apps/desktop/src/navprobe.html` | `crates/web-ui/src/navprobe.html` |

Leave every reference to `navwatch.rs`, `main.rs`, `menubar/`, `tray/`, `overlay.rs` and `dialog.rs` alone — those modules never moved.

Then verify nothing was missed, and note the trap: a compound `grep` with `--include` filters returned a false empty during the controller's first sweep. Use a positive control — confirm the command finds a reference you know exists before trusting it to report none.

```bash
grep -rn "apps/desktop/src" crates/ scripts/ justfile README.md | grep -vE "navwatch|main\.rs|menubar|tray|overlay|dialog"
```

Expected: no output. Sanity-check the command first by dropping the second `grep` and confirming it prints the surviving `navwatch` lines.

- [ ] **Step 6: Run the headless part of the ladder**

```bash
bash scripts/c1-crates.sh && bash scripts/m1-smoke.sh && bash scripts/m2-invariant.sh && bash scripts/m3-sync.sh && bash scripts/n1-index.sh && bash scripts/n3-home.sh && bash scripts/n4-palette.sh && bash scripts/n5-vaults.sh && bash scripts/n6-templates.sh && bash scripts/e1-contract.sh && bash scripts/e2-packages.sh && bash scripts/e3-library.sh && bash scripts/e4-gallery.sh && bash scripts/d4-preferences.sh && bash scripts/d6-residue.sh
```

Expected: all green. Report the result; the controller runs the GUI-session gates (`d2-home.sh`, `d3-menus.sh`, `d5-documents.sh`) separately — do not attempt them.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "test(c1): gate the extraction — tauri-free crates, tests in their new homes"
```

---

## Phase 3 — Build the CLI

### Task 9: Publish the runtime record and authenticate the control port

**Files:**
- Create: `crates/app-core/src/runtime_record.rs`
- Modify: `crates/app-core/src/control.rs`, `crates/app-core/src/lib.rs`, `crates/app-core/src/boot.rs`, `apps/desktop/src/main.rs`, `apps/desktop/src/bin/headless.rs`, `scripts/n5-vaults.sh`, `scripts/d5-documents.sh`

**Interfaces:**
- Consumes: `app_core::control::serve_control`.
- Produces: `app_core::runtime_record::{RuntimeRecord, write_record, read_record, remove_record, require_token, RECORD_FILE_NAME, TOKEN_HEADER}`. `RuntimeRecord { pid: u32, proxy_port: u16, control_port: u16, token: String }` derives `Serialize`, `Deserialize`, `Clone`, `Debug`. `write_record(data_dir: &Path, record: &RuntimeRecord) -> anyhow::Result<()>`; `read_record(data_dir: &Path) -> anyhow::Result<RuntimeRecord>`; `remove_record(data_dir: &Path)`. `RECORD_FILE_NAME = "runtime.json"`; `TOKEN_HEADER = "x-penpot-control-token"`.

This is the only new behaviour in the chapter, and the spike proved both halves are required. The CLI cannot find a running app today: nothing in the data dir records a pid or a port, and the control server starts only when `PENPOT_LOCAL_CONTROL_PORT` is set, which no normal install sets. Making it unconditional then exposes `POST /open` — a DB wipe plus a full reconcile — to any web page, because the same-origin policy blocks reading a cross-origin response, not sending the request.

- [ ] **Step 1: Write the failing tests**

`crates/app-core/src/runtime_record.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn guarded(token: &str) -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .layer(axum::middleware::from_fn_with_state(
                token.to_string(),
                require_token,
            ))
            .with_state(token.to_string())
    }

    async fn status_of(req: Request<Body>) -> StatusCode {
        guarded("s3cret").oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn a_request_without_a_token_is_refused() {
        let req = Request::builder().uri("/ping").body(Body::empty()).unwrap();
        assert_eq!(status_of(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_wrong_token_is_refused() {
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_right_token_passes() {
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_browser_request_is_refused_even_with_a_valid_token() {
        // The attack this exists for: a web page CAN send a cross-origin POST
        // to 127.0.0.1. It must never reach POST /open, which wipes the DB.
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_origin_header_is_refused_even_with_a_valid_token() {
        // Older browsers send Origin without Sec-Fetch-Site. A real CLI never
        // sends Origin at all, so its presence alone is disqualifying.
        let req = Request::builder()
            .uri("/ping")
            .header(TOKEN_HEADER, "s3cret")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(req).await, StatusCode::FORBIDDEN);
    }

    fn sample() -> RuntimeRecord {
        RuntimeRecord {
            pid: 4242,
            proxy_port: 9062,
            control_port: 9063,
            token: "s3cret".to_string(),
        }
    }

    #[test]
    fn the_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        let read = read_record(dir.path()).unwrap();
        assert_eq!(read.pid, 4242);
        assert_eq!(read.proxy_port, 9062);
        assert_eq!(read.control_port, 9063);
        assert_eq!(read.token, "s3cret");
    }

    #[test]
    fn reading_a_missing_record_names_the_daemon_verb() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_record(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("penpot daemon"),
            "a missing record must tell the user how to fix it, got: {err}"
        );
    }

    #[test]
    fn removing_the_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        remove_record(dir.path());
        remove_record(dir.path());
        assert!(read_record(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &sample()).unwrap();
        let mode = std::fs::metadata(dir.path().join(RECORD_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "runtime.json must be 0600, got {mode:o}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p app-core -- runtime_record::
```

Expected: FAIL — the module does not compile, because nothing above the test block is defined yet.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/app-core/src/runtime_record.rs`:

```rust
//! The runtime record and control-port auth (chapter C).
//!
//! Before chapter C a CLI had no way to find a running app: nothing in the
//! data dir recorded a pid or a port, and the control server started only when
//! `PENPOT_LOCAL_CONTROL_PORT` was set — which no normal install sets, so
//! `/open`, `/active`, `/list` and `/windows` did not exist at all.
//!
//! `runtime.json` fixes discovery, and the control server becomes
//! unconditional. That changes the threat model: ANY web page can send
//! `POST http://127.0.0.1:<port>/open` cross-origin. The same-origin policy
//! blocks reading the response, not sending the request, and `POST /open`
//! wipes the disposable DB and reconciles from scratch.
//!
//! Two independent defences, both required:
//!  1. a random per-boot token, written to a `0600` `runtime.json`, demanded
//!     on every request;
//!  2. a browser check — a real CLI never sends `Origin` or `Sec-Fetch-Site`,
//!     so either header's presence disqualifies the request outright.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};

/// Name of the discovery file the daemon writes into the app data dir.
pub const RECORD_FILE_NAME: &str = "runtime.json";

/// Header carrying the per-boot control token.
pub const TOKEN_HEADER: &str = "x-penpot-control-token";

/// What a running instance publishes so a CLI invocation can find it,
/// authenticate to it, and tell whether it is still alive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub pid: u32,
    pub proxy_port: u16,
    pub control_port: u16,
    pub token: String,
}

fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RECORD_FILE_NAME)
}

/// Write `runtime.json` with owner-only permissions.
pub fn write_record(data_dir: &Path, record: &RuntimeRecord) -> Result<()> {
    let path = record_path(data_dir);
    std::fs::write(&path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod {}", path.display()))?;
    }
    Ok(())
}

/// Read `runtime.json`. The error names the fix, because "no instance running"
/// is the CLI's most common failure and it must never read as a bug.
pub fn read_record(data_dir: &Path) -> Result<RuntimeRecord> {
    let path = record_path(data_dir);
    let body = std::fs::read(&path).with_context(|| {
        format!(
            "no running Penpot Local instance ({} absent) — start one with `penpot daemon`",
            path.display()
        )
    })?;
    serde_json::from_slice(&body).with_context(|| format!("malformed {}", path.display()))
}

/// Remove the record on shutdown. Idempotent and never fatal: a stale record
/// is a nuisance, but failing a clean shutdown over one would be worse.
pub fn remove_record(data_dir: &Path) {
    let _ = std::fs::remove_file(record_path(data_dir));
}

/// Reject unauthenticated requests, and browser-initiated ones outright.
pub async fn require_token(
    State(expected): State<String>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = req.headers();
    if headers.contains_key("origin") || headers.contains_key("sec-fetch-site") {
        return Err(StatusCode::FORBIDDEN);
    }
    let presented = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if presented.is_empty() || presented != expected {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}
```

Add `pub mod runtime_record;` to `crates/app-core/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -p app-core -- runtime_record::
```

Expected: PASS, 9 tests.

- [ ] **Step 5: Make the control server unconditional and apply the layer**

In `crates/app-core/src/control.rs`, change `serve_control` to take the token and the data dir, apply the middleware, and publish the record:

```rust
pub async fn serve_control(
    runner: Arc<VaultRunner>,
    windows: WindowRegistry,
    port: u16,
    token: String,
    data_dir: &Path,
    proxy_port: u16,
) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("control server cannot bind {addr}"))?;
    let bound = listener.local_addr()?.port();
    crate::runtime_record::write_record(
        data_dir,
        &crate::runtime_record::RuntimeRecord {
            pid: std::process::id(),
            proxy_port,
            control_port: bound,
            token: token.clone(),
        },
    )?;
    tracing::info!(%addr, "vault control server listening (token-authenticated)");
    let router = control_router(runner, windows).layer(
        axum::middleware::from_fn_with_state(token, crate::runtime_record::require_token),
    );
    axum::serve(listener, router).await.context("control server error")?;
    Ok(())
}
```

Then, at both call sites (`apps/desktop/src/main.rs` and `apps/desktop/src/bin/headless.rs`), start the server **always**, not only when the env var is set: keep `control_port_from_env()` as the port override and fall back to `0` (let the OS choose, then publish the bound port). Generate the token with `rand`. Call `remove_record(&config.data_dir)` on the shutdown path in both binaries.

- [ ] **Step 6: Repoint the gates that drive the control port**

`scripts/n5-vaults.sh` and `scripts/d5-documents.sh` call `POST /open`, `GET /active`, `GET /list` and `GET /windows`, and will now get 401 without the header. Update each to read the token from `"$DATA_DIR/runtime.json"` and pass `-H "x-penpot-control-token: $TOKEN"` on every control-port request.

- [ ] **Step 7: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
bash scripts/n5-vaults.sh
```

Expected: both green. If `n5-vaults` fails with 401, a request is missing the header — that is the gate working. `d5-documents.sh` needs a GUI session: report that it was not run, and the controller will run it.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(c3): publish runtime.json, make the control port unconditional and token-authenticated"
```

### Task 10: The `penpot` binary — Tier-1 verbs

**Files:**
- Create: `apps/cli/Cargo.toml`, `apps/cli/src/main.rs`, `apps/cli/src/lib.rs`, `apps/cli/src/attach.rs`, `apps/cli/src/verbs/mod.rs`, `apps/cli/src/verbs/tier1.rs`
- Modify: `Cargo.toml`, and whichever library items Step 5 finds are private

**Interfaces:**
- Consumes: `app_core::{AppConfig, runtime_record}`, `vault_ops::{vault, checkpoint, docopen}`, `packages::{packages, templates}`, `app_state::prefs`.
- Produces: the `penpot` binary and the `penpot_cli` library target. `attach::Attached { base: String, token: String }`, `attach::attach(data_dir: &Path) -> anyhow::Result<Attached>`, and `Attached::request(reqwest::Method, &str) -> reqwest::RequestBuilder` — Task 11 builds on all three.

Tier 1 is the spike's "pure offline" set: it reads the tree and the data dir with no app and no backend running.

- [ ] **Step 1: Write the failing test**

`apps/cli/src/attach.rs`:

```rust
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
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p penpot-cli -- attach::
```

Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Create the crate**

`apps/cli/Cargo.toml`:

```toml
[package]
name = "penpot-cli"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local's command line: the app's third front-end, beside the native chrome and the served HTML pages. INTERNAL — no stability contract, no public surface; it may change in any chapter."

[lib]
name = "penpot_cli"
path = "src/lib.rs"

[[bin]]
name = "penpot"
path = "src/main.rs"

[dependencies]
tokio = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }
reqwest = { version = "0.12", default-features = false, features = ["json"] }
app-core = { path = "../../crates/app-core" }
app-state = { path = "../../crates/app-state" }
vault-ops = { path = "../../crates/vault-ops" }
packages = { path = "../../crates/packages" }

[dev-dependencies]
tempfile = "3"
```

Add `"apps/cli",` to the root `Cargo.toml` `members`.

- [ ] **Step 4: Implement attach**

Above the tests in `apps/cli/src/attach.rs`:

```rust
//! Attach-or-fail: find a running daemon, or explain how to start one.
//!
//! The CLI never boots a stack implicitly. The spike measured a second stack
//! dying on the postgres lock in ~2 s — the data dir is exclusive — and a warm
//! self-boot costs ~11 s. Booting from a verb would therefore either race the
//! running app or stall an interactive command; either way the core invariant
//! assumes ONE aggressive reconciliation at boot, not two concurrent ones.

use std::path::Path;

use anyhow::Result;

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

pub fn attach(data_dir: &Path) -> Result<Attached> {
    let record = app_core::runtime_record::read_record(data_dir)?;
    Ok(Attached {
        base: format!("http://127.0.0.1:{}", record.control_port),
        token: record.token,
    })
}
```

`apps/cli/src/lib.rs`:

```rust
//! `penpot` — the CLI front-end (chapter C).
//!
//! INTERNAL surface: no stability contract, no versioned API, no end-user
//! documentation. Verbs fall in three tiers (see docs/spikes/cli-verbs.md):
//! Tier 1 runs in-process against the tree and data dir with nothing else
//! running; Tiers 2 and 3 need the running app and go over loopback, never by
//! linking the library — the spike proved an in-process caller becomes an
//! undetected concurrent writer to app-owned state.

pub mod attach;
pub mod verbs;
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cargo test -p penpot-cli -- attach::
```

Expected: PASS, 3 tests.

- [ ] **Step 6: Implement the Tier-1 verbs**

`apps/cli/src/verbs/tier1.rs` implements each verb as a `pub fn` returning `anyhow::Result<serde_json::Value>`, so `main.rs` prints JSON on stdout and human text on stderr — the Task 12 gate asserts on stdout without parsing prose.

The set, from the spike's Tier 1: `vault list` (`vault_ops::vault::VaultRegistry::load`), `checkpoint` (`vault_ops::checkpoint::decide` plus its runner), `open <path>` resolution only (`vault_ops::docopen::resolve`), `tpl list` (`packages::templates::enumerate_catalog`), `prefs get` (`app_state::prefs::load`), `pkg list` on the lockfile subset (`packages::packages::read_manifest` and the lockfile reader), and `sync status` on the manifest subset (`sync_core`'s manifest).

Two of these lose fields when run offline, and the output must say so rather than imply completeness: `pkg list` has the pins but not `live` (whether the pinned `fileId` is still in the DB — the DB-wipe witness), and `sync status` has `lastSyncedHash`/`lastSyncedAt` but not `paused`, `lastError`, or the per-file `state`/`conflict` classification, which exist only in the daemon's in-memory snapshot. Emit `"partial": true` with a `"missing"` array naming the absent fields.

Most of these readers are already `pub`. Where one is not, make it `pub` in its owning crate as part of this task and say so in the report — do not work around it by duplicating logic in the CLI.

`apps/cli/src/main.rs`:

```rust
mod attach;
mod verbs;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "penpot", about = "Penpot Local (internal CLI)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the known vaults and mark the active one (Tier 1).
    VaultList,
    /// Commit the active vault's current state (Tier 1).
    Checkpoint,
    /// Resolve what a path would open, without opening it (Tier 1).
    Open { path: String },
    /// List the built-in template catalog (Tier 1).
    TplList,
    /// Print the preferences file (Tier 1).
    PrefsGet,
    /// List installed packages from lock.json (Tier 1, partial).
    PkgList,
    /// Print per-file sync state from the manifest (Tier 1, partial).
    SyncStatus,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let config = app_core::AppConfig::resolve()?;
    let out = match cli.command {
        Command::VaultList => verbs::tier1::vault_list(&config)?,
        Command::Checkpoint => verbs::tier1::checkpoint(&config)?,
        Command::Open { path } => verbs::tier1::open(&config, &path)?,
        Command::TplList => verbs::tier1::tpl_list(&config)?,
        Command::PrefsGet => verbs::tier1::prefs_get(&config)?,
        Command::PkgList => verbs::tier1::pkg_list(&config)?,
        Command::SyncStatus => verbs::tier1::sync_status(&config)?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
```

- [ ] **Step 7: Verify the verbs run with no stack**

With no Penpot process running:

```bash
cargo run -p penpot-cli -- vault-list
cargo run -p penpot-cli -- pkg-list
cargo run -p penpot-cli -- tpl-list
cargo run -p penpot-cli -- sync-status
```

Expected: each prints JSON and exits 0. That is the whole point of the tier split.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(c3): add the penpot CLI with its Tier-1 verbs"
```

### Task 11: Tier-2/3 verbs and `penpot daemon`

**Files:**
- Create: `apps/cli/src/verbs/daemon.rs`, `apps/cli/src/verbs/loopback.rs`
- Modify: `apps/cli/src/main.rs`, `apps/cli/src/verbs/mod.rs`, `apps/desktop/src/bin/headless.rs`, `apps/desktop/Cargo.toml`

**Interfaces:**
- Consumes: `attach::{Attached, attach}` from Task 10; `app_core::{boot, control, runtime_record}`.
- Produces: `penpot_cli::verbs::daemon::run() -> anyhow::Result<()>`, which prints `READY <url>` on stdout exactly as `bin/headless` does today, and `STOPPED` on clean exit.

- [ ] **Step 1: Write the failing test**

In `apps/cli/src/verbs/loopback.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier2_verb_with_no_instance_fails_naming_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let err = route_for(dir.path(), Verb::SyncStatus).unwrap_err().to_string();
        assert!(err.contains("penpot daemon"), "got: {err}");
    }

    #[test]
    fn each_verb_maps_to_its_own_route() {
        assert_eq!(Verb::SyncStatus.route(), "/__api/prefs");
        assert_eq!(Verb::VaultSwitch.route(), "/open");
        assert_ne!(Verb::SyncStatus.route(), Verb::VaultSwitch.route());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p penpot-cli -- loopback::
```

Expected: FAIL — `Verb` and `route_for` are undefined.

- [ ] **Step 3: Implement the loopback verbs**

`Verb` is an enum with a `route()` method; `route_for(data_dir, verb)` attaches and returns the built request, so the "no instance" error surfaces before any network call. Wire the Tier-2/3 verbs the spike named: `sync status` (live), `sync pause`/`resume`, `prefs get`/`set`, `pkg add`, `boards`, `search`, `reveal`, `export`, `new --from-template`, `pkg install`, `vault switch`, `reboot`.

Record in the module doc the spike's RC-5 finding: `sync pause` over loopback persists the preference and does **not** wait for the daemon's idle barrier, so a `pause` that returns is not proof the daemon is quiescent.

- [ ] **Step 4: Run it to verify it passes**

```bash
cargo test -p penpot-cli -- loopback::
```

Expected: PASS.

- [ ] **Step 5: Add `penpot daemon`**

`apps/cli/src/verbs/daemon.rs` holds the body currently in `apps/desktop/src/bin/headless.rs` — the boot, the `READY <url>` line, the SIGTERM/SIGINT loop, the SIGUSR1 sync toggle, the `STOPPED` line — lifted verbatim, plus the `remove_record` shutdown call Task 9 added. Add `Daemon` to the `Command` enum.

- [ ] **Step 6: Reduce `bin/headless` to a shim**

```rust
//! Compatibility shim (chapter C): `penpot daemon` under the old name.
//!
//! Every milestone gate launches `--bin headless` and waits for `READY <url>`
//! on stdout. Those suites are the net that protected the whole chapter-C
//! extraction, so the name outlives the refactor. Removing this shim is a
//! one-line change in a later chapter.

fn main() -> anyhow::Result<()> {
    penpot_cli::verbs::daemon::run()
}
```

Add `penpot-cli = { path = "../cli" }` to `apps/desktop/Cargo.toml`.

- [ ] **Step 7: Verify the shim kept the contract**

```bash
bash scripts/m1-smoke.sh
bash scripts/m3-sync.sh
```

Expected: both green. `m1-smoke` asserts the `READY`/`STOPPED` contract and clean shutdown; `m3-sync` drives the SIGUSR1 pause hook. A failure here means the verbatim lift dropped something.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(c3): penpot daemon + Tier-2/3 loopback verbs; headless becomes a shim"
```

### Task 12: The C2 gate and the chapter close

**Files:**
- Create: `scripts/c2-cli.sh`
- Modify: `justfile`, `README.md`, `docs/superpowers/specs/2026-07-26-penpot-cli-design.md`

**Interfaces:**
- Consumes: everything above.
- Produces: `just c2`.

- [ ] **Step 1: Write the gate**

`scripts/c2-cli.sh`, following `scripts/n5-vaults.sh`'s structure for booting a stack on dedicated ports and its `pass`/`fail` helpers:

```bash
#!/usr/bin/env bash
# C2 CLI gate (chapter C). A LIVE gate over the third front-end:
#   (a) TIER-1 verbs run with NO stack — the assert that the tier split is real
#       rather than nominal — and the partial ones declare what they lack
#       ("partial": true + a "missing" array), so an offline answer can never be
#       mistaken for a complete one.
#   (b) A TIER-2/3 verb with no daemon running fails and NAMES `penpot daemon`.
#       Attach-or-fail never self-boots: the data dir is exclusive (a second
#       stack dies on the postgres lock in ~2s) and a warm self-boot costs ~11s.
#   (c) With a daemon up, the same verb succeeds, and runtime.json carries a
#       live pid, the proxy port, the control port and a token.
#   (d) The control port REFUSES a tokenless request (401), a wrong-token
#       request (401), and a browser-shaped one carrying a VALID token
#       (403, Sec-Fetch-Site) — POST /open wipes the DB, and any web page can
#       send a cross-origin POST.
#   (e) runtime.json is REMOVED on clean shutdown, so a stale record never
#       makes the CLI attach to a dead port.
# Dedicated ports: proxy 9062, backend 6522, postgres 5597, valkey 6540,
# control 9063. Chained into `just e2e` — chapter C lands product code.
set -uo pipefail
```

- [ ] **Step 2: Run it**

```bash
chmod +x scripts/c2-cli.sh && bash scripts/c2-cli.sh
```

Expected: all legs pass.

- [ ] **Step 3: Prove leg (d) can fail**

Temporarily remove the `.layer(...)` line from `serve_control`, re-run, confirm leg (d) reports FAIL, then revert.

```bash
bash scripts/c2-cli.sh; git checkout crates/app-core/src/control.rs
```

Expected: a FAIL for the tokenless request, then green after the revert.

- [ ] **Step 4: Wire into the justfile**

```make
# C2 CLI gate (chapter C): Tier-1 verbs run with no stack and declare what
# they lack; a Tier-2/3 verb with no daemon fails naming `penpot daemon`
# (never self-boots — the data dir is exclusive); with a daemon it succeeds
# and runtime.json is well-formed; the control port refuses tokenless,
# wrong-token AND browser-shaped requests (POST /open wipes the DB); and the
# record is removed on clean shutdown. Dedicated ports 9062/6522/5597/6540
# (control 9063). Chained into `just e2e`.
c2:
    bash scripts/c2-cli.sh
```

Add `bash scripts/c2-cli.sh` to the `e2e` chain, after `bash scripts/c1-crates.sh`.

- [ ] **Step 5: Run the headless ladder**

Run the same headless chain as Task 8 Step 5, plus `bash scripts/c2-cli.sh`. Report the result; the controller runs `d2-home.sh`, `d3-menus.sh`, and `d5-documents.sh`.

- [ ] **Step 6: Update the docs to match what shipped**

In `README.md`, add `apps/cli` and the six new crates to the component table, and document `penpot daemon` beside the existing headless instructions. In the spec, correct three things rather than editing them silently: the crate map (`navprobe`, `status`, `prefs_http` moved; `web-ui` precedes `app-core`), the offline/online split (three tiers, not two), and the "no script changes" claim — launch commands were untouched, but `d3-menus.sh`, `d5-documents.sh` and `n5-vaults.sh` all needed edits. Record each as a finding with its cause.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "test(c2): live CLI gate — tier split, attach-or-fail, control-port auth, record lifecycle"
```

---

## Self-Review

**Spec coverage.** Revised crate map → Tasks 2–7 in dependency order. Extraction gate → Task 8. Discovery + control-port security → Task 9. Tier-1 verbs → Task 10. Tier-2/3 + daemon + shim → Task 11. Live CLI gate + doc corrections → Task 12. The spike's RC-6 ("promote verb cores") is deliberately absent: the loopback decision for Tier 2/3 dissolves it, and the reason is recorded in "What Task 1 changed" rather than left implicit.

**Corrections carried into Task 12 Step 6.** The spec claims no script changes; three scripts change (`d3-menus.sh`, `d5-documents.sh`, `n5-vaults.sh`), because two pin test names by crate and one drives the control port that now demands a token. The spec's crate map and its two-tier offline/online split are both superseded by the spike.

**Type consistency.** `RuntimeRecord { pid, proxy_port, control_port, token }`, `RECORD_FILE_NAME`, `TOKEN_HEADER`, `write_record`, `read_record`, `remove_record`, and `require_token` are defined in Task 9 and used with the same spelling in Tasks 10 and 11. `Attached { base, token }`, `attach`, and `Attached::request` are defined in Task 10 and consumed in Task 11. `app_core::AppConfig` keeps the `data_dir` / `runtime_dir` fields `bin/headless.rs` already reads. `serve_control`'s new signature (Task 9 Step 5) matches its call sites in Task 9 Step 5 and its use in Task 11's daemon.

**Known risk.** Task 9 changes `serve_control`'s signature and makes the control server unconditional, so it is the one Phase 3 task that is not purely additive: `n5-vaults.sh` and `d5-documents.sh` fail with 401 until Step 6 lands. This is expected and staged inside the task.
