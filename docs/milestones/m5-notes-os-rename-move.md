# M5 component notes: OS-side create/rename/move → Penpot

Integrator notes for the sync-daemon/penpot-rpc component of M5 ("file/project
create-rename-move from the OS side reflected in Penpot"). All claims below
were verified live against Penpot 2.16.2 on a dev stack (ports
8912/6387/5461/6404) on 2026-07-13.

## What landed

### penpot-rpc (3 new commands, shapes from `/api/main/doc/openapi.json` + curl probes)

- `rename_file(id, name)` — `rename-file`, `{id, name}` → **200** with a
  "SimplifiedFile" body (`id`, `name`, `createdAt`, `modifiedAt`).
  **Bumps the file's `modifiedAt`** (a rename IS a poll-surface change),
  does not touch `revn`.
- `move_files(&[id], project_id)` — `move-files`, `{ids: [...], projectId}`
  (camelCase JSON, ids is an array) → **204**. Does **NOT** bump the moved
  files' `modifiedAt`.
- `rename_project(id, name)` — `rename-project`, `{id, name}` → **204**.

Wire tests in `crates/penpot-rpc/tests/wire.rs`, live tests (`#[ignore]`) in
`crates/penpot-rpc/tests/live.rs` (`live_rename_file_move_files_and_rename_project`).

### sync-daemon: "path is identity" relaxed

New pure planner in `crates/sync-daemon/src/plan.rs` (`plan_rekeys`,
exhaustively unit-tested): manifest entries whose dir vanished are paired
with unclaimed on-disk dirs by **semantic tree hash == the entry's
`lastSyncedHash`**, with the pairing required to be **unique on both sides**.
Classification:

- same folder, new stem → `rename-file` (same file id, **no reimport**);
- different folder → `move-files` to the target project (resolved via the
  manifest's folder↔project mapping, else find-or-create by name — a brand
  new folder gets a brand new project), plus `rename-file` if the stem also
  changed;
- a whole project folder rename (same projectId for every pair, identical
  sub-paths, old folder gone, no survivors under it, every vanished sibling
  paired, new folder not owned by another project) → **one** `rename-project`
  + re-key of all entries. Project identity comes from the manifest's
  projectId mapping, exactly as specced.

Execution (`engine.rs::rekey_pass`) re-keys the manifest path under the SAME
fileId and mirrors to the DB best-effort: an RPC failure (e.g. the file is
gone from a wiped DB) is logged loudly but never blocks the re-key —
identity preservation is the invariant, the DB is a disposable cache, and
the resurrect/import paths fix the DB later under the same id.

Trigger points:

1. watcher vanish for a manifest-known dir (before the M3 "deleted, DB kept"
   log fires);
2. watcher appear of an unknown dir (before import-as-new);
3. a **structural sweep**: macOS FSEvents fires rename events only for a
   renamed *project folder* itself, never for its children, so
   `watcher::is_structural_event` paths (non-`.penpot`, non-dot, inside the
   root) arm a debounced sweep that runs the re-key pass and then routes
   leftovers into the normal per-dir handling (unclaimed dirs → import-as-new,
   e.g. a whole new project folder moved into the root);
4. startup reconciliation, BEFORE the decision join and BEFORE the DB
   snapshot (so any project the pass creates/renames is visible to the
   decisions) — a dir moved across a shutdown is re-keyed instead of
   "orphan re-export at the old path + import-as-new at the new one".

Safe degradation (all verified by unit tests, several live): hash mismatch
(edited-and-moved), ambiguity on either side (two identical vanished entries
or two identical unclaimed dirs), or an unpaired event → the M3 behavior is
unchanged (vanish = loud log + DB kept; appear = import-as-new). A COPY of an
existing dir never re-keys (the original still claims its path) — it imports
as new. Never data loss, never DB deletion.

### Why a rename deliberately leaves `(revn, dbModifiedAt)` stale in the manifest

`rename-file` bumps `modifiedAt`, and the exported binfile **embeds the file
name** in `files/<id>.json`. The re-key intentionally does NOT fast-forward
the manifest's advisory pair, so the next poll sees "DB moved, disk clean" →
runs the export pipeline → the on-disk JSON gets the new name and the ledger
a fresh hash. Suppressing that would leave the old name on disk, and a later
disk-side in-place import would silently revert the rename in the DB.

## Live proofs (`crates/sync-daemon/tests/live_m5.rs`, `#[ignore]`, all green)

Observed latencies (2 s fs-debounce dominates every live number):

| scenario | latency | proof of no-reimport |
|---|---|---|
| `mv` rename while running | 2.27 s to `rename-file` visible | same id, `revn` untouched, exactly 1 file in the project, `(revn, modifiedAt)` frozen over an idle window after the name-refresh export, no conflict copy |
| `mv` across project folders | 2.30 s to `move-files` visible | same id, `modifiedAt` byte-identical (move does not bump it), manifest re-keyed to the new project |
| `mv` into a brand-new folder | 2.23 s | project created on the fly, same file id |
| `mv` a whole project folder | 2.25 s to `rename-project` visible | same project id, no duplicate project, all entries re-keyed, no churn |
| copy into a new folder (unknown content) | 2.26 s | import-as-new + create-project (the degradation arm), original untouched |
| rename while STOPPED → boot | re-keyed 0.21 s after spawn; DB rename visible right after | 1 file (no duplicate), same id, old path NOT re-exported as an orphan, content intact, no conflict copy |

`live_m3` (two-way sync + conflicts + startup conflict arm) re-run against
the same modified engine: green, latencies unchanged (Direction B 2.24 s,
conflict-on-resume 0.32 s).

## THE ASYMMETRY the integrator must document (Direction A renames)

**Penpot-side renames remain cosmetic. This is one-way by design (for now).**

- OS → Penpot: renaming/moving a `.penpot` dir renames/moves the Penpot
  file/project (this milestone).
- Penpot → OS: renaming a file or project in the Penpot UI does **NOT**
  rename the directory on disk. `paths::allocate_file_path` keeps the
  manifest path forever ("path is identity" still holds in that direction);
  the only disk effect is the name field inside the exported JSON. The next
  OS-side rename of that dir will again win and rename the Penpot file back
  to the dir stem... only if the user renames the dir — otherwise the DB
  name and the dir stem simply stay different, which is harmless and stable
  (no churn: the hash ledger and the advisory pair both converge).

Full DB→FS rename sync (moving directories on disk when the user renames in
Penpot) is future work and was deliberately NOT half-implemented: moving a
dir underneath the watcher requires the same vanish/appear pairing machinery
in reverse plus protection against user edits racing the move, and a botched
half-version risks exactly the duplicate-import it is meant to prevent. The
building blocks (re-key + `plan_rekeys`) are now in place if a later
milestone wants it.

## Smaller facts worth keeping

- `move-files` not bumping `modifiedAt` is what makes cross-project moves
  bounce-free: nothing to export afterwards. If a future Penpot version
  starts bumping it, the follow-up export is a harmless no-op-safe swap.
- The structural sweep also fires for stray files created in project folders
  (e.g. `readme.txt`) — it early-returns when no manifest entry is missing,
  so this is noise-free in practice.
- `walk_penpot_dirs` skips conflict copies, so a conflict copy can never be
  a re-key candidate on either side.
- Sweep-armed leftovers reuse the normal per-dir handler, so a project
  folder moved INTO the root imports all its files even though FSEvents
  never fired for the individual `.penpot` dirs.

## Regression status at hand-off (this component's runs)

- `cargo test --workspace`: 281 passed / 0 failed / 11 ignored (includes the
  other M5 components' in-flight code in the shared tree).
- `scripts/m2-invariant.sh`, `scripts/m3-sync.sh`: see the final component
  report (run after this note was written).
