# M0 round-trip byte-diff report

Generated 2026-07-13 09:04 UTC by `scripts/roundtrip.py` against Penpot 2.16.2
(`penpot-m0` compose stack, backend `http://localhost:6060`, assets via `http://localhost:9001`).
All numbers below are from a real run; work dir with the actual zips/trees:
`/Users/albertoiannaccone/Workspace/penpot-desktop/m0/roundtrip-work`.

## TL;DR

- **In-place import keeps the file id** (verified in two consecutive cycles).
- **Formatting-only normalization is NOT byte-stable**: the server rewrites
  exactly two fields on every import — `createdAt` and `modifiedAt` in
  `files/<fid>.json` (both set to the import wall-clock time). Nothing else
  changes: no id churn, no ordering noise, no float reformatting.
- **Formatting normalization + stripping `createdAt`/`modifiedAt` IS
  byte-stable: True** across consecutive in-place cycles.
- Import-as-new remaps every uuid (file/page/shape) but is otherwise a pure
  id-rewrite plus the same two timestamps.

## Setup

- Test file: `m0-roundtrip-spike` id `3a4be581-6d37-8010-8008-51f0c6eb307f` in project `e4ebd8e6-e0d6-8139-8008-51ec9531fcd2`,
  revn 1 / vern 0 at start
  (reused from a previous run).
- Export produces a binfile-v3 zip: `manifest.json`
  (generatedBy `penpot/2.16.2`) + `files/<fid>.json` +
  `files/<fid>/pages/<pid>.json` + one JSON per shape (the root frame is shape
  `00000000-0000-0000-0000-000000000000`).
- Zip entries in export A (6 files):

```
files/3a4be581-6d37-8010-8008-51f0c6eb307f.json
files/3a4be581-6d37-8010-8008-51f0c6eb307f/pages/3a4be581-6d37-8010-8008-51f0c6eb3080.json
files/3a4be581-6d37-8010-8008-51f0c6eb307f/pages/3a4be581-6d37-8010-8008-51f0c6eb3080/00000000-0000-0000-0000-000000000000.json
files/3a4be581-6d37-8010-8008-51f0c6eb307f/pages/3a4be581-6d37-8010-8008-51f0c6eb3080/9b2d22e2-c7fa-4ef6-b74a-d48ee3e6162e.json
files/3a4be581-6d37-8010-8008-51f0c6eb307f/pages/3a4be581-6d37-8010-8008-51f0c6eb3080/eb22daf7-5b9f-40b5-98f3-5cb0b7c70e5a.json
manifest.json
```

## Normalization tiers tested

1. **Formatting tier**: every `.json` rewritten as
   `json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False)` + trailing
   `\n`, LF endings; non-JSON files untouched.
2. **Volatile-strip tier**: tier 1 plus recursively dropping keys
   `createdAt` and `modifiedAt` from every `.json`.

Tree hash = sha256 over sorted (relative path, sha256(content)) pairs.
The zip container itself is never compared (entry order/timestamps/compression
are irrelevant; only the extracted tree matters).

## In-place round-trip (cycle 1): export A -> normalize -> re-zip -> import in-place -> export B

- import-binfile with `file-id=3a4be581-6d37-8010-8008-51f0c6eb307f` returned id `3a4be581-6d37-8010-8008-51f0c6eb307f` —
  **same file id: True**.
- revn/vern before: 1/0; after in-place import:
  1/0 (revn is reset to the value inside the zip,
  not incremented).
- Formatting-tier tree hash A: `09cd02ac3e06b357643e6b553212326f7d05a14b3be2c1ddf25831828cd57e7c`
- Formatting-tier tree hash B: `823336e07726cb0391a66300028e9bb90362837933fbf27d158b1b6e1bce72ce`
- **Byte-identical at formatting tier: False**

Per-file diff A vs B (formatting tier):

  - `files/3a4be581-6d37-8010-8008-51f0c6eb307f.json` — 2 changed path(s):
    - `$.createdAt` (value-changed): `"2026-07-13T09:01:16.409256Z"` -> `"2026-07-13T09:04:42.658798Z"`
    - `$.modifiedAt` (value-changed): `"2026-07-13T09:01:16.409256Z"` -> `"2026-07-13T09:04:42.658798Z"`

With `createdAt`/`modifiedAt` stripped (volatile-strip tier):

  - (no differences)

- Volatile-strip hash A: `b2124a9b263292b7416d44db6f3c0a11328968917dc29987c1c386a9503d31b0`
- Volatile-strip hash B: `b2124a9b263292b7416d44db6f3c0a11328968917dc29987c1c386a9503d31b0`
- **Byte-identical at volatile-strip tier: True**

## Stability (cycle 2): import B in-place -> export C

- Same file id again: True.
- Formatting-tier hash C: `c44ca5d411fe93b243a5e1cb57a4b58298697209497f6993ccc72971d18a7d2c` — equals B: False
- Volatile-strip hash C: `b2124a9b263292b7416d44db6f3c0a11328968917dc29987c1c386a9503d31b0` — **equals B: True**

