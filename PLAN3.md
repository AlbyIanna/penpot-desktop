# Penpot Local — chapter 3: THE ECOSYSTEM

Chapter 1 (PLAN.md, M0–M5) built the engine: supervised embedded stack, custom proxy, two-way sync
between the disposable DB and a folder tree of git-diffable `.penpot` dirs, conflict copies, an
offline dmg. Chapter 2 (PLAN2.md, N1–N6) made the folder tree the front door: a lighttable home off
disk, offline FTS5 search, a Cmd+K palette, manual git checkpoints, plural vaults with zero
cross-vault spill, and — the first foothold of packaging — an offline template gallery that imports a
builtin binfile and settles it to a fixpoint. Chapter 3 is the reason folder-is-truth was worth
building: if a design file is just a folder, then **a package is just another folder**, and sharing
becomes `git clone`. It realizes the durable values in
[docs/ecosystem-concept.md](docs/ecosystem-concept.md) — local-first/no-server, git-repos-not-a-registry,
flat governance, surface-don't-apply, contract-over-implementation — and settles or spikes the three
open questions in [docs/ecosystem-design.md](docs/ecosystem-design.md). The contract-extractability
spike already ran ([docs/ecosystem-spikes/contract-extractability.md](docs/ecosystem-spikes/contract-extractability.md)),
verdict **GO-with-caveats** — the contract IS machine-extractable, id-free, round-trip-stable.

## Vision

> A component library, a token set, a plugin — each is a git repo of plain files under
> `.penpot-packages/` in your vault. Clone one and it is on disk; nothing phones home to find,
> install, or use it. Install is an explicit verb — the sync daemon never auto-imports a package —
> so what enters your file is a choice, not a surprise. A tool lints your own library before you
> share it: "this edit dropped a variant your consumers use — that's a major bump." When a package
> you depend on changes, the app shows you the contract diff and the new version; it never rewrites
> your files. Nothing is applied silently, nothing is inlined, nothing needs a server or a registry
> or a blessed publisher. Delete the app and every package is still a folder you can grep and diff.

**North-star story:** a designer clones a teammate's button library into `.penpot-packages/`,
installs it with one click, links it, drops instances. Weeks later it ships v2.1; the app surfaces
"minor: 2 variants added, contract intact" — she updates when she wants. She edits her own icon
library and the lint says "major: you renamed `size/lg` — bump or your consumers dangle." No server ran.

## Invariants

Chapters 1+2 invariants remain sacred and are still the test suites:

> **Core (P0):** delete the entire database, restart, and every project/file is rebuilt from the
> folder tree with no data loss. The folder tree is the source of truth; the DB is a disposable cache.

> **Zero cross-vault spill (P0):** switching vaults never surfaces, imports, or writes a file from
> another vault (PLAN2 invariant 2).

> **The SPA stays byte-untouched (PLAN2 invariant 3):** no serve-time patching of upstream JS/CSS, no
> injected scripts, no plugin as OUR integration mechanism; only URLs reach the canvas. A plugin
> PACKAGE is different — user-installed through Penpot's OWN native Plugin Manager URL/manifest
> boundary, in Penpot's own sandbox on Penpot's origin, which we neither patch nor drive. We never ship
> our features as a plugin.

New ecosystem invariants:

1. **A package is just a folder / git repo.** Packages live under `.penpot-packages/` inside the
   vault. Because that dir is dot-prefixed, both sync-daemon code paths are already blind to it —
   the event watcher (`watcher.rs:41`) and the full reconcile walk (`engine.rs:1878`) skip
   `.`-prefixed dirs, the same guarantee N5's `.penpot-vault` relies on. Packages are therefore
   never auto-imported, hashed, or conflict-swept. The daemon and the installer are **separate
   machines** that meet only at: package bytes on disk (truth) → explicit install → DB (disposable).
2. **Contract over implementation.** A package's versioned surface is its contract
   ({variant names, exposed properties, tokens used} for components; exported color/typography/token
   names for libraries; the API surface for plugins). Implementation-only edits are `patch` and
   invisible to consumers. `revn` is advisory (CLAUDE.md) — update detection rides the contract
   diff, never `revn`.
