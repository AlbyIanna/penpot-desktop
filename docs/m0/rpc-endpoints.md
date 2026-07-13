# Penpot RPC endpoints for the sync daemon (M0 spike)

Everything below was verified by real calls against **Penpot 2.16.2** running via
`m0/docker/docker-compose.yaml` (project `penpot-m0`) on 2026-07-13.

- Backend (direct): `http://localhost:6060`
- Frontend nginx proxy (baseline behavior): `http://localhost:9001`
- RPC shape: `POST /api/rpc/command/<command-name>`
- Test identities: team `e4ebd8e6-e0d6-8139-8008-51ec952e5c36` ("Default"),
  project `e4ebd8e6-e0d6-8139-8008-51ec9531fcd2` ("Drafts"),
  profile `e4ebd8e6-e0d6-8139-8008-51ec952603ac` (`m0@local.test`).

## Content types and key casing

- Penpot natively speaks transit+json, but **plain JSON works both directions**:
  send `Content-Type: application/json` and `Accept: application/json`.
  JSON request/response keys are **camelCase** (`projectId`, `sessionId`, `fileId`).
- **Multipart commands are the exception**: form field names must be
  **kebab-case** (`project-id`, `file-id`, `upload-id`, `session-id`).
  Sending `projectId` as a multipart field returns HTTP 400
  `{"type":"validation","code":"params-validation", ...}` with the full malli
  schema in `explain` (which is a handy way to discover exact param names).
- Commands that take no params still need a body: send `-d '{}'`.
- Validation errors are HTTP 400 JSON with `type`/`code`/`explain`.

## Machine-readable API doc

- `GET http://localhost:6060/api/main/doc/openapi.json` → full OpenAPI document
  (~1 MB) with per-command request schemas. HTML browser at
  `/api/main/doc` (and `/api/doc` 308-redirects there, but through the
  `PENPOT_PUBLIC_URI` host, i.e. port 9001).

## Authentication

Two methods, both verified; every authenticated command below works with either.

1. **Session cookie** — name `auth-token`, value is a JWE. Obtained from
   `login-with-password` via `Set-Cookie`. Attributes observed:
   `Path=/; HttpOnly; SameSite=Lax`, 7-day `Expires`, plus a non-standard
   `Comment=Renewal at: <date>` attribute (renewal at half-life).
2. **Personal access token** — header `Authorization: Token <token>` (literal
   word `Token`, single space, then the JWE token from `create-access-token`).
   Requires backend flag `enable-access-tokens` (on in this stack).
   Tokens survive backend restarts only because `PENPOT_SECRET_KEY` is pinned
   in the compose file.

---

## login-with-password

- **URL**: `POST http://localhost:6060/api/rpc/command/login-with-password`
- **Auth**: none (this is how you get the cookie)
- **Request**: `Content-Type: application/json`

```json
{"email": "m0@local.test", "password": "m0-spike-password"}
```

- **Response**: `200 OK`, `content-type: application/json`, plus:

```
Set-Cookie: auth-token=eyJhbGciOiJBMjU2S1ci...; Path=/; HttpOnly;
  Expires=<+7d>; Comment=Renewal at: <+6h>; SameSite=Lax
```

Body is the profile object — **already contains everything discovery needs**:

```json
{
  "id": "e4ebd8e6-e0d6-8139-8008-51ec952603ac",
  "email": "m0@local.test",
  "fullname": "M0 Spike",
  "defaultTeamId": "e4ebd8e6-e0d6-8139-8008-51ec952e5c36",
  "defaultProjectId": "e4ebd8e6-e0d6-8139-8008-51ec9531fcd2",
  "isActive": true, "isAdmin": false, "isBlocked": false, "...": "..."
}
```

- **Quirks**: also works through the frontend proxy on 9001. Wrong credentials
  return 400 with `code: wrong-credentials` (not 401).

## get-profile

- **URL**: `POST /api/rpc/command/get-profile`
- **Auth**: verified with `Authorization: Token <tok>` (cookie also works)
- **Request**: `application/json`, body `{}` (empty body without `-d {}` fails)
- **Response**: `200`, same profile shape as login (incl. `defaultTeamId`,
  `defaultProjectId`, and a `props` map with UI state).

## get-teams

- **URL**: `POST /api/rpc/command/get-teams`
- **Auth**: token header (verified)
- **Request**: `application/json`, `{}`
- **Response**: `200`, JSON array:

