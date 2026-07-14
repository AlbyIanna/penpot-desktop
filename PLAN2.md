# Penpot Local — chapter 2: THE VAULT

Chapter 1 (PLAN.md, M0–M5, all shipped and verified) built the engine: supervised embedded
Postgres/Valkey/backend-JVM, a custom proxy replacing nginx, two-way sync between the Penpot
DB (disposable cache) and a folder tree of unzipped, git-diffable `.penpot` directories with
conflict copies and OS rename mirroring, and a 386 MB offline dmg. The result is "upstream
Penpot + invisible sync superpowers" — the folder tree is the soul of the app, but the user
never sees it. Chapter 2 inverts: **the folder tree becomes the front door**. The app opens
on your library read straight off disk; the unmodified upstream canvas is the darkroom you
deep-link into to edit one file. The models: darktable's lighttable/darkroom split,
Obsidian's vault, and the Arduino IDE — a plain folder of plain files as the workspace, the
IDE a thin shell around a toolchain it doesn't own, an Examples menu built from files
shipped on disk. Every original surface lives beside or above the webview — never inside it.

The project's durable values are now written down in [docs/ecosystem-concept.md](docs/ecosystem-concept.md)
(the ecosystem design sketch they imply is in `docs/ecosystem-design.md`). Two of those
principles are load-bearing for this chapter: **surface, don't apply** (updates and conflicts
are shown, never applied silently — the conflict strip and "Checkpoint now" both instantiate it)
and **git repos, not a registry** (checkpoints and, later, package distribution are plain git).
When a chapter-2 feature touches sharing, versioning, or templates, it must not contradict those
values; where it can't yet honor them (e.g. the palette's "New file from template" verb, which
is the first foothold of the package ecosystem), it defers to N6 rather than half-building them.

## Vision

> Your designs are just a folder on your disk — and now the app treats it that way. It
> opens on a lighttable of every board in your vault, read straight off the disk: search
> the text inside any design, hit Cmd+K and be on the right board in two keystrokes, flip
> through boards like a contact sheet — all offline, before a single canvas loads. The
> unmodified Penpot editor is the darkroom you enter to work on one file; everything
> original lives beside the canvas, never inside it. A live strip shows every sync; when
> two versions of a file collide, both are preserved and shown. Point the app at any
> folder — empty, your existing tree, a git clone of a teammate's vault — and it becomes
> a working design library, because the database was always just a cache. Checkpoint the
> vault with one action; carry your history in git. Delete the app tomorrow and you lose
> nothing: readable JSON you can grep, diff, and back up.

**North-star story:** a designer opens onto a grid of every board across 12 client
projects, types Cmd+K "checkout button", lands on the exact board two keystrokes later,
fixes it, sees "synced 2s ago" on return, hits "Checkpoint now": one git commit, her folder.

## Invariants

Chapter 1's core invariant remains sacred and is still the test suite:

> Delete the entire database, restart the app, and every project/file is rebuilt from the
> folder tree with no data loss. P0 if it ever fails.

New invariants for chapter 2:

1. **Derived state is disposable.** Every chapter-2 artifact (search index, thumbnails,
   home-page state) lives OUTSIDE `.penpot` dirs (foreign files inside them get swept —
   PLAN.md:197-199), is rebuilt from disk alone, and is never an input to sync.
2. **Zero cross-vault spill.** Switching vaults must never surface, import, or write a
   file from another vault; user disk stays byte-untouched across switches. A spill is P0.
3. **The SPA stays byte-untouched.** All chapter-2 UI is proxy-served app pages, native
   shell chrome, or deep-link URL navigation. No serve-time patching of upstream JS/CSS,
   no plugin, no injected scripts. The only channels into the canvas are URLs.
4. The conflict rule and normalization spec from CLAUDE.md are unchanged; chapter 2 adds
   a *surface* for conflicts, never new conflict semantics.

## Product pillars (ordered)