3. **Surface, don't apply — lifted from files to packages.** A package update is shown as a
   contract bump; drift of a managed package produces a `.conflict-<ts>` copy; neither side is
   ever silently overwritten. This is the chapter-1 conflict rule applied to dependencies.

## Architecture — where the ecosystem lives

| Surface | What | Notes |
|---|---|---|
| `.penpot-packages/` (in-vault) | Package home: one git repo per package; a git-diffable `lock.json` at its root | Dot-dir → invisible to sync (watcher.rs:41, engine.rs:1878). Travels with a `git clone` of the vault, like `.penpot-vault` (vault.rs:9-13). |
| `crates/vault-index/src/contract.rs` | Contract extractor: `extract_contracts(&sem)` beside the existing `extract_docs(&sem)` | Reuses the shipped python-parity normalizer + `semantic_view`/`tree_hash` (hash.rs:80-105); slots into `Indexer::reindex_file` (lib.rs:211-221) verbatim. Ships a `diff_contracts` patch/minor/major classifier. Port of the proven spike scripts. |
| `lock.json` (per vault) | Pins each install: `{id, version, contentHash, contractHash, sourceGitUrl}` | Near-clone of `sync_core::manifest.rs:80-121` (versioned, hard-fail on unknown, atomic tmp+fsync+rename, sorted/LF). Records the DB-only pointers (plugin registry, library file-id) so they re-derive after a DB wipe. |
| Installer | Generalized from N6's `templates.rs::import_new_from_template` (import-binfile + `settle_to_fixpoint`, templates.rs:416-515) | One installer for all design-data package types; template is package-type #0, already shipped. |
| Proxy (`crates/proxy`) | `/__packages` gallery page + `/__api/packages/*`; serves `/__packages/<pkg>/manifest.json` for plugin packages | The `/__templates`,`/__home`,`/__palette` extra-router precedent (`bind_with_router`). Framework-free HTML/JS. Deep-links via `workspace_deep_link` (query.rs). |
| Upstream canvas | Untouched. Libraries link natively (`link-file-to-library`); plugins install via the native Plugin Manager; only URLs/RPCs reach it. | No plugin, no injection, no iframe embedding (invariant 3). |

**Three types, one distribution (git), differing activation.** *Templates* (N6, done): import-as-new +
settle, no refs, the proven pattern. *Component libraries*: design data the installer imports; a consumer
links them and references components by vault-local file-id. *Token packages*: DTCG sets mirrored
read-only into a consumer's `tokens.json` (Penpot has no shared tokens — TokensLib is file-scoped),
origin-stamped, drift = conflict copy. *Plugins*: NOT design data — a repo of static assets served at a
local proxy URL and pointed at through Penpot's own Plugin Manager; its only DB footprint is a
profile-props pointer recorded in `lock.json` so it survives the core invariant.

**The spine — cross-package identity.** Two hard problems are one. `appliedTokens` values are bare
dotted paths with no owning-package qualifier (spike caveat 5; no value in the tokens-starter-kit dump
carries one — E5 re-verifies live); `:component-file` is a vault-local minted uuid that `import-as-new`
remaps per DB. Penpot mandates both bare forms and round-trips them verbatim, so the package/version →
local resolution map must live in OUR `lock.json`, never in the shape/token JSON. Lockfile + resolver
are the spine; every package type hangs off it. The resolver is always a headless build/verify GATE
(like `roundtrip.py`), never an injected runtime — invariant 3 forbids a live in-canvas resolver.

## Milestones

Every milestone lands one `scripts/e<N>-*.sh` gate (run-twice idempotent, chained into `just e2e`,
which already chains m1–m3+m5 + n1–n6; the dmg leg `m4-artifact-test` runs solo) and one
`docs/milestones/e<N>.md` with exit criteria copied in. Spikes additionally write a
`docs/ecosystem-spikes/<name>.md` verdict (the contract-extractability.md precedent) on dedicated
ports; any live leg first re-provisions `m0@local.test` (`manage.py create-profile`) — the learning-rig
volume was wiped by the contract spike. E1–E4 are **builds** (each ships standalone
headless-verifiable value); E5–E7 are **spikes** that de-risk the hard/open parts before a
next-chapter build consumes them (the M0/N2 pattern), E7 also landing a thin plugin install + the dmg.

