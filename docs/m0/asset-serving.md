# M0 — Asset serving without nginx (PLAN.md risk 6)

Characterization of how Penpot 2.16.2 serves binary assets (images, fonts, thumbnails) when
`PENPOT_OBJECTS_STORAGE_BACKEND=fs`, and what the bundled proxy in the Tauri core must
implement to replace the upstream nginx frontend.

All findings below were verified by execution against the running `penpot-m0` compose stack
(backend `penpotapp/backend:2.16.2` on host port 6060, frontend nginx on 9001) on 2026-07-13,
and cross-checked against the actual handler source extracted from `penpot.jar`
(`app/http/assets.clj`, `app/storage/impl.clj`, `app/config.clj`).

## TL;DR

**The backend never serves asset bytes itself.** With the `fs` storage backend it answers
every `/assets/*` request with `HTTP 204 No Content` plus an `x-accel-redirect` header
pointing at an internal path like `/internal/assets/9b/6d/4690443a40fc88d6941beb488226`.
Upstream nginx has an `internal` location that aliases that path onto the shared assets
volume and serves the file itself. A custom proxy **must** implement the same X-Accel
dance (or read the file from disk directly); simply forwarding `/assets` to the backend
yields empty 204 responses and a broken UI.

## Repro: what was done

1. Created a scratch file via `POST /api/rpc/command/create-file` (JSON body
   `{"projectId": …, "name": "m0-asset-spike"}`) → file id `3a4be581-6d37-8010-8008-51edb9e2a137`.
2. Uploaded a locally generated 75-byte 8×8 PNG via multipart:

   ```
   POST /api/rpc/command/upload-file-media-object
   Authorization: Token <personal-access-token>
   Content-Type: multipart/form-data
     file-id = 3a4be581-6d37-8010-8008-51edb9e2a137
     is-local = true
     name = tiny.png
     content = <tiny.png; type=image/png>
   ```

   Response (JSON):

   ```json
   {
     "id": "3a4be581-6d37-8010-8008-51edce6c250c",   // file-media-object id
     "mediaId": "9b6d4690-443a-40fc-88d6-941beb488226", // storage-object id
     "fileId": "3a4be581-6d37-8010-8008-51edb9e2a137",
     "mtype": "image/png", "width": 8, "height": 8, "isLocal": true
   }
   ```

   Both the access-token header and the `auth-token` session cookie work for this call.

## Asset URL shapes the frontend uses

Grepping the deployed frontend bundle (`/var/www/app/js/shared.js`) shows exactly two
URL prefixes: `assets/by-id/` and `assets/by-file-media-id/`. The backend route table
(from `app/http/assets.clj` in the 2.16.2 jar) is:

| Route | Resolves via |
|---|---|
| `GET /assets/by-id/<storage-object-id>` | storage object directly (fonts, thumbnails, profile photos) |
| `GET /assets/by-file-media-id/<file-media-object-id>` | `file_media_object.media_id` → storage object |
| `GET /assets/by-file-media-id/<id>/thumbnail` | `thumbnail_id`, falling back to `media_id` |

For the uploaded PNG the frontend would request
`/assets/by-file-media-id/3a4be581-6d37-8010-8008-51edce6c250c`.

## Through the frontend nginx (port 9001) — baseline

```
GET http://localhost:9001/assets/by-file-media-id/3a4be581-6d37-8010-8008-51edce6c250c

HTTP/1.1 200 OK
Server: nginx
Content-Type: image/png
Content-Length: 75
Last-Modified: Mon, 13 Jul 2026 08:45:11 GMT
cache-control: max-age=86400000
x-internal-redirect: /internal/assets/9b/6d/4690443a40fc88d6941beb488226
Accept-Ranges: bytes
(+ the usual security headers)
```

Body: the exact 75 PNG bytes (md5 matches the upload). Range requests work
(`Range: bytes=0-9` → `206 Partial Content`, 10 bytes) because nginx serves the file
statically. Note nginx even echoes the internal path in a debug-ish
`x-internal-redirect` header.

## Directly against the backend (port 6060) — the raw behavior

```
GET http://localhost:6060/assets/by-file-media-id/3a4be581-6d37-8010-8008-51edce6c250c

HTTP/1.1 204 No Content
cache-control: max-age=86400000
content-type: image/png
x-accel-redirect: /internal/assets/9b/6d/4690443a40fc88d6941beb488226
```

- **Zero body bytes.** Not a redirect, not the file — a 204 with an `x-accel-redirect`
  header. `/assets/by-id/9b6d4690-…` behaves identically.
- The backend itself does **not** serve `/internal/assets/...` — requesting that path on
  6060 returns 404. Only something with filesystem access to the assets volume can finish
  the request.
- `content-type` comes from storage-object metadata (files on disk have **no extension**,
  so the proxy cannot infer MIME from the path — it must propagate this header).
- `cache-control` is `max-age=<ms>` (a long-standing upstream quirk: the value is
  milliseconds, 86400000, though the header spec says seconds — copy it verbatim anyway).

### Handler source (ground truth, from penpot.jar `app/http/assets.clj`)

```clojure
(defn- serve-object-from-fs
  [{:keys [::path ::cache-max-age]} obj]
  (let [cch-max-age (or cache-max-age default-cache-max-age)
        purl    (u/join (u/uri path) (sto/object->relative-path obj))
        mdata   (meta obj)
        headers {"x-accel-redirect" (:path purl)
                 "content-type" (:content-type mdata)
                 "cache-control" (str "max-age=" (inst-ms cch-max-age))}]
    {::yres/status 204
     ::yres/headers headers}))

(defn- serve-object
  [cfg {:keys [backend] :as obj}]
  (case backend
    (:s3 :assets-s3) (serve-object-from-s3 cfg obj)   ; 307 + presigned Location
    (:fs :assets-fs) (serve-object-from-fs cfg obj))) ; 204 + x-accel-redirect
```