1. **The Lighttable home.** Launch → a thumbnail grid of every board in the vault; filter
   by project, sort by recency; card click deep-links the main window to
   `/#/workspace?team-id&file-id&page-id` (route strings verified in the compiled bundle
   `runtime/frontend/js/main.js`; query-param destructuring verified in upstream 2.16.2
   `frontend/src/app/main/ui.cljs`; `routes-gate.sh` adds the live assert;
   `window.navigate` already drives the SPA — `apps/desktop/src/main.rs:110-137`). Served
   same-origin at `/__home` via `Proxy::bind_with_router` (`crates/proxy/src/lib.rs:143-164`)
   → auth cookie + full RPC for free. Permanent **escape hatch** verb to
   `/#/dashboard/recent`: the upstream dashboard is one click away, never lost.
   Thumbnails: render path decided by the N2 spike.
2. **The Vault Index — offline full-content search.** "checkout button" → every text layer,
   component, color, typography containing it, across all projects, one click from the
   exact board — search nobody else does offline. The corpus already exists: all design
   state is normalized JSON on disk (PLAN.md:30-53, `crates/sync-core/src/normalize.rs`).
   A bundled-SQLite FTS5 sidecar crate cloned from the board-export recipe: read-only
   manifest consumer + own state file, re-derive exactly when `lastSyncedHash` moves (the
   `needs_render` pattern, `crates/board-export/src/state.rs`).
3. **Activity/conflict strip.** A persistent strip on `/__home`: "homepage.penpot synced
   2s ago", and `Conflict{copy_path}` states rendered first-class with reveal-both-versions
   actions. Feeds off shipped plumbing: `SyncStatusSnapshot`/`ExportStatusSnapshot` watch
   channels + late-binding bridges (`crates/sync-daemon/src/status.rs:20-55`,
   `apps/desktop/src/main.rs:47-131`); `MockStatusSource` (tray-demo-only today,
   `PENPOT_LOCAL_TRAY_DEMO=1`) extends to feed the strip windowless.
4. **Quick-open palette (Cmd+K).** Global-shortcut overlay window (Tauri multi-window is
   unblocked, but the shortcut needs the not-yet-added `tauri-plugin-global-shortcut`)
   pointing at proxy-served `/__palette`; fuzzy match over projects/files/pages/boards
   from the index; verbs: Reveal in Finder, Export board, New file from template, Switch
   vault, Copy board link, and **Checkpoint now** — manual-only: one labeled git commit
   via the shipped `designs-git-init.sh` machinery (`apps/desktop/src/gitinit.rs`); never
   commits except on explicit user action — no auto-chronicle, no history UI.
5. **Peek.** Space on any card → instant Quick Look-style full-size preview served from the
   board's `.exports` dir (M5's atomic, hash-gated pipeline), arrow keys flip through
   siblings; "Present" opens `/#/view?file-id&page-id&frame-id&section=interactions`
   (viewer query params — `file-id`, `page-id`, `frame-id`, `section`, `index` — verified
   in upstream 2.16.2 `app/main/ui.cljs`; live-asserted by `routes-gate.sh`). Review a
   project without booting the canvas. **Conditional on N2:** all `.exports` renders go
   through the dev-mode-only exporter today (risk 1); Peek ships in N2's render currency.
6. **Vaults, plural.** First-run "Choose your design vault" picker; File > Open Vault
   switching. The core invariant IS the switch mechanism: DB reset + reconcile from the new
   tree — machinery M2 proved (`scripts/m2-invariant.sh`). Vault-local settings travel in a
   dotfolder at the vault root. Scope subject to the product-owner question in Known risks.
7. **New-from-template + packaged vault experience.** Offline template gallery (the Arduino
   Examples-menu move) seeded from the 15 builtin-template binfiles already shipped
   (`runtime/backend/builtin-templates/`). **Format caveat (verified):** only 4 of 15 are
   binfile-v3 zips; 11 are legacy binary binfiles (magic `0x010B1A86`), and `import-binfile`
   is verified for v3 zips only (docs/m0/rpc-endpoints.md:278-345). Verified path:
   import-as-new via RPC, then normal DB→FS export materializes the tree; the legacy 11
   need an N6 spike or a one-time ship-side conversion. The dmg boots offline to the lighttable.

