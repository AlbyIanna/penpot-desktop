# Penpot Local

A local-first desktop app wrapping Penpot's open-source stack. Read [PLAN.md](PLAN.md) first;
milestone status and architecture decisions live there. M0 evidence is in `docs/m0/`. The
project's durable values are in [docs/ecosystem-concept.md](docs/ecosystem-concept.md) — the
"why"; specific designs may change, those principles don't.

## Core invariant (P0 if it ever fails)

> Delete the entire database, restart the app, and every project/file is rebuilt from the
> folder tree with no data loss. The user's folder tree is the source of truth; the Penpot
> database is a disposable cache.

## Rules that must never be violated

- **Conflict rule:** never silently overwrite either side. If both the DB and the filesystem
  changed since `lastSyncedHash`, export the DB version as a `.conflict-<timestamp>.penpot/`
  copy next to the file and surface it. (Server-side conflict detection does not exist:
  `revn` is advisory — in-place import resets it, stale `revn` on `update-file` is accepted.)
- **Normalization spec** (verified in M0, see `docs/m0/roundtrip-report.md`): every `.json` in
  an unzipped binfile gets sorted keys, 2-space indent, `ensure_ascii=False`, LF endings,
  trailing newline; strip `createdAt`/`modifiedAt` before hashing; tree hash = sha256 over
  sorted `(relpath, content-sha256)` pairs; never compare zip containers, only extracted trees.
- **FS→DB sync uses in-place import** (`import-binfile` with `file-id`, kebab-case multipart
  fields) — it preserves all UUIDs. Import-as-new is the fallback only.

## Gotchas (all verified against Penpot 2.16.2 — see docs/m0/rpc-endpoints.md)

- JSON RPC bodies/responses are camelCase; multipart import fields are kebab-case.
- `export-binfile` returns an SSE stream; the final event's URI downloads via the `/assets/`
  path, which requires X-Accel handling (`docs/m0/asset-serving.md` has the full proxy spec).
- Pin `PENPOT_SECRET_KEY` or every backend restart invalidates all sessions/access tokens.
- The frontend nginx refuses to boot if the `penpot-exporter` upstream hostname doesn't resolve.

## Verification

Before claiming sync/round-trip work done: start the spike stack
(`docker compose -f m0/docker/docker-compose.yaml up -d`) and run `python3 scripts/roundtrip.py`
— it is idempotent and must report identical semantic hashes A=B=C. This script is the seed of
the e2e harness (grows into `just e2e` per PLAN.md).

The milestone suites are that harness grown up — they must stay green before claiming any
milestone-touching work done (each has a dedicated port set; see the script headers):
`cargo test --workspace`, `scripts/m1-smoke.sh` (`just smoke`), `scripts/m2-invariant.sh`
(`just invariant` — THE core invariant), `scripts/m3-sync.sh` (`just m3`),
`scripts/m4-artifact-test.sh` (rebuild `scripts/build-dmg.sh` first if dist/ is stale),
`scripts/m5-features.sh` (`just m5` — needs the dev-mode exporter: fetch-penpot.sh
--with-browsers + host node).
