# M5 — per-board SVG/PNG auto-export (board-export + exporter child)

Component notes for the M5 "freedom features" milestone: every board of every
synced file gets an always-fresh `<board>.svg` + `<board>.png` next to the
sources, rendered by the upstream `penpot-exporter` service. **Dev-mode
only** — the exporter is NOT packaged (see "Explicitly out of scope").

## What landed

- **`scripts/fetch-penpot.sh`**: also extracts `penpotapp/exporter:2.16.2`
  `/opt/penpot/exporter` into `runtime/exporter/` (own `VERSION` file, so an
  existing runtime gains it without `--force`; skip with `--no-exporter`).
  `--with-browsers` installs the playwright-managed chromium into
  `runtime/exporter-browsers/` (~500 MB on disk; the exporter calls
  playwright's `chromium.launch()` with no `executablePath`, so the system
  Chrome is never used). Verified idempotent (second run skips everything).
- **`crates/supervisor`**: optional 4th child `exporter`
  (`SupervisorConfig::exporter: Option<ExporterSpec>`, default `None` — off
  by default, byte-identical behavior when unset). `exporter_command()` is
  the pure env contract from the spike: `PENPOT_SECRET_KEY` (must match the
  backend — HKDF-derived exporter key), `PENPOT_PUBLIC_URI` (proxy origin),
  `PENPOT_REDIS_URI` (shared valkey), `PENPOT_HTTP_SERVER_PORT`,
  `PENPOT_TEMPDIR` (`<data>/exporter-tmp`), `PLAYWRIGHT_BROWSERS_PATH`.
  Readiness probe `GET /readyz` → 200. Starts last, stops first; covered by
  the orphan watchdog (pid pushed + fed like the other children).
