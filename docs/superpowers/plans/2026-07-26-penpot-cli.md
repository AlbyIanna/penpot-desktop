# penpot-cli Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the ~11,000 GUI-free lines out of `apps/desktop` into six library crates, then add `penpot` as a third front-end over them.

**Architecture:** Chapter C (the command line), following the M/N/E/D chapter pattern. Phase 1 spikes the verb list against a live stack and throws the code away. Phase 2 moves six crates bottom-up, one `git mv` plus Cargo wiring per task, with no behaviour change. Phase 3 builds the real CLI: offline verbs run with no stack, online verbs attach to a running daemon over the existing localhost control port, which first gains token auth.

**Tech Stack:** Rust 2021, cargo workspace, axum 0.8, clap 4 (new, `apps/cli` only), tokio, anyhow. Gates are bash scripts under `scripts/`, chained from the `justfile`.

**Spec:** [`docs/superpowers/specs/2026-07-26-penpot-cli-design.md`](../specs/2026-07-26-penpot-cli-design.md)

## Global Constraints

- **Behaviour is frozen in Phase 2.** Every task in Phase 2 is a move plus Cargo wiring. If a step appears to need a logic change, stop the task and land the change as its own commit before or after the move — never inside it.
- **The full gate ladder is the oracle.** `cargo test --workspace`, `just smoke`, `just invariant`, `just m3` must be green before AND after every Phase 2 task. The full `just e2e` runs at the end of Phase 2 and again at the end of Phase 3.
- **No new crate may depend on `tauri`.** Enforced by Task 9's gate.
- **Module names never change.** `docopen`, `windows`, `recent`, `reveal` keep their exact module paths inside their new crates, so the pinned test names in `scripts/d3-menus.sh` and `scripts/d5-documents.sh` stay valid — only the `-p <package>` flag changes.
- **`bin/headless` stays.** It keeps its path, its `READY <url>` stdout contract, and its `STOPPED` line. No gate script's launch command changes.
- **Axum routers stay with their domain.** `packages`, `templates`, `manage`, and `checkpoint` each build their own `Router`; `crates/web-ui` holds only the two HTML page modules (`home`, `prefs_http`), not "the HTTP layer".
- **Crate manifests follow the existing pattern:** `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`, and a `description` that states what the crate owns (see `crates/vault-index/Cargo.toml`).
- **Chapter C port set (verified free against every existing gate):** proxy `9062`, backend `6522`, postgres `5597`, valkey `6540`, control `9063`.

---

## File Structure