## Architecture — what gets built where

| Surface | What | Notes |
|---|---|---|
| Proxy (`crates/proxy`) | `/__home`, `/__palette`, `/__api/vault/*` (search, boards, status SSE, actions) | `bind_with_router` extra routes take precedence over the SPA fallback; same-origin = auth cookie + RPC for free (`apps/desktop/src/lib.rs:416-467`). Plain HTML/vanilla JS, no framework. |
| New crate `crates/vault-index` | SQLite FTS5 index of the vault; read-only manifest consumer | Board-export pattern: reads `.penpot-sync.json` (`crates/sync-core/src/manifest.rs:25-60`), own state file, reindex exactly when `lastSyncedHash` moves. Disposable by invariant 1. |
| Thumbnails | Extend `crates/board-export` per the N2 decision | Options in risk 1. NOTE: the existing M5 pipeline renders BOTH svg and png through the exporter child (`crates/board-export/src/lib.rs` — `ExporterClient` is the only render path), which is dev-mode only and not packaged. There is no free SVG path today. |
| Native shell (`apps/desktop`) | Landing navigation to `/__home` (replaces the placeholder-dist page as the real home), palette overlay window + global shortcut, vault picker dialog, checkpoint verb | `window.navigate` pattern already exists; tray stays as-is. |
| Sync daemon | Untouched except N5: open/close a vault root at runtime instead of one fixed root | Reconciliation, conflict rule, debounce all unchanged. |
| Upstream canvas | Untouched, full-window, reached only by deep links | No plugin, no injection, no iframe embedding (invariant 3). |

Coexistence: the webview shows either `/__home` (ours) or the SPA (theirs) — never both
composed. Navigation between them is ordinary same-origin URL navigation; the whole chapter
adds only ONE new upstream coupling (hash-route shapes) beyond what M0–M5 already carry.

## Milestones

Every milestone lands a `scripts/n<X>-*.sh` gate, run-twice idempotent, in the house style
of `m2-invariant.sh`/`m5-features.sh`. N1 creates the `just e2e` target chapter 1 promised
but never shipped (`justfile` today: separate smoke/invariant/m3/m5 recipes): it chains
those plus every landed n-gate. One milestone = one issue file in
`docs/milestones/n<X>.md` with exit criteria copied in.

### N1 — Vault Index: search the corpus (standalone value, zero render risk, no new deps) — ✅ DONE 2026-07-14 (offline FTS5 search; 100×10 torture fixture; `scripts/n1-index.sh` all-green; delete-db rebuild byte-identical; run-twice hash-gated no-op)
Goal: offline full-content search across the vault, exposed at `/__api/vault/search` + a
minimal proxy-served results page (palette comes later, the value is the query). Also lands
the shared seed tooling: a 100-file/1000-board torture fixture (N3/N5 reuse it), `just e2e`.
**Exit criteria:** `scripts/n1-index.sh` plants a needle string in one shape via
`update-file` RPC, waits one sync window, queries `/__api/vault/search?q=` over HTTP and
gets the correct file/page/board ids in <100ms against the torture fixture; deleting the
index db + reboot rebuilds identical results from disk alone (invariant 1 proven); an edit
renaming the needle removes the stale hit within the debounce window; run twice = no
reindex churn (hash-gated no-op); `just e2e` exists and chains m1-smoke→m5→n1 green.

