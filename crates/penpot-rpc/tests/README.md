# penpot-rpc tests

## `wire.rs` — offline wire-shape tests

Run with the normal test suite; no stack required:

```sh
cargo test -p penpot-rpc
```

They assert the exact request/response shapes documented in
`docs/m0/rpc-endpoints.md` against an in-process `wiremock` server.

## `live.rs` — live integration tests (`#[ignore]`)

These drive the real RPC surface end-to-end (poll surface, create/update/
delete, export-binfile SSE → authenticated download through the proxy,
import-binfile as-new and in-place) against a running Penpot stack.

### 1. Boot a stack

Use the M1 headless runner with a fresh data dir and dedicated ports so you
never collide with a dev instance (defaults 8686/6161/5433/6380; m0 spike owns
9001/6060, m1 smoke owns 8788/6263/5435/6382):

```sh
source "$HOME/.cargo/env"
DATA_DIR="$(mktemp -d)"
PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
PENPOT_LOCAL_PROXY_PORT=8890 \
PENPOT_LOCAL_BACKEND_PORT=6365 \
PENPOT_LOCAL_POSTGRES_PORT=5437 \
PENPOT_LOCAL_VALKEY_PORT=6384 \
cargo run -p penpot-desktop --bin headless
# wait for:  READY http://localhost:8890
# (first boot on a machine downloads embedded postgres binaries — be patient)
```

Provisioning persists an access token in `$DATA_DIR/credentials.json`
(key `access_token`).

### 2. Run the suite

```sh
export PENPOT_RPC_LIVE_BASE_URL=http://localhost:8890   # the PROXY url
export PENPOT_RPC_LIVE_TOKEN="$(python3 -c 'import json,os; print(json.load(open(os.environ["DATA_DIR"]+"/credentials.json"))["access_token"])')"
cargo test -p penpot-rpc --test live -- --ignored
```

Notes:

- `PENPOT_RPC_LIVE_BASE_URL` **must be the proxy URL**, not the bare backend:
  `export-binfile` artifact URIs point at `PENPOT_PUBLIC_URI` (= the proxy)
  and only the proxy fulfils `/assets/**` downloads (the backend alone
  answers `204` + `x-accel-redirect`).
- Tests are parallel-safe and re-runnable: each one creates its own uniquely
  named project and deletes it at the end.
- If the backend JVM was just killed/restarted it takes ~30–60 s before RPC
  answers again; a `Transport`/5xx failure right after a crash is transient.
- Cleanup uses `delete-project`, which is a **soft delete**: 204, but the
  project keeps showing up in `get-projects` with `deletedAt` set (~7 days
  out). Repeated runs therefore accumulate soft-deleted `rpc-live-*` projects
  in the team until GC — harmless, and the tests never assume they're absent.

### 3. Shut the stack down

SIGTERM the headless process (clean shutdown kills postgres/valkey/java
children) and remove the temp data dir.
