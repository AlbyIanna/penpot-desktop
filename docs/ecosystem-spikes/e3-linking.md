# E3 de-risk spike — single-vault component-library linking

**Verdict: GO — WITH ONE MANDATORY BUILD CAVEAT** (the on-disk export flag).

Every claim below is backed by a live run against the pinned Penpot **2.16.2**
stack booted by `scripts/ecosystem-spike/e3_boot.sh` (headless binary, embedded
postgres/valkey/backend JVM), dedicated E3 ports (proxy 8974, backend 6435, pg
5509, valkey 6452, renders off). Probe: `scripts/ecosystem-spike/e3_probe.py`
(`author` phase → delete-DB + reboot → `rebuild` phase). Two consecutive full
runs, both green (`done (FAIL=0)`). The RPC signatures were cross-read from
`runtime/backend/penpot.jar : app/rpc/commands/files.clj`, but **nothing here is
asserted from the jar** — every value is a captured wire response or on-disk byte
from this run.

The three library RPCs (`set-file-shared`, `link-file-to-library`,
`unlink-file-from-library`) had never been exercised live in this project. They
all work exactly as the schema implies. The single real finding that gates the
build is that the sync daemon's **current export flags (`embedAssets=true`)
destroy the cross-file library reference on disk** — E3 must change the on-disk
representation for linked consumer files. That is the caveat, and it is fully
solved below with a proven recipe.

---

## 1. `set-file-shared` — publish a library (WORKS)

Wire is camelCase (`isShared`), not the kebab `is-shared` seen in the jar.

Request (captured):
```json
{"id": "<libId>", "isShared": true}
```
Response (captured):
```json
{"id": "<libId>", "name": "e3-library", "isShared": true}
```
`get-file {id:libId}` afterwards returns `isShared: true`. The flip is confirmed.
`set-file-shared` is idempotent server-side (files.clj re-sends are a no-op when
already in the desired state). Un-publishing (`isShared:false`) additionally
**deletes every `file_library_rel` where this file is the library** and runs an
"absorb-library" pass — so un-publishing a live library detaches all its
consumers. E3 should treat publish as sticky and never auto-unpublish.

The library authored for the run is a real first-class **variant set**: a
component (`add-component` with `variantId` + `variantProperties`
`[{name:"State",value:"Default"}]`, path `Controls / Button`) plus an exported
color (`add-color`, Brand Teal) and typography (`add-typography`, Heading XL) —
i.e. a genuine library contract, not a bare rect.

**`isShared` travels on disk.** It is a top-level key in the normalized
`files/<libId>.json` (confirmed: `isShared: true` present in the exported tree)
and is restored into the DB by the resurrect import. So the shared flag is **not**
a DB-only pointer — it round-trips through the file tree for free.

## 2. `link-file-to-library` — link a consumer (WORKS)

Request (captured):
```json
{"fileId": "<consumerId>", "libraryId": "<libId>"}
```
Response (captured): `[]`

The response is the *recursive* list of libraries used by the linked library
(`bfc/get-libraries`); our library depends on nothing, so `[]`. Server-side it is
`insert into file_library_rel (file_id, library_file_id) … on conflict do
nothing` plus an `upsert-file-library-sync!` row — so **re-linking is idempotent**
(safe to re-run on every rebuild). It rejects `fileId == libraryId`
(`:invalid-library`) and requires edit permission on both files (always true in
single-user local mode).

## 3. THE CRUX — `:component-file` resolves to the library's vault-local id (WORKS)

The consumer instance was placed with `update-file` (`add-obj`, one change,
`skipValidate:true`), the instance root shape carrying
`{componentId, componentFile:<libId>, componentRoot:true, shapeRef:<libMainId>}`.
Reading it back via `get-file {id:consumerId}`:

- **JSON path (DB / `get-file`):**
  `data.pagesIndex.<pageId>.objects.<instanceId>.componentFile`
- **Value:** `<libId>` — the library's vault-local file-id. ✅
- Siblings on the same shape: `componentId` = the component id, `shapeRef` = the
  library's main-instance shape id, `componentRoot: true`.