```json
[{"id": "e4ebd8e6-...-51ec952e5c36", "name": "Default", "isDefault": true,
  "permissions": {"type": "membership", "isOwner": true, "isAdmin": true, "canEdit": true},
  "features": ["fdata/path-data", "components/v2", "..."], "...": "..."}]
```

- **Quirks**: the team `features` array is what you pass as `features` to
  file-level commands if you ever need to opt in explicitly (not needed in
  practice — `create-file` inherits team features).

## get-projects

- **URL**: `POST /api/rpc/command/get-projects`
- **Auth**: token header (verified)
- **Request**: `application/json`, `{"teamId": "<uuid>"}`
- **Response**: `200`, array of
  `{"id", "teamId", "name", "isDefault", "isPinned", "count", "totalCount", ...}`.
  The default project is named "Drafts", `isDefault: true`.

## get-project-files

- **URL**: `POST /api/rpc/command/get-project-files`
- **Auth**: token header (verified)
- **Request**: `application/json`, `{"projectId": "<uuid>"}`
- **Response**: `200`, array of file summaries (no `data`):

```json
[{"id": "3a4be581-...-51ee126e1fb4", "name": "m0-rpc-spike",
  "projectId": "...", "teamId": "...", "revn": 3, "vern": 0,
  "isShared": false, "modifiedAt": "...", "createdAt": "..."}]
```

- **Quirks**: `revn`/`vern` here are exactly what `update-file` needs, so a
  sync daemon can poll this for change detection (`revn` + `modifiedAt`).

## get-file (bonus — needed to read content)

- **URL**: `POST /api/rpc/command/get-file`, body `{"id": "<file-uuid>"}`
- **Response**: `200`, full file including `data.pages` (ordered page-id list)
  and `data.pagesIndex.<pageId>.objects` (shape map keyed by uuid; the root
  frame is always uuid `00000000-0000-0000-0000-000000000000`).

## create-file

- **URL**: `POST /api/rpc/command/create-file`
- **Auth**: token header (verified)
- **Request**: `application/json`

```json
{"name": "m0-rpc-spike", "projectId": "e4ebd8e6-e0d6-8139-8008-51ec9531fcd2"}
```

  Optional params (from OpenAPI schema): `id` (client-chosen file uuid),
  `isShared`, `features`.

- **Response**: `200`, the complete file object: `id`, `revn: 0`, `vern: 0`,
  `version` (67 on 2.16.2), `features` (inherited from team), and full `data`
  with one page ("Page 1") already containing the root frame object.
- **Quirks**: response gives you the generated page id — keep it, `update-file`
  changes need a `pageId`.

## update-file (add a rectangle via `add-obj`)

- **URL**: `POST /api/rpc/command/update-file`
- **Auth**: token header (verified)
- **Request**: `application/json`. Required top-level params (per schema and
  verified): `id` (file uuid), `sessionId`, `revn`, `vern`, `changes`.

```json
{
  "id": "<file-uuid>",
  "sessionId": "<any client-generated uuid v4>",
  "revn": 0,
  "vern": 0,
  "changes": [
    {
      "type": "add-obj",
      "id": "<shape-uuid>",
      "pageId": "<page-uuid>",
      "frameId": "00000000-0000-0000-0000-000000000000",
      "parentId": "00000000-0000-0000-0000-000000000000",
      "obj": {
        "id": "<shape-uuid, same as above>",
        "type": "rect",
        "name": "M0 Rectangle",
        "x": 100, "y": 100, "width": 200, "height": 150,
        "rotation": 0,
        "selrect": {"x": 100, "y": 100, "width": 200, "height": 150,
                     "x1": 100, "y1": 100, "x2": 300, "y2": 250},
        "points": [{"x": 100, "y": 100}, {"x": 300, "y": 100},
                    {"x": 300, "y": 250}, {"x": 100, "y": 250}],
        "transform": {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0},
        "transformInverse": {"a": 1.0, "b": 0.0, "c": 0.0, "d": 1.0, "e": 0.0, "f": 0.0},
        "parentId": "00000000-0000-0000-0000-000000000000",
        "frameId": "00000000-0000-0000-0000-000000000000",
        "fills": [{"fillColor": "#B1B2B5", "fillOpacity": 1}],
        "strokes": []
      }
    }
  ]
}
```

  `AddObjChange` schema: required `type`, `id`, `obj`, `frameId`; optional
  `pageId`, `componentId`, `parentId`, `index`, `ignoreTouched`. For a shape on
  a page you must send `pageId`. `selrect`/`points`/`transform`/
  `transformInverse` must be supplied by the client — the server does not
  derive them from x/y/width/height.