**New crates** (each gets `Cargo.toml` + `src/lib.rs` declaring its modules; the module files themselves are `git mv`'d unchanged):

| Path | Owns | Depends on |
|---|---|---|
| `crates/installer/` | `installer.rs` — import-as-new + settle-to-fixpoint | `penpot-rpc`, `sync-core` |
| `crates/app-state/` | `prefs.rs`, `recent.rs`, `last_op.rs` — JSON stores in the data dir | `serde`, `serde_json` |
| `crates/packages/` | `packages.rs`, `templates.rs` — package home, lockfile, gallery | `installer`, `penpot-rpc`, `sync-core`, `axum` |
| `crates/vault-ops/` | `vault.rs`, `checkpoint.rs`, `gitinit.rs`, `manage.rs`, `docopen.rs`, `reveal.rs` | `installer`, `penpot-rpc`, `sync-core`, `axum` |
| `crates/app-core/` | `lib.rs`→`boot.rs`, `control.rs`, `layout.rs`, `preflight.rs`, `windows.rs` | `app-state`, `vault-ops`, `supervisor`, `proxy`, `penpot-rpc`, `sync-daemon`, `board-export`, `vault-index` |
| `crates/web-ui/` | `home.rs`, `prefs_http.rs` — the two served HTML pages | `app-core`, `app-state`, `vault-ops`, `sync-daemon` |
| `apps/cli/` | `main.rs`, `verbs/`, `attach.rs` — the `penpot` binary | all of the above, `clap` |

**Modified:**
- `Cargo.toml` — workspace `members`
- `apps/desktop/Cargo.toml` — swap direct module ownership for the new crate deps
- `apps/desktop/src/lib.rs` — shrinks to the GUI module list plus re-exports
- `apps/desktop/src/bin/headless.rs` — imports move to `app_core`
- `scripts/d3-menus.sh:298`, `scripts/d5-documents.sh:294` — `-p` flags
- `justfile` — `c1`, `c2` recipes, `e2e` chain

**Stays in `apps/desktop`:** `main.rs`, `menubar/`, `tray/`, `overlay.rs`, `dialog.rs`, `navwatch.rs`, `navprobe.rs`, `status.rs` (5,647 lines).

---

## Phase 1 — Spike the verbs

### Task 1: Verb spike (throwaway)

**Files:**
- Create: `apps/desktop/src/bin/cli.rs` (deleted at the end of this task)
- Create: `docs/spikes/cli-verbs.md`

**Interfaces:**
- Consumes: nothing.
- Produces: `docs/spikes/cli-verbs.md`, holding the confirmed verb list and any runtime coupling that forces a crate-map change.

This task is a spike, not a TDD cycle. Its deliverable is the document; the code is scaffolding and gets deleted.

- [ ] **Step 1: Write the rough spike binary**

Inside the current crate, Tauri linked, ergonomics ignored. Hand-roll argument parsing — do not add `clap` here.

```rust
//! THROWAWAY verb spike (chapter C, Task 1). Deleted at the end of the task.
//! Its only purpose is to discover RUNTIME couplings that the static import
//! graph cannot show: shared locks, boot ordering, live state held between
//! calls. Do not add dependencies for it; do not make it nice.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("help");
    eprintln!("spike verb: {verb}");
    match verb {
        "vault-list" => { /* call penpot_desktop::vault:: registry read */ }
        "checkpoint" => { /* call penpot_desktop::checkpoint:: */ }
        "pkg-list" => { /* call penpot_desktop::packages:: */ }
        "export" => { /* call penpot_desktop::board_export path */ }
        "sync-status" => { /* call penpot_desktop::status:: */ }
        other => eprintln!("unknown verb {other}"),
    }
    Ok(())
}
```

- [ ] **Step 2: Fill in each verb against the real modules**

Work verb by verb. For each one, record in a scratch note: which modules it had to import, and whether it needed live state that outlives a single call.

- [ ] **Step 3: Run each verb against a live stack**

```bash
cargo run -p penpot-desktop --bin headless
```

Then, in a second shell, run each verb. Expected: each either produces its result or fails with a clear reason.

- [ ] **Step 4: Write the findings document**

`docs/spikes/cli-verbs.md` must answer three questions explicitly:
1. **The verb list** — which of `export`, `sync status`, `sync pause`, `checkpoint`, `vault list`, `vault switch`, `pkg add`, `pkg list`, `new --from-template`, `open <path>` survive, and which were added or dropped.
2. **Offline/online split** — for each surviving verb, whether it needs a running backend.
3. **Crate-map verdict** — CONFIRMED, or a revised table with the evidence that forced the change.

- [ ] **Step 5: Delete the spike binary and commit**

```bash
git rm apps/desktop/src/bin/cli.rs
git add docs/spikes/cli-verbs.md
git commit -m "docs(spike): chapter C verb list + runtime-coupling findings"
```

- [ ] **Step 6: Gate on the verdict**

If Step 4 revised the crate map, **stop**. Update the spec and Tasks 2–8 before starting Task 2. A revised map invalidates the dependency ordering those tasks rely on.

---

## Phase 2 — Extract the crates

Every Phase 2 task follows the same five-step shape. The "test" is the existing suite: these are moves, so a green suite before and after is exactly the right oracle.

### Task 2: `crates/installer`

**Files:**
- Create: `crates/installer/Cargo.toml`, `crates/installer/src/lib.rs`
- Move: `apps/desktop/src/installer.rs` → `crates/installer/src/lib.rs`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/packages.rs`, `apps/desktop/src/templates.rs`, `apps/desktop/src/manage.rs`

**Interfaces:**
- Consumes: nothing (leaf crate).
- Produces: `installer::MAX_SETTLE_CYCLES: usize`; `installer::default_project_id(&PenpotClient, &str) -> anyhow::Result<String>`; `installer::import_binfile_and_settle(&PenpotClient, &str, &str, Vec<u8>, Option<u8>) -> anyhow::Result<(String, usize)>`; `installer::settle_to_fixpoint(&PenpotClient, &str, &str, &str) -> anyhow::Result<usize>`; `installer::first_page_id(&PenpotClient, &str) -> Option<String>`. All are `async` except `MAX_SETTLE_CYCLES`.

- [ ] **Step 1: Record the green baseline**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS. Write down the test count — Step 4 must match it.

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

- [ ] **Step 3: Move the file and wire it up**

```bash
mkdir -p crates/installer/src
git mv apps/desktop/src/installer.rs crates/installer/src/lib.rs
```

Add `"crates/installer",` to the `members` list in the root `Cargo.toml`. Add `installer = { path = "../../crates/installer" }` to `apps/desktop/Cargo.toml`. Delete the `pub mod installer;` line from `apps/desktop/src/lib.rs`. In `packages.rs`, `templates.rs`, and `manage.rs`, rewrite `crate::installer` to `installer`.

- [ ] **Step 4: Verify the suite is unchanged**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS, with the same test count as Step 1. A *lower* count means a test module was lost in the move — investigate before continuing.

- [ ] **Step 5: Verify the crate is Tauri-free**

```bash
cargo tree -p installer -e normal | grep -c '^\|[[:space:]]*tauri '
```

Expected: `0`.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/installer from apps/desktop"
```

### Task 3: `crates/app-state`

**Files:**
- Create: `crates/app-state/Cargo.toml`, `crates/app-state/src/lib.rs`
- Move: `apps/desktop/src/{prefs,recent,last_op}.rs` → `crates/app-state/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/control.rs`, `apps/desktop/src/prefs_http.rs`, `apps/desktop/src/menubar/mod.rs`, `apps/desktop/src/menubar/model.rs`, `scripts/d3-menus.sh:298`

**Interfaces:**
- Consumes: nothing.
- Produces: the `app_state::prefs`, `app_state::recent`, and `app_state::last_op` modules, with every currently-public item unchanged. `last_op` keeps its internal dependency on `prefs`, now written `crate::prefs` within `app-state`.

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

- [ ] **Step 3: Move the files and wire them up**

```bash
mkdir -p crates/app-state/src
git mv apps/desktop/src/prefs.rs crates/app-state/src/prefs.rs
git mv apps/desktop/src/recent.rs crates/app-state/src/recent.rs
git mv apps/desktop/src/last_op.rs crates/app-state/src/last_op.rs
```

Add `"crates/app-state",` to the root `members`. Add `app-state = { path = "../../crates/app-state" }` to `apps/desktop/Cargo.toml`. Delete the three `pub mod` lines from `apps/desktop/src/lib.rs`. Rewrite `crate::prefs` → `app_state::prefs`, `crate::recent` → `app_state::recent`, `crate::last_op` → `app_state::last_op` in `control.rs`, `prefs_http.rs`, `menubar/mod.rs`, and `menubar/model.rs`. Inside `last_op.rs`, `crate::prefs` still resolves correctly — it now means `app-state`'s own `prefs`.

- [ ] **Step 4: Repoint the D3 gate**

`scripts/d3-menus.sh:298` currently runs one command for four module filters. `recent::` now lives in `app-state`. Replace the single invocation with two, keeping both logs feeding the same `REQUIRED_TESTS` grep:

```bash
(cd "$ROOT" && cargo test -p penpot-desktop -- menubar:: windows:: reveal::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
(cd "$ROOT" && cargo test -p app-state -- recent::) >>"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$(( CARGO_TEST_EXIT + $? ))
```

The five `recent::tests::*` entries in `REQUIRED_TESTS` keep their exact names and need no edit — the grep reads the combined log.

- [ ] **Step 5: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
cargo test -p app-state -- recent:: 2>&1 | tail -5
```

Expected: both PASS, and the second lists `most_recent_first`, `reopening_moves_to_front_without_duplicating`, `the_list_is_capped`, and `a_missing_or_corrupt_store_is_an_empty_list_not_an_error`.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/app-state (prefs, recent, last_op)"
```

### Task 4: `crates/packages`

**Files:**
- Create: `crates/packages/Cargo.toml`, `crates/packages/src/lib.rs`
- Move: `apps/desktop/src/{packages,templates}.rs` → `crates/packages/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, and every caller of `crate::packages` / `crate::templates`

**Interfaces:**
- Consumes: `installer::*` from Task 2.
- Produces: the `packages::packages` and `packages::templates` modules with their existing public items, including the axum `Router` builders each already exposes.

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
axum = "0.8"
http = "1"

[dev-dependencies]
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

- [ ] **Step 3: Move the files and wire them up**

```bash
mkdir -p crates/packages/src
git mv apps/desktop/src/packages.rs crates/packages/src/packages.rs
git mv apps/desktop/src/templates.rs crates/packages/src/templates.rs
```

Add `"crates/packages",` to the root `members`, add `packages = { path = "../../crates/packages" }` to `apps/desktop/Cargo.toml`, delete the two `pub mod` lines from `apps/desktop/src/lib.rs`. Find every caller and repoint it:

```bash
grep -rn "crate::packages\|crate::templates" apps/desktop/src
```

Rewrite each hit to `packages::packages` / `packages::templates`.

- [ ] **Step 4: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
cargo tree -p packages -e normal | grep -c '[[:space:]]tauri '
```

Expected: PASS, and `0`.

- [ ] **Step 5: Run the live package gates**

```bash
bash scripts/e2-packages.sh
bash scripts/n6-templates.sh
```

Expected: both all-green. These two exercise the moved code against a real stack; the unit suite alone does not.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/packages (packages, templates)"
```

### Task 5: `crates/vault-ops`

**Files:**
- Create: `crates/vault-ops/Cargo.toml`, `crates/vault-ops/src/lib.rs`
- Move: `apps/desktop/src/{vault,checkpoint,gitinit,manage,docopen,reveal}.rs` → `crates/vault-ops/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/control.rs`, `apps/desktop/src/home.rs`, `apps/desktop/src/tray/mod.rs`, `apps/desktop/src/menubar/mod.rs`, `scripts/d3-menus.sh`, `scripts/d5-documents.sh:294`

**Interfaces:**
- Consumes: `installer::*` from Task 2.
- Produces: `vault_ops::{vault, checkpoint, gitinit, manage, docopen, reveal}`. `vault::{SwitchMarker, VaultRef, VaultRegistry}` and `vault::reset_disposable_state` stay public — `app-core`'s `control.rs` imports all four in Task 6.

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
axum = "0.8"
http = "1"
rand = "0.8"

[dev-dependencies]
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

- [ ] **Step 3: Move the files and wire them up**

```bash
mkdir -p crates/vault-ops/src
for m in vault checkpoint gitinit manage docopen reveal; do
  git mv "apps/desktop/src/$m.rs" "crates/vault-ops/src/$m.rs"
done
```

Add `"crates/vault-ops",` to the root `members`, add `vault-ops = { path = "../../crates/vault-ops" }` to `apps/desktop/Cargo.toml`, delete the six `pub mod` lines from `apps/desktop/src/lib.rs`. Repoint callers:

```bash
grep -rn "crate::vault\b\|crate::checkpoint\|crate::gitinit\|crate::manage\|crate::docopen\|crate::reveal" apps/desktop/src
```

Rewrite each to `vault_ops::<module>`. Note `vault.rs`'s doc comment references `[`crate::control`]`, which will no longer resolve — change it to plain text ``` `app-core`'s `control` ``` so `cargo doc` stays clean.

- [ ] **Step 4: Repoint the D3 and D5 gates**

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

All fourteen `docopen::tests::*` names and the `reveal::tests::macos_reveal_uses_open_dash_r_on_the_item_itself` name stay exactly as written.

- [ ] **Step 5: Verify the pinned names still report by name**

```bash
cargo test -p vault-ops -- docopen:: 2>&1 | grep -c '\.\.\. ok$'
```

Expected: at least `14`. This is the check that matters: the D5 gate greps for these names, so a silently-renamed module path would be caught here rather than at the end of a stack boot.

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/vault-ops; repoint the D3/D5 test pins"
```

### Task 6: `crates/app-core`

**Files:**
- Create: `crates/app-core/Cargo.toml`, `crates/app-core/src/lib.rs`, `crates/app-core/src/boot.rs`
- Move: `apps/desktop/src/{control,layout,preflight,windows}.rs` → `crates/app-core/src/`; the boot body of `apps/desktop/src/lib.rs` → `crates/app-core/src/boot.rs`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, `apps/desktop/src/main.rs`, `apps/desktop/src/bin/headless.rs`, `apps/desktop/src/home.rs`, `apps/desktop/src/prefs_http.rs`, `apps/desktop/src/tray/mod.rs`, `scripts/d3-menus.sh`

**Interfaces:**
- Consumes: `app_state::*` (Task 3), `vault_ops::*` (Task 5).
- Produces: `app_core::AppConfig` (with `AppConfig::resolve() -> anyhow::Result<AppConfig>`, fields `data_dir: PathBuf`, `runtime_dir: PathBuf`), `app_core::RunningApp`, `app_core::boot`, `app_core::control::{VaultRunner, boot_active_vault, serve_control, control_port_from_env, CONTROL_PORT_ENV}`, `app_core::windows::{OpenWindow, WindowRegistry}`, `app_core::layout::*`, `app_core::preflight::*`.

This is the largest move, because `apps/desktop/src/lib.rs` is the crate root: its module declarations stay behind while its boot body leaves.

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
description = "Penpot Local boot sequence and live-stack ownership (M1, N5, D4): resolve config -> pin PENPOT_SECRET_KEY -> supervise postgres/valkey/backend -> provision the single user -> serve the proxy, plus the VaultRunner that owns the running stack across vault switches and reboots-in-place, the packaged runtime layout resolver, the non-BMP boot pre-flight, and the Tauri-free window registry."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
app-state = { path = "../app-state" }
vault-ops = { path = "../vault-ops" }
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

Move everything below the `pub mod` block — `AppConfig`, `RunningApp`, `boot`, the bootstrap route, and the `#[cfg(test)] mod tests` at line 1263 — into `crates/app-core/src/boot.rs`. Then create `crates/app-core/src/lib.rs`:

```rust
//! Penpot Local core boot sequence (M1), extracted in chapter C.
//!
//! Shared by every front-end: the Tauri shell, the headless daemon, and the
//! CLI. Resolve configuration -> load-or-generate the pinned
//! `PENPOT_SECRET_KEY` -> start the supervisor (postgres -> valkey -> backend
//! JVM) -> first-boot single-user provisioning over RPC -> start the local
//! proxy with the `/__bootstrap` auto-login route -> expose readiness.

mod boot;

pub mod control;
pub mod layout;
pub mod preflight;
pub mod windows;

pub use boot::*;
```

The `pub use boot::*;` keeps `app_core::AppConfig`, `app_core::RunningApp`, and `app_core::boot` at the paths the rest of the code already spells.

- [ ] **Step 4: Move the four modules**

```bash
for m in control layout preflight windows; do
  git mv "apps/desktop/src/$m.rs" "crates/app-core/src/$m.rs"
done
```

Add `"crates/app-core",` to the root `members` and `app-core = { path = "../../crates/app-core" }` to `apps/desktop/Cargo.toml`.

In `apps/desktop/src/lib.rs`, keep only the GUI module declarations plus a re-export line, so `main.rs` and the two integration tests under `apps/desktop/tests/` keep compiling unchanged:

```rust
//! Penpot Local desktop shell: the Tauri-facing half of the app.
//!
//! The boot sequence, the vault runner, and every GUI-free verb now live in
//! `app-core` and its sibling crates (chapter C). What remains here is the
//! native chrome — menus, tray, windows, overlay — plus its own pure model
//! layer, and the navigation policy that keeps the SPA inside the app.

pub mod dialog;
pub mod menubar;
pub mod navprobe;
pub mod navwatch;
pub mod overlay;
pub mod status;
pub mod tray;

pub use app_core::{boot, control, layout, preflight, windows, AppConfig, RunningApp};
```

In `apps/desktop/src/bin/headless.rs`, change the import:

```rust
use app_core::{control, AppConfig};
```

- [ ] **Step 5: Repoint the D3 gate one last time**

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

- [ ] **Step 6: Verify the boot path end to end**

```bash
cargo test --workspace 2>&1 | tail -5
bash scripts/m1-smoke.sh
bash scripts/m2-invariant.sh
bash scripts/n5-vaults.sh
```

Expected: all green. `m1-smoke` proves the moved boot sequence still boots; `n5-vaults` proves the moved `VaultRunner` still switches vaults without spill.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/app-core (boot, control, layout, preflight, windows)"
```

### Task 7: `crates/web-ui`

**Files:**
- Create: `crates/web-ui/Cargo.toml`, `crates/web-ui/src/lib.rs`
- Move: `apps/desktop/src/{home,prefs_http}.rs` → `crates/web-ui/src/`
- Modify: `Cargo.toml`, `apps/desktop/Cargo.toml`, `apps/desktop/src/lib.rs`, and the callers that mount these routers

**Interfaces:**
- Consumes: `app_core::*`, `app_state::*`, `vault_ops::reveal`.
- Produces: `web_ui::home` and `web_ui::prefs_http`, each keeping its existing router builder.

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
description = "Penpot Local's served HTML pages: the N3 lighttable home (/__home, thumbnails, activity/conflict strip) and the D4 Preferences surface (/__api/prefs*). Framework-free pages served through the proxy's extra routes — the app's second front-end, beside the native chrome and the CLI. Domain routers live with their domain crates; only these two page modules live here."

[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
app-core = { path = "../app-core" }
app-state = { path = "../app-state" }
vault-ops = { path = "../vault-ops" }
sync-daemon = { path = "../sync-daemon" }
axum = "0.8"
http = "1"

[dev-dependencies]
tempfile = "3"
tower = { version = "0.5", features = ["util"] }
```

`crates/web-ui/src/lib.rs`:

```rust
//! The served HTML pages (chapter C extraction).
//!
//! These two modules were already thin clients over an in-process API before
//! the CLI existed — that is why the CLI is the app's THIRD front-end, not a
//! new layer beneath the other two.

pub mod home;
pub mod prefs_http;
```

- [ ] **Step 3: Move the files and wire them up**

```bash
mkdir -p crates/web-ui/src
git mv apps/desktop/src/home.rs crates/web-ui/src/home.rs
git mv apps/desktop/src/prefs_http.rs crates/web-ui/src/prefs_http.rs
```

Add `"crates/web-ui",` to the root `members`, add `web-ui = { path = "../../crates/web-ui" }` to `apps/desktop/Cargo.toml`, drop the two `pub mod` lines from `apps/desktop/src/lib.rs`, and repoint callers:

```bash
grep -rn "crate::home\|crate::prefs_http" apps/desktop/src crates/app-core/src
```

Rewrite each hit to `web_ui::home` / `web_ui::prefs_http`. Inside the moved `home.rs`, `crate::reveal::reveal` becomes `vault_ops::reveal::reveal`.

- [ ] **Step 4: Verify**

```bash
cargo test --workspace 2>&1 | tail -5
bash scripts/n3-home.sh
bash scripts/d4-preferences.sh
```

Expected: all green. These two gates drive exactly the moved routes.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(c2): extract crates/web-ui (home, prefs_http)"
```

### Task 8: Prove the extraction, and lock it

**Files:**
- Create: `scripts/c1-crates.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: every crate from Tasks 2–7.
- Produces: `just c1`.

This is the task that makes the extraction stick. Without it, the first convenient `use tauri::` puts the coupling back, spread across more files than before.

- [ ] **Step 1: Write the failing gate**

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
#   (c) apps/desktop keeps only the GUI: its src/ holds no module that the
#       crate map assigned elsewhere.
# No ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

EXTRACTED=(installer app-state packages vault-ops app-core web-ui)

echo "-- (a) no extracted crate depends on tauri"
for c in "${EXTRACTED[@]}"; do
    if (cd "$ROOT" && cargo tree -p "$c" -e normal 2>/dev/null | grep -qE '^[^a-z]*tauri v|[[:space:]]tauri v'); then
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
       gitinit manage docopen reveal control layout preflight windows
       home prefs_http)
