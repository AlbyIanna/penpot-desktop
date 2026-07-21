# Penpot Local

A local-first desktop app wrapping [Penpot](https://penpot.app)'s open-source stack.
Your folder tree is the **source of truth**; the Penpot database is a **disposable cache**.
Delete the entire database, restart the app, and every project/file is rebuilt from disk —
if that ever fails, it's a P0 bug.

## How it works

```
┌─ Tauri app (Rust) ────────────────────────────────────────────┐
│  window ──► local proxy :8686 ──► Penpot backend :6161 (JVM)  │
│             │  static SPA          │                          │
│             │  /api, /ws           ├─► embedded Postgres :5433│
│             └  X-Accel assets      └─► Valkey :6380           │
│  supervisor: spawns/monitors/restarts all children,           │
│              provisions the single user, auto-login           │
│  sync daemon (M2+): folder tree ⇄ DB via binfile round-trips  │
└───────────────────────────────────────────────────────────────┘
```

Everything runs offline on localhost. No Docker at runtime, no accounts, no telemetry.
Design files live on disk as unzipped, git-diffable binfile-v3 directories (JSON + media).

See [PLAN.md](PLAN.md) for the architecture, milestones, and risk register;
[CLAUDE.md](CLAUDE.md) for the invariants that must never break;
`docs/m0/` and `docs/milestones/` for verified evidence per milestone.

## Status

| Milestone | State |
|---|---|
| M0 — prove the binfile round-trip | ✅ done (`docs/m0/`) |
| M1 — process supervisor: `cargo tauri dev` = working offline Penpot | ✅ done (`docs/milestones/m1.md`) |
| M2 — one-way sync (DB → FS) + startup reconciliation | ✅ done (`docs/milestones/m2.md`) |
| M3 — two-way sync + conflicts | ✅ done (`docs/milestones/m3.md`) |
| M4 — packaging (AppImage/dmg/Nix) | ✅ done — macOS dmg verified; AppImage CI-only; NixOS VM deferred (`docs/milestones/m4.md`) |
| M5 — per-board exports, OS rename/move, git helpers, hardening | ✅ done — exporter dev-mode only; notarization + NixOS VM still open (`docs/milestones/m5.md`) |

That table covers chapter 1 — the sync engine and packaging. Two further chapters shipped on top
of it before chapter 4, below: chapter 2 (vault ergonomics — search, quick-open, multi-vault,
templates: [`docs/milestones/n1.md`](docs/milestones/n1.md) through
[`n6.md`](docs/milestones/n6.md)) and chapter 3 (the plugin/library ecosystem — contracts,
package installs, component-library linking, plugin packages:
[`docs/milestones/e1.md`](docs/milestones/e1.md) through [`e4.md`](docs/milestones/e4.md), plus
spikes in [`docs/ecosystem-spikes/`](docs/ecosystem-spikes/)).

## Chapter 4 — the desktop app

Chapters 1–3 made Penpot Local a correct, offline, syncing app. Chapter 4 (milestones D0–D6)
made it feel like a native macOS app rather than a web page in a window — by configuration,
navigation policy, and native chrome, never by patching Penpot's own frontend (invariant 3, see
[CLAUDE.md](CLAUDE.md)). It shipped:

- **A native home as the front door** — `/__home` replaces Penpot's dashboard entirely for
  project/file management ([D2](docs/milestones/d2/README.md)).
- **A native menu bar and keyboard shortcuts** — File/Edit/View/Window/Help, window-per-file,
  Open Recent ([D3](docs/milestones/d3/README.md)).
- **Native Preferences** (`⌘,`) — vault, sync, renders, plugins, CSP, with a reboot-in-place
  that never wipes the database ([D4](docs/milestones/d4/README.md)).
- **Document integration** — a `.penpot` opens in its own window from a CLI argument, a second
  launch, drag-drop, or a Finder double-click ([D5](docs/milestones/d5/README.md)).
- **Proven-offline** — zero non-loopback connection attempts, checked at both the browser and
  process level, across the dev stack and, as of D6, the packaged artifact
  ([D1](docs/milestones/d1/README.md), [D6](docs/milestones/d6/README.md)).

