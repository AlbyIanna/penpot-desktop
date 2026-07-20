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

# E4 package gallery + surface-don't-apply update/conflict + ecosystem gate
# (PLAN3 milestone E4). A LIVE gate: indexes a managed component-library package
# into the DocKind::Package FTS gallery, deep-links a card to its exact
# /#/workspace URL via a bundled-offline headless browser (routes-gate style),
# surfaces the correct minor/major bump through the /__api/packages/updates poll
# WHILE the consumer's materialized .penpot stays byte-unchanged (surface, don't
# apply), preserves a drifted managed package as a .conflict-<ts> copy that
# overwrites neither side, proves the package rows come back identically after an
# index-db wipe (invariant 1), and searches 200 synthetic torture-scale package
# rows returning correct ids in <100ms. Dedicated ports 8986/6447/5521/6464
# (control 8987); live stack + the runtime bundle WITH browsers
# (`bash scripts/fetch-penpot.sh --with-browsers`). Flat gallery — no verified
# tier, badges, or monetization (docs/ecosystem-concept.md). The offline
# packaged-artifact leg (g4) lives in scripts/m4-artifact-test.sh (needs a dmg).
e4:
    bash scripts/e4-gallery.sh

# E5 cross-package token-resolver SPIKE gate (PLAN3 milestone E5). Ships a
# VERDICT (docs/ecosystem-spikes/token-resolver.md) + this gate — no UI, no
# product Rust changes. Against a fresh re-provisioned 2.16.2: mirrors the
# tokens-starter-kit's DTCG sets into a consumer (edit exported tree +
# in-place re-import, provenance-stamped via a theme id -> tokens_lib
# :external-id) and asserts the merged file round-trips A=B per roundtrip.py;
# asserts a scripted collision resolves to the tokenSetOrder-winner AND that
# flipping the order flips the resolved value (order-is-contract, RPC-
# observable); runs the STATIC resolver headless over the starter-kit dump
# (per-token free-variable deps; pre-existing dangling paths pinned as
# baseline noise; synthetic dropped token = MAJOR) — and re-runs it with the
# stack DOWN (never injected, ecosystem invariant 3). Drift of a mirrored set
# is detected and prescribes the conflict copy. Dedicated ports
# 8998/6459/5533/6476 (control 8999); live stack + the runtime bundle.
# DECISION: deliberately NOT chained into `just e2e` — spike precedent (the
# contract-extractability spike was never chained); E5 lands no product code,
# so the ladder has nothing of E5's to regress. Re-run on Penpot version
# bumps or when the token-package build (PLAN3 ch.4) starts.
e5:
    bash scripts/e5-tokens-spike.sh

# E6 cross-vault library-portability SPIKE gate (PLAN3 milestone E6). Ships a
# VERDICT (docs/ecosystem-spikes/library-portability.md) + a REWRITE TOOL
# (scripts/ecosystem-spike/e6_rewrite.py) + this gate — no UI, no product Rust
# changes. Live, on ONE app instance switching vaults via the control endpoint:
# installs the SAME component-library package into vaults A and B and asserts
# the minted :component-file ids DIFFER (and CAPTURES whether internal
# component/shape ids are preserved or remapped — on 2.16.2 binfile-v3
# import-as-new remaps FILE ids only, so they are preserved); authors in A a
# consumer with a root-only + a nested-subtree instance PLUS library-styled
# shapes (fill/stroke color + typography ASSET refs); carries its on-disk
# tree to B, proves the naive carry DANGLES, runs the offline static rewrite
# (E1-keyed identity map — components, shapes, colors, typographies — over
# the two materialized library trees), pins a lock.json entry +
# link-file-to-library, and asserts ZERO dangling refs in B over RPC and on
# disk; then delete-DB + reboot on BOTH vaults re-asserts (invariant 1),
# including a post-wipe static on-disk verify. An offline selftest leg
# (e6_rewrite.py selftest) proves the mapping DISCRIMINATES and the
# duplicate-key/subtree-size refusals fire. Dedicated ports 9010/6472/5545/6488 (control 9011; the plan
# sketch's 6471 is N2's exporter port — 6472 is the free neighbor); live stack
# + the runtime bundle. DECISION: deliberately NOT chained into `just e2e` —
# spike precedent (E5, contract-extractability): E6 lands no product code, so
# the ladder has nothing of E6's to regress. Re-run on every Penpot version
# bump: the preserved-internal-ids behavior is UNDOCUMENTED in upstream's
# binfile-v3 import, and this gate records which world the running Penpot is in.
e6:
    bash scripts/e6-library-portability-spike.sh

