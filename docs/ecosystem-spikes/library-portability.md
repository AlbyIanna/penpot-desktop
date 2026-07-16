# Spike: cross-vault library portability (id-remap resolver)

Answers PLAN3's E6 — the biggest engineering wall before any portable-library build.
`import-binfile` **without** a `file-id` (import-as-new) is per-DB non-deterministic:
the SAME component-library package installed into two different vaults mints a
**different `:component-file` id**, so a consumer file authored in vault A and carried
to vault B dangles every component instance. [E3](e3-linking.md) works single-vault
only because one vault mints the id once and in-place import preserves it forever
(M2 resurrect-by-id). This spike converts that wall into an **executed GO/NO-GO** on a
stable package-identity → local-file-id map plus an install-time **rewrite pass**.

**Verdict: GO WITH CAVEATS.** The wall is real and live-witnessed (the naive carry
dangles), the identity map is derivable **statically** from the two vaults' materialized
`.penpot` trees joined on E1's uuid-stable keys — components **and** exported
colors/typographies, so library-styled fills/strokes/text (the `*RefId`/`*RefFile`
asset refs) are covered, not just component instances — the offline rewrite pass
produces zero dangling refs in the destination vault (verified over RPC **and** on
disk), and both vaults survive delete-DB + reboot (invariant 1) with `file_library_rel`
re-derived from `lock.json`. The honest surprise: **the wall is narrower than PLAN3
feared** — on 2.16.2 only the *file* id is remapped; every id *inside* the library file
rides the source bytes unchanged. The caveats — version-drift of that undocumented
property (including one named hole: swap-slot refs), the carried consumer's mandatory
lock entry, and same-version map scope — are listed at the end; none is a blocker.

