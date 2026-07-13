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
| M3 — two-way sync + conflicts | next |
| M4 — packaging (AppImage/dmg/Nix) | — |
| M5 — per-board exports, git helpers | — |

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
python3 scripts/roundtrip.py  # M0 binfile round-trip check (needs the m0 docker stack:
                              # docker compose -f m0/docker/docker-compose.yaml up -d)
```

Repo layout:

| Path | What |
|---|---|
| `apps/desktop` | Tauri v2 shell, boot sequence, headless bin |
| `crates/supervisor` | child-process supervision: embedded Postgres, Valkey, backend JVM |
| `crates/proxy` | local replacement for Penpot's nginx (SPA, `/api`, websockets, X-Accel assets) |
| `crates/penpot-rpc` | typed client for the Penpot RPC surface |
| `scripts/` | `fetch-penpot.sh` (extract pinned artifacts), smoke + round-trip tests |
| `runtime/` | extracted Penpot backend/frontend (gitignored; regenerate with the fetch script) |
| `m0/docker/` | upstream Penpot compose stack used by the M0 spike and `roundtrip.py` |