### N2 — Package the exporter as the render path (parallel with N1; blocks N3) — ✅ DONE 2026-07-14 (node v24.16.0 + chromium headless-shell bundled; offline packaged renders every board; both M5 exporter bugs fixed with live regressions; `scripts/n2-thumbs.sh` all-green; m4-artifact-test green with renders ON)
Goal: **decided with the product owner (risk 1: option a)** — pixel thumbnails are worth the
dmg growth. Package the exporter: node v24.16.0 (upstream pin) + chromium headless-shell
(~93.5 MB floor) join the runtime bundle; board-export becomes a packaged-mode capability.
Packaging puts the two known dev-mode bugs on the critical path — both MUST be fixed here:
stale-exporter adoption (boot must verify the probed exporter is its own child or refuse the
busy port) and the shutdown hang while renders fail (cancellation-aware retry, biased select)
(`docs/milestones/m5.md:184-231`). Degraded mode still required: board with no render yet →
placeholder card, never an error.
**Exit criteria:** `scripts/n2-thumbs.sh` renders thumbnails for every board of a seeded
vault on a **packaged-shape artifact** (no host node, no docker, offline — the chromium and
node ship in the bundle and the m4 poisoned-proxy test still passes with renders on), second
run a hash-gated no-op; degraded mode exercised in the same script; kill -9 during a failing
render batch → restart adopts nothing stale and shutdown stays <20s (regression tests for
both m5 bugs); `docs/milestones/n2.md` records the final dmg-size delta and per-board latency.

### N3 — Lighttable home + activity strip (the app opens on the vault; needs N1+N2) — ✅ DONE 2026-07-14 (`/__home` is the post-boot landing view; `/__api/vault/boards` grid over the 100×10 fixture with exact `/#/workspace` deep links, project filter + recency/name sort, escape hatch; degraded + real thumbnails; activity/conflict strip fed by the daemon (mock in CI) with a working reveal-both-versions action; `scripts/routes-gate.sh` (bundled chromium + playwright, offline) + `scripts/n3-home.sh` all-green; grid-load 49.8 ms/1000 boards, edit→card 5.6 s; serialized gate ladder re-run one-stack-at-a-time all green — routes-gate → n3-home → m1 → m2 → m3 → m5 → n1 → n2 → fresh dmg → m4-artifact-test (renders ON + packaged `/__api/vault/search` offline hit) → `just e2e`)
Goal: `/__home` becomes the landing view: board grid with N2 thumbnails, project filter,
recency sort, escape hatch, live activity/conflict strip. Largest of the ladder; if it
overruns a day, the pre-drawn split is N3a grid+deep-links / N3b strip+conflict-surface.
**Exit criteria:** `scripts/n3-home.sh` boots headless and asserts over HTTP: `/__home`
lists every board of the torture fixture with fresh thumbnails (or N2's degraded mode),
each card's href the exact `/#/workspace?...` deep link (string-asserted from the
`/__api/vault/boards` payload); a headless-browser leg (our pages and the SPA are plain
web pages behind the proxy) performs one real card-click and one escape-hatch navigation,
asserting the landed URLs — this IS the first `scripts/routes-gate.sh`; an `update-file`
RPC edit updates card and strip within the poll+debounce window; an injected
simultaneous-edit conflict appears in the strip within one sync window with a working
reveal-both-versions action; the strip's SSE endpoint runs off `MockStatusSource` in CI.

### N4 — Quick-open palette + peek + Checkpoint now (needs N1+N3) — ✅ DONE 2026-07-14 (`just e2e` all-green incl. N4-PALETTE; packaged palette offline in the dmg; checkpoint proven never to touch user history; grid rebuild-churn fixed; `docs/milestones/n4.md`)
Goal: keyboard-first navigation over the whole vault, contact-sheet review, and the manual
git checkpoint verb. Second-largest; pre-drawn split: N4a palette+peek / N4b checkpoint.
"Checkpoint now" is the concrete instance of the **surface, don't apply** + **git repos, not a
registry** values (`docs/ecosystem-concept.md`): manual-only, one labeled commit on explicit
action, never auto-chronicle, never rewriting history. The palette's "New file from template"
verb is a stub here — it opens the (empty until N6) template surface; templates ship in N6, so
N4 must not half-build the ecosystem. **Prerequisite fix (N3 debt):** `/__home` (and the new
peek/contact-sheet) must stop rebuilding the whole grid DOM every 5 s — replace the
`innerHTML=""`-on-interval with diff/patch that preserves scroll and selection, or Peek's
keyboard flipping is unusable at vault scale. Gate it: after a card update the scroll position
and focused card are unchanged (assert via the headless-browser leg).
**Exit criteria:** `scripts/n4-palette.sh`: ranking and verbs asserted over HTTP against
`/__api/vault/*` (a fuzzy query ranks the target board first; the Enter payload is its
exact deep link) plus one headless-browser navigation assert; a headless-browser assert that a
board-list refresh preserves scroll + focused card (the N3-debt grid fix); the OS pieces (global
shortcut, overlay focus) get Linux-CI coverage + an explicit manual-QA checklist —
**tauri-driver does not support macOS**, no exit criterion may depend on it locally; Space
serves the board's preview from `.exports` (HTTP 200 + content hash, render currency per
N2); Reveal-in-Finder resolves the correct directory; "Checkpoint now" on a fresh vault
creates exactly one commit containing manifest + `.penpot` dirs, on a pre-existing repo
adds exactly one commit rewriting no history, and with no edits since the last checkpoint
is a clean no-op — all asserted via `git log`/`git status`.

