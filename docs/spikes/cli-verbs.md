# Chapter C spike — the `penpot` CLI verb list and its runtime couplings

Date: 2026-07-26. Penpot 2.16.2 (pinned `runtime/`). Chapter C, Task 1.

**Status: the crate map is REVISED, not confirmed.** Three module reassignments are
required before Task 2 starts, and one task-ordering swap. See
[Q3](#q3--crate-map-verdict-revised).

## What this spike was and how it was run

A throwaway binary (`apps/desktop/src/bin/cli.rs`, deleted in the same commit that
added this file) reached each candidate verb two ways: through
`penpot_desktop::*`'s **public** surface in-process, and through **raw loopback
HTTP** to the running app. It carried no new dependencies — the HTTP client is
~30 lines of hand-rolled HTTP/1.1 over `tokio::net::TcpStream`, precisely because
a real CLI's reachability question is "what can a separate process get at", not
"what can a well-equipped process get at".

The live stack was `cargo run -p penpot-desktop --bin headless` on a scratch data
dir, chapter-C ports (proxy 9062, backend 6522, postgres 5597, valkey 6540,
control 9063, exporter 6542), `PENPOT_LOCAL_EXPORTS=1`, and the pinned
`runtime/` artifacts. Every claim below marked "observed" was produced by that
stack; claims marked **unverified** were not.

Static-analysis note: the chapter brief states the module graph has "only ten
edges across 25 modules" and that "`lib.rs` references no other module". Both are
wrong, and the difference is load-bearing. Counting non-doc-comment references
(a line whose trimmed form starts with `//` is excluded; `use crate::X`,
`super::X`, `penpot_desktop::X` and `X::` are all counted), the graph has **48
module→module edges**, of which **30 cross the proposed crate seams**, and
`lib.rs` alone has **12 outgoing edges**. The ten-edge figure is roughly what you
get if you *only* count `use crate::…` import lines and skip path-qualified
call sites like `.merge(navprobe::router())`. The corrected table is in
[Appendix A](#appendix-a--the-real-module-graph).

---

## Q1 — the verb list

All ten proposed verbs survive. Six more are added; none are dropped.

| Verb | How it was exercised | Result (observed) |
|---|---|---|
| `export` | assembled one-shot from public pieces: `PenpotClient::get_file` → `board_export::list_boards` → `unique_stems` → `export_payload` → `ExporterClient::render`, session cookie from `login_with_password` | 31 boards found; first two rendered to SVG (635 228 and 907 425 bytes) |
| `sync status` | live: `GET /__api/vault/strip`; offline: `sync_core::Manifest::load` | both worked; see [tiering](#q2--offlineonline-split) for what each one loses |
| `sync pause` / `resume` | `POST /__api/prefs {"syncEnabled":false}` | `{"ok":true,"needsReboot":false}`; `GET /__api/prefs` then reported `syncPaused:true` and the strip `"paused":true`; resume symmetric |
| `checkpoint` | in-process `checkpoint::{probe, decide, checkpoint}` | all three decisions exercised: `Init` (commit `470650c`, on a no-repo vault), `NoOp` ("nothing changed"), `Commit` (`b5eccd3`) |
| `vault list` | in-process `vault::VaultRegistry::load` | the active path plus its registry entries, fully offline |
| `vault switch` | live: `POST /open` on the control server; offline: registry write | live switch succeeded in **12.1 s** (full stack reboot); the offline path **silently diverges** — see RC-4 |
| `pkg add` | `POST /__api/packages/fetch` against a `file://` git repo | `{"cloned":true,"id":"pkgsrc"}`; `fetch_package` is a bare `git clone` in `spawn_blocking` (code-verified), so no backend is involved |
| `pkg list` | live: `GET /__api/packages`; offline: `sync_core::Lockfile::load` | live carries `live:true` (the DB-wipe witness); offline carries the pins only |
| `new --from-template` | `POST /__api/templates/new {"templateId","name"}` | 200 with `fileId`, `pageId`, `settleCycles:2`, `deepLink`; the `.penpot` tree was on disk within ~12 s |
| `open <path>` | in-process `docopen::resolve` | `InVault { file_id, title }` for a synced doc; `NotAPenpot { reason: … }` for a missing one — both offline |

### Verbs to add

- **`pkg install <id>`** — must be its own verb, not folded into `pkg add`. Fetch
  and install are separate endpoints, *and* the id is derived from the URL
  basename by `packages::id_from_url`, not read from `package.json`. My test repo
  declared `"id":"spike-kit"` but installed under `pkgsrc` (the clone directory
  name); `pkg-install spike-kit` failed with `package "spike-kit" not found under
  .penpot-packages (fetch it first)`. So `penpot pkg add <url>` **must print the
  derived id**, and `pkg install` must take that id.
- **`prefs get`** / **`prefs set`** — not cosmetic. `GET /__api/prefs` is the only
  place that reports `syncPaused`, `rendersRunning`, the active `vault`, and
  `lastOp` together, and `POST /__api/prefs` is the only loopback write path for
  the live sync/renders toggles (RC-5).
- **`reboot`** — the necessary completion of `prefs set` whenever it answers
  `needsReboot:true` (`POST /__api/prefs/reboot`, ack-then-detach).
- **`boards`** (and later `search`) — `vault-index` already serves
  `/__api/vault/boards`; observed 31 boards with `relPath`, `deepLink`, project.
  One GET, zero new code.
- **`tpl list`** — `templates::enumerate_catalog` over the runtime bundle;
  observed 15 builtin templates, 0 dropped. Fully offline, and it is the input a
  user needs before `new --from-template`.
- **`doctor`** / **`status`** (naming TBD) — a "is the app running, on which port,
  which vault, is the daemon paused" probe. This one is *forced* by RC-1 and
  RC-14: nearly every verb has to branch on whether the app owns the data dir, and
  today nothing on disk answers that question.

### Verbs NOT to add (yet), with reasons

- `pkg publish` / `pkg link` — endpoints exist (`/__api/packages/publish`,
  `/__api/packages/link`) but they are library-authoring verbs, not consumer
  verbs; out of the chapter's scope.
- `delete` / `rename` / `move` / `duplicate` — `manage.rs` serves them, but
  `delete` is the one verb that pauses the daemon *with an idle barrier*
  (`pause_and_wait_idle`) and takes a process-local `delete_lock`. Exposing it to
  a CLI is a correctness question, not a plumbing one. Defer.
- `reveal` — works (`home.rs` shells out via `reveal::reveal`), but a CLI that
  opens Finder is a worse `open` than the shell already has.

---

## Q2 — offline/online split

The proposed binary split ("needs a running backend" vs "doesn't") is not the
shape the system actually has. There are **three** tiers, because "needs the
backend" and "needs the running app" are different axes.

### Tier 1 — pure offline (no app, no backend; reads/writes the tree + data dir)

`vault list`, `checkpoint`, `open` (resolution only), `tpl list`, `prefs get`
(file only), `pkg list` (lockfile subset), `sync status` (manifest subset).

Verified: every one of these produced correct output with the stack **stopped**.
`checkpoint` in particular committed the package file that had just synced, with
the backend gone.

What the offline variants lose:
- `pkg list` offline has the pins but **not** `live` — whether the pinned
  `fileId` is still in the DB. That field is the DB-wipe witness and it requires
  the backend.
- `sync status` offline has `lastSyncedHash`/`lastSyncedAt` per file but **not**
  `paused`, `lastError`, or the per-file `state`/`conflict` classification —
  those live only in the daemon's in-memory `SyncStatusSnapshot` (RC-10).

### Tier 2 — needs the running *app*, but the work itself is disk-only

`pkg add` (a `git clone`; no RPC), `boards` / `search` (vault-index reads disk),
`sync status` (live snapshot), `sync pause`/`resume`, `prefs get`/`set`, `reveal`.

These are Tier 2 not because of the backend but because the code that does them
sits behind the app's proxy router. `pkg add` is the clearest case: nothing in it
talks to Penpot at all, yet a CLI cannot run it without the app up, because
`fetch_package` is a private axum handler (RC-6).

### Tier 3 — needs a running backend

`export` (backend `get-file` + the exporter child; the artifact download leg goes
through the proxy's `/assets` X-Accel path per `board_export::exporter`'s code and
module doc — I did not test it with the proxy down), `new --from-template` (`import-binfile` + settle), `pkg install`
(same), `vault switch` (tears the stack down and boots the target),
`reboot`.

### The fourth case the plan has to decide

For a Tier 2 or Tier 3 verb with the app **not** running, the CLI's only option is
to boot its own stack. Measured: **11 s to READY** on an already-provisioned data
dir (dev build; the cost is JVM + postgres start, so a release build won't help
much). First boot on a fresh data dir is minutes. That makes self-boot viable for
batch use (`penpot export` in CI) and unusable for anything interactive — and it
is *mutually exclusive* with a running app (RC-1).

---

## Q3 — crate-map verdict: REVISED

Three modules are in the wrong crate. Two of the three make the proposed map
**uncompilable**, not merely awkward: they create dependency cycles between
crates.

### Required change 1 — `navprobe` cannot stay in `apps/desktop`

`lib.rs:1074` — `.merge(navprobe::router())`. `boot()` mounts it. A library crate
cannot depend on the binary crate. `navprobe.rs` is 39 lines of pure axum route
serving a compiled-in HTML page and has zero Tauri references.

**Move `navprobe` → `crates/web-ui`.**

### Required change 2 — `status` cannot stay in `apps/desktop`

`lib.rs:1174` — `status::MockStatusSource::demo_frames()`, in `boot()`'s N3 strip
feeder. Same illegal direction. `status.rs` (453 lines) has zero Tauri references;
it is `DaemonStatusBridge` / `ExportStatusBridge` / `MockStatusSource`, all plain
`tokio::watch` plumbing. Its consumers are `lib.rs` (app-core), `tray/` and
`main.rs` (desktop) — so it must sit somewhere both can reach.

**Move `status` → `crates/app-core`.** It has no outgoing module edges, so it
lands as a leaf there. (A standalone `crates/status` would also work; app-core is
fewer moving parts.)

### Required change 3 — `prefs_http` cannot live in `crates/web-ui`

This is a genuine two-way cycle, and it is the one static analysis is most likely
to miss because one direction is an import and the other is a call site:

- `prefs_http.rs:78` — `use crate::control::RunnerSlot;` → **web-ui → app-core**.
  `PrefsState` *holds* the slot, and the handlers call `runner.active()`,
  `sync_control()`, `export_status()`, `set_renders_enabled()`,
  `reboot_in_place()`.
- `lib.rs:1064` — `prefs_http::router(config.data_dir.clone(), runner_slot.clone())`
  → **app-core → web-ui**.

**Move `prefs_http` → `crates/app-core`.** It is not a page module; it is the
stack-control surface that happens to serve one HTML page. `web-ui` then keeps
`home` + `navprobe` (+ `home.html`, `palette.html`, `navprobe.html`), and its only
outgoing edge is `home.rs:241 crate::reveal::reveal(&abs)` → `vault-ops`, which is
acyclic.

The alternative — invert the dependency by defining a `StackControl` trait in
`web-ui` and implementing it for `VaultRunner` in `app-core` — is a real
refactor, not a move, and Phase 2 is explicitly move-only. Recommend the move; do
the trait later if `web-ui` ever needs to be independently testable.

### Revised map

| Crate | Modules | Depends on |
|---|---|---|
| `crates/installer` | `installer` | — |
| `crates/app-state` | `prefs`, `recent`, `last_op` | — |
| `crates/packages` | `packages`, `templates` | `installer` |
| `crates/vault-ops` | `vault`, `checkpoint`, `gitinit`, `manage`, `docopen`, `reveal` | `installer` |
| `crates/web-ui` | `home`, **`navprobe`** | `vault-ops` |
| `crates/app-core` | `lib.rs`(boot), `control`, `layout`, `preflight`, `windows`, **`status`**, **`prefs_http`** | `app-state`, `packages`, `vault-ops`, `web-ui` |
| `apps/desktop` (stays) | `main`, `menubar/`, `tray/`, `overlay`, `dialog`, `navwatch` | all of the above, incl. `installer` directly |

Changes in **bold**. `navprobe`, `status`, `prefs_http` moved; nothing else.

### Task-ordering consequence

`app-core` now depends on `web-ui`, so **`web-ui` must be extracted before
`app-core`** — the reverse of the order in the chapter table. Extraction order:

```
installer → app-state → packages → vault-ops → web-ui → app-core → (desktop shrinks)
```

`packages` and `vault-ops` are independent of each other and can be done in
either order or in parallel.

### One more thing to write into the plan, not a map change

`apps/desktop` needs `installer` as a **direct** dependency
(`menubar/mod.rs:1033`, `:1043` call `crate::installer::default_project_id` /
`first_page_id`). Easy to miss because the module map reads as if only
`vault-ops` and `packages` use it.

### Two claims from the static pass that DO hold

- `vault.rs → control.rs` is a doc-comment link only (`vault.rs:4`,
  `[crate::control]` in a rustdoc link). No real edge, no cycle. Confirmed.
- `manage.rs → installer.rs` (`manage.rs:331`) is real and is the reason
  `installer` is its own leaf crate. Confirmed. Five further consumers exist:
  `packages` (5 refs), `templates` (3), `menubar` (5).

I also checked six other suspicious-looking edges and they are all doc-comment
only: `docopen → menubar`, `last_op → prefs_http`, `last_op → vault`,
`windows → tray`, `prefs_http → overlay`, `menubar → status`. In particular
`windows → tray` would have been a second app-core↔desktop cycle if real.

---

## Runtime couplings — the half the import graph cannot show

### RC-1 (hard constraint) — the data dir is exclusive, enforced by postgres

**Observed.** With one stack running, I started a second `headless` on the **same
data dir** with a completely disjoint port set (proxy 9064, backend 6523,
postgres 5598, valkey 6541, exporter 6543). It died in ~2 s:

```
pg_ctl: another server might be running; trying to start server anyway
pg_ctl: could not start server
Error: supervisor failed to start
   0: postgres: Command error: stdout=waiting for server to start.... stopped waiting
```

This is not a new discovery — `main.rs:114-117` already records it as an M4
finding, which is why `tauri_plugin_single_instance` is registered first — but it
had never been stated as a *CLI* constraint, and it is the single biggest one:

> The `penpot` CLI can never be "a second stack". Per invocation it is either
> **(a)** a client of the running app over loopback, or **(b)** the sole owner of
> the data dir. There is no third mode, and the two are mutually exclusive.

### RC-2 — self-boot costs ~11 s warm

**Observed.** `headless` on an already-provisioned data dir reached `READY` in
**11 s**. Mode (b) is a batch mode only.

### RC-3 — there is no advisory file locking anywhere in the workspace

**Observed.** `grep -rE 'flock|FileLock|lock_exclusive|fs2::'` over `crates/` and
`apps/` returns nothing. The only cross-process mutual exclusion in the whole
system is (i) postgres's `postmaster.pid` (RC-1) and (ii)
`tauri_plugin_single_instance`, which is GUI-only *and* forwards argv to the
running instance rather than failing.

Every mutable state file — `vaults.json`, `preferences.json`, `lock.json`,
`recent-files.json`, `last-op.json`, `.penpot-sync.json`, `plugin-consent.json` —
is written via atomic rename, so a write is never *torn*. But a
read-modify-write from two processes silently loses one side, last-writer-wins.
Every mutex that exists (`VaultRunner.switch_lock`, `ManageState.delete_lock`,
`manage_sync: OnceLock<SyncControl>`) is process-local and gives a CLI nothing.

### RC-4 (observed divergence) — offline `vault switch` lies to the running app

**Observed.** From the CLI, with the app running, I wrote
`vaults.json.active = <vault2>` through the public `VaultRegistry` API. Then:

- `GET /active` on the running app: still the **old** vault.
- `vaults.json` on disk: `active` = vault2.

`VaultRunner` caches `active: Mutex<VaultRef>`, populated once at boot. So the
process and the disk disagree, and the *next* app start would silently open a
vault the user never chose. Doing the same switch through `POST /open` worked
correctly (12.1 s, `/active` updated).

Consequence for the CLI: any verb that mutates `vaults.json` must refuse while
the app is up, or route through the app. That guard does not exist and cannot be
written today, because of RC-14.

### RC-5 — `sync pause` exists over loopback, but only as a *persisted preference*

**Observed.** I probed the control server's whole route surface:

```
/health   -> 200 ok
/active   -> 200 {...}
/list     -> 200 {...}
/windows  -> 200 {"windows":[]}
/sync/pause -> 404
/pause      -> 404
```

There is no dedicated pause route. The only loopback pause is
`POST /__api/prefs {"syncEnabled":false}` — which worked (verified through both
`GET /__api/prefs` `syncPaused:true` and the strip's `"paused":true`). Two
consequences:

1. **Semantics.** That route also *persists* the preference, so
   `penpot sync pause` would be sticky across reboots. If the intended verb is
   "pause until I resume", it needs its own surface.
2. **Race.** `post_prefs` calls the bare `SyncControl::pause()` flag flip. The
   barrier version, `pause_and_wait_idle()`, is used by exactly one caller in the
   workspace — `manage::delete_file` — precisely because a poll cycle already in
   flight keeps writing regardless of the flag. So a CLI that pauses and then
   immediately touches the vault races the daemon. `manage.rs`'s module doc spells
   out why that matters; a CLI pause inherits the hazard without inheriting the
   fix.

### RC-6 (the biggest plan gap) — the verbs live inside private axum handlers

Every actual mutation is a private `async fn` taking `State<Arc<…>>`:
`fetch_package`, `install_package`, `do_install`, `do_publish`, `do_link`,
`list_packages`, `list_plugins`, `updates`, `preserve_drift`,
`new_from_template`, `import_new_from_template`, `new_project`, `new_file`,
`rename`, `move_files`, `duplicate_file`, `delete_file`, `strip`, `thumb`,
`reveal_action`, `get_prefs`, `post_prefs`, `post_reboot`, `post_vault`. Only the
*pure* helpers next to them are `pub` (`is_safe_id`, `id_from_url`,
`classify_update`, `compute_updates_model`, `valid_name`, `decide`, `probe`,
`resolve`, `enumerate_catalog`, `build_strip_model`, …).

So a CLI has exactly two options per Tier 2/3 verb: go over HTTP, or the owning
crate must grow a handler-free verb core. **Moving a module does not create that
core.** This is an API-design task the plan does not currently contain. The
codebase already has the right shape to copy: `packages::reapply_links` and
`packages::reapply_plugin_props` are `pub async fn` verb cores with no axum in
the signature — boot reaches them through the thin `spawn_relink_reconcile` /
`spawn_plugin_reconcile` retry wrappers, and a CLI could call the cores directly.

Recommendation: keep Phase 2 move-only as planned, and add a Phase-3 task
"promote verb cores" *before* the task that writes the real CLI. Trying to do it
during the moves will make the "green suite before and after" oracle useless.

### RC-7 — some public verb functions *can* be driven out-of-process, and that is a hazard

**Observed.** In the CLI process I constructed a full `packages::PackagesState`
(token from `credentials.json`, team id, proxy origins, a fresh
`spawn_updates_poller`) and ran, against the live vault and DB, while the app was
running its own copies of the same reconcilers:

```
relink  = Ok(RelinkSummary { applied: 0, blocked: 0 })
plugins = Ok(0)
```

Nothing detected or prevented the second writer (see RC-3). Both reconcilers
write `lock.json`; the plugin one also writes the consent ledger and calls
`update-profile-props`. A CLI that offers these as in-process verbs would be a
silent concurrent writer to app-owned state.

Also note the shape problem this exposed: constructing the state *required*
spawning a second `updates_poller` background task, because
`PackagesState.updates_rx` is a `watch::Receiver` with no non-spawning
constructor. Live-service state is baked into what should be a plain verb input —
worth fixing when the verb cores are promoted (RC-6).

### RC-8 — `boot()` is the cycle-maker

`lib.rs::boot` (lines 892–1260) builds every router state in one function and
merges eight routers. That is *why* `app-core` ends up depending on `packages`,
`vault-ops`, `web-ui`, `app-state` at once — all fine, all downward — and it is
also why the `navprobe`/`status` assignments in the proposed map are impossible.
Splitting `boot` is **not** required for the revised map. Don't do it in Phase 2.

### RC-9 — `control` and `boot` are mutually recursive; the map is right to co-locate them

`control.rs:57` `use crate::{boot, AppConfig, RunningApp};` and `lib.rs:892`
`pub async fn boot(config: AppConfig, runner_slot: control::RunnerSlot)`.
`control::boot_active_vault` calls `boot`, and `boot` takes a `control` type.
These cannot be separated without a trait. Putting both in `app-core` is correct.

### RC-10 — three separate late-binding mechanisms, all inside `boot`, none reachable by a CLI

This is the live-state answer to the brief's question:

1. `RunnerSlot = Arc<AsyncMutex<Option<Arc<VaultRunner>>>>` — created in
   `control::boot_active_vault`, passed *into* `boot`, handed to
   `prefs_http::router`, and filled **after** the stack exists. The Preferences
   routes therefore hold a handle to the very stack they are mounted on.
2. `manage_sync: Arc<OnceLock<SyncControl>>` (`lib.rs:1047`) — the delete route's
   pause handle, `.set()` at `lib.rs:1142` once the daemon spawns. A delete
   arriving before that is rejected, not silently un-paused.
3. `strip_tx`/`strip_rx` (`lib.rs:980`) — the home strip's status channel exists
   before the daemon; the feeder task is spawned after it.

All three are process-local and un-reconstructable from outside. They are the
concrete reason the mutating verbs must be reached over loopback rather than by
linking the library — and, per RC-9, the reason `control` and `boot` are one
crate.

### RC-11 — `checkpoint` is the cleanest verb in the set

**Observed.** It ran correctly with the daemon live (`Init`, commit `470650c`),
correctly no-op'd on an immediate re-run, and correctly committed with the stack
fully **down** (`Commit`, `b5eccd3`). It never touched the DB, took no lock, and
needed no token. It is a pure Tier-1 verb.

**Unverified caveat:** I did not construct a checkpoint-vs-daemon-write race. The
daemon swaps `.penpot` trees two-phase (`sync_core::commit_dir_swap`), so a
*tree* is never half-written, but nothing makes the tree swap and the
`.penpot-sync.json` write atomic with respect to each other — so a checkpoint
could in principle commit a tree newer than the manifest. Plausible, untested.

### RC-12 — credentials are readable but not exposed

`Credentials` is `pub` and `Deserialize`, but the filename constant
(`CREDENTIALS_FILE`) and the loader (`provision`) are private in `lib.rs`. My
spike hard-coded `credentials.json` and hit a real trap: the file is **snake_case**
(`access_token`, `profile_id`), and a `camelCase` deserialize silently yielded
`access_token: None` and then an HTTP 401 `authentication-required` from the
backend — a wrong-looking auth failure rather than a parse error. Every Tier-3
verb needs this. `app-core` (or `app-state`) should expose
`pub fn load_credentials(data_dir: &Path) -> Option<Credentials>` when the verb
cores are promoted.

### RC-13 — `pkg add` derives the id from the URL, not the manifest

Covered in Q1; recorded here too because it is a behaviour a CLI surface has to
expose, not hide.

### RC-14 — the CLI has no way to discover a running app

**Observed.** Nothing in the data dir records a pid or a live port: after a full
run the dir held only `assets/`, `credentials.json`, `designs/`, `exporter-tmp/`,
`postgres/`, `preferences.json`, `secret.key`, `valkey/`, `vault-index/`,
`vaults.json`. And the control server is **opt-in**: `main.rs:528` only starts it
when `PENPOT_LOCAL_CONTROL_PORT` is set, which no default install sets — so in a
normal GUI install `/open`, `/active`, `/list` and `/windows` do not exist at all,
and `vault switch` has no loopback surface.

That leaves the proxy (`/__api/*`, default port 8686) as the only always-on
out-of-process surface, discoverable only by guessing the default or reading the
env var. Given RC-1 and RC-4, the CLI *must* be able to answer "is the app
running, and where" before almost every verb. Something has to give: a small
runtime record in the data dir (pid + proxy port + control port, cleaned up on
shutdown), or making the control server unconditional. **This is a design decision
the chapter needs to make; it is not resolved by any crate move.**

Note that the pieces for `penpot open <path>` in the running-GUI case already
exist and need no new surface at all: `main.rs:129`'s single-instance callback
takes `argv`, calls `document_arg(argv)`, and forwards a `.penpot` path to
`menubar::open_document`. `penpot open <path>` can simply exec the app bundle
with the path.

---

## What I could not verify

- **Windows/Linux.** Everything here is macOS (darwin 25.5.0). `dialog.rs` is
  `osascript`-only; `reveal.rs` branches per-OS. Nothing about the crate map
  depends on the platform, but the `export` path's exporter-child behaviour was
  only exercised on macOS with the pinned bundle.
- **The checkpoint/daemon write race** (RC-11).
- **Concurrent-writer data loss** (RC-3). I proved the *absence of locking* by
  code search and proved a second writer *runs* (RC-7); I did not construct a
  losing interleaving.
- **`export` for a file with no `.exports` state, and multi-file export.** I
  rendered 2 of 31 boards on one file. The full `board_export` service's
  debounce/atomic-swap path was **not** exercised by the CLI — my one-shot wrote
  plain `.svg` files, deliberately bypassing `commit_dir_swap`. A real
  `penpot export` should reuse the service's write path, not my shortcut.
- **`pkg install` on a plugin-kind package** (E7). Only a `design-data` package
  was installed.
- **Whether `apps/desktop` still compiles after the moves.** That is Tasks 2–8's
  job; this spike changed no module locations.

## Verification state of the tree at the time of writing

`cargo test --workspace` — **600 passed, 0 failed**. No production code was
changed by this task; the spike binary was added and deleted in the same commit.

---

## Appendix A — the real module graph

Non-doc-comment references only. 48 module→module edges; 30 cross a proposed
crate seam. Crate labels use the **revised** map, so a `[desktop -> app-core]`
label is legal (binary → library) and there are no remaining cycles.

| Edge | Refs | Crate seam (revised map) |
|---|---|---|
| `control → vault` | 12 | app-core → vault-ops |
| `home → reveal` | 1 | web-ui → vault-ops |
| `manage → installer` | 1 | vault-ops → installer |
| `packages → installer` | 5 | packages → installer |
| `templates → installer` | 3 | packages → installer |
| `lib → layout` | 17 | (internal to app-core) |
| `lib → packages` | 6 | app-core → packages |
| `lib → prefs` | 4 | app-core → app-state |
| `lib → templates` | 2 | app-core → packages |
| `lib → manage` | 2 | app-core → vault-ops |
| `lib → checkpoint` | 1 | app-core → vault-ops |
| `lib → home` | 1 | app-core → web-ui |
| `lib → navprobe` | 1 | app-core → web-ui *(after move; was illegal)* |
| `lib → status` | 1 | internal to app-core *(after move; was illegal)* |
| `lib → prefs_http` | 1 | internal to app-core *(after move; was a cycle)* |
| `lib → control`, `lib → preflight` | 1, 1 | internal to app-core |
| `prefs_http → prefs` | 7 | app-core → app-state *(after move)* |
| `prefs_http → last_op` | 4 | app-core → app-state *(after move)* |
| `prefs_http → control` | 1 | internal to app-core *(after move; was the cycle)* |
| `checkpoint → gitinit` | 1 | internal to vault-ops |
| `control → windows` | 1 | internal to app-core |
| `menubar → docopen` | 23 | desktop → vault-ops |
| `menubar → dialog` | 29 | internal to desktop |
| `menubar → recent` | 9 | desktop → app-state |
| `menubar → installer` | 5 | desktop → installer |
| `menubar → windows` | 3 | desktop → app-core |
| `menubar → navwatch`, `→ overlay`, `→ tray`, `→ reveal` | 2, 4, 1, 1 | desktop-internal except `reveal` (→ vault-ops) |
| `main → menubar` | 12 | internal to desktop |
| `main → control` | 4 | desktop → app-core |
| `main → tray`, `→ dialog`, `→ overlay`, `→ status`, `→ navwatch` | 4, 3, 3, 3, 1 | desktop-internal except `status` (→ app-core, after move) |
| `main → windows`, `→ preflight` | 1, 1 | desktop → app-core |
| `tray → status` | 3 | desktop → app-core *(after move)* |
| `tray → checkpoint` | 2 | desktop → vault-ops |
| `tray → reveal` | 2 | desktop → vault-ops |
| `tray → gitinit` | 1 | desktop → vault-ops |
| `tray → dialog`, `→ overlay` | 4, 2 | internal to desktop |
| `bin/headless → control` | 3 | desktop → app-core |
| `bin/headless → windows` | 1 | desktop → app-core |

Doc-comment-only (no real edge): `vault → control`, `docopen → menubar`,
`last_op → prefs_http`, `last_op → vault`, `windows → tray`,
`prefs_http → overlay`, `menubar → status`, `control → prefs`.

### Appendix B — Tauri reference counts, for calibrating "what must stay"

Modules with **zero** Tauri references: every one of the 25 except `main.rs`
(20 refs), `overlay.rs` (9), `menubar/mod.rs` (36) and `tray/mod.rs` (12).
`reveal.rs`'s single hit is a doc comment explaining why it does *not* use
`tauri-plugin-opener`.

Worth noting for the record — this does not change the map, but it explains why
`apps/desktop` will still be large after Phase 2: `dialog.rs` (320 lines,
`osascript`), `navwatch.rs` (407, the route-classification table),
`menubar/model.rs` (694) and `tray/model.rs` (752) are all Tauri-free pure logic
that the map keeps in the binary. That is a defensible call — they are the GUI
shell's *policy* — but it means "GUI-free" and "stays in apps/desktop" are not
the same predicate, and roughly 2,200 Tauri-free lines stay behind by choice.