### E1 — Contract extractor + version classifier (build; standalone; the proven-GO piece)
Goal: turn the contract spike into a real Rust component. `extract_contracts(&sem)` beside
`vault-index/extract.rs` emits, per variant-set, `{variant names, exposed properties, tokens used}`
plus a library's exported color/typography/token names; `diff_contracts` labels a delta `patch`
(impl only) / `minor` (grew) / `major` (lost or renamed). Zero packaging: a designer gets a "did this
edit break my library's contract?" lint over any folder, offline. A straight port of the spike's
python oracle onto the same normalized JSON the ledger hashes — id-free and round-trip-stable (a
component's uuid churns on import; its name/path/type do not). New work is per-set aggregation: group
by first-class `variantId` else legacy shared `path`; set-union `appliedTokens` over the main-instance
subtree; collect `variantProperties[].name`.
**Depends on:** none. **Exit:** `scripts/e1-contract.sh` (`just contract`), run-twice idempotent: over
an authored fixture pair extracts `{(set name/path, exposedProperties[], tokensUsed[])}` and proves
`extract(A) == extract(A')` where A' is A after `settle_to_fixpoint` (uuid churns; keyed by name/path
never the remapped `variantId` — caveat 3); classifies a curated delta matrix (impl-only $value→patch;
added→minor; removed/renamed/$type-changed→major) matching the oracle exactly; **special-cases
legacy→first-class migration** so an empty-exposedProperties legacy set migrating to `variantProperties`
does NOT read as a spurious minor (caveat 2); delete-index-db + reindex rebuilds identical contracts
from disk (invariant 1). Fixture is authored/injected — no shipped file combines components + tokens.

### E2 — Package home + lockfile + generalized installer (build; the spine's on-disk half)
Goal: (1) `.penpot-packages/` as the in-vault package home (the git repos), blind to both sync paths.
(2) A git-diffable `lock.json` (manifest.rs clone) with `{version, contentHash, contractHash (E1),
sourceGitUrl}`, recording only the DB-only pointers a package needs (library-rel, plugin registry).
(3) Generalize N6's template installer into a package installer that records each install in the
lockfile. **Model:** installing a design-data package materializes it as an ORDINARY vault `.penpot`
file (N6 already lands new-from-template in Drafts on disk) — so DB-wipe rebuild is the proven M2
reconcile (resurrect-by-id from the sync manifest), and the lockfile only re-derives the DB-only
pointers the normal tree can't carry. Install is an explicit verb; fetch is `git clone/fetch` into
`.penpot-packages/` — git repos, not a registry.
**Depends on:** E1. **Exit:** `scripts/e2-packages.sh`, run-twice idempotent: drops a template and a
component-library `.penpot` under `.penpot-packages/` and asserts the daemon NEVER enumerates, hashes,
or conflict-copies them (edit a file inside → no manifest entry, no `.conflict` copy); an explicit
install imports and settles to a fixpoint (two equal semantic hashes — no phantom diff on first
rebuild, the N6 P0) and writes a lock entry; delete-DB + reboot re-applies every locked package
deterministically (in-place import preserves file-ids, invariant 1) with NO user-disk write outside
`.penpot-packages/`; `git clone <url>` lands a package and it installs offline; run-twice = no-op.