### N5 — Vaults, plural (adversarial zero-spill; needs N3; scope decision in risk 3) — ✅ DONE 2026-07-14 (registry + `.penpot-vault/` id marker + switch-in-progress marker; `File > Open Vault` = M2 DB-wipe→reconcile pointed at a new tree, headless-driven via a localhost control endpoint `POST /open {path}`; `scripts/n5-vaults.sh` all-green: A→B→A zero cross-vault spill (DB/boards/index) after every switch, original file ids preserved on BOTH vaults, trees byte-identical, mid-switch SIGKILL→reboot recovers FORWARD to a single consistent vault; switch ≈10 s; GUI picker manual-QA)
Goal: first-run vault picker; File > Open Vault switching via DB reset + reconcile;
vault-identity marker + settings in a vault dotfolder.
**Exit criteria:** `scripts/n5-vaults.sh` creates vaults A and B with overlapping project
names, switches A→B→A: no file of either vault ever appears in the other's tree,
lighttable, or index (asserted after every switch); every file keeps its original Penpot
id; both trees byte-identical before/after (recursive hash); `m2-invariant.sh` green on
both vaults; a mid-switch SIGKILL + reboot recovers to a consistent single-vault state.

### N6 — New-from-template + packaged vault experience (needs all previous)
Goal: offline template gallery from the bundled binfiles (spike first: legacy-format
import per pillar 7 — ship however many of the 15 clear it, 4 minimum); the dmg ships the
whole chapter.
**Exit criteria:** extend `scripts/m4-artifact-test.sh`: a fresh dmg under `env -i` +
poisoned proxies boots offline to the lighttable of an empty vault; "New file from
template" produces a working file on disk whose page/board count and text content match
the template (import-as-new remaps ids, so no hash equality with the source; instead the
new tree itself must round-trip A=B per `roundtrip.py` semantics); `scripts/n1..n5` all
pass against the packaged artifact; `just e2e` runs both chapters' suites green.

## Known risks (read before coding)

