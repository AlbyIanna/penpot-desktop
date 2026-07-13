# Penpot Local — implementation plan

A local-first desktop app wrapping Penpot's open-source stack. The user's folder tree is
the **source of truth**; the Penpot database is a **disposable cache**.

**Chosen architecture:** native JVM + embedded Postgres · unzipped JSON directories on disk · debounced auto-sync.

## Core invariant (this is also the test suite)

> Delete the entire database, restart the app, and every project/file is rebuilt from the
> folder tree with no data loss. If this ever fails, it's a P0 bug.

## Stack

| Component | Choice | Notes |
|---|---|---|
| Shell | Tauri v2 (Rust) | Sync daemon + process supervisor live in the Rust core. Lighter than Electron; good NixOS story. |
| Penpot backend | Upstream JAR, pinned version | Run as child process with a jlink'd JRE of the **exact JDK major** the pinned release builds with (the jar runs `--enable-preview`, which hard-fails on version mismatch; upstream develop is on JDK 26). Config via env vars. |
| Penpot frontend | Upstream static build | Served by a small local proxy (Rust axum/hyper in the Tauri core) that also proxies `/api`, the `/ws/notifications` WebSocket, and implements X-Accel asset serving — model on upstream `nginx.conf.template`. See risk 6. |
| Database | Embedded Postgres (Rust `postgresql_embedded` crate, or zonky.io binary artifacts extracted at build time) | Data dir in XDG data dir. Random localhost port. Treated as cache. |
| Redis | Bundled Valkey binary | Penpot needs it for msgbus/websockets. Child process, localhost only. |
| Exporter | Deferred (Milestone 5) | Node + Puppeteer; only needed for server-side PNG/PDF export. |

### Single-user mode
- On first boot: create the single user via backend RPC, store credentials in OS keychain.
- Sync daemon auth: personal access token (`enable-access-tokens` flag), stored in the keychain — no cookie scraping for RPC calls.
- Auto-login (webview only): inject the auth cookie on startup, or serve a bootstrap page that calls the `login` RPC from page context (cookie injection is platform-fiddly: WKWebView vs WebView2 APIs). No registration UI ever shown.
- `PENPOT_FLAGS`: `disable-email-verification disable-secure-session-cookies disable-onboarding enable-access-tokens`. (Telemetry is disabled by default; no flag needed.)

## On-disk layout

```
~/Designs/                          # user-chosen root
  .penpot-sync.json                 # manifest: fileId ↔ path ↔ revn ↔ lastSyncedHash
  client-x/                         # folder = Penpot project
    homepage.penpot/                # directory = one Penpot file (unzipped binfile-v3)
      manifest.json                 # binfile manifest (files, features, library relations)
      files/<file-id>.json          # file-level data (normalized JSON)
      files/<file-id>/pages/<page-id>.json        # per-page JSON (finer git diffs)
      files/<file-id>/{colors,components,typographies}/  # library assets, tokens.json
      objects/<id>.json             # media metadata …
      objects/<id>.<ext>            # … and binary blobs (images/fonts)
    homepage.exports/               # auto-rendered SVG per board (Milestone 5)
```

App-internal state (Postgres data dir, logs, JRE, binaries) lives in XDG dirs
(`~/.local/share/penpot-local/`), never inside the user's Designs folder.

### JSON normalization (git-diffability)
After unzipping a binfile export, rewrite every `.json` deterministically:
sorted object keys, 2-space indentation, trailing newline, LF endings.
Round-trip must be byte-stable: export → normalize → zip → import → export → normalize
produces identical bytes (modulo regenerated IDs — see Known risks).

## Sync daemon (Rust, inside the Tauri core)