# E7 plugin packages: staged ACTIVATION spike + THIN-BUILD gate (PLAN3 E7).
# Activation landed GO (+ CSP-GO), the thin build shipped, and this gate now
# drives the REAL product routes on a dedicated stack booted with PRODUCT
# DEFAULTS (enable-plugins + local-origin penpotPluginsWhitelist + the CSP
# header are default-ON — no plugin env): serves a fixture plugin package at
# /__packages/<pkg>/ (hardened route, dotfile -> 400), drives Penpot's OWN
# native Plugin Manager via the bundled-chromium browser leg (install +
# consent + open), and proves: shape effect (RPC), SPA-bytes-unchanged
# (invariant 3), the update-profile-props pointer + the product capture pin
# into lock.json + the local consent LEDGER (re-apply authority, never travels
# with the vault), delete-DB -> product boot RE-APPLY gated on the ledger +
# content hash (invariant 1; a cloned lock pin alone auto-registers NOTHING),
# the /__api/packages/plugins discovery surface (surface-don't-apply), and the
# CSP-egress probe (header OFF -> beacon observed; product-default ON ->
# fetch+image beacons blocked by default-src while the plugin still loads =
# CSP-GO). The offline dmg leg (g5) lives in scripts/m4-artifact-test.sh.
# DECISION: chained into `just e2e` (build precedent, like E1-E4 — E7 landed
# product code the ladder must keep honest), unlike the E5/E6 pure-verdict
# spikes. Needs the bundled browsers (fetch-penpot.sh --with-browsers) + host node.
e7:
    bash scripts/e7-plugins-spike.sh

# D0 navigation-control SPIKE gate (PLAN4 milestone D0). Answers whether the
# webview can observe + redirect Penpot's SPA HASH navigation without touching
# the SPA (invariant 3). REQUIRES A GUI SESSION — opens a real Tauri window, so
# it is NOT CI-headless. Dedicated ports 9034/6496/5569/6512 (control 9037).
# DECISION: deliberately NOT chained into `just e2e` — pure-verdict spike, no
# product behaviour changes by default (E5/E6 precedent).
d0:
    bash scripts/d0-navigation-spike.sh

# D1 offline + config hardening (PLAN4 milestone D1). Sets every Penpot flag
# that deletes a cloud surface (registration, the dashboard's cloud templates
# section, the Google-fonts provider, login-with-password) and proves the
# offline promise: the flags are SERVED, they actually TOOK EFFECT (the
# surface is gone — a renamed upstream flag must turn this red, not leave it
# green), the registration closure is proven separately via the navwatch
# policy's own unit tests (registration=present is a documented, tolerated
# outcome — it is closed by navigation, not by the flag), and a full session
# makes ZERO non-loopback connections, checked on BOTH sides (the SPA's
# request log and the supervised processes' sockets). Dedicated ports
# 9046/6508/5581/6524 (control 9047). Chained into `just e2e` — D1 lands
# product code, unlike the D0 spike.
d1:
    bash scripts/d1-offline.sh