for m in "${MOVED[@]}"; do
    if [ -f "$ROOT/apps/desktop/src/$m.rs" ]; then
        fail "(c) apps/desktop/src/$m.rs still exists — it belongs in a crate"
    else
        pass "(c) $m.rs is out of apps/desktop"
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

Expected: `C1: 30 passed, 0 failed`.

- [ ] **Step 3: Prove the gate can fail**

Temporarily add `tauri = "2"` to `crates/app-state/Cargo.toml`, re-run, and confirm leg (a) reports FAIL for `app-state`. Then revert. A gate never verified against a red case is not a gate.

```bash
bash scripts/c1-crates.sh; git checkout crates/app-state/Cargo.toml
```

Expected: the run reports `FAIL  (a) app-state depends on tauri`, then the revert restores green.

- [ ] **Step 4: Wire it into the justfile**

Add the recipe:

```make
# C1 extraction gate (chapter C): the six extracted crates are tauri-free,
# their tests pass in their new homes, and apps/desktop holds only GUI
# modules. Fast, pure, no stack, no ports. Chained into `just e2e`.
c1:
    bash scripts/c1-crates.sh
```

Add `bash scripts/c1-crates.sh` to the `e2e` chain, immediately before `bash scripts/d1-offline.sh`.

- [ ] **Step 5: Run the full ladder**

