# M0 — Spike summary

Status of PLAN.md's M0 exit criteria, verified by real execution against
**Penpot 2.16.2** (compose stack `penpot-m0`, `m0/docker/docker-compose.yaml`;
backend `http://localhost:6060`, frontend nginx `http://localhost:9001`) on
2026-07-13. Round-trip harness: `scripts/roundtrip.py` (idempotent,
re-runnable; auth via `PENPOT_TOKEN` env or login fallback). Work artifacts
(zips + extracted trees): `m0/roundtrip-work/`.

## Exit criteria

| # | Criterion (PLAN.md M0) | Status | Evidence |
|---|---|---|---|
| 1 | Documented list of RPC endpoints | **MET** | [`rpc-endpoints.md`](rpc-endpoints.md) — 17 commands verified by execution (login, discovery, create/update-file, export/import-binfile incl. chunked upload, access tokens, delete), with request/response shapes, auth, and quirks. |
| 2 | Byte-diff report of what changes across a round-trip | **MET** | [`roundtrip-report.md`](roundtrip-report.md) — per-field diffs across two in-place cycles + one as-new cycle; exactly two volatile fields (`createdAt`/`modifiedAt` in `files/<fid>.json`); tree hashes reproduced across runs. |
| 3 | In-place import (`import-binfile` with `file-id`) round-trips with the same file ID | **MET** | [`roundtrip-report.md`](roundtrip-report.md) §cycle 1–2 — same file id returned in both consecutive cycles; all file/page/shape uuids survive; byte-stable under the normalization spec below. |
| 4 | Asset serving without nginx characterized (X-Accel, risk 6) | **MET** | [`asset-serving.md`](asset-serving.md) — 204 + `x-accel-redirect` behavior verified live on both ports and cross-checked against `app/http/assets.clj` in the 2.16.2 jar; full proxy spec written for M1. |

All four criteria are met. Honest caveats (none blocks a MET verdict):

- "Byte-stable round-trip" holds only under the final normalization spec
  (formatting **plus** stripping `createdAt`/`modifiedAt`). Pure formatting
  normalization is *not* byte-stable — the server rewrites those two
  timestamps on every import. PLAN.md anticipated exactly this ("modulo
  regenerated IDs / timestamps"), and the report pins it down to precisely
  two fields.
- The spike used a small test file (2 shapes, 1 page, no media blobs,
  `embedAssets: false`). Round-trip of embedded media/fonts inside the
  binfile was not exercised; it should be added to the harness before M2
  relies on it.
- `get-access-tokens` is documented from the OpenAPI schema but was not
  exercised (its companions `create-`/`delete-access-token` were).

## Implications for M1/M2

### Blockers

None found. Nothing in M0 invalidates the planned architecture. Specific
green lights:

- In-place import is reliable (PLAN.md risk 1 does **not** return): two
  consecutive cycles preserved the file id and every uuid inside.
- Plain JSON RPC works both directions (camelCase keys, `Accept:
  application/json`) — no transit+json client needed in `crates/penpot-rpc`.
- Access-token auth (`Authorization: Token <tok>`) works for every command
  the daemon needs, including multipart import — no cookie scraping.
- The X-Accel contract is tiny and fully specced (one header + one shared
  directory); the M1 proxy design in `asset-serving.md` is low-risk.

### Surprises (things PLAN.md didn't anticipate)

1. **`export-binfile` does not return the ZIP.** It returns an SSE stream
   (`text/event-stream`) whose `end` event carries a transit-encoded URI to
   `/assets/by-id/<id>` on the `PENPOT_PUBLIC_URI` host. The daemon must
   parse SSE, then fetch the ZIP **through the frontend proxy** (fetching
   from the backend directly yields 204 + x-accel and zero bytes). In M1
   this means the daemon depends on our own proxy (or reads the asset file
   straight off disk) even for exports. `import-binfile` responses are SSE
   too.
2. **Multipart field names are kebab-case** (`file-id`, `project-id`,
   `upload-id`, `session-id`) while JSON bodies are camelCase — mixing them
   up is a silent-until-400 footgun for the typed client.
3. **Stale `revn` is NOT rejected by `update-file`**, and **in-place import
   resets `revn` to the value inside the zip** (observed 4 → 3). revn is not
   monotonic and the server never conflict-errors; conflict detection is
   entirely the daemon's job (PLAN.md's Direction-B conflict rule must be
   enforced client-side, comparing revn/hash *before* importing — the server
   is pure last-write-wins).
4. **The `name` form field is ignored on v3 import** — the file name comes
   from `manifest.json`. Renames must go through the manifest (or a rename
   RPC), not the import call.
5. **In-place import rewrites `modifiedAt` (and `createdAt`) to import
   time**, so Direction A's poll signal (`revn` + `modifiedAt`) fires after
   the daemon's own import. The hash ledger (step "record hash before the
   swap") is what breaks the loop — `modifiedAt` alone cannot distinguish
   "user edited" from "we just imported".
6. **The exporter service is required even in M0-style stacks**: the
   frontend nginx config declares upstream `penpot-exporter` and dies
   without it, and the exporter refuses to boot without `PENPOT_SECRET_KEY`.
   PLAN.md defers the exporter to M5; the M1 proxy replaces nginx so this
   doesn't bite, but any compose-based test rig needs the service.
7. **`PENPOT_SECRET_KEY` must be pinned** or the backend regenerates it per
   restart, invalidating all sessions and access tokens. M1's supervisor
   must generate one on first boot and persist it (keychain/XDG) alongside
   the access token.
8. **Asset cache-control is in milliseconds** (`max-age=86400000`) — an
   upstream quirk; the proxy must copy it verbatim, not "fix" it.
9. **Public asset buckets need no auth**: `file-media-object`,
   `file-object-thumbnail`, `team-font-variant`, `file-data-fragment` are
   whitelisted; other buckets (e.g. `profile`) need credentials, so the
   proxy must forward `Cookie`/`Authorization` untouched.

### Decisions the normalization spec must encode (M2 hash ledger)

Full spec in [`roundtrip-report.md`](roundtrip-report.md); the binding
decisions:

1. **Formatting**: re-serialize every `.json` with sorted keys, 2-space
   indent, `ensure_ascii=False`, LF endings, single trailing newline.
2. **Strip `createdAt` and `modifiedAt`** (recursively is safe) before
   hashing — these are the *only* unstable fields; without stripping, every
   import produces a phantom change. They may stay on disk for humans as
   long as they're excluded from the tree hash.
3. **Tree hash** = sha256 over sorted `(relative path, content sha256)`
   pairs of the extracted tree. **Never hash or compare the zip container**
   (entry order/timestamps/compression are noise).
4. **Treat `revn` as advisory only** — in-place import resets it to the
   zip's value; never use it as a "newer than" signal across imports.
5. **Import-as-new fallback = full uuid rewrite** (file, pages, shapes, and
   every internal reference). The daemon must update its fileId↔path
   manifest on fallback; content is otherwise preserved verbatim (residual
   diff after id-mapping was exactly the two volatile timestamps — nothing
   else: no ordering noise, no float reformatting).

## Files

- [`rpc-endpoints.md`](rpc-endpoints.md) — verified RPC catalog + auth + quirks
- [`roundtrip-report.md`](roundtrip-report.md) — byte-diff report + normalization spec
- [`asset-serving.md`](asset-serving.md) — X-Accel characterization + M1 proxy spec
- `../../scripts/roundtrip.py` — re-runnable round-trip harness
- `../../m0/docker/docker-compose.yaml` — pinned 2.16.2 stack (exporter + secret-key fixes)