The shape schema (`app/common/types/shape.cljc`) defines `:component-id`,
`:component-file`, `:component-root`, `:main-instance`, `:shape-ref` as optional
base-shape attributes, so an instance is a normal shape plus these fields. The
default local flags (`DEFAULT_PENPOT_FLAGS`) do **not** enable `:file-validation`,
so hand-authored instances are accepted; we also pass `skipValidate:true`
defensively.

## 4. `get-file-libraries` + `file_library_rel` — the DERIVED, DISPOSABLE pointer

`get-file-libraries {fileId:<consumerId>}` returns the linked library with full
metadata (`id`, `name`, `isShared:true`, `features`, `revn`, `version`,
`syncedAt`, `projectId`, `teamId`). This is the surfaced `file_library_rel`
relation.

**It is a DB-only relation — it is NOT in the binfile** (see §5) and therefore
**does not survive a DB wipe**. Post-wipe, before any re-link,
`get-file-libraries` returns `[]` (captured: `relGoneAfterWipe: true`). Re-running
`link-file-to-library` re-creates it and it returns again
(`relReDerivable: true`). So `file_library_rel` is exactly the kind of
disposable, lockfile-re-derivable DB pointer E2's lockfile already models — one
lock entry `{consumerId → libId}` per link, re-applied via `link-file-to-library`
on rebuild.

## 5. Export → on-disk representation (THE CAVEAT)

`export-binfile` has three flag combinations, and the instance's on-disk
`componentFile` differs in each. All three were captured this run:

| export flags | on-disk `componentFile` | library inlined? | `manifest.relations` |
|---|---|---|---|
| `embedAssets=true` (⚠️ **the daemon's current flag**, engine.rs:1145) | **`<consumerId>`** (self-ref) | no (embedded into consumer) | `[]` |
| plain (`embed=false, include=false`) — DETACH | **`null`** (stripped) | no | `[]` |
| `includeLibraries=true` | **`<libId>`** ✅ | **yes** (2nd file tree) | `[[consumerId, libId]]` |

Why: `app/binfile/v3.clj` computes `detach? = (not embed-assets) and (not
include-libraries)`. Plain export runs `ctf/detach-external-references` +
`dissoc :libraries` → the reference is **erased**. `embedAssets` embeds the
library's assets into the consumer and **re-points `componentFile` to the
consumer's own id** → a self-contained detached copy, the library link is gone.
Only `includeLibraries` preserves `componentFile=<libId>` — but it does so by
writing the **entire library file as a second `files/<libId>/…` subtree** plus a
`relations` entry in the manifest (this is the `include-libraries` anti-pattern
E3 was told to avoid, now confirmed concretely — see §7).

**Consequence:** the sync daemon currently exports every file with
`embedAssets=true` (`crates/sync-daemon/src/engine.rs:1145`,
`export_binfile(id, false, true)`). If E3 shipped on top of that unchanged, a
linked consumer would land on disk **self-referencing** — the DB-wipe rebuild
would resurrect it as a standalone file with an embedded copy, **not** an
instance of the library. Re-linking would not reconnect the instance. This is the
one thing E3 must change.

### The proven E3 on-disk representation

E3 must write linked-consumer trees from the **`includeLibraries=true` export,
trimmed to the consumer's own file subtree** (keep `files/<consumerId>/…` +
`files/<consumerId>.json`, drop the inlined `files/<libId>/…`, set
`manifest.relations = []`, keep `manifest.files` to the single consumer entry).
This yields a one-file `.penpot` tree whose instance carries
`componentFile=<libId>` as a **bare id reference, with the library NOT inlined**.
The trim is implemented in the probe (`trim_to_single_file`) and verified:
`e3TreeInstanceComponentFile == <libId>` on disk.

## 6. INVARIANT 1 (P0) — delete-DB + reboot rebuild (PASSES)

Sequence: export both files → delete the postgres cluster → reboot → resurrect
both by their **original ids** using the exact M2 daemon recipe
(`crates/sync-daemon/src/engine.rs::import_in_place`): **`create-file` with the
old id, then `import-binfile` in-place**. (Note: raw `import-binfile` with a
`file-id` on an absent file **fails** with `object-not-found` — the create-with-
id step is mandatory; this is why the first probe attempt errored, now fixed.)

Captured post-rebuild results:
- `reimportLibSameId: true`, `reimportConsumerSameId: true` — both files back
  under their ORIGINAL file-ids (M2 resurrect-by-id holds).