Everything below was produced by execution against **Penpot 2.16.2** (the app's own
re-provisioned headless stack, dedicated E6 ports 9010/6472/5545/6488, control 9011)
on 2026-07-16, green on two consecutive fresh-stack runs. The executable gate is
[`scripts/e6-library-portability-spike.sh`](../../scripts/e6-library-portability-spike.sh)
(`just e6`); its helpers are
[`scripts/ecosystem-spike/e6_probe.py`](../../scripts/ecosystem-spike/e6_probe.py)
(live probe, reusing `scripts/roundtrip.py`'s RPC client) and
[`scripts/ecosystem-spike/e6_rewrite.py`](../../scripts/ecosystem-spike/e6_rewrite.py)
(**the rewrite tool** — stdlib-only, static, offline; never injected, never drives the
SPA — ecosystem invariant 3). A ports note: the PLAN3 sketch reserved backend 6471, but
that is N2's exporter port; the gate uses 6472, the first free neighbor.

**Evidence classes — read claims precisely:**

1. **Live-captured**: produced by the app's own stack and read back over RPC or from
   exported bytes (minted ids, dangle witness, zero-dangling verification, invariant 1).
2. **Static-over-live-bytes**: computed by our `e6_rewrite.py` over the live-exported
   normalized trees (the identity map, the on-disk zero-dangling verify).
3. **Jar-confirmed**: the *mechanism* of the id behavior, read from
   `app/binfile/v3.clj` + `app/binfile/common.clj` inside `runtime/backend/penpot.jar`
   — see "the wall, precisely" below.
4. **Selftest-proven**: the mapping's *discrimination* — exercised on a CHECKED-IN
   offline fixture with genuinely different ids (`e6_rewrite.py selftest`, run as the
   gate's first leg on every run), because on 2.16.2 the live path cannot distinguish a
   correct mapping from a no-op. The selftest asserts correct component/shape/color/
   typography pairing, the dropped-child (`subtree size differs`) refusal, the
   duplicate-key refusal, and the asset-ref rewrite on a synthetic consumer.

## The wall, precisely (exit criterion a)

The gate authors the package source in a scratch vault, builds a `file://`-fetchable
git repo (`e6-button-kit@1.0.0`: a variant-set component `Controls / Button` / `Primary`
whose main instance is a frame with two children, a plain component `Controls` / `Badge`,
plus an exported color and typography), then installs + publishes the SAME repo into
vaults A and B through the E2/E3 verbs (`/__api/packages/fetch` + `publish`). Live
capture:

```
PASS: minted :component-file ids DIFFER (src=d131575e-720c-81a5-8008-55ef3781b77e
      A=b67c3740-8d76-8186-8008-55ef4230f0d9 B=3ee5b21b-b3b7-8190-8008-55ef5960c9f9)
```

Three-way distinct — the per-DB non-determinism is real, and it is the hard assert.

**Honest correction to the premise.** PLAN3's E6 sketch assumed import-as-new also
remaps every id *inside* the library file (componentId, mainInstanceId, subtree shape
ids). Live capture says otherwise on 2.16.2:

```
PASS: CAPTURED: internal componentId/mainInstanceId/shape ids are PRESERVED from the
      package source (2.16.2 v3 import remaps only file ids) — the FILE-id fields
      (componentFile + the asset *RefFile trio) are the ONLY dangling ref class today
```

Mechanism (jar-confirmed): binfile-v3's `import-file` builds its remap index
(`bfc/update-index`) over **file ids plus media/thumbnail object ids only**
(`uuid/next` per file); `relink-refs` passes un-indexed shape/component ids through a
`lookup-index` that returns them unchanged. So the Badge's `componentId` is
byte-identical in the source repo, vault A, and vault B. Consequently the naive carried
consumer verified against B's library dangles **only on the FILE-id ref class** —
`componentFile` on both instance roots plus the asset `*RefFile` fields on every
library-styled fill/stroke/typography node (7 refs total in the fixture: 2 + 5) — and
nothing else:

```
PASS: (C) NAIVE carry dangles in B (7 dangling refs — the wall is real)
PASS: (C) the naive dangle set includes ASSET refs (5 of them — the verifier
      enumerates fill/stroke/typography refs)
PASS: (C) vault-A library file id is NOT live in vault B (get-file fails)
```

This behavior is **captured, not presumed**: the gate's phase-1 assert hard-fails only
on the file-id non-determinism and *records* which of the two worlds — PRESERVED or
REMAPPED internals — the running Penpot lives in, passing in either. The rewrite tool
implements the full component-level map regardless, so it covers both worlds (for
preserved internals the componentId/shapeRef halves are identity maps).

## The identity map (design)

A **static materialized join** — never a live query — over two normalized `.penpot`
library trees (the OLD one the consumer references, i.e. vault A's materialized file,
and the NEW one in the destination vault):

- **Components**: `files/<fid>/components/<cid>.json` (tombstones skipped), keyed by
  E1's proven uuid-free identity — JSON of `{path, name, sorted variantProperties}`.
  Never `id`/`variantId` (those are exactly what import-as-new may remap). E1 proved
  this keying is uuid-invariant; the gate re-proves it here across three mintings:
  `PASS: uuid-stable E1 keys join source/A/B 1:1 (2 components)`.
- **Colors / typographies**: `files/<fid>/colors/<id>.json` and
  `files/<fid>/typographies/<id>.json`, keyed by `{path, name}` (the E1 contract
  already treats exported color/typography names as identity). These feed
  `colorIdMap` / `typographyIdMap` — the maps behind the ASSET refs
  (`fillColorRefId`, `strokeColorRefId`, `typographyRefId`).
- **Shapes**: all `files/<fid>/pages/<pid>/<sid>.json` page shapes, `id → shape`.
- **Join**: per key present in both libraries, emit `componentId(old) → componentId(new)`,
  and pair the component's **main-instance subtree** by lockstep DFS in the parent's
  `shapes`-vector order, asserting `(name, type)` equality at every position. A
  mismatch drops that subtree's map and reports a named problem — the tool **refuses
  rather than guesses**.
- **Duplicate keys refuse too**: a key duplicated within EITHER tree (two live
  components with the same path+name+variantProperties, or two colors/typographies
  with the same path+name) is **excluded** from the map and reported under
  `duplicates` — a silently-picked survivor would be walk-order nondeterministic and
  could produce a resolving-but-WRONG mapping. `derive-map` standalone exits nonzero
  whenever duplicates exist; `rewrite` refuses (nonzero, the duplicate named in
  `unmappable`) exactly when a consumer ref needs an excluded key. Proven offline in
  the selftest (evidence class 4).
- **Output**: `fileIdMap` (`{oldFid → newFid}`) + `componentIdMap` + `shapeIdMap` +
  `colorIdMap` + `typographyIdMap` + per-component entries + `problems` +
  `duplicates`.

Captured map from the gate run:

```
map: fileId b67c3740-8d76-8186-8008-55ef4230f0d9 -> 3ee5b21b-b3b7-8190-8008-55ef5960c9f9;
     2 components; 4 subtree shapes; 1 colors; 1 typographies; problems=0 duplicates=0
entry Controls/Badge: depth=1 structuralMatch=True
entry Controls / Button/Primary: depth=2 structuralMatch=True
```

The package SOURCE tree provides the same keyed capture for provenance
(`source-ids.json`), proving key stability across source/A/B. The map needs **no live
stack**: both inputs resurrect by id from disk under M2, so the map is a pure function
of the two vaults' trees.

## The rewrite pass (exit criterion b)

`e6_rewrite.py rewrite <consumer_tree> <old_lib_tree> <new_lib_tree> <out_tree>`
walks every `.json` in the consumer tree and rewrites, per node:

- dicts carrying `componentFile == oldLibFileId`: `componentFile → newLibFileId`,
  `componentId` and `shapeRef` through the maps (unmappable values are *recorded*, not
  skipped silently);
- dicts carrying a **bare `shapeRef`** into the library subtree (nested copy shapes):
  `shapeRef` through the shape map (a bare ref into an excluded duplicate-key subtree
  is recorded unmappable, never silently passed through);
- dicts carrying a **library ASSET ref** — `fillColorRefFile`/`fillColorRefId`
  (fills), `strokeColorRefFile`/`strokeColorRefId` (strokes),
  `typographyRefFile`/`typographyRefId` (these live on text **content** nodes:
  paragraphs and spans, jar `app/common/types/text.cljc`) — `*RefFile → newLibFileId`,
  `*RefId` through `colorIdMap`/`typographyIdMap` (else recorded unmappable). Field
  names verified live: the gate authors library-styled shapes over RPC and reads the
  camelCase fields back from both the RPC data and the exported tree.

Everything else — including the consumer's own file id and internal shape ids
(import-as-new remints those anyway) — is untouched. Output keeps the M0 normalization
spec. Exit 0 iff map `problems=[]` AND `unmappable=[]` AND the built-in post-verify
reports zero dangling.

**Timing is load-bearing**: the carried tree is rewritten ON DISK and copied into
vault B **while the stack is down** (no sync race), *before* the daemon ever sees it.
Full captured flow:

```
PASS: (C) REWRITE TOOL: … rewritten: componentFile=2 componentId=2 shapeRef=4
      assetRefFile=5 assetRefId=5 (files touched: 4)
PASS: (C) all 5 library ASSET refs rewritten (fill+stroke color, paragraph+span
      typography, span fill)
PASS: (D) rewritten consumer tree copied into vault B (stack down — no sync race)
PASS: (D) carried consumer imported as-new in B (fresh fileId)
PASS: (D) lock.json pins the carried consumer + its library link (re-derivable)
PASS: (D) link-file-to-library re-established file_library_rel in B
PASS: (D) DAEMON re-exported the consumer surgically: componentFile=<libB> kept, embedded id now B-minted
PASS: ZERO dangling refs over RPC (instances=2, assetRefs=5, refsChecked=9)
PASS: (D) ZERO dangling refs in B's ON-DISK exported tree (static verify)
```

"Zero dangling" is checked **both ways**: live (`get-file` over RPC — every
`componentFile` equals B's library id, every `componentId` a live component there,
every `shapeRef` (root and nested) a real shape there, every asset `*RefFile` equals
B's library id and every `*RefId` a live color/typography in its
`data.colors`/`data.typographies`) and static (the same definition in
`e6_rewrite.py verify` over B's daemon-exported tree — asset refs enumerated
recursively through fills, strokes and text content, so a dangling asset ref fails
loudly in both verifiers). The static verifier's mainInstance exemption applies ONLY
when `componentFile` is the consumer's own file id, mirroring the live verifier's
strictness. The daemon minting a fresh consumer id in B is expected and fine — the
consumer's identity in B is its lock entry plus its disk tree, not the A-era id.

## Nested shapeRef depth — the honest statement

- **Proven live to depth 2**: a real copied instance subtree (frame root carrying
  `{componentId, componentFile, componentRoot, shapeRef}` + 2 children each carrying a
  bare per-shape `shapeRef` into the library's main-instance subtree) resolves in B
  with zero dangling after the rewrite, over RPC and on disk — alongside the
  asset-styled shapes (library color on fill+stroke, library typography on text
  content).
- **BUT** on 2.16.2 the live nested shapeRefs (and asset `*RefId`s) would resolve
  *even unmapped*, because v3 import preserves internal ids. So the live leg proves
  the **pipeline** (authoring → surgical export → carry → rewrite → import → link →
  verify), while the **mapping** is proven discriminating by the CHECKED-IN
  `e6_rewrite.py selftest` — synthetic library trees with genuinely different ids for
  every component/shape/color/typography, run as the gate's first leg on every run:
  the lockstep pairing produces the correct cross pairing, a dropped child produces
  `subtree size differs (3 vs 2)` with a refusal, duplicated identity keys are
  excluded and refused when referenced, and the asset-ref rewrite lands on the NEW
  library's ids — never a guess, and no dead-scratchpad evidence: the fixture is the
  tool's own permanent selftest.
- Depth beyond 2 is the same induction over the `shapes` vectors but was **not
  exercised live**.
- The structural pairing **assumes both vaults materialized the same package version**;
  same-key components with reordered/renamed children are detected and refused with a
  named problem.

## What lock.json pins

The existing schema already carries everything the map needs to be re-derived: per
package `{id → fileId, version, contentHash, contractHash, sourceGitUrl}` and per
consumer `links[{libraryFileId, libraryPackageId, version, contractHash}]`.
`libraryPackageId + version` is the cross-vault "same package" join key. The identity
map itself is **not** pinned — it is re-derivable statically from the two on-disk
materialized library trees (each resurrects by id under M2).

The ONE thing E6 adds: **a carried consumer must get a lock entry in the destination
vault** (`fileId` = the id the destination's import minted; `links` → the destination's
library file id) *before the daemon's next export of it*. Without it the daemon exports
with `embedAssets=true` and severs `componentFile` on disk (the E3 caveat), and the
boot re-link has nothing to re-derive `file_library_rel` from. Rules for the future
carry verb (see caveat 2 for the MUST-level ordering):

- write the entry immediately after import — the lock entry write MUST precede, or be
  atomic with, making the tree visible to the daemon (the gate's `add_lock_entry`
  writes the existing `LockEntry` schema, kind `carried-consumer`; empty `contentHash`
  is tolerated and the E4 updates poller degrades gracefully on the missing package
  dir);
- read the destination `fileId` from **`.penpot-sync.json`** (DB-authoritative), never
  from `/__api/vault/boards` — the boards index is disk-derived and reports the carried
  tree's embedded *foreign* id until the daemon re-exports.

## Invariant 1 (exit criterion c)

Both vaults, live:

```
PASS: (E) B after delete-DB+reboot: SAME ids resurrected, zero dangling,
      file_library_rel re-derived from lock.json
PASS: (E) vault-B lock.json byte-identical across the wipe
PASS: (E) post-wipe ON-DISK zero-dangling over the carried consumer's tree (static verify)
PASS: (E) A after the switch-back (its own wipe+rebuild): SAME ids, zero dangling, relinked
```

On B: clean shutdown, `rm -rf <data>/postgres`, reboot — the boot relink reconcile
consumed the carried consumer's lock entry **without any manual re-link**, and the
zero-dangling claim is re-asserted on BOTH halves: over RPC *and* statically over the
consumer's on-disk tree (the disk half of "invariant 1 preserves the rewrite" is
asserted, not inferred from the RPC leg). On A: a vault switch IS a DB wipe +
reconcile-from-disk (N5), and the consumer + library came back under their original A
ids with both instances (and all styled asset refs) resolving.

## Caveats (none blocking)

1. **Version-drift**: internal-id preservation is an UNDOCUMENTED property of 2.16.2's
   binfile-v3 import (the remap index covers file + media ids only). The rewrite tool
   already implements the full E1-keyed component/shape/color/typography map, so it
   survives a Penpot that starts remapping internals — **with one named hole in that
   insurance claim: component-swap slots.** A swapped instance records
   `swap-slot-<uuid>` entries in its `touched` set; those embedded uuids are neither
   rewritten nor verified by this tool. Under 2.16.2's preserved internals they stay
   valid across the carry; in a REMAPPED-internals world a carried consumer that used
   component swap would keep stale slot ids and nothing here would catch it. `just e6`
   must be re-run on every Penpot version bump; the gate explicitly captures PRESERVED
   vs REMAPPED and passes in both worlds — but a REMAPPED capture means swap-slot
   handling becomes real work before any carry verb ships.
2. **Build wiring (MUST)**: a carried consumer needs its destination-vault lock entry
   (fileId + links) before the daemon's next export, or the on-disk representation is
   severed by the embed path. For the future carry verb this is a MUST-level rule:
   **the lock entry write must precede, or be atomic with, making the tree visible to
   the daemon** (the gate satisfies it trivially by copying the tree while the stack
   is down and pinning the entry before the first nudge). The adverse interleaving —
   daemon sees the tree, exports embedded (severing `componentFile` on disk), then the
   process crashes before the lock entry lands, after which invariant 1 faithfully
   restores a SEVERED consumer — is real but was NOT exercised by this gate; the
   ordering rule exists precisely so that window cannot open. The fileId must come
   from `.penpot-sync.json`, never the boards index.
3. **Map scope**: the join assumes both vaults materialized the SAME package version.
   Version skew must route through the E4 update channel *before* rewriting;
   mismatched/missing components, structural mismatches, and duplicated identity keys
   (same path+name+variantProperties, or same color/typography path+name, twice in one
   tree) are refused with named problems, never guessed.
4. **Nested depth**: proven live to depth 2 and selftest-proven discriminating; deeper
   nesting is the same induction, unexercised live (see the honesty section).

## What a portable-library build does with this

The next-chapter "carry a vault (or a consumer file) to another machine" feature
reduces to a deterministic recipe, all of it proven here:

1. **Install** the same packages in the destination vault (E2/E3 verbs; lock.json's
   `sourceGitUrl` + `version` say what to fetch). Version skew → E4 update channel
   first (caveat 3).
2. **Derive the map** statically from the two materialized library trees. The OLD
   tree travels with the carried file; the "or fetch it by
   `libraryPackageId + version`" shortcut works **only in the PRESERVED-internals
   world** (caveat 1), where a fresh fetch reproduces the source-byte internal ids
   the consumer references. In a REMAPPED world the old ids exist nowhere except the
   vault-A materialization — carry without the traveling old tree becomes impossible,
   and the tool refuses (the map cannot join, every ref lands unmappable).
3. **Rewrite** the carried consumer tree offline (`e6_rewrite.py` is the seed of this
   pass — instance refs AND asset refs), refusing on any named problem.
4. **Drop** the rewritten tree into the destination vault while sync is down/paused;
   let the daemon import-as-new.
5. **Pin** the lock entry (fileId from `.penpot-sync.json`, written BEFORE the tree is
   visible to the daemon — caveat 2) and `link-file-to-library` — from then on the E3
   surgical export + M2 resurrect-by-id machinery owns correctness, including across
   DB wipes.

On 2.16.2 step 3 degenerates to remapping the file-id fields — `componentFile` per
instance root plus the asset `*RefFile` trio per styled node (one file-id map that is
already in lock.json links) — materially better than PLAN3 feared — with the full
component/shape/color/typography map as insurance for future Penpots (insurance that
excludes swap slots; caveat 1). The half that actually gates correctness of the
destination's on-disk representation is the E3 surgical-export + lock-entry machinery,
not the rewrite itself.

## Gate + decision

`just e6` runs the whole flow (offline selftest → author source → install into A and B
→ dangle witness → rewrite → carry → zero dangling → invariant 1 on both vaults, with
the post-wipe on-disk static verify) on the dedicated E6 ports
with fresh mktemp vaults, run-twice idempotent, dirs kept on failure, all ports freed on
exit. **Decision: NOT chained into `just e2e`** — spike precedent (E5, and the
contract-extractability spike before it): E6 lands no product code, so the ladder has
nothing of E6's to regress. Re-run it on every Penpot version bump (caveat 1) and when
the portable-library build starts.