```bash
just e2e
```

Expected: all green. This is the end-of-Phase-2 checkpoint — do not start Phase 3 with a red ladder.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "test(c1): gate the extraction — tauri-free crates, tests in their new homes"
```

---

## Phase 3 — Build the CLI

### Task 9: Token-authenticate the control port

**Files:**
- Create: `crates/app-core/src/control_auth.rs`
- Modify: `crates/app-core/src/control.rs`, `crates/app-core/src/lib.rs`

**Interfaces:**
- Consumes: `app_core::control::serve_control`.
- Produces: `app_core::control_auth::{ControlFile, write_control_file, read_control_file, require_token, CONTROL_FILE_NAME, TOKEN_HEADER}`. `ControlFile { port: u16, token: String }` derives `Serialize`, `Deserialize`, `Clone`, `Debug`. `write_control_file(data_dir: &Path, port: u16, token: &str) -> anyhow::Result<()>`; `read_control_file(data_dir: &Path) -> anyhow::Result<ControlFile>`. `CONTROL_FILE_NAME = "control.json"`; `TOKEN_HEADER = "x-penpot-control-token"`.

This is the only new behaviour in the whole chapter. It is required, not optional: promoting the control port from test-only to always-on exposes `POST /open` — a DB wipe plus a full reconcile — to any web page, because the same-origin policy blocks reading a cross-origin response, not sending the request.

- [ ] **Step 1: Write the failing tests**

`crates/app-core/src/control_auth.rs`:

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

    #[test]
    fn the_control_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_control_file(dir.path(), 9063, "s3cret").unwrap();
        let read = read_control_file(dir.path()).unwrap();
        assert_eq!(read.port, 9063);
        assert_eq!(read.token, "s3cret");
    }

    #[cfg(unix)]
    #[test]
    fn the_control_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_control_file(dir.path(), 9063, "s3cret").unwrap();
        let mode = std::fs::metadata(dir.path().join(CONTROL_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "control.json must be 0600, got {mode:o}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p app-core -- control_auth::
```