- **`crates/board-export`** (new): self-contained `ExportRenderer` service.
  Polls `.penpot-sync.json` **read-only** every 2 s; re-renders a file iff
  its `lastSyncedHash` moved past the hash in
  `<name>.exports/.exports-state.json` (pure decision `state::needs_render`).
  Renders are debounced 3 s (re-armed while the hash keeps moving),
  serialized, retried with the sync-daemon backoff profile (10 attempts,
  0.5 s→15 s), and cooled down 60 s after a failed batch. Output dir is
  replaced with a two-phase atomic swap (`sync-core::commit_dir_swap`);
  interrupted swaps are swept at startup. Board names are sanitized +
  case-insensitively deduplicated (`Board`, `board` → `board-2`; APFS).
  Auth: exporter needs **session-cookie auth** (access tokens do NOT work —
  spike trap #1); the service mints a session via `login-with-password`
  lazily and re-mints after any failure. `get-file` reads use the token.
- **`crates/proxy`**: `ProxyConfig::exporter_addr: Option<SocketAddr>` —
  `/api/export` is reverse-proxied to the exporter when set (the Penpot UI's
  own export button works), the clear 502 stub stays when unset (default).
- **`apps/desktop`**: `PENPOT_LOCAL_EXPORTS=1` (default **OFF**) enables the
  whole path. Pre-flight (`resolve_exporter_layout`) fails the boot with an
  actionable message per missing piece (exporter app → run fetch script;
  node → install/set `PENPOT_LOCAL_NODE`; browsers → `--with-browsers`).
  Overrides: `PENPOT_LOCAL_EXPORTER_DIR`, `PENPOT_LOCAL_NODE`,
  `PENPOT_LOCAL_EXPORTER_BROWSERS`, `PENPOT_LOCAL_EXPORTER_PORT` (default
  6363). Status is tracing-only; a tray "exports" line is a marked hook
  (`TRAY-HOOK(M5)` in `lib.rs`) left for the tray owner.

## Sync-daemon interplay (no changes needed — verified against its code)

`*.exports/` dirs are invisible to the sync daemon: its watcher
(`map_event_path`) only maps paths inside `*.penpot` dirs and its disk walker
(`walk_penpot_dirs`) only collects `*.penpot` dirs. Verified live: export
swaps triggered zero imports/exports and the idle designs-dir stat
fingerprint (path+mtime+size+inode over the whole tree) is byte-stable.
Manifest entries that vanish leave their stale `.exports` dir in place — the
service never deletes user-visible outputs (documented behavior; prune by
hand). If the rename/move workstream later *moves* a `.penpot` dir, the old
`.exports` dir stays behind and a fresh one is rendered at the new path.

## Live proof (2026-07-13, stack on 8910/6385/5459/6402, exporter 6467)

- Boot with `PENPOT_LOCAL_EXPORTS=1`: exporter child READY (log: "welcome to
  penpot, module=exporter", redis connected); boot-to-READY ≈ 13 s.
- 2 files seeded via RPC (alpha: boards "Cover"+"Detail", beta: "Solo") →
  all `.svg`+`.png` + state files appeared next to sources in ≤ 15 s
  (SVG = valid 800×600 with exact shape fills; PNG = 800×600 8-bit RGB).
- Idle 12 s → designs-tree stat fingerprint identical (zero churn).
- RPC edit of ONE shape in alpha → only alpha re-rendered (state hash moved
  to the new manifest hash; beta's fingerprint identical, incl. inodes);
  edited rect visible in the fresh SVG. Edit→exports-on-disk latency ≈ 20 s
  (2 s sync poll + 3 s sync debounce + export + 2 s manifest poll + 3 s
  render debounce + 9.3 s render batch).
- Render latencies (M-series, debug build): first render 5.1 s (browser
  launch), warm 2.2–2.3 s per board/format; each render carries a hardcoded
  1 s sleep in the upstream renderer, so ~2.2 s is near the floor.
  Batch of 2 boards × 2 formats: 9.3–12.0 s.
- `/api/export` through the proxy with a session cookie → transit response +
  authenticated artifact download through the existing `/assets` X-Accel
  path (the UI leg).
- SIGTERM → exporter stopped first, node child reaped, all 5 ports freed,
  `STOPPED`.

## Regressions after all changes (this machine, 2026-07-13)

`cargo test --workspace` 281 passed / 0 failed (includes the parallel M5
workstreams); `m1-smoke.sh` ALL PASS; `m2-invariant.sh` ALL PASS
(sameIds=True); `m3-sync.sh` ALL PASS; `build-dmg.sh` + `m4-artifact-test.sh`
— see the main M5 verification record.

## Host requirements (dev-mode, documented not bundled)

- `node` on the host: upstream exporter image pins v24.16.0; **v25.8.1
  verified working end-to-end** (the extracted app is pure JS — zero native
  `.node` bindings, the linux `node_modules` run on macOS as-is). Default
  path `/opt/homebrew/bin/node`, override `PENPOT_LOCAL_NODE`.
- playwright chromium (headless-shell 149.0.7827.55 / v1228 at this pin),
  93.5 MB download / ~500 MB on disk via `fetch-penpot.sh --with-browsers`.
- docker daemon only for the one-time image extraction.

## Explicitly out of scope (documented, not attempted)

- **Exporter packaging/bundling**: needs node (pin v24.16.0 per the upstream
  image on version bumps — re-probe v25 compatibility then) + a bundled
  chromium (93.5 MB headless-shell is the size floor) and re-opens the
  dmg-size levers from m4.md. Dev-mode only in M5.
- **Upstream limitation (verified)**: the exporter's HTTP server binds
  **0.0.0.0** — not configurable in the compiled bundle. Accepted for
  dev-mode; the packaging phase must firewall or patch it.
- Multi-board zip exports / PDF (`:export-frames`) — single-export
  `"~:wait": true` is synchronous and exactly right for per-board files;
  jpeg/webp exist upstream (webp shells out to ImageMagick `convert`).