### Direction A: DB → filesystem (user edits in Penpot)
1. Poll backend RPC (`get-project-files` per project) every 2s; compare `revn` + `modified-at` against manifest.
2. On change, start/reset a **3s debounce timer** per file.
3. On fire: call `export-binfile` (binfile-v3), unzip to temp dir, normalize JSON.
4. Hash the normalized tree. If equal to `lastSyncedHash`, discard (no-op save — don't touch mtimes).
5. Two-phase swap (POSIX `rename(2)` cannot replace a non-empty directory): write to
   `<name>.penpot.tmp-<rand>/`, rename the target to `<name>.penpot.old-<rand>/`, rename tmp
   into place, delete old. Startup reconciliation cleans up orphaned `.tmp-*`/`.old-*` dirs.
6. Record new hash in the ledger **before** the swap lands, so the watcher ignores it.

### Direction B: filesystem → DB (external edit: git checkout, script, hand-edit)
1. Watch the root with the `notify` crate; debounce events 2s per file-directory.
2. Compute tree hash. If it matches the ledger (own write), skip.
3. Validate: parseable JSON, binfile manifest sane. On failure → surface error in UI, do nothing destructive.
4. Zip the directory, call `import-binfile` passing the existing `file-id` → **in-place import**
   (supported since 1.20 for single-file binfile-v3 archives): same file ID, no manifest churn.
   Fallback if in-place import fails: import-as-new → delete the old file, update the manifest mapping.
5. Conflict rule: if DB `revn` also advanced since `lastSyncedHash`, do **not** import.
   Export the DB version as `<name>.conflict-<timestamp>.penpot/` next to the file and notify. Never silently overwrite either side.

### Startup reconciliation
On boot, walk the folder tree vs the manifest vs the DB:
- On disk but not in DB → import (this is how the invariant is satisfied). If the manifest
  knows the file's old id, resurrect it under that id: `create-file` with the client-chosen
  uuid, then in-place import onto it (in-place import alone fails on nonexistent ids —
  verified on 2.16.2). Soft-deleted ids are unavailable for ~7 days (500) → import-as-new
  and re-key the manifest, loudly.
- In DB but not on disk → export (first run / file created before daemon started).
- Hash mismatch on both sides → conflict rule above.

## Milestones

### M0 — Spike: prove the round-trip (throwaway code allowed) — ✅ DONE 2026-07-13

All exit criteria met; evidence in `docs/m0/` (README grades each criterion), spike stack in
`m0/docker/`, re-runnable script in `scripts/roundtrip.py`. Headline findings: in-place import
preserves all UUIDs; only `createdAt`/`modifiedAt` are volatile (normalization = formatting +
strip those two); `export-binfile` returns an SSE stream whose download URI itself goes through
the X-Accel asset path; import multipart fields are kebab-case; `revn` is reset by in-place
import and stale `revn` on `update-file` is NOT rejected — conflict detection is fully client-side.

Run upstream Penpot via docker-compose (just for learning the API). Script against the HTTP
RPC: login, create file, `export-binfile`, unzip, normalize, re-zip, `import-binfile`, re-export.
**Exit criteria:** documented list of RPC endpoints + a byte-diff report of what changes across a
round-trip (IDs, timestamps) + **in-place import** (`import-binfile` with `file-id`) verified to
round-trip with the same file ID + asset serving without nginx characterized (X-Accel behavior, risk 6).

### M1 — Process supervisor — ✅ DONE 2026-07-13

All exit criteria met (independently verified + follow-up shutdown fix); evidence and M2
implications in `docs/milestones/m1.md`. Headline: 56 workspace tests green; 9/9 smoke steps
(incl. X-Accel round-trip, crash-restart of valkey/backend, token persistence across boots);
signal-clean shutdown on both headless and GUI paths. Notable: upstream `app.main` opens an
unauthenticated nrepl on 0.0.0.0:6064 with no off switch — the app boots the backend via a
`clojure.main -e` entry instead (guarded by a unit test). Runtime deps discovered for M4
packaging: ImageMagick `identify`, node, JDK 26, valkey.

Tauri app that launches embedded Postgres + Valkey + Penpot backend as supervised children,
provisions the single user, serves the frontend, opens one window auto-logged-in.
Clean shutdown kills children; crash of a child restarts it.
**Exit criteria:** `cargo tauri dev` gives a working offline Penpot with zero manual steps.

### M2 — One-way sync (DB → FS) + reconciliation — ✅ DONE 2026-07-13

Core invariant passed: `rm -rf` the postgres data dir → restart → all projects/files rebuilt
from disk **under their original file ids**, disk byte-untouched, third boot a pure no-op.
`scripts/m2-invariant.sh` 17/17 (run twice + unicode-names probe); 129 workspace tests; M1
smoke not regressed. Evidence + M3 implications in `docs/milestones/m2.md`. Key discovery:
on 2.16.2, in-place import onto a *nonexistent* file-id fails (`object-not-found`) — the
resurrect recipe is `create-file` with the manifest's old id (client-chosen uuid), then
in-place import onto it; a soft-deleted id 500s (~7-day GC) → fallback import-as-new.
Direction A above, plus startup import of anything on disk. Ship the manifest + hash ledger.
**Exit criteria:** the core-invariant test passes: `rm -rf` the Postgres data dir, restart, all files restored from disk.

### M3 — Two-way sync + conflicts
Direction B, loop prevention, conflict copies, error surfacing in a small status UI (tray/menubar: last sync, per-file state, pause button).
**Exit criteria:** `git checkout` an older version of a file directory → it appears in Penpot within seconds; simultaneous-edit test produces a conflict copy, never data loss.

### M4 — Packaging
jlink-minimized JRE, pinned Penpot release fetch script, AppImage + dmg, **Nix flake** (dev shell + package).
**Exit criteria:** a fresh machine (or clean NixOS VM) runs the app from a single artifact.

### M5 — Freedom features
Per-board SVG/PNG auto-export next to sources (needs the exporter service), `git init` helper +
sensible `.gitignore`, "reveal in file manager", file/project create-rename-move from the OS side reflected in Penpot.

## Known risks (read before coding)

1. **ID regeneration on import (fallback path only).** The primary FS→DB path is in-place import
   (`file-id` param), which preserves the file ID. Only the import-as-new fallback assigns new IDs,
   breaking shared links/comments and **cross-file library references**. M0 must verify the
   in-place round-trip; if it proves unreliable, this risk returns in full.
2. **Round-trip instability.** Exports may embed timestamps/ordering noise. The normalization
   step must strip/sort everything non-semantic, or the hash ledger produces false "changes".
   M0's byte-diff report drives what normalization must do.
3. **Penpot version drift.** binfile-v3 and RPC signatures can change (e.g. export `version` param
   removed in 2.12, storage env vars renamed in 2.11). Pin the Penpot version; read the release's
   "Breaking changes & Deprecations" notes (2.16.0 has one); upgrading is a deliberate, tested step
   (run M0's round-trip script against the new version first).
4. **Redis requirement — settled.** Cannot be disabled (msgbus/websocket notifications), but
   upstream's own docker-compose now ships `valkey/valkey:8.1`, so bundling Valkey is the
   blessed path, not a gamble.
5. **Large media.** Media blobs live in Penpot's storage backend. Configure
   `PENPOT_OBJECTS_STORAGE_BACKEND=fs` + `PENPOT_OBJECTS_STORAGE_FS_DIRECTORY` (names since 2.11)
   and export with `embed-assets`/`include-libraries` so the folder is self-contained.
6. **fs asset serving depends on nginx X-Accel-Redirect.** With the fs backend, the backend
   answers asset requests with an `X-Accel-Redirect` header that upstream nginx resolves against
   an internal alias — a plain static server or the Tauri asset protocol would silently break all
   image loading. The bundled proxy must implement X-Accel-style internal redirects (or serve the
   assets directory directly) in addition to proxying `/api` and the `/ws/notifications` WebSocket.
   Characterize the exact behavior in M0.

## Claude Code readiness

- **Monorepo:** `apps/desktop` (Tauri), `crates/sync-daemon`, `crates/penpot-rpc` (typed client),
  `scripts/` (fetch-penpot, roundtrip-test), `tests/e2e`.
- **CLAUDE.md at repo root:** the core invariant, the conflict rule ("never overwrite either side"),
  normalization spec, and "run `just e2e` before claiming a milestone done".
- **Everything testable headless:** the e2e harness boots the full stack (no window), drives RPC
  directly, asserts on the filesystem. This is what lets Claude Code verify its own work —
  invest in it early (it's basically M0's script grown up).
- **One milestone = one issue file** in `docs/milestones/` with its exit criteria copied in,
  so each Claude Code session has a self-contained goal.