- `instance.componentFile_afterRebuild == <libId>` → **`instanceStillResolves:
  true`** — the instance still points at the library's id after the wipe, using
  the trimmed-include representation from §5. ✅
- `relGoneAfterWipe: true` — `file_library_rel` absent until re-linked.
- After one `link-file-to-library` re-run: `relReDerivable: true`,
  `get-file-libraries` returns the library again.
- `isShared: true` on the resurrected library **without** re-running
  set-file-shared (it rode the file tree, §1).

So invariant 1 holds provided (a) the library is resurrected before/with the
consumer, and (b) `file_library_rel` is re-derived from the lockfile via
`link-file-to-library`. Both are cheap and deterministic.

## 7. `include-libraries` is the WRONG model (CONFIRMED)

Same consumer, `includeLibraries=true`, captured:
- `manifest.files` = **2 files** — `[consumerId, libId]` (the whole library file
  is copied in).
- `manifest.relations` = `[[consumerId, libId]]`.
- On disk: a second complete `files/<libId>/…` subtree appears inside the
  consumer archive.

So `include-libraries` **inlines/embeds the library** into the consumer export:
the library's components/colors/typographies are duplicated, and the archive is
no longer one-file-per-tree. Contrast with linking, where the consumer keeps a
bare `componentFile=<libId>` id and the library lives once, in its own vault
tree. E3 references by id; it must never persist the include-libraries multi-file
archive as-is (we only borrow its *reference-preserving* behavior, then trim the
inlined library away — §5). Confirmed unused as a storage model.

## 8. `unlink-file-from-library` (WORKS)

Request (captured):
```json
{"fileId": "<consumerId>", "libraryId": "<libId>"}
```
Response: `null`. `get-file-libraries` afterwards returns `[]`. Clean removal of
the relation (a single `db/delete!` on `file_library_rel`).

---

## Verdict & concrete build guidance

**GO — with the §5 export-flag caveat, which is fully solved.** All three RPCs
behave; the reference is by the library's vault-local file-id at
`objects.<id>.componentFile`; invariant 1 holds with the trimmed-include on-disk
representation; `file_library_rel` is disposable and lockfile-re-derivable;
`include-libraries` is confirmed as the wrong (inlining) storage model.

**Blockers:** none absolute. **One mandatory change:** the sync/export path for
linked consumer files must stop using bare `embedAssets=true` (which self-embeds
and severs the link) and use the trimmed-`includeLibraries` representation.

Concrete guidance for the build:

- **`sync-daemon` / export path (engine.rs ~1145):** for a file that has ≥1
  `file_library_rel` (or a lockfile link entry), export with
  `include_libraries=true` and **trim the archive to the file's own subtree**
  before writing to disk (`manifest.files` → the one entry, `manifest.relations`
  → `[]`, drop other `files/<id>/…` subtrees). Files with no links keep the
  current `embedAssets=true` path unchanged. Reuse the probe's
  `trim_to_single_file` as the reference implementation. This is the whole
  caveat.

- **`lock.rs`:** add a per-consumer `libraries` list of link entries
  `{consumerFileId → libraryFileId, version, contractHash}` (E1 contract hash of
  the library = union of its components' contracts + exported color/typography
  names, per PLAN3 E3). `file_library_rel` and the library's `isShared` are the
  DB-only/derived state; the lock entry is their on-disk source of truth. On
  delete-DB rebuild, after resurrecting files by id, re-apply each link with one
  idempotent `link-file-to-library {fileId, libraryId}` call (and defensively
  re-run `set-file-shared {id:libId, isShared:true}` even though isShared rides
  the tree — cheap insurance).