D0 ([spike](docs/milestones/d0.md)) and D1 ([D1](docs/milestones/d1/README.md)) removed the
cloud-account surfaces first; D6 ([D6](docs/milestones/d6/README.md)) closed the chapter by
auditing every reachable web route — not just the ones the app's own UI links to — and cancelling
the one gap that audit found (`#/debug`). What the app still can't remove from inside the canvas,
and why invariant 3 keeps it that way, is catalogued in full in
[`docs/known-limits.md`](docs/known-limits.md) rather than repeated here.

Native chrome — the menu bar, Preferences window, native dialogs — is OS-level UI outside any
browser and is verified manually on a real GUI session and by spike, per
[PLAN4.md](PLAN4.md); it is not exercised by headless CI, and each milestone doc says so
explicitly rather than implying otherwise.

## Running it (macOS, dev)

Prerequisites (one-time):

```sh
# Rust toolchain + Tauri CLI
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install cargo-binstall && cargo binstall tauri-cli

# Runtime dependencies for the Penpot backend
brew install openjdk valkey imagemagick   # JDK major must be 26 for Penpot 2.16.2

# Docker (only needed once, to extract the pinned Penpot artifacts)
./scripts/fetch-penpot.sh                 # populates runtime/ (gitignored)
```

Then:

```sh
cd apps/desktop && cargo tauri dev
```

That's the whole flow: first boot downloads the embedded Postgres binaries (needs network
once), initializes the database, provisions the single user, and opens a window already
logged in. App state lives in `~/Library/Application Support/penpot-local` (override with
`PENPOT_LOCAL_DATA_DIR`; ports via `PENPOT_LOCAL_{PROXY,BACKEND,POSTGRES,VALKEY}_PORT`).

Headless (no window, same stack — used by tests and useful for debugging):

```sh
cargo run -p penpot-desktop --bin headless   # prints "READY <url>" — open it in a browser
```

## Development

```sh
cargo test --workspace      # unit + integration tests (proxy, supervisor, rpc client)
bash scripts/m1-smoke.sh    # full-stack smoke test: fresh boot → auth → X-Accel
                            # asset round-trip → clean shutdown → reboot persistence
bash scripts/m2-invariant.sh  # THE core invariant: wipe the DB, restart, rebuilt from disk
bash scripts/m3-sync.sh       # two-way sync + conflict-copy exit criteria
bash scripts/m5-features.sh   # M5: auto-export, OS rename/move, git helper, pre-flight
python3 scripts/roundtrip.py  # M0 binfile round-trip check (needs the m0 docker stack:
                              # docker compose -f m0/docker/docker-compose.yaml up -d)
```

Optional per-board SVG/PNG auto-export (M5, dev-mode only — needs a host `node` and a
one-time `./scripts/fetch-penpot.sh --with-browsers` for the exporter + chromium):
launch with `PENPOT_LOCAL_EXPORTS=1` and every board renders into `<file>.exports/`
next to its sources.

Repo layout:

| Path | What |
|---|---|
| `apps/desktop` | Tauri v2 shell, boot sequence, tray UI, headless bin, boot pre-flight |
| `crates/supervisor` | child-process supervision: embedded Postgres, Valkey, backend JVM, optional exporter |
| `crates/proxy` | local replacement for Penpot's nginx (SPA, `/api`, websockets, X-Accel assets, `/api/export`) |
| `crates/penpot-rpc` | typed client for the Penpot RPC surface |
| `crates/sync-core` | manifest, normalization/hash, atomic dir swaps |
| `crates/sync-daemon` | two-way folder⇄DB sync, conflicts, OS rename/move re-keying |
| `crates/board-export` | per-board SVG/PNG auto-export next to sources (M5) |
| `scripts/` | `fetch-penpot.sh` (extract pinned artifacts), milestone test suites |
| `runtime/` | extracted Penpot backend/frontend/exporter (gitignored; regenerate with the fetch script) |
| `m0/docker/` | upstream Penpot compose stack used by the M0 spike and `roundtrip.py` |
