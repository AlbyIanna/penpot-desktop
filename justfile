# Penpot Local task runner. Requires: rust stable, tauri-cli.
# cargo may not be on PATH by default; recipes source ~/.cargo/env when present.

set shell := ["bash", "-cu"]

cargo_env := '[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env";'

default: check

# Type-check the whole workspace
check:
    {{cargo_env}} cargo check --workspace

# Run all workspace tests
test:
    {{cargo_env}} cargo test --workspace

# Headless smoke test of the full stack (M1: boot, auto-login, X-Accel assets, clean shutdown, restart)
smoke:
    bash scripts/m1-smoke.sh

# THE core-invariant test (M2 exit criterion): wipe the Postgres data dir, restart,
# everything rebuilt from the designs folder with the same file ids. P0 if it fails.
invariant:
    bash scripts/m2-invariant.sh

# M3 exit criteria: git-checkout an older file dir -> appears in Penpot within
# seconds; simultaneous edit -> exactly one conflict copy, never data loss.
m3:
    bash scripts/m3-sync.sh

# M5 freedom features: per-board auto-export (needs the dev-mode exporter:
# scripts/fetch-penpot.sh --with-browsers + host node), OS-side rename/move,
# git-init helper, non-BMP pre-flight, single-instance behavior.
m5:
    bash scripts/m5-features.sh

# N1 vault index: torture fixture (100 files / 1000 boards), FTS search
# correctness + latency, delete-index-db rebuild (invariant 1), rename
# staleness, hash-gated no-reindex.
n1:
    bash scripts/n1-index.sh

# N2 packaged render path: exporter (node + chromium headless-shell) resolves
# from the runtime bundle, offline renders every board, hash-gated no-op, plus
# live regressions for the two dev-mode exporter bugs (stale-adoption,
# shutdown-hang). Requires the bundle first (`bash scripts/build-runtime-bundle.sh`).
n2:
    bash scripts/n2-thumbs.sh

# N3 lighttable home: /__home board grid over the 100x10 torture fixture with
# exact /#/workspace deep links, project filter + recency/name sort, degraded
# + planted thumbnails, live headless-browser routes-gate, edit→card+strip
# update, and the mock-driven conflict strip + reveal action.
n3:
    bash scripts/n3-home.sh

# SPA hash-route version-bump gate (PLAN2 risk 2): grep the route strings out
# of the compiled bundle + a live headless-browser navigation assert. Boots its
# own throwaway stack unless ROUTES_GATE_BASE points at a running one. Run this
# (with roundtrip.py) first after any Penpot version bump.
routes-gate:
    bash scripts/routes-gate.sh

# THE e2e chain (PLAN2.md N1): every milestone suite, serialized — the
# suites are concurrency-UNSAFE against sibling stacks (m4's lsof lesson),
# so never run them in parallel. Chains every landed n-gate (N1, N2).
# n2-thumbs needs the runtime bundle (`bash scripts/build-runtime-bundle.sh`);
# m4-artifact-test.sh stays separate: it needs a dmg build
# (`bash scripts/build-dmg.sh` first).
e2e:
    bash scripts/m1-smoke.sh
    bash scripts/m2-invariant.sh
    bash scripts/m3-sync.sh
    bash scripts/m5-features.sh
    bash scripts/n1-index.sh
    bash scripts/n2-thumbs.sh
    bash scripts/n3-home.sh

# M5: enable git versioning for a designs folder (idempotent; the tray's
# "Enable git versioning" action runs this same script).
git-init designs_dir:
    bash scripts/designs-git-init.sh "{{designs_dir}}"

# Run the desktop app in dev mode. The SIGKILL orphan watchdog is a separate
# bin in crates/supervisor that `cargo tauri dev` won't build on its own —
# build it first so boot finds the target/debug/penpot-watchdog sibling.
dev:
    {{cargo_env}} cargo build -p supervisor --bin penpot-watchdog
    {{cargo_env}} cd apps/desktop && cargo tauri dev