- **`packages.rs` (installer):** publishing a library package = resurrect/import
  its tree (existing E2 path) **then** `set-file-shared`. Installing a consumer
  that depends on a library package = ensure the library is installed/live, then
  `link-file-to-library`, then materialize the consumer tree carrying
  `componentFile=<libId>`. Record both in the lockfile. Resolution is by
  vault-local id, so this stays entirely under the cross-vault id wall (no remap
  machinery — that's E6).

- **`contract.rs`:** the library contract is already E1's component + token/asset
  contract; E3 adds nothing to extraction. Surface consumer-visible changes as
  **contract diffs (E1)**, never `revn` — confirmed sound because `revn` resets
  to the zip value on in-place import (M0) and is advisory. A patch edit to the
  library (impl only) yields no contract bump; minor/major follow the E1 matrix.

- **Instance authoring:** the wire format for placing an instance is a single
  `update-file` `add-obj` change whose `obj` is a normal shape plus
  `{componentId, componentFile:<libId>, componentRoot:true, shapeRef:<libMainId>}`;
  pass `skipValidate:true`. (For a real editor-authored instance the frontend
  emits the full copied-subtree; for package materialization the id-preserving
  import of the authored tree is enough.)

- **Gate shape (E3 exit / `scripts/e3-library.sh`):** the probe here is the seed.
  Assert: publish flips `isShared`; link returns and `get-file-libraries` shows
  the library; instance `componentFile == libId`; delete-DB + reboot →
  `instanceStillResolves` with re-derived `file_library_rel`; `include-libraries`
  unused as storage (single-file trees on disk); unlink clears the relation.

### Spike artifacts (this run)
- `scripts/ecosystem-spike/e3_probe.py` — the live probe (`author` / `rebuild`).
- `scripts/ecosystem-spike/e3_boot.sh` — the dedicated-port boot harness.
- Captured JSON: `findings-author.json` / `findings-rebuild.json` in the run's
  work dir (kept on exit; both runs green).

## 9. Flag-combo follow-up (embed + include) — decides the daemon change shape

The §5 caveat needs the daemon to preserve `componentFile=<libId>` on disk for a
linked consumer, WITHOUT losing that file's own embedded media. `export_binfile`
takes two independent booleans, so this probe
(`scripts/ecosystem-spike/e3_flagcombo_probe.py`, dedicated E3fc ports) authored a
linked consumer that ALSO carries its own uploaded raster (image fill) and captured
four on-disk exports. Every value below is a real captured byte from the run
(`EXIT=0`).

- **Q0 — is `(include=true, embed=true)` even allowed?** **NO — server-rejected**
  (`accepted: false`). The two flags cannot be combined, so there is no single
  export that both preserves the library link and embeds the file's own assets.
- **Q1 — which achievable mode keeps the link?** `(include=true, embed=false)+trim`
  → on-disk `componentFile == <libId>` (**GOOD**). `(include=false, embed=true)`
  (today's daemon flag) → `componentFile == <consumerId>` (self-ref, link severed).
- **Q2 — does the link-preserving mode keep the consumer's OWN media?** **NO.**
  `(include=true, embed=false)` detaches storage objects, so the consumer's own
  raster blob is absent on disk. Tradeoff, captured: **no single export preserves
  BOTH link and own media.**
- **Q3 — could the daemon use one uniform flag for all files?** **NO — NOT-EQUAL.**
  For an unlinked file with media, `(include=true, embed=false)+trim` ≠
  `(include=false, embed=true)` (media stripped). A uniform switch would regress
  media survival (N2 thumbnails, N5 media) for every plain file.
- **Q4 — is trim a no-op for the unlinked include-export?** **NO** (the modes
  already differ by media, so trim can't reconcile them).

**Recommendation (captured): SURGICAL.** Only files with ≥1 link get the
`(include=true, embed=false)+trim-to-own-subtree` export; every other file keeps
the current `(include=false, embed=true)` path **byte-for-byte** — so M2/M3/N5
cannot regress. Link detection is via the lockfile's link entries (E3's own source
of truth), not a per-poll `get-file-libraries` RPC.

**Known limitation (E3 scope boundary, documented — not a silent gap):** because
Q0/Q2 show no export preserves both a library link and the consumer's own embedded
media, a linked consumer that ALSO uploads its own raster media would lose that
media on disk under E3's linked-file export. E3's model is a consumer that *places
component instances* (vector, referenced by id), which this does not affect. The
general "linked consumer with its own media" case needs an upstream link-preserving
+ embed-own-media export mode (or a two-export merge) and is deferred beyond E3.

### Flag-combo artifacts
- `scripts/ecosystem-spike/e3_flagcombo_probe.py`, `e3_flagcombo_boot.sh` — the
  combo probe + its dedicated-port harness (`EXIT=0`, `recommendation: SURGICAL`).
