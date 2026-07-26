# penpot-cli: library extraction, then a third front-end

**Date:** 2026-07-26
**Status:** shipped, **with four corrections** — see [What shipped differed](#what-shipped-differed) below. The
design's argument (extract the library, not a process; the CLI is a third front-end) survived
intact; four of its concrete claims did not, and each is corrected in place rather than quietly
edited away.
**Supersedes:** the PLAN2.md:272 non-goal, which deferred this to "its own future chapter". This is that chapter.

## What shipped differed

Four claims below are wrong, and the reasons are worth more than the corrections. Each is also
flagged inline at the section it affects.

**1. The crate map moved three modules, and reversed the last two extractions.**
`navprobe` went to `crates/web-ui`, not `apps/desktop`; `status` and `prefs_http` went to
`crates/app-core`, not `apps/desktop`. *Cause:* the map was derived by scanning modules for
`use` statements, which cannot see a call site that imports nothing. `boot` calls
`.merge(navprobe::router())` and `status::MockStatusSource::demo_frames()`; leaving either module
in `apps/desktop` would have made a library crate depend on the binary crate. `prefs_http` was a
real two-way cycle — `use crate::control::RunnerSlot` one way, `prefs_http::router(...)` the other
— which an import-only analysis sees as a single edge. *Consequence:* `app-core` depends on
`web-ui`, so **`web-ui` is extracted BEFORE `app-core`**, the reverse of the table's numbering.
Found by the Phase-1 spike, whose whole job was to surface what the static graph could not.

**2. The offline/online split is three tiers, not two.** *Cause:* "needs the backend" and "needs
the running app" are different axes, and the two-way split conflated them. `pkg add` is a
`git clone` that never touches Penpot, yet a CLI cannot run it with the app down, because
`fetch_package` is a private axum handler. Tier 1 is pure offline; Tier 2 needs the running app
but the work is disk-only; Tier 3 needs the supervised backend as well. A second thing the binary
split had no way to express: two Tier-1 answers are real but **incomplete** (`pkg list` cannot see
`live`; `sync status` cannot see `paused`/`lastError`/`state`/`conflict`), so they ship
`"partial": true` plus a `missing` array naming exactly what is absent.

**3. "No script changes" was too strong, and so was "no change to `just e2e`".** *Cause:* the
claim conflated *launch commands* with *scripts*. The launch commands really were untouched —
`bin/headless` kept its path, its `READY <url>` stdout contract and its `STOPPED` line, so not one
gate's launch line changed. But five existing scripts needed real edits, and a sixth
(`c2-cli.sh`) is new. `d3-menus.sh` and `d5-documents.sh` pin unit tests BY NAME per crate, so
their `cargo test -p <package>` flag moved when the modules did — the test names themselves stayed
byte-identical, which is what kept the edit to one flag. `n5-vaults.sh`, `d5-documents.sh` (via
`d5_docs_helper.py`) and `e6-library-portability-spike.sh` drive the control port, which is now
always-on and therefore token-authenticated. And `d1-offline.sh`'s port-ledger reasoning had to be
restated, because an always-on control server binds an ephemeral port even when a gate never asks
for one (a LISTEN socket, which its egress parser ignores — the offline claim is unchanged, the
argument for it is not). A further eight scripts and helpers carried `apps/desktop/src/...` path
references the extraction made stale; those are comment-only repairs. `just e2e` gained two links,
`c1-crates.sh` and `c2-cli.sh`.

**4. `export` is not implemented.** It is listed as a candidate verb and as an Online verb; it
shipped as neither. *Cause:* `crates/board-export` registers no routes at all. It is a poller keyed
off `.penpot-sync.json` hash movement, enabled at boot from `exports_enabled` and live-toggled
through `VaultRunner::set_renders_enabled` — there is no "export now" surface for a CLI to call.
Hand-assembling `PenpotClient` + `ExporterClient` in the CLI's own process is exactly the
second-writer boundary the spike's RC-7 rules out, and wiring the verb to
`POST /__api/prefs {"exportsEnabled":true}` would be worse than omitting it: it returns
immediately, while a caller typing `penpot export` expects rendered files to exist. The verb needs
a handler-free verb core in `board-export` plus a route the app serves — the RC-6 "promote verb
cores" work this chapter deliberately avoided by routing Tier-2/3 over loopback. Recorded at the
point of omission in `apps/cli/src/verbs/loopback.rs`'s module doc, under
"`export` is deliberately absent".

## The question

Does it make sense to extract a `penpot-cli` — in the style of `arduino-cli` — that holds the
core functionality and runs behind the desktop app?

## The answer

Partly. Extract the **library**; the CLI then costs almost nothing. Do not extract a **process**.

`arduino-cli` was carved out because the core logic sat trapped inside a Java IDE while the new
IDE was written in TypeScript. A language boundary forced a process boundary, and gRPC paid for
itself. Here the shell and the core are both Rust, so a crate boundary already delivers the same
"one implementation, many consumers" guarantee without serialization, a versioned IPC contract,
or cross-process debugging.

The real problem is narrower and duller: `apps/desktop/src` holds 16,526 lines, and only about
3,100 of them touch Tauri. The rest is domain logic living in an application crate by historical
accident. Because `apps/desktop/Cargo.toml` depends on `tauri`, every unit test of `packages.rs`,
`vault.rs`, and `checkpoint.rs` links the whole Tauri toolchain to exercise pure logic.

The app already runs three front-ends, not one. `home.rs` (`/__home`) and `prefs_http.rs` serve
framework-free HTML pages that talk to an in-process API — they are thin clients already. The CLI
becomes the third front-end over the same core, not a new layer beneath it.

## Scope

The CLI stays **internal**. It carries no stability contract, no versioned public surface, and no
end-user documentation. It may change in any later chapter. This is what makes it cheap.

## Target architecture

The existing crates (`sync-core`, `sync-daemon`, `penpot-rpc`, `supervisor`, `proxy`,
`board-export`, `vault-index`) do not change. All the work happens inside `apps/desktop`.

> **Corrected (1).** The table below is the map as designed, kept for the record. As shipped:
> `navprobe` is in `crates/web-ui`; `status` and `prefs_http` are in `crates/app-core`; and
> `web-ui` (6) is extracted BEFORE `app-core` (5), because `app-core` depends on it. See
> [What shipped differed](#what-shipped-differed).

| # | Crate | Modules | Lines | Depends on |
|---|---|---|---|---|
| 1 | `crates/installer` | `installer` | 127 | — (leaf) |
| 2 | `crates/app-state` | `prefs`, `recent`, `last_op` | 613 | — |
| 3 | `crates/packages` | `packages`, `templates` | 3,422 | `installer` |
| 4 | `crates/vault-ops` | `vault`, `checkpoint`, `gitinit`, `manage`, `docopen`, `reveal` | 2,662 | `installer` |
| 5 | `crates/app-core` | `lib`(boot), `control`, `layout`, `preflight`, `windows` | 3,234 | `app-state`, `vault-ops` |
| 6 | `crates/web-ui` | `home`, `prefs_http` | 821 | `app-core`, `app-state`, `vault-ops` |
| — | `apps/desktop` (remains) | `main`, `menubar/`, `tray/`, `overlay`, `dialog`, `navwatch`, `navprobe`, `status` | 5,647 | all of the above |
| — | `apps/cli` (new) | — | — | all of the above |

The line counts sum to 16,526, the current size of `apps/desktop/src`. `apps/desktop` keeps 5,647
rather than the ~3,100 that touch Tauri, because it also keeps the GUI's own model layer — see
below.

### Why the boundaries fall here

The module dependency graph holds only ten edges across 25 modules, and `lib.rs` references none
of the other modules. `vault.rs → control.rs` looks like a cycle against `control.rs → vault.rs`,
but the first is a doc-comment link, so no cycle exists.

`installer` earns its own crate because `manage.rs` — a vault verb — imports it, and so do
`packages.rs` and `templates.rs`. Folding it into `packages` would force `vault-ops` to depend on
`packages`, which inverts the real relationship.

`reveal.rs` and `windows.rs` read as GUI modules but are pure: `reveal.rs` documents its
deliberate avoidance of `tauri-plugin-opener`, and `windows.rs` calls itself "deliberately free of
Tauri types". Both move into libraries.

`status.rs` stays in `apps/desktop`. No web page imports it; only the tray and the menubar do. It
is GUI model code, not domain code. For the same reason `apps/desktop` keeps `tray/model.rs` and
`menubar/model.rs` — 1,446 pure, well-tested lines. The GUI keeps its own testable layer rather
than exiling it.

## Phase 1 — Spike the verbs (throwaway)

Write a deliberately rough `apps/desktop/src/bin/cli.rs`: inside the current crate, Tauri linked,
ergonomics ignored. This is not the CLI. It answers questions the static graph cannot.

The static analysis above already found the seams, including the cross-boundary
`manage.rs → installer.rs` edge. The spike's remaining job is narrower: confirm the verb list, and
surface the **runtime** couplings that imports never show — shared locks, boot ordering, live
state held across calls.

Candidate verbs, drawn from the module docs. The spike ran every one of them and added more
(`tpl list`, `prefs get|set`, `sync resume`, `boards`, `search`, `reveal`, `pkg install`,
`reboot`, `daemon`); the shipped set is the `Command` enum in `apps/cli/src/main.rs`. Only
`export` failed to ship — see [correction 4](#what-shipped-differed):

- `export` — **not implemented**; `crates/board-export` serves no route to call
- `sync status`, `sync pause`
- `checkpoint`
- `vault list`, `vault switch`
- `pkg add`, `pkg list`
- `new --from-template`
- `open <path>`

**Exit criterion:** the verbs run against a real stack, and the crate map above is either
confirmed or rewritten with evidence. Then delete the file.

Phase 1 touches no script and does not move `bin/headless`.

## Phase 2 — Extract the crates

Move crates bottom-up, in the numbered order of the table. Each step is one `git mv` plus Cargo
wiring, with **no rewrites**. The green suite is the safety net, and it holds only while behaviour
stays fixed. If a step seems to need a logic change, stop that step and make the change a separate
commit before or after — never inside the move.

## Phase 3 — Build the real CLI

### Reaching the stack

Verbs split by nature:

- **Offline** — `checkpoint`, `vault list`, `pkg list`, path resolution for `open`. These read and
  write the vault tree and need no running backend. Fast, pure, cheap to test.
- **Online** — `export`, `sync status|pause`, `new --from-template`, `pkg add`. These need Penpot
  running.

> **Corrected (2).** This binary split is not the shape the system has. There are **three** tiers,
> because "needs the backend" and "needs the running app" are different axes — `pkg add` is a
> `git clone` with no RPC at all, and still cannot run with the app down. And two offline answers
> are real but incomplete, which a two-way split cannot say: `pkg list` and `sync status` ship
> `"partial": true` plus a `missing` array. See [What shipped differed](#what-shipped-differed).

Online verbs use **attach-or-fail**: the verb finds a running instance, or exits with a message
telling the user to start `penpot daemon`. It never boots a stack implicitly.

Booting postgres, valkey, and the JVM from a CLI invocation would cost more than 30 seconds, and
worse, it would let two stacks race over one data dir and one port set. The core invariant assumes
a single aggressive reconciliation at boot; two concurrent boots fall outside what it covers.

The attach point exists: `control.rs::serve_control` already answers `POST /open`, `GET /active`,
and `GET /list` on a localhost port. The daemon writes a `control.json` in the data dir so the CLI
can discover it.

> **Shipped as `runtime.json`** (`app_core::runtime_record::RECORD_FILE_NAME`), carrying `pid`,
> `proxy_port`, `control_port` and `token` — a rename plus two additions the design did not
> anticipate. The `pid` is what lets a CLI reject a record left behind by a killed app instead of
> dialing a dead port; the `proxy_port` is needed because the routes a CLI wants are split across
> two listeners, and the control server serves only its own.

### Securing the control port

Today the control server starts only when `PENPOT_LOCAL_CONTROL_PORT` is set, which happens only
in tests. Keeping it always on for the CLI to attach opens a real hole: any web page in any browser
can send `POST http://127.0.0.1:<port>/open`, because the same-origin policy blocks reading the
response, not sending the request. That endpoint wipes the DB and triggers a full reconciliation.

The daemon must therefore:

1. generate a random token at boot and write it to `control.json` with `0600` permissions;
2. require that token in a header on every request;
3. reject requests whose `Origin` or `Sec-Fetch-Site` marks them as browser-initiated.

### Keeping the harness still

`bin/headless` stays where it is, reduced to a three-line shim over the same entry point as
`penpot daemon`. No script changes. `scripts/m1-smoke.sh`, `m2-invariant.sh`, `m3-sync.sh`, and the
N-gates all launch `--bin headless` and wait for `READY <url>` on stdout; those suites are the net
protecting the whole extraction, and cutting the net in the chapter it protects is backwards.
Deleting the shim is a one-line change in a later chapter.

> **Corrected (3).** The shim half held exactly: `bin/headless` kept its path, its `READY <url>`
> contract and its `STOPPED` line, and not one gate's launch command changed. "No script changes"
> did not: five existing scripts needed real edits — two pin unit tests by crate, three drive the
> now-token-authenticated control port (`d5-documents.sh` is in both groups), and `d1-offline.sh`
> had to restate its port ledger — plus eight comment-only path repairs. See
> [What shipped differed](#what-shipped-differed).

## Verification

The extraction preserves behaviour, so the existing gates are the oracle: `cargo test --workspace`,
`just smoke`, `just invariant`, `just m3`, `scripts/m4-artifact-test.sh`, and `just m5` — green
**before and after each crate move**, not only at the end.

Two new gates carry the point of the exercise:

1. **No new crate depends on Tauri.** A `cargo tree` assertion in CI. Without it the extraction
   regresses silently at the first convenient `use tauri::`, leaving today's coupling spread across
   more files.
2. **Offline verbs run with no stack.** A smoke script exercises them against a fixture vault
   without booting Penpot. This proves the separation is real rather than nominal.

Both shipped, as `scripts/c1-crates.sh` (`just c1`) and `scripts/c2-cli.sh` (`just c2`). The second
grew past "offline verbs run with no stack", because that leg alone cannot fail for the reason that
matters: it stays green against a CLI that has no tier split at all. So it also asserts the ports
are still free AFTER the offline verbs run (nothing self-booted), that a Tier-2/3 verb with no
daemon exits non-zero naming `penpot daemon` and writes nothing to stdout, that the control port
refuses tokenless, wrong-token and browser-shaped requests, and that `runtime.json` is removed on
clean shutdown.

## Non-goals

- **No public contract.** The CLI is internal, unstable by design, and undocumented for outside
  users.
- **No process daemon.** The GUI does not become a client of a separate core process. Phase 3's
  HTTP attach point is precisely where that work would start if it is ever wanted. The argument
  against it now: its main prize — the GUI dies while sync survives — is already won by the core
  invariant. When the GUI dies and sync stops, nothing is lost, because the tree is the truth and
  the next boot reconciles.
- **No new functionality.** The CLI exposes verbs that already exist, which keeps the existing
  suites meaningful as an oracle.
- **No harness rewrite**, and no change to `just e2e`.

> **Corrected (3, second half).** The harness was not rewritten, but `just e2e` did change: it
> gained `c1-crates.sh` and `c2-cli.sh`, on the same precedent as every other chapter that landed
> product code. A chapter whose gates are not in the chain is a chapter the ladder cannot keep
> honest.