# D2 front-door gate (PLAN4 milestone D2). Drives the full project/file
# lifecycle (create/rename/duplicate/move/delete) through
# /__api/vault/manage/* only, asserts the folder tree reflects every
# operation (polled, never on the first read), and asserts a deleted file
# STAYS deleted across a full restart of the stack — the load-bearing check,
# because that is exactly where the core invariant (disk resurrects anything
# missing from the DB) and the delete verb collide. Also asserts `/dashboard`
# is never loaded in the whole session (D0's navwatch mechanism, REQUIRES A
# GUI SESSION like d0 — not CI-headless) and discharges D0's deferred
# caveat: D0 only ever measured a seeded canary with no workspace open; this
# opens a real file, lets it fully render, attempts a #/dashboard
# navigation, and hashes the vault tree before/after. Dedicated ports
# 9048/6510/5583/6526. Chained into `just e2e` — D2 lands product code.
d2:
    bash scripts/d2-home.sh

# D3 menu-bar gate (PLAN4 milestone D3). A native menu cannot be clicked in
# CI, so this proves the menu bar three other ways: (a) the pure model's
# shape via pinned unit-test names (menubar::/windows::/recent::/reveal:: —
# a renamed or weakened test fails this, not just "the suite passed"); (b)
# every command's underlying route/function against a live stack (New
# File/New Project via /__api/vault/manage/*, the five View pages by
# fetching and asserting a real render, Reveal via its pure command
# builder, Open Vault via the control server) with a printed
# covered/not-covered table so a gap is never silent; (c) no orphaned items
# in either direction (dead item <-> dead command, both directions pinned).
# The GUI half of window-per-file (two real windows, two titles, ⌘Q) has no
# automation surface available (no Tauri IPC command exists to drive it, no
# window-enumeration tool here) — printed as an explicit SKIPPED leg, never
# a silent or counted-as-pass omission, distinct from the GUI boot-only
# smoke check which DOES run and assert PASS/FAIL. Dedicated ports proxy
# 9050, backend 6512, postgres 5585, valkey 6528, control 9051. Chained
# into `just e2e` — D3 lands product code.
d3:
    bash scripts/d3-menus.sh

# SPA hash-route version-bump gate (PLAN2 risk 2): grep the route strings out
# of the compiled bundle + a live headless-browser navigation assert. Boots its
# own throwaway stack unless ROUTES_GATE_BASE points at a running one. Run this
# (with roundtrip.py) first after any Penpot version bump.
routes-gate:
    bash scripts/routes-gate.sh

# THE e2e chain (PLAN2.md N1): every milestone suite, serialized — the
# suites are concurrency-UNSAFE against sibling stacks (m4's lsof lesson),
# so never run them in parallel. Chains every landed gate (N1–N6, E1–E4, E7,
# D1–D3). e1-contract is a fast static gate (no stack); e2/e3/e4/e7/d1/d2/d3
# boot their own live stacks (dedicated ports) like the m/n suites, safe to
# chain at the tail. n2-thumbs + e4-gallery + e7-plugins need the runtime
# bundle WITH browsers (`bash scripts/fetch-penpot.sh --with-browsers`);
# d1-offline and d2-home need the same bundled browsers (their behavioural
# checks drive a headless browser too). d2-home ALSO needs a GUI session for
# its navwatch leg (same operational constraint as d0 — not CI-headless) —
# unlike d0, d2 stays in the chain because it lands product code. d3-menus
# also boots a real GUI session for its boot-only smoke leg (same
# constraint), chained for the same reason: D3 lands product code.
# m4-artifact-test.sh stays separate: it needs a dmg build
# (`bash scripts/build-dmg.sh` first) and carries the E4 offline
# packaged-gallery leg (g4) + the E7 offline plugin-serving leg (g5).
# e5-tokens-spike.sh + e6-library-portability-spike.sh stay out by decision:
# pure-verdict SPIKE gates with no product code to regress (see their recipe
# comments) — e7 and d1 are chained because their thin builds DID land
# product code. d0-navigation-spike.sh stays out too: pure-verdict spike, no
# product behaviour changes by default (D1's navwatch policy IS the product
# behaviour D0 spiked — D1 exercises it via cargo test, not by re-running D0).
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
    bash scripts/e4-gallery.sh
    bash scripts/e7-plugins-spike.sh
    bash scripts/d1-offline.sh
    bash scripts/d2-home.sh
    bash scripts/d3-menus.sh

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