- **Response**: `200`

```json
{"revn": <file revn BEFORE this update>,
 "lagged": [{"id": "<change-entry-uuid>", "revn": 1, "fileId": "...",
              "sessionId": "...", "changes": [ ...the stored changes... ]}]}
```

### session-id / revn semantics (verified by experiment)

- `sessionId` is just a client-generated uuid identifying the editing session;
  it is echoed back in change entries and used to attribute concurrent edits.
  Any fresh uuid v4 works.
- `revn` is the file revision the client believes it has. Each accepted
  `update-file` increments the file's `revn` by 1 (verified 0→1→2→3).
- `lagged` in the response = all stored change entries with `revn` greater
  than the `revn` you sent (this includes your own change, which lands at
  `old revn + 1`). A client that is up to date sees exactly one lagged entry —
  its own.
- **A stale `revn` is NOT rejected.** Sending `revn: 0` against a file at
  revn 2 still applied the change (file went to revn 3) and returned all
  changes since revn 0 in `lagged`. Concurrency control is optimistic/advisory:
  the sync daemon must inspect `lagged` itself to detect concurrent edits;
  the server will not conflict-error on its own.
- `vern` is a separate "version epoch" counter (bumped by snapshot restores,
  not by normal updates); pass the current value from `get-file`/
  `get-project-files` (it stayed 0 throughout).

## export-binfile

- **URL**: `POST /api/rpc/command/export-binfile`
- **Auth**: token header (verified)
- **Request**: `application/json` — all three params required:

```json
{"fileId": "<file-uuid>", "includeLibraries": false, "embedAssets": false}
```

- **Response**: `200` — but **NOT the ZIP itself**. Content-Type is
  `text/event-stream;charset=UTF-8` (SSE). The stream emits `progress` events
  (whose `data` payloads are transit-encoded even when you send
  `Accept: application/json`) and finishes with an `end` event whose data is a
  transit URI pointing at the produced artifact:

```
event: progress
data: {"~:section":"~:file","~:id":"~u<file-uuid>","~:name":"m0-rpc-spike"}

event: end
data: {"~#uri":"http://localhost:9001/assets/by-id/<asset-uuid>"}
```

- **Fetching the ZIP**: GET that URI **with auth** (both the `auth-token`
  cookie and the `Authorization: Token` header work). It returns
  `200`, `content-type: application/zip`. Quirk: the URI host is
  `PENPOT_PUBLIC_URI` (port 9001, the frontend). Fetching the same path from
  the backend directly (6060) returns `204 No Content` with an
  `x-accel-redirect: /internal/assets/...` header — asset downloads are
  designed to be fulfilled by the frontend nginx, so **go through 9001**.
- **How v3 manifests**: there is no `version` param on export in 2.16.2; you
  always get binfile-v3, which is a ZIP ("legacy zip" v1 export exists only in
  older versions/UI paths). Inspecting the ZIP:
  - `manifest.json` — `{"type": "penpot/export-files", "version": 1,
    "generatedBy": "penpot/2.16.2", "files": [{id, name, features}], "relations": []}`
    (that `"version": 1` is the manifest schema version, not the binfile
    version).
  - `files/<file-id>.json` — file metadata **including `revn`, `vern`,
    `version` (data-model version, 67) and applied `migrations`**.
  - `files/<file-id>/pages/<page-id>.json` and
    `files/<file-id>/pages/<page-id>/<shape-id>.json` — one JSON per shape.
- The whole SSE dance is required; there is no direct binary response mode.

## import-binfile — as a NEW file

- **URL**: `POST /api/rpc/command/import-binfile`
- **Auth**: token header (verified)
- **Request**: `multipart/form-data` with **kebab-case** field names:

| field | encoding | value |
|---|---|---|
| `name` | plain text part | display name (see quirk below) |
| `project-id` | plain text part | destination project uuid |
| `file` | file part (`filename=` + `Content-Type: application/zip`) | the `.penpot` ZIP bytes |

```
curl -H "Authorization: Token $TOK" \
  -F 'name=m0-imported-copy' \
  -F 'project-id=e4ebd8e6-e0d6-8139-8008-51ec9531fcd2' \
  -F 'file=@export.penpot;type=application/zip' \
  http://localhost:6060/api/rpc/command/import-binfile
```