Per-file diff B vs C (formatting tier):

  - `files/3a4be581-6d37-8010-8008-51f0c6eb307f.json` — 2 changed path(s):
    - `$.createdAt` (value-changed): `"2026-07-13T09:04:42.658798Z"` -> `"2026-07-13T09:04:42.685385Z"`
    - `$.modifiedAt` (value-changed): `"2026-07-13T09:04:42.658798Z"` -> `"2026-07-13T09:04:42.685385Z"`

Per-file diff B vs C (volatile-strip tier):

  - (no differences)

## Import-as-NEW variant (diff vs A)

- New file id minted: `3a4be581-6d37-8010-8008-51f246b50ca8` (original `3a4be581-6d37-8010-8008-51f0c6eb307f`).
- Raw path diff before id-mapping: 5 path(s) only in
  A, 5 only in the as-new export — every path
  containing a uuid changes, because **ALL ids are remapped**: the file id,
  every page id, every shape id, and every reference to them inside the JSON
  (`id`, `parentId`, `frameId`, `pages` lists, manifest `files[].id`,
  directory/file names).
- After mapping new ids back to old ids (file id via manifest order, page ids
  via path order, shape ids via shape `name`),
  the residual diff is:

  - `files/3a4be581-6d37-8010-8008-51f0c6eb307f.json` — 2 changed path(s):
    - `$.createdAt` (value-changed): `"2026-07-13T09:01:16.409256Z"` -> `"2026-07-13T09:04:42.707487Z"`
    - `$.modifiedAt` (value-changed): `"2026-07-13T09:01:16.409256Z"` -> `"2026-07-13T09:04:42.707487Z"`

After also stripping volatile keys:

  - (no differences)

## Exactly which fields change across an in-place round-trip

- `files/3a4be581-6d37-8010-8008-51f0c6eb307f.json:$.createdAt (value-changed)`
- `files/3a4be581-6d37-8010-8008-51f0c6eb307f.json:$.modifiedAt (value-changed)`

- IDs: **unchanged** (file, page, shape uuids all survive in-place import).
- Ordering: **unchanged** (no array reordering or key-order noise observed
  after sort_keys normalization).
- revn/vern/version/migrations inside `files/<fid>.json`: **unchanged**
  (revn round-trips as the value stored in the zip).
- Timestamps: `createdAt` and `modifiedAt` in `files/<fid>.json` are both
  rewritten to the import time (observed: they become equal to each other
  after an in-place import). These are the ONLY unstable fields.

## Does normalization suffice?

- Pure formatting normalization (sorted keys / indent / LF / trailing
  newline): **no** — `createdAt`/`modifiedAt` still differ every cycle and
  would make the hash ledger see a phantom change after every import.
- Formatting + strip `createdAt` + `modifiedAt`: **yes — byte-stable across consecutive cycles**.


## Normalization spec for the sync daemon (driven by the data above)

1. Parse and re-serialize every `.json`: **sorted object keys, 2-space
   indent, non-ASCII preserved (`ensure_ascii=False`), LF line endings,
   single trailing newline**.
2. **Strip `createdAt` and `modifiedAt`** (at minimum from
   `files/<fid>.json`; stripping them recursively everywhere is safe and
   future-proof) before hashing for the ledger. Alternative: keep them on
   disk for humans but exclude them from the tree hash.
3. Hash the tree as sha256 over sorted (relative path, content sha256)
   pairs; never hash or compare the zip container itself.
4. Treat `revn` inside the export as advisory only: in-place import resets
   the DB revn to the zip's value, so revn is not monotonic and must not be
   used as a "newer than" signal across imports.
5. If the daemon ever falls back to import-as-new, it must expect a full
   uuid rewrite (file, pages, shapes) and update its fileId<->path manifest;
   content is otherwise preserved verbatim (confirmed: residual diff after id-mapping was only the volatile timestamps).

## Run log

```
logged in as m0@local.test, project e4ebd8e6-e0d6-8139-8008-51ec9531fcd2
reusing test file 'm0-roundtrip-spike' id=3a4be581-6d37-8010-8008-51f0c6eb307f
file revn=1 vern=0
export A: 3952 bytes zip, 6 entries, normalized tree hash 09cd02ac3e06b357643e6b553212326f7d05a14b3be2c1ddf25831828cd57e7c
in-place import #1: returned file id 3a4be581-6d37-8010-8008-51f0c6eb307f (same as original: True)
after in-place import #1: revn=1 vern=0
export B: normalized tree hash 823336e07726cb0391a66300028e9bb90362837933fbf27d158b1b6e1bce72ce (equal to A: False)
A vs B: 5 identical files, 1 changed, 0/0 only-in-one
A vs B with volatile keys (createdAt, modifiedAt) stripped: identical = True
in-place cycle #2: same id True, tree hash c44ca5d411fe93b243a5e1cb57a4b58298697209497f6993ccc72971d18a7d2c, stable vs cycle #1: formatting-only False, volatile-stripped True
import-as-new: created file id 3a4be581-6d37-8010-8008-51f246b50ca8 (differs from original: True)
as-new vs A (raw): 5 new paths; after id-mapping: 1 changed files, 5 identical; after also stripping volatile keys: 0 changed
deleted as-new file 3a4be581-6d37-8010-8008-51f246b50ca8 (cleanup)
```