There is **no configuration that makes the backend stream the bytes itself** — `fs`
always emits X-Accel, `s3` always emits a 307 to a presigned URL. The only knob is the
X-Accel path prefix: `PENPOT_ASSETS_PATH`, default `"/internal/assets/"`
(`app/config.clj`).

### Authentication

`app/http/assets.clj` (2.16.2) whitelists four "public buckets" that are served
**without authentication**: `file-media-object`, `file-object-thumbnail`,
`team-font-variant`, `file-data-fragment`. Verified: the request above succeeds with no
cookie and no token. Other buckets (e.g. `profile`) return 401 unless a session cookie or
`Authorization: Token` is present — the assets routes run through both the session and
access-token middleware, so either credential works. Since our proxy is localhost-only
single-user, this distinction barely matters, but the proxy should forward `Cookie` and
`Authorization` headers untouched so non-public buckets keep working.

## Upstream nginx config (the contract to reimplement)

From `penpot-m0-penpot-frontend-1:/etc/nginx/nginx.conf`:

```nginx
location /assets {
    proxy_pass http://penpot-backend:6060/assets;
    recursive_error_pages on;
    proxy_intercept_errors on;
    error_page 301 302 307 = @handle_redirect;   # s3 backend only

    include /etc/nginx/overrides/assets.d/*.conf;
}

location /internal/assets {
    internal;                                    # not reachable from outside
    alias /opt/data/assets;                      # the fs storage volume
    include /etc/nginx/nginx-security-headers.conf;
    add_header x-internal-redirect "$upstream_http_x_accel_redirect";
}
```

nginx's built-in X-Accel-Redirect handling does the actual work: when the upstream 204
carries `x-accel-redirect: /internal/assets/<p>`, nginx internally re-dispatches to the
`internal` location, serves `/opt/data/assets/<p>` from disk, and (per nginx semantics)
**preserves the upstream's `Content-Type` and `Cache-Control`** on the final response
while adding static-file niceties (`Content-Length`, `Last-Modified`, `Accept-Ranges`).
The `@handle_redirect` location is only exercised by the S3 backend (it re-proxies the
presigned `Location`); irrelevant for our fs-only desktop app.

## On-disk layout (fs volume)

Backend env: `PENPOT_OBJECTS_STORAGE_FS_DIRECTORY=/opt/data/assets` (same volume mounted
into the frontend container at the same path).

```
/opt/data/assets/9b/6d/4690443a40fc88d6941beb488226    (75 bytes, mode 0644, owner penpot)
```

- **Path is the storage-object UUID, not a content hash**: uuid hex without dashes,
  sharded as `<byte0>/<byte1>/<rest>` (`app/storage/impl.clj id->path`:
  `9b6d4690-443a-40fc-88d6-941beb488226` → `9b/6d/4690443a40fc88d6941beb488226`).
- The stored file is **byte-identical to the upload** (md5 verified) — no re-encoding,
  no extension.
- **Dedup happens at the DB layer, not the path layer.** `storage_object.metadata`
  records a content hash: `{"~:hash": "blake2b:e31cc7f4…", "~:bucket":
  "file-media-object", "~:content-type": "image/png"}`. Re-uploading the identical PNG
  created a second `file_media_object` row but returned the **same** `mediaId` and left
  exactly one file on disk. So: content-addressed *semantics* (dedup by blake2b hash per
  bucket), UUID-addressed *layout*.

## Spec: what the bundled proxy (M1) must implement

For `GET|HEAD /assets/**`:

1. Forward the request to the backend as-is (including `Cookie` / `Authorization`).
2. If the response has an `x-accel-redirect` header (status 204):
   a. Strip the configured prefix (`/internal/assets/`, keep in sync with
      `PENPOT_ASSETS_PATH`) and resolve the remainder against
      `PENPOT_OBJECTS_STORAGE_FS_DIRECTORY`. **Reject path traversal** (`..`, absolute
      paths) — nginx's `internal`+`alias` gave this for free.
   b. Serve that file: correct `Content-Length`, honor `Range` (frontend relies on
      `Accept-Ranges` for fonts/large images), sensible `Last-Modified`/`ETag` optional.
   c. Copy `content-type` and `cache-control` from the backend's 204 onto the final
      response (there is no file extension to sniff from).
   d. Do not expose `/internal/assets/...` as an externally routable path.
3. Otherwise pass the backend response through unchanged (404 for missing objects,
   401 for non-public buckets without credentials).
4. 301/302/307-with-Location handling (`@handle_redirect`) is only needed for the S3
   backend — skip it, we pin `fs`.

Alternative worth considering for the desktop app: since the proxy and backend share a
filesystem anyway, the proxy could skip step 2a's indirection subtleties — but the
X-Accel contract is tiny (one header + one directory), stable across Penpot versions, and
keeps the backend authoritative over auth/404 decisions, so implementing it as specced is
the low-risk path.

Everything else nginx does for `/api` (plain reverse proxy, `proxy_buffering off`) and
`/ws/notifications` (WebSocket upgrade) is orthogonal to asset serving and already
planned in PLAN.md.

## Scratch objects created (safe to delete)

- Penpot file `m0-asset-spike` (`3a4be581-6d37-8010-8008-51edb9e2a137`) in the Drafts
  project, containing two `file_media_object` rows (`…51edce6c250c`, `…51ee53ff748c`)
  both pointing at storage object `9b6d4690-443a-40fc-88d6-941beb488226`.