- **Response**: `200`, again `text/event-stream`. `progress` events for
  sections `manifest`, `storage-objects`, `file`, `page`, `relations`; the
  `end` event data is a transit array of the created file id(s):

```
event: end
data: ["~u3a4be581-6d37-8010-8008-51eecd7dc111"]
```

- **Quirks**:
  - A **new** file id is minted (the ids inside the binfile are remapped).
  - The `name` form field was **ignored** for a v3 binfile — the imported file
    kept the name stored in `manifest.json` ("m0-rpc-spike"), not
    "m0-imported-copy". `name` is still schema-required, so send it anyway.
  - camelCase field names (`projectId`) fail with 400 params-validation.
  - The imported file preserved the source file's `revn` (3).

## import-binfile — IN-PLACE (existing file-id)

Same endpoint/encoding as above, plus a `file-id` form field:

```
curl -H "Authorization: Token $TOK" \
  -F 'name=m0-rpc-spike' \
  -F 'project-id=e4ebd8e6-e0d6-8139-8008-51ec9531fcd2' \
  -F 'file-id=3a4be581-6d37-8010-8008-51ee126e1fb4' \
  -F 'file=@export.penpot;type=application/zip' \
  http://localhost:6060/api/rpc/command/import-binfile
```

- **Response**: `200` SSE; `end` data is the **same file id you passed**:
  `["~u3a4be581-6d37-8010-8008-51ee126e1fb4"]` — verified the id survives.
- **revn behavior (verified)**: in-place import **replaces the file content
  wholesale and sets `revn` to the value stored inside the binfile** — it does
  not increment. Experiment: file at revn 3 was exported; file then edited to
  revn 4 (extra rect); in-place import of the older ZIP rolled content back
  (extra rect gone) and `revn` went **4 → 3**. So `revn` can move backwards;
  a sync daemon must not assume revn is monotonic across in-place imports,
  and should treat in-place import as "last write wins".
- Per the endpoint's own doc string: in-place is only supported for binfile-v3
  and only when the `.penpot` archive contains exactly one file.

### Chunked upload variant (`upload-id`)

For archives larger than the multipart size limit. Verified end-to-end:

1. `POST /api/rpc/command/create-upload-session`, JSON `{"totalChunks": 2}`
   → `200` `{"sessionId": "<uuid>"}`.
2. For each chunk: `POST /api/rpc/command/upload-chunk`, multipart with fields
   `session-id` (text), `index` (text, 0-based), `content` (file part with the
   raw chunk bytes) → `200` `{"sessionId": "...", "index": 0}`.
3. `POST /api/rpc/command/import-binfile`, multipart with `name`,
   `project-id`, and `upload-id=<sessionId>` **instead of** the `file` part
   (add `file-id` for in-place) → same SSE response as the direct variant.

Chunks are concatenated in index order; arbitrary split points work (tested
with a byte-level split into 2 chunks).

## create-access-token

- **URL**: `POST /api/rpc/command/create-access-token`
- **Auth**: works with the session cookie **and** (verified) with an existing
  access token — tokens can mint more tokens.
- **Request**: `application/json`

```json
{"name": "m0-spike"}
```

  Optional `expiration` as a duration string (verified `"3600s"`; omit for a
  non-expiring token). Schema also lists an optional `type` string.

- **Response**: `200`

```json
{"id": "<token-uuid>", "profileId": "<profile-uuid>", "name": "m0-spike",
 "token": "eyJhbGciOiJBMjU2S1ci...", "createdAt": "...", "updatedAt": "..."}
```

- **Quirks**: the `token` value is only returned at creation time. Companion
  commands verified in passing: `delete-access-token` with `{"id": "<uuid>"}`
  → `204`. (`get-access-tokens` exists per OpenAPI doc but was not exercised.)
  `delete-file` with `{"id": "<uuid>"}` → `204` was also verified during
  cleanup.

---

## State left behind for later phases

- File `3a4be581-6d37-8010-8008-51ee126e1fb4` ("m0-rpc-spike", revn 3) in the
  Drafts project, containing page `3a4be581-6d37-8010-8008-51ee126e1fb5` with
  three rectangles (`M0 Rectangle`, `Rect 2`, `Rect stale`).
- The pre-existing `m0-asset-spike` file from the setup phase is untouched.
- Access token named `m0-spike` remains valid (secret key is pinned).
