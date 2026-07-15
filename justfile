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

# N4 quick-open palette + Peek + Checkpoint now: fuzzy ranking over
# /__api/vault/palette (target board first, Enter = exact deep link), a
# headless-browser palette-Enter nav + grid-scroll/focus-preservation assert
# (the N3-debt diff/patch fix), Peek preview from .exports (200 + content
# hash), reveal, and the Checkpoint-now git decision table (fresh→1 commit,
# no-op, +1 no-rewrite, dirty→loud refusal). Renders stay OFF (planted render).
n4:
    bash scripts/n4-palette.sh

# N5 vaults, plural (adversarial zero-spill): two vaults A/B with overlapping
# project names, switch A→B→A headlessly via the localhost control endpoint
# (POST /open {path}); after EVERY switch assert ZERO cross-vault spill
# (DB/get-project-files, /__api/vault/boards, /__api/vault/search), original
# file ids preserved, both .penpot trees byte-identical (user disk untouched),
# per-vault M2 wipe→rebuild on both; a mid-switch SIGKILL + reboot recovers to
# a consistent single vault (the target) with no cross-contamination/orphans.
n5:
    bash scripts/n5-vaults.sh

# N6 template gallery + New-from-template (pillar 7): GET /__api/templates
# lists the shippable builtin set; /__templates serves offline; for a
# representative template of each format (v3-zip + legacy, incl. the settle
# path) "New file from template" imports-as-new → a real .penpot dir appears
# in the vault, its page/board count + text match the template, and the
# materialized tree round-trips A=B (roundtrip.py semantics). Renders OFF.
n6:
    bash scripts/n6-templates.sh

# E1 contract extractor + version classifier (PLAN3 milestone E1). A FAST,
# PURE, STACK-FREE static gate: authors a combined fixture (first-class +
# legacy variant models + applied tokens + tokens.json — no shipped template
# has all three), extracts its contract, proves extract(A)==extract(A') under
# uuid churn (keyed by name/path, never the remapped variantId), classifies
# the curated delta matrix (impl->patch, added->minor, removed/renamed->major)
# matching the python spike oracle exactly, special-cases the legacy->first-class
# migration so it is not a spurious minor, and shows the contract is a pure
# function of disk (invariant 1). No Penpot stack, no ports.
contract:
    bash scripts/e1-contract.sh

# E2 package home + lockfile + generalized installer (PLAN3 milestone E2). A
# LIVE gate: drops a template + a component-library `.penpot` under
# `.penpot-packages/` and proves the sync daemon is BLIND to them (edit inside →
# no manifest entry, no `.conflict`, no import); an explicit install imports +
# settles to a fixpoint (two equal semantic hashes) + writes a `lock.json` entry;
# `git clone` a local bare repo lands a package that installs OFFLINE; delete-DB
# + reboot re-applies every locked package deterministically (M2 resurrect-by-id)
# with no user-disk write outside `.penpot-packages/`; run-twice is a no-op.
# Dedicated ports 8962/6423/5497/6440; needs the runtime bundle + a live stack.
e2:
    bash scripts/e2-packages.sh

# E3 component-library linking (PLAN3 milestone E3). A LIVE gate: publishes a
# component-library package shared (`set-file-shared`), links a consumer package
# to it (`link-file-to-library`) that places a component instance referencing the
# library by its vault-local file-id, and proves the surgical linked export keeps
# that reference on disk WITHOUT inlining the library (single-file tree). Then
# delete-DB + reboot rebuilds both files under their original ids with the
# instance still resolving and file_library_rel re-derived from the lockfile
# (invariant 1); a patch edit surfaces no bump while minor/major surface the
# correct bump via the E1 contract-diff channel (not `revn`); unlink clears the
# relation. Dedicated ports 8974/6435/5509/6452 (control 8975); live stack + the
# runtime bundle. Documented E3-scope limit: a linked consumer that ALSO uploads
# its own raster media loses that media on disk (spike §9) — E3 consumers place
# vector instances only.
e3:
    bash scripts/e3-library.sh

# SPA hash-route version-bump gate (PLAN2 risk 2): grep the route strings out
# of the compiled bundle + a live headless-browser navigation assert. Boots its
# own throwaway stack unless ROUTES_GATE_BASE points at a running one. Run this
# (with roundtrip.py) first after any Penpot version bump.
routes-gate:
    bash scripts/routes-gate.sh

# THE e2e chain (PLAN2.md N1): every milestone suite, serialized — the
# suites are concurrency-UNSAFE against sibling stacks (m4's lsof lesson),
# so never run them in parallel. Chains every landed gate (N1–N6, E1, E2).
# e1-contract is a fast static gate (no stack); e2-packages boots its own live
# stack (dedicated ports) like the m/n suites, safe to chain at the tail.
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
    bash scripts/n4-palette.sh
    bash scripts/n5-vaults.sh
    bash scripts/n6-templates.sh
    bash scripts/e1-contract.sh
    bash scripts/e2-packages.sh
    bash scripts/e3-library.sh

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