### E3 — Component-library packages, single-vault (build; scoped UNDER the cross-vault wall)
Goal: the first real dependency-carrying package type, end to end within one vault (under the id wall,
so it stays cheap and proven). Publish a `.penpot` as shared (`set-file-shared`), let a same-vault
consumer link it (`link-file-to-library`) and reference components by the vault-local file-id, pin
`library@version+contractHash` in the consumer's lockfile, surface updates as CONTRACT diffs (E1) not
`revn`. Library contract = union of its components' contracts + exported color/typography/token names.
Single-vault id-stability is already proven (M2's in-place import preserves file-ids across a delete-DB
rebuild) — no new id machinery. `set-file-shared`/`link-file-to-library`/`unlink-file-from-library` are
confirmed present in the pinned jar (`files.clj`) but E3 is their FIRST live exercise — wire and assert
them before building on top. Library referenced, never inlined (`include-libraries` is the wrong model).
**Depends on:** E1, E2. **Exit:** `scripts/e3-library.sh`, run-twice idempotent: installs + publishes
a library, authors a consumer that links it and places an instance, asserts the instance's
`:component-file` resolves to the library's vault-local id; delete-DB + reboot rebuilds both files and
the instance STILL resolves (invariant 1); a patch edit surfaces NO bump while minor/major edits surface
the correct bump via the lockfile-diff channel (NOT `revn`); `file-library-rel` is re-established from
the lockfile on rebuild (derived/disposable); asserts `include-libraries` is unused.

### E4 — Package gallery + surface-don't-apply update channel + ecosystem dmg (build)
Goal: a browse/search surface plus the update/conflict surface that makes surface-don't-apply real for
dependencies, and the chapter's packaged-artifact proof. **Largest build of the ladder (N3's shape) —
three sub-surfaces + a dmg leg; pre-drawn split if it overruns a day: E4a gallery + `DocKind::Package`
index, E4b update/conflict status-surface + the extended dmg leg.** Index installed/available contracts into a
`DocKind::Package` FTS5 row (Indexer `needs_reindex` gate + `replace_file`-per-owner txn); serve a
framework-free `/__packages` page; render "update available" / "contract-major bump" as a
status-snapshot state parallel to the N3 activity/conflict strip; handle drift of a managed package
with the EXACT conflict rule (`.conflict-<ts>` copy, overwrite neither side); deep-link via
`workspace_deep_link`. Flat gallery — no verified tier, no badges, no monetization.
**Depends on:** E2, E3. **Exit:** `scripts/e4-gallery.sh`, run-twice idempotent: indexes N packages
and asserts `/__api/packages` search returns correct ids in <100ms at torture scale; a headless-browser
leg (routes-gate style) loads `/__packages` and deep-links one package file to its exact `/#/workspace`
URL (string-asserted from the payload); an edited package surfaces the correct minor/major bump in the
status SSE within the poll+debounce window while the consumer file stays BYTE-UNCHANGED (surfaced, not
applied); a drifted managed-package copy produces a `.conflict-<ts>` copy overwriting neither side; an
extended `m4-artifact-test` leg boots a fresh dmg under `env -i` + poisoned proxies offline to
`/__packages`; delete-index-db rebuild identical (invariant 1).

### E5 — SPIKE: cross-package token resolver (open question #2)
Goal: convert this session's code-read + on-disk-dump token findings into an EXECUTED GO/NO-GO on the
mirror-and-surface token-package model — the line between "just a folder" and a package manager. Prove
on a live 2.16.2 that merging a package's DTCG sets into a consumer resolves and round-trips A=B; that
collision precedence behaves as upstream's real `get-tokens-in-active-sets`/`get-active-themes`
(tokens_lib.cljc) imply (later set in `tokenSetOrder` wins; the same bare path `layerBase.text` resolves
differently by active theme WITHIN one file); and that a STATIC resolver is headless and gate-able
(never injected — invariant 3). Token surface = sets walked to `(path,$type)` leaves; declared deps =
free variables in each `$value`'s `{refs}` minus own exports (token math `{modular.xl}*{density}` is
real). `tokenSetOrder` + theme/set-activation are PART of the contract: order-flip-on-collision = major;
shadowing-add reads-minor-behaves-major (special-case, the token analogue of caveat 2); theme-only change
= major-behavioral. A dangling-ref baseline separates "package dropped a token you depend on" (major)
from "ref was already open" (noise — the starter kit ships pre-existing dangling refs; E5 pins the set).
**Depends on:** E1, E2. **Exit:** `scripts/e5-tokens-spike.sh` (dedicated ports), against a fresh
re-provisioned 2.16.2: mirrors a package's sets into a consumer and asserts the merged file round-trips
A=B per `roundtrip.py`; asserts a scripted collision resolves to the `tokenSetOrder`-winner AND that
flipping the order flips the resolved value (order-is-contract); runs the static resolver headless over
the starter-kit dump and asserts it (a) lists each token's free-variable deps, (b) reproduces the
starter-kit's pre-existing dangling paths as already-open not new breakage, (c) flags a synthetic dropped token as
major; writes `docs/ecosystem-spikes/token-resolver.md` with a GO/NO-GO verdict + the mirror-ownership
(read-only, `external-id`-stamped) and drift=conflict-copy rule. Ships VERDICT + GATE, not a UI. Green twice.