Expected: FAIL — the module does not compile, because nothing above it is defined yet.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/app-core/src/control_auth.rs`:

```rust
//! Token auth for the localhost control port (chapter C).
//!
//! Before chapter C the control server started only when
//! `PENPOT_LOCAL_CONTROL_PORT` was set, which happened only in tests. The CLI
//! attaches to it, so it is now always on — and that changes the threat model:
//! ANY web page can send `POST http://127.0.0.1:<port>/open` cross-origin. The
//! same-origin policy blocks reading the response, not sending the request,
//! and `POST /open` wipes the disposable DB and reconciles from scratch.
//!
//! Two independent defences, both required:
//!  1. a random per-boot token, written to a `0600` `control.json` in the data
//!     dir, demanded on every request;
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
pub const CONTROL_FILE_NAME: &str = "control.json";

/// Header carrying the per-boot control token.
pub const TOKEN_HEADER: &str = "x-penpot-control-token";

/// What the daemon publishes so a CLI invocation can find and authenticate to
/// the running instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFile {
    pub port: u16,
    pub token: String,
}

fn control_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONTROL_FILE_NAME)
}

/// Write `control.json` with owner-only permissions.
pub fn write_control_file(data_dir: &Path, port: u16, token: &str) -> Result<()> {
    let path = control_file_path(data_dir);
    let body = serde_json::to_vec_pretty(&ControlFile {
        port,
        token: token.to_string(),
    })?;
    std::fs::write(&path, body)
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod {}", path.display()))?;
    }
    Ok(())
}