1. **Thumbnail path is the keystone** — and a product call, not just N2's technical one.
   **Every** M5 render (SVG and PNG) goes through the exporter child, dev-mode only, not
   packaged (`ExporterClient` is board-export's only render path) — nothing is already
   paid for. Options: (a) package the exporter — ~93.5 MB chromium + node v24.16.0 pin +
   stale-adoption and shutdown-hang bugs (`docs/milestones/m5.md:184-231`); (b) the
   in-bundle wasm rasterizer (`runtime/frontend/rasterizer.html`, `render-wasm.wasm`) —
   uncharacterized; (c) the canvas-generated thumbnails the SPA stores in the backend
   (`create-file-thumbnail`/`get-file-object-thumbnails`, in `shared.js`) — zero footprint
   but DB-resident and only for files once opened: best-effort under invariant 1;
   (d) metadata-only cards. **DECIDED (product owner, 2026-07-14): option (a) — package
   the exporter.** Pixel renders are worth +~100 MB on the dmg. N2 is rewritten
   accordingly; the two dev-mode exporter bugs move onto the critical path and are fixed
   in N2; (d) remains only as the per-board degraded mode while a render is pending.
2. **Upstream drift exposure, per pillar.** The chapter deliberately adds only one new
   upstream coupling: hash-route shapes for pillars 1/4/5. Mitigation: `routes-gate.sh`
   (grep the route strings + a live navigation assert) joins `roundtrip.py` as a mandatory
   version-bump gate. Pillars 2/3/6 read only disk and our own channels (drift-immune);
   pillar 7 rides binfile-v3, already gated by `roundtrip.py`. Rejected couplings
   (Valkey msgbus schema, plugin API, ws frames) stay rejected for this exact reason.
3. **Vault switching is the most dangerous milestone** — a spill violates the app's
   never-lose-data soul, and single-team provisioning is baked in (one profile, daemon
   bound to `default_team_id`, `apps/desktop/src/lib.rs:653-671`): a switch is a full
   reset+reconcile, not a hot swap. **DECIDED (product owner, 2026-07-14): ship N5 as
   designed** — the adversarial zero-spill gate stays the milestone's centerpiece.
4. **How completely should the lighttable replace the upstream dashboard?**
   Libraries/fonts/settings stay upstream. **DECIDED (product owner, 2026-07-14):
   permanently delegate to the escape hatch** — near-zero cost, no dashboard absorption
   this chapter. The lighttable owns browse/search/open; everything else deep-links out.
5. **Git coexistence for "Checkpoint now".** Even manual-only committing can fight a
   user's own repo (mid-rebase, staged work, hooks). Rule: detect dirty/in-progress state
   and refuse loudly; never touch history; never auto-run. N4's exit criteria encode this.
6. **Index correctness vs sync races.** The index reads trees the daemon may be
   mid-swapping. Mitigation: index only after the two-phase swap lands (watch the manifest,
   not the dirs), like board-export; hash-gate everything. Staleness is fine (derived
   state); results pointing at nonexistent boards are not — N1 asserts the rename case.
7. **Palette shortcut + overlay portability.** Global shortcuts collide with OS/user
   bindings; the overlay must not steal focus mid-edit. Keep the shortcut configurable and
   the palette reachable from the tray; Linux stays CI-only, honest asymmetry as in M4.
8. **Scale.** A vault with hundreds of files × dozens of boards must not melt `/__home` or
   the indexer. FTS5 and lazy thumbnail loading handle this on paper; N1's 100-file torture
   fixture (reused by N3/N5) exists so we learn before users do.

## Out of scope (non-goals for this chapter)

- **No forking or patching the SPA** — no CSS/JS injection, no serve-time index.html edits;
  the canvas is reached by URL only. The SPA is a compiled, minified ClojureScript bundle:
  every patch is unmaintainable and multiplies PLAN.md risk 3 (version drift).
- **No in-canvas plugin, no plugin-runtime coupling, no multi-webview/docked embedding** —
  originality lives before and between canvas sessions, beside the canvas, never inside it.
- **No `penpot-cli` public contract** (its own future chapter; the headless bin stays the
  internal harness) and **no Valkey pub/sub / sub-second sync** (highest-drift internal
  coupling; the 2s poll + reconciliation backstop remain).
- **No auto-chronicle, history UI, or visual diff** — Timekeeper is a clean future chapter
  on top of the vault; chapter 2 ships only the manual checkpoint verb.
- **No cloud sync, no multi-user, no Windows port** — git + any file sync remains the answer.

## Claude Code readiness

- `crates/vault-index` follows the board-export recipe verbatim — point sessions at
  `crates/board-export/src/{lib,state}.rs` as the template. App pages are framework-free
  HTML/JS served by the proxy; N3 extends the `MockStatusSource` pattern to a mock-driven
  `/__api` so pages develop stackless.
- Everything testable headless: each milestone's script boots via the headless bin, drives
  RPC + HTTP against the proxy, asserts on filesystem/URLs. Suites run solo (m4's lsof
  lesson). Extend `just e2e`; never rebuild the harness.
- Before claiming a milestone done: `just e2e` green + `routes-gate.sh` once it exists. On
  any Penpot version bump: `roundtrip.py` + `routes-gate.sh` both green before anything else.