### E6 — SPIKE: cross-vault component-library id-remap resolver
Goal: de-risk the biggest engineering wall before any next-chapter portable-library build. `import-as-new`
is per-DB non-deterministic, so the SAME package in a different vault gets a DIFFERENT `:component-file`
id and a consumer carried across machines dangles every instance (E3 works only because one vault mints
the id once and in-place import preserves it). Prove a stable package-identity (repo URL + version, or
content hash) → local-file-id map plus an install-time REWRITE pass rewriting a consumer's
`:component-file`/`:component-id`/`:shape-ref` to this-vault ids, pinned in the lockfile,
delete-DB-rebuild clean. The component analog of E5's token seam — ONE cross-package identity problem.
**Depends on:** E3. **Exit:** `scripts/e6-library-portability-spike.sh` (dedicated ports), live:
installs the SAME package into vaults A and B and asserts the minted file-ids DIFFER; authors in A a
consumer referencing the library, carries that file to B, runs the rewrite pass, asserts every instance
resolves in B with ZERO dangling refs; delete-DB + reboot on BOTH vaults and re-assert (id-stability +
rewrite both hold, invariant 1); writes `docs/ecosystem-spikes/library-portability.md` with a GO/NO-GO
verdict. Ships verdict + rewrite tool + gate, not a UI. Green twice.