/// Read `control.json`. Errors when no daemon is running, which is exactly the
/// signal the CLI's attach-or-fail path needs.
pub fn read_control_file(data_dir: &Path) -> Result<ControlFile> {
    let path = control_file_path(data_dir);
    let body = std::fs::read(&path)
        .with_context(|| format!("no running instance: cannot read {}", path.display()))?;
    serde_json::from_slice(&body)
        .with_context(|| format!("malformed {}", path.display()))
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

Add `pub mod control_auth;` to `crates/app-core/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -p app-core -- control_auth::
```

Expected: PASS, 7 tests.

- [ ] **Step 5: Apply the layer to the real control router**

In `crates/app-core/src/control.rs`, change `serve_control` to take a token, apply the middleware, and publish the file:

```rust
pub async fn serve_control(
    runner: Arc<VaultRunner>,
    windows: WindowRegistry,
    port: u16,
    token: String,
    data_dir: &Path,
) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("control server cannot bind {addr}"))?;
    let bound = listener.local_addr()?.port();
    crate::control_auth::write_control_file(data_dir, bound, &token)?;
    tracing::info!(%addr, "vault control server listening (token-authenticated)");
    let router = control_router(runner, windows).layer(
        axum::middleware::from_fn_with_state(token, crate::control_auth::require_token),
    );
    axum::serve(listener, router).await.context("control server error")?;
    Ok(())
}
```

Update the two call sites in `apps/desktop/src/bin/headless.rs` and the GUI path to generate a token with `rand` and pass `config.data_dir`.

- [ ] **Step 6: Verify the gates that drive the control port still pass**

The N5 and D5 gates call `POST /open` and `GET /windows` over this port and will now get 401 unless they send the token. Update them to read `control.json` and pass the header.

```bash
bash scripts/n5-vaults.sh
bash scripts/d5-documents.sh
```

Expected: both green. If either now fails with 401, its request is missing the header — that is the gate working.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(c3): token-authenticate the control port, reject browser-initiated requests"
```

### Task 10: The `penpot` binary — offline verbs

**Files:**
- Create: `apps/cli/Cargo.toml`, `apps/cli/src/main.rs`, `apps/cli/src/attach.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `app_core::{AppConfig, control_auth}`, `vault_ops::{vault, checkpoint}`, `packages::packages`.
- Produces: the `penpot` binary. `attach::Attached { base: String, token: String }` with `attach::attach(&AppConfig) -> anyhow::Result<Attached>`, used by Task 11.

Use the verb list from Task 1's findings document. The set below is the plan's baseline; replace it with the spike's list where they differ.

- [ ] **Step 1: Write the failing test**

`apps/cli/src/attach.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaching_with_no_running_instance_names_the_daemon_verb() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_control(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("penpot daemon"),
            "attach failure must tell the user how to fix it, got: {err}"
        );
    }

    #[test]
    fn attaching_finds_a_published_instance() {
        let dir = tempfile::tempdir().unwrap();
        app_core::control_auth::write_control_file(dir.path(), 9063, "s3cret").unwrap();
        let attached = load_control(dir.path()).unwrap();
        assert_eq!(attached.base, "http://127.0.0.1:9063");
        assert_eq!(attached.token, "s3cret");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p penpot-cli -- attach::
```

Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Create the crate and implement attach**

`apps/cli/Cargo.toml`:

```toml
[package]
name = "penpot-cli"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Penpot Local's command line: the app's third front-end, beside the native chrome and the served HTML pages. INTERNAL — no stability contract, no public surface; it may change in any chapter."

[[bin]]
name = "penpot"
path = "src/main.rs"

[dependencies]
tokio = { workspace = true }
anyhow = { workspace = true }
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

`apps/cli/src/attach.rs`, above the tests:

```rust
//! Attach-or-fail: find a running daemon, or explain how to start one.
//!
//! The CLI never boots a stack implicitly. Booting postgres, valkey and the
//! JVM from a verb would cost 30+ seconds, and worse, would let two stacks
//! race over one data dir and one port set — the core invariant assumes a
//! single aggressive reconciliation at boot, not two concurrent ones.

use std::path::Path;

use anyhow::{Context, Result};

pub struct Attached {
    pub base: String,
    pub token: String,
}

pub fn load_control(data_dir: &Path) -> Result<Attached> {
    let file = app_core::control_auth::read_control_file(data_dir)
        .context("no running Penpot Local instance — start one with `penpot daemon`")?;
    Ok(Attached {
        base: format!("http://127.0.0.1:{}", file.port),
        token: file.token,
    })
}
```

Add `"apps/cli",` to the root `Cargo.toml` `members`.

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p penpot-cli -- attach::
```

Expected: PASS, 2 tests.

- [ ] **Step 5: Implement the offline verbs**

`apps/cli/src/main.rs`:

```rust
//! `penpot` — the CLI front-end (chapter C).
//!
//! INTERNAL surface: no stability contract, no versioned API, no end-user
//! documentation. Verbs split by nature — OFFLINE verbs read and write the
//! vault tree with no backend running; ONLINE verbs attach to a daemon.

mod attach;

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
    /// List the known vaults and mark the active one (offline).
    VaultList,
    /// Commit the active vault's current state (offline).
    Checkpoint,
    /// List installed packages from lock.json (offline).
    PkgList,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let config = app_core::AppConfig::resolve()?;
    match cli.command {
        Command::VaultList => { /* read the registry via vault_ops::vault */ }
        Command::Checkpoint => { /* run vault_ops::checkpoint */ }
        Command::PkgList => { /* read lock.json via packages::packages */ }
    }
    Ok(())
}
```

Fill each arm using the exact functions Task 1 recorded. Print machine-readable JSON on stdout and human text on stderr, so the Task 12 gate can assert on stdout without parsing prose.

- [ ] **Step 6: Verify the verbs run with no stack**

```bash
cargo run -p penpot-cli -- vault-list
cargo run -p penpot-cli -- pkg-list
```

Expected: both print JSON and exit 0 with no Penpot process running. That is the whole point of the offline/online split.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(c3): add the penpot CLI with its offline verbs"
```

### Task 11: Online verbs and `penpot daemon`

**Files:**
- Modify: `apps/cli/src/main.rs`, `apps/cli/src/attach.rs`, `apps/desktop/src/bin/headless.rs`
- Create: `apps/cli/src/verbs/daemon.rs`

**Interfaces:**
- Consumes: `attach::{Attached, load_control}` from Task 10; `app_core::{boot, control}`.
- Produces: `penpot daemon`, which prints `READY <url>` on stdout exactly as `bin/headless` does.

- [ ] **Step 1: Write the failing test**

In `apps/cli/src/attach.rs`:

```rust
    #[tokio::test]
    async fn an_online_verb_sends_the_token_header() {
        let dir = tempfile::tempdir().unwrap();
        app_core::control_auth::write_control_file(dir.path(), 9063, "s3cret").unwrap();
        let attached = load_control(dir.path()).unwrap();
        let req = attached.request(reqwest::Method::GET, "/active");
        let built = req.build().unwrap();
        assert_eq!(
            built.headers().get(app_core::control_auth::TOKEN_HEADER).unwrap(),
            "s3cret"
        );
        assert!(built.headers().get("origin").is_none(), "the CLI must never send Origin");
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p penpot-cli -- attach::an_online_verb
```

Expected: FAIL with "no method named `request`".

- [ ] **Step 3: Implement the request builder**

In `apps/cli/src/attach.rs`:

```rust
impl Attached {
    /// Build a request to the running daemon, carrying the control token and
    /// deliberately no `Origin` — the server rejects anything browser-shaped.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header(app_core::control_auth::TOKEN_HEADER, &self.token)
    }
}
```

- [ ] **Step 4: Run it to verify it passes**

```bash
cargo test -p penpot-cli -- attach::
```

Expected: PASS, 3 tests.

- [ ] **Step 5: Add `penpot daemon` and the online verbs**

`apps/cli/src/verbs/daemon.rs` holds the body currently in `apps/desktop/src/bin/headless.rs` — the boot, the `READY <url>` line, the SIGTERM/SIGINT loop, the SIGUSR1 sync toggle, the `STOPPED` line — lifted verbatim.

Add to the `Command` enum:

```rust
    /// Boot the full stack and run until SIGTERM (prints `READY <url>`).
    Daemon,
    /// Print the sync daemon's status (online).
    SyncStatus,
    /// Switch the active vault (online).
    VaultSwitch { path: String },
```

- [ ] **Step 6: Reduce `bin/headless` to a shim**

`apps/desktop/src/bin/headless.rs` keeps its path and its stdout contract, so no gate script's launch command changes:

```rust
//! Compatibility shim (chapter C): `penpot daemon` under the old name.
//!
//! Every milestone gate launches `--bin headless` and waits for `READY <url>`
//! on stdout. Those suites are the net that protected the whole chapter-C
//! extraction, so the name outlives the refactor. Removing this shim is a
//! one-line change in a later chapter.

fn main() -> anyhow::Result<()> {
    penpot_cli::daemon::run()
}
```

This requires `apps/cli` to expose a library target alongside its binary: add `[lib] name = "penpot_cli" path = "src/lib.rs"` to `apps/cli/Cargo.toml`, with `src/lib.rs` declaring `pub mod attach; pub mod verbs;` and `pub use verbs::daemon;`. Add `penpot-cli = { path = "../cli" }` to `apps/desktop/Cargo.toml`.

- [ ] **Step 7: Verify the shim is byte-compatible with the old behaviour**

```bash
bash scripts/m1-smoke.sh
bash scripts/m3-sync.sh
```

Expected: both green. `m1-smoke` asserts the `READY`/`STOPPED` contract and clean shutdown; `m3-sync` drives the SIGUSR1 pause hook. If either fails, the shim dropped something the verbatim lift was supposed to keep.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(c3): penpot daemon + online verbs; headless becomes a shim"
```

### Task 12: The C2 gate and the chapter close

**Files:**
- Create: `scripts/c2-cli.sh`
- Modify: `justfile`, `README.md`, `docs/superpowers/specs/2026-07-26-penpot-cli-design.md`

**Interfaces:**
- Consumes: everything above.
- Produces: `just c2`.

- [ ] **Step 1: Write the gate**

`scripts/c2-cli.sh`:

```bash
#!/usr/bin/env bash
# C2 CLI gate (chapter C). A LIVE gate over the third front-end:
#   (a) OFFLINE verbs run with NO stack — the assert that the offline/online
#       split is real rather than nominal.
#   (b) An ONLINE verb with no daemon running fails and NAMES `penpot daemon`
#       — attach-or-fail never silently boots a second stack over one data dir.
#   (c) With a daemon up, the same online verb succeeds.
#   (d) The control port REFUSES a tokenless request and a browser-shaped one
#       (Sec-Fetch-Site) even with a valid token — the D-chapter threat model:
#       POST /open wipes the DB, and any web page can send a cross-origin POST.
# Dedicated ports: proxy 9062, backend 6522, postgres 5597, valkey 6540,
# control 9063. Chained into `just e2e` — chapter C lands product code.
set -uo pipefail
```

Fill in the four legs following `scripts/n5-vaults.sh`'s structure for booting a stack on dedicated ports and its `pass`/`fail` helpers. Leg (d) asserts three status codes: `401` with no header, `401` with a wrong token, `403` with a valid token plus `Sec-Fetch-Site: cross-site`.

- [ ] **Step 2: Run it**

```bash
chmod +x scripts/c2-cli.sh && bash scripts/c2-cli.sh
```

Expected: all legs pass.

- [ ] **Step 3: Prove leg (d) can fail**

Temporarily remove the `.layer(...)` line from `serve_control`, re-run, and confirm leg (d) reports FAIL. Revert.

```bash
bash scripts/c2-cli.sh; git checkout crates/app-core/src/control.rs
```

Expected: the run reports a FAIL for the tokenless request, then the revert restores green.

- [ ] **Step 4: Wire into the justfile**

```make
# C2 CLI gate (chapter C): offline verbs run with no stack; an online verb
# with no daemon fails naming `penpot daemon`; with a daemon it succeeds; and
# the control port refuses tokenless AND browser-shaped requests (POST /open
# wipes the DB — any web page can send a cross-origin POST). Dedicated ports
# 9062/6522/5597/6540 (control 9063). Chained into `just e2e`.
c2:
    bash scripts/c2-cli.sh
```

Add `bash scripts/c2-cli.sh` to the `e2e` chain, after `bash scripts/c1-crates.sh`.

- [ ] **Step 5: Run the whole ladder**

```bash
just e2e
```

Expected: all green, including `c1` and `c2`.

- [ ] **Step 6: Update the docs to match what shipped**

In `README.md`, add `apps/cli` and the six new crates to the component table, and document `penpot daemon` beside the existing headless instructions. In the spec, correct the "Keeping the harness still" section: the launch commands were indeed untouched, but `scripts/d3-menus.sh` and `scripts/d5-documents.sh` needed `-p` changes, because they pin test names by crate. Record that as a finding, not a silent edit.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "test(c2): live CLI gate — offline verbs, attach-or-fail, control-port auth"
```

---

## Self-Review

**Spec coverage.** Target architecture → Tasks 2–7. Phase 1 spike → Task 1. Phase 2 bottom-up moves → Tasks 2–7 in dependency order. Attach-or-fail → Tasks 10–11. Control-port security → Task 9. `bin/headless` shim → Task 11 Step 6. Verification gate 1 (no Tauri) → Task 8 leg (a). Verification gate 2 (offline verbs, no stack) → Task 12 leg (a). Non-goals → carried into Task 10's crate description and Task 12's README edit.

**One spec correction, carried into Task 12 Step 6.** The spec claims no script changes. That holds for launch commands, but not for `scripts/d3-menus.sh:298` and `scripts/d5-documents.sh:294`, which pin test names inside `cargo test -p penpot-desktop`. Four pinned modules move crate — `windows`, `recent`, `reveal`, `docopen` — so those two scripts need `-p` edits, staged across Tasks 3, 5, and 6 as each module moves. The names themselves never change, and both gates grep for names, so a missed edit fails loudly rather than passing vacuously.

**Type consistency.** `ControlFile { port, token }`, `CONTROL_FILE_NAME`, `TOKEN_HEADER`, `write_control_file`, `read_control_file`, and `require_token` are defined in Task 9 and used with the same spelling in Tasks 10 and 11. `Attached { base, token }` and `load_control` are defined in Task 10 and extended in Task 11. `app_core::AppConfig` keeps the `data_dir` / `runtime_dir` fields `bin/headless.rs` already reads.

**Known dependency.** Task 10's verb list is the plan's baseline, not its authority. Task 1 Step 6 gates the whole of Phase 2 on the spike's verdict: if the findings document revises the crate map, Tasks 2–8 must be revised before Task 2 starts.