### E7 — SPIKE + thin build: plugin packages (activation vs invariant 3, CSP-egress) + dmg
Goal: settle the third package type honestly and close the chapter's dmg. **Staged: the ACTIVATION
spike runs first (does install-through-the-native-Plugin-Manager work on 2.16.2 without driving the
SPA?); the thin build ships ONLY if activation lands GO — a NO-GO ships the verdict alone and defers
plugins whole.** A plugin package = a git repo
of static assets (`manifest.json` + `plugin.js` + icon) under `.penpot-packages/`, served at the local
proxy (`/__packages/<pkg>/manifest.json`), carried-and-pointed-at, never imported into the design DB.
Prove on a live 2.16.2 self-hosted stack that it installs through Penpot's OWN native Plugin Manager URL
boundary — never by patching/injecting/driving the SPA (invariant 3 intact). The registry pointer is
written via the PUBLIC `update-profile-props` RPC (same class as `import-binfile`); it is DB-only derived
state, so record it in `lock.json` and re-apply after a wipe. Name the supply-chain limit precisely and
spike the one open mitigation: can our proxy inject a Content-Security-Policy RESPONSE HEADER on the
plugin frontend without touching JS (a header at the proxy is arguably inside invariant 3; a JS patch is
not). Ship a THIN install (content-pinned + offline + `penpotPluginsWhitelist` pinned to the local origin
+ Penpot's native consent gate) with the explicit promise it is NOT egress-contained unless the CSP spike
lands GO. Honor surface-don't-apply: present the discovered plugin, the USER clicks Install — never
silent auto-registration.
**Depends on:** E2. **Exit:** `scripts/e7-plugins-spike.sh` (dedicated ports), live. **Activation
(the gate):** a headless-browser leg (routes-gate style — test automation drives the native UI, it is
not our integration mechanism) opens the native Plugin Manager, enters the local `/__packages/<pkg>/manifest.json`
URL, passes Penpot's own consent prompt, and asserts the plugin's own observable effect appears in the
workspace — WHILE the served SPA bytes are unchanged (index.html + JS bundle sha256 identical before/after,
no injected script). **Persistence:** assert the pointer is written via `update-profile-props`; delete the
DB, reboot, assert the plugin re-registers from `lock.json`. **CSP-egress probe (concrete):** the fixture
plugin attempts a `fetch()` to an off-origin URL; with the proxy CSP response header OFF the request is
observed leaving (network log), with it ON the request is CSP-blocked and absent — GO only if ON blocks and
the plugin still loads. **Consent:** assert no pointer is written without the browser-leg's explicit install
step (no pre-seeding path). Record GO/NO-GO in `docs/ecosystem-spikes/plugin-supply-chain.md` with the EXACT
promise ("content-pinned + offline + whitelisted-origin + consent-gated; NOT sandboxed-from-your-data unless
CSP-GO"). If activation NO-GO: ship the verdict, no build. An extended `m4-artifact-test` leg boots a fresh
dmg under `env -i` + poisoned proxies offline to `/__packages` with a plugin package present. Green twice.

## Known risks (read before coding)

1. **Cross-vault library portability is unsolved and structural.** Every cross-file ref joins on the
   library's vault-local minted uuid (`:component-file`); `import-as-new` mints a different id per DB, so
   a component-library package — meant to be globally shared — is single-vault-only until E6's id-remap
   resolver exists; a consumer carried to another machine dangles every instance with no error. Same seam
   as tokens (bare `appliedTokens` paths carry no package qualifier — spike caveat 5): treat cross-package
   identity (file-ids AND token-paths) as ONE spine problem. Do not let E3's single-vault success hide
   that portability is a separate, harder piece.
2. **Mirror-and-surface tokens fight the folder-is-truth invariant.** Penpot has NO shared/linked
   tokens (TokensLib is file-scoped in 2.16.2), so a usable token package must physically mirror its
   sets into every consuming file's `tokens.json`. That copy is derived state that can drift — a
   data-loss-class failure if silently overwritten. Fix the rule before E5: package-owned sets are
   mirrored READ-ONLY, origin-stamped via `TokenTheme.external-id`, drift → `.conflict` copy. And
   resolution is theme- and `tokenSetOrder`-dependent and computed live: a token package can change a
   consumer's resolved values WITHOUT changing any value it exports (order-flip = major, no value diff).
3. **Plugin supply chain cannot be fully contained — over-promising is the trap.** A plugin is
   arbitrary executable JS from an arbitrary repo; once the user grants `content:write` it can read
   AND rewrite every shape/component/token in the open file and, being live iframe JS, fetch arbitrary
   URLs to exfiltrate. We can honestly promise content-pinned + offline + `penpotPluginsWhitelist`-pinned
   + Penpot's native consent gate, but NOT data-isolation or egress control without a proxy-injected CSP
   whose feasibility-within-invariant-3 is unproven. Ship only with the modest, explicit promise; never
   claim containment until E7's CSP spike lands GO. Never ship OUR features as a plugin (invariant 3).
4. **Two variant models coexist forever, and a migration is a false positive.** First-class
   `variantProperties` AND the legacy shared-`path` convention both persist; a legacy set has empty
   exposedProperties, so a legacy→first-class migration reads as "contract grew" (spurious minor) while
   behaving like a rename/major. The classifier MUST special-case it (E1). Match variant-set identity by
   name/path, never the import-remapped `variantId` (caveat 3).
5. **The shipped corpus cannot verify a full contract.** No bundled file combines components + tokens
   (`penpot-design-system` is 100% legacy / 0 tokens; `tokens-starter-kit` is 0 components), so a
   full three-legged contract needs an AUTHORED/INJECTED combined fixture, not the shipped templates
   (as the spike did). All plugin/library/token findings this session are code-read + on-disk-dump
   only, never run against a live 2.16.2 (the m0 rig was wiped) — E5/E6/E7 exist to convert code-read →
   executed, each re-provisioning `m0@local.test` first.
6. **Upstream drift + maintenance weight.** New couplings this chapter: the token model
   (`tokens_lib.cljc` resolution + DTCG shape), the plugin subsystem (`register.cljs`
   `update-profile-props`, `penpotPluginsWhitelist`), and `link-file-to-library`/`set-file-shared`.
   All are upstream-owned and only lightly explored live. Mitigation: each rides an executable gate
   joined to `just e2e`; the contract extractor reads only normalized disk (drift-immune, like the
   index). One-person + AI team → keep the resolver a static gate, not a runtime; reuse N6's installer
   and manifest.rs's lockfile rather than inventing; defer both hard walls behind spikes.

## Open product questions (decide with product owner)

The first three set the chapter's SHAPE and should be decided before E1; the last two can wait.

> **DECIDED (product owner, 2026-07-15):** (1) **single-vault now** — E3 ships libraries that work
> within one vault; portability stays the E6 spike (does not graduate to a build this chapter). (2)
> **defer token packages** — E5 ships the resolver VERDICT + gate only; token-package UI is chapter 4.
> (3) **thin plugin install** — E7 ships the consent-gated, content-pinned, whitelisted, user-click-only
> install with the explicit "NOT egress-contained" caveat, staged so a NO-GO on activation ships
> verdict-only. Chapter 3 = E1–E4 builds + E5/E6/E7 spikes (E7 with its thin build).

1. **Cross-vault portability — is single-vault enough?** E3 ships libraries that work within one vault
   (id-stability is free); portable cross-machine sharing needs the E6 build-out, and sharing is the
   ecosystem's whole point. Is the chapter-3 promise "packages work in your vault" (portability
   spiked/deferred), or must portable libraries actually ship (E6 graduates spike→build)?
2. **Token packages — ship or defer?** Recommended: chapter 3 delivers the E5 resolver VERDICT + GATE
   only, with token-package UI a chapter-4 build (tokens are the hardest type — theme-dependent
   resolution, order-as-contract, mirror drift, free-variable deps). Confirm.
3. **Plugins — ship the honest limited promise, or hold for CSP-GO?** Ship the thin, consent-gated,
   content-pinned, whitelisted install with the explicit "NOT egress-contained" caveat, or gate the
   whole feature on E7's CSP result? Activation recommend: user-click-only in the native Plugin Manager
   (honors "the user chose it"), never pre-seeding the pointer.
4. **Federated discovery — any index-runner this chapter?** Or is "clone a git URL, then install" the
   whole distribution story for now (recommended — E2 already gives it)?
5. **Template contract — confirm templates stay contract-free** (a starting point, not a dependency),
   so E1's classifier need not version them and they remain package-type #0.

## Out of scope (non-goals for this chapter)

- **No registry, no monetization/tiers/badges.** Distribution is `git clone`/`fetch`; the lockfile
  records the source URL. Discovery indexes are federated, optional, deferred. Flat governance — the
  gallery ranks nothing by publisher (ecosystem-concept.md).
- **No forking or patching the SPA, no live in-canvas resolver.** No CSS/JS injection, no serve-time
  index.html edits, no plugin as OUR mechanism; packages reach the canvas only as URLs/native-RPCs; the
  resolver is always a headless gate, never an injected runtime. The one boundary case is a proxy CSP
  *response header* (spiked in E7 — a header, never a JS patch).
- **No portable-library build, no token-package UI, no plugin containment guarantee** this chapter —
  each is spiked to a verdict (E5/E6/E7) and built next chapter once GO.
- **No auto-install.** The daemon is blind to `.penpot-packages/`; install is always explicit (surface,
  don't apply).

## Claude Code readiness

- **Reuse, don't reinvent.** The contract extractor is a sibling of `vault-index/extract.rs`; the
  installer is N6's `templates.rs`; the lockfile is `manifest.rs`; the gallery is the Indexer +
  a `/__templates`-style page; git plumbing follows `checkpoint.rs`'s safe-verb discipline. Point
  sessions at those files (paths in the milestone tables). The `scripts/ecosystem-spike/` scripts seed
  `just contract`, the way `roundtrip.py` seeded the harness.
- **Everything headless; extend `just e2e`, never rebuild the harness.** Each milestone boots the
  headless bin, drives RPC + HTTP, asserts on filesystem/URLs; suites run solo (m4's lsof lesson).
- **Before claiming a milestone done:** `just e2e` + `routes-gate.sh` green. Any Penpot version bump:
  `roundtrip.py` + `routes-gate.sh` first. Any live spike leg re-provisions `m0@local.test`.
