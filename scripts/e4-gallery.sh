#!/usr/bin/env bash
# E4 package-gallery / surface-don't-apply update+conflict / ecosystem gate
# (PLAN3.md milestone E4, `just e4`).
#
# Chapter 3's browse+update surface, on-disk and offline. A package gallery
# (`DocKind::Package` FTS rows keyed `pkg:<id>`, served at `/__packages` +
# `/__api/packages/search`), the surface-don't-apply update channel
# (`/__api/packages/updates`, a watch-channel poll parallel to N3's activity
# strip — NO real SSE), and the drift-of-a-managed-package conflict rule
# (`/__api/packages/preserve-drift`, the verbatim `.conflict-<ts>` copy that
# overwrites neither side). Mostly composition of already-proven pieces (E1
# contract diff, E2 install/lockfile, E3 library authoring, N3 routes-gate).
#
# Exit criteria (PLAN3 E4, verbatim), each a PASS/FAIL block in the house style:
#   (1) index N packages at TORTURE SCALE and assert /__api/packages/search
#       returns the CORRECT ids in <100ms (server-measured tookMs).
#   (2) a headless-browser leg (routes-gate style, bundled offline chromium)
#       loads /__packages and deep-links one package file to its EXACT
#       /#/workspace URL, string-asserted from the /__api/packages payload.
#   (3) an edited package surfaces the correct MINOR (grow) / MAJOR (removal)
#       bump in the update-status poll within the poll+debounce window WHILE the
#       consumer's materialized `.penpot` file stays BYTE-UNCHANGED (raw-hash
#       equal before/after — surfaced, never applied).
#   (4) a drifted managed-package copy produces a `.conflict-<ts>` copy that
#       overwrites NEITHER the installed file NOR the package source.
#   (5) delete-index-db + rebuild identical (invariant 1): the package rows come
#       back byte-for-byte (id/name/version/kind/fileId/deepLink) after the
#       disposable index sqlite is wiped and rebuilt from disk alone.
#
# Dedicated ports (ledger): proxy 8986, backend 6447, postgres 5521, valkey
# 6464, control 8987 (exporter 6527 reserved; renders OFF — the gallery needs no
# renders). Fresh mktemp dirs; ANSI-stripped log greps; pg-install cache seeded;
# dirs kept on failure. Run-twice idempotent (fresh dirs each run).
#
# Browser (leg 2): the bundled chromium headless-shell + playwright (N2), fully
# offline — same driver family as scripts/routes_gate_nav.cjs.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh
# --with-browsers), JDK 26, valkey-server, node, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E4_PROXY_PORT:-8986}"
BACKEND_PORT="${E4_BACKEND_PORT:-6447}"
POSTGRES_PORT="${E4_POSTGRES_PORT:-5521}"
VALKEY_PORT="${E4_VALKEY_PORT:-6464}"
CONTROL_PORT="${E4_CONTROL_PORT:-8987}"
FIRST_BOOT_TIMEOUT="${E4_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
MATERIALIZE_TIMEOUT="${E4_MATERIALIZE_TIMEOUT:-180}"
SYNC_TIMEOUT="${E4_SYNC_TIMEOUT:-240}"
# The E4b update poller interval is 2s + a debounce; give the surface a generous
# window so the assert is never a timing flake.
UPDATE_WINDOW="${E4_UPDATE_WINDOW:-30}"
# Torture scale: how many synthetic package rows the gallery must search under.
TORTURE_N="${E4_TORTURE_N:-200}"
NONCE="e4tortnonce"
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e4-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e4-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e4-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
E2HELPER="$ROOT/scripts/e2_packages_helper.py"
E3HELPER="$ROOT/scripts/e3_library_helper.py"
E4HELPER="$ROOT/scripts/e4_gallery_helper.py"
NAV_DRIVER="$ROOT/scripts/e4_gallery_nav.cjs"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
PLAYWRIGHT="${ROUTES_GATE_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
NODE_BIN="${ROUTES_GATE_NODE:-node}"
INDEX_DB_DIR="$DATA_DIR/vault-index"
HEADLESS_PID=""
FAILURES=0

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
e2helper() { python3 "$E2HELPER" "$@"; }
e3helper() { python3 "$E3HELPER" "$@"; }
e4helper() { python3 "$E4HELPER" "$@"; }
sha() { shasum -a 256 | cut -d' ' -f1; }

PG_CACHE="${E4_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE for future runs)"
    fi
}

cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 20); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT log=$LOG work=$WORK_DIR"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
last_json_field() { tail -1 | json_field "$1"; }

# Raw byte hash of an on-disk .penpot tree (every file, sorted) — the
# byte-unchanged witness for surface-don't-apply.
raw_hash() {
    find "$1" -type f -print0 2>/dev/null | LC_ALL=C sort -z |
        xargs -0 shasum -a 256 2>/dev/null | sha
}

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died; last log lines:" >&2; tail -25 "$LOG" >&2; return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2; return 1
}

wait_log() { # wait_log <timeout> <pattern>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" | grep -q "$2" && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died waiting for '$2'" >&2; return 1; }
        sleep 2
    done
    echo "timed out waiting for '$2' ($1s)" >&2; return 1
}

stop_headless() {
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 25); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then HEADLESS_PID=""; return 0; fi
        sleep 1
    done
    return 1
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

author_package() { # author_package <pkg_id> <kind> <version> <src_rel>
    local id="$1" kind="$2" version="$3" src_rel="$4"
    local dest="$VAULT/.penpot-packages/$id"
    mkdir -p "$dest"
    cp -R "$VAULT/$src_rel" "$dest/$id.penpot"
    cat >"$dest/package.json" <<EOF
{
  "id": "$id",
  "version": "$version",
  "kind": "$kind",
  "name": "Managed Button Library"
}
EOF
}

echo "== E4 package-gallery gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   vault (empty): $VAULT"

# --- pre-flight (leg 2 needs the bundled browser, like routes-gate) ---------
[ -f "$NAV_DRIVER" ] || { fail "nav driver missing: $NAV_DRIVER"; exit 1; }
if [ ! -e "$PLAYWRIGHT" ]; then
    fail "bundled playwright missing at $PLAYWRIGHT — run scripts/fetch-penpot.sh --with-browsers"; exit 1
fi

# --- build ------------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ---------------------------------------------------------------------------
# Boot + author a managed component-library package with a real exported color.
# ---------------------------------------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then pass "boot READY on an empty vault"; else fail "boot"; exit 1; fi
read_token || { fail "no access token"; exit 1; }

echo "== authoring a managed library package (template + E3 library content) =="
if OUT="$(e2helper new_template "$BASE" "$VAULT" welcome "$MATERIALIZE_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    SRC_REL="$(echo "$OUT" | tail -1 | json_field relPath)"
    SRC_FID="$(echo "$OUT" | tail -1 | json_field fileId)"
    pass "materialized a source template tree ($SRC_REL)"
else
    fail "authoring source template"; echo "$OUT" >&2; exit 1
fi
# Author a real exported color ("Brand Teal") + component + typography into the
# template file via the E3 RPC path, so the package source carries a genuine,
# round-trippable contract element to grow (minor) and remove (major).
if OUT="$(e3helper add_library_content "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$SRC_FID" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    pass "authored library contract (Brand Teal color + variant set + typography)"
else
    fail "authoring library contract"; echo "$OUT" >&2; exit 1
fi
# Wait for Brand Teal to land on disk BEFORE snapshotting the tree into a package.
if e4helper wait_color_ondisk "$BASE" "$VAULT" "$SRC_FID" "Brand Teal" "$SYNC_TIMEOUT" >/dev/null 2>&1; then
    pass "Brand Teal exported color synced into the source tree on disk"
else
    fail "Brand Teal never synced to disk"; exit 1
fi
author_package managed-lib component-library 1.0.0 "$SRC_REL"
pass "authored .penpot-packages/managed-lib from the library tree"

echo "== install the managed package (real import, round-trip fixpoint) =="
if e2helper install_verify "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$VAULT" managed-lib "$MATERIALIZE_TIMEOUT" \
    >"$WORK_DIR/install.out" 2>&1; then
    grep '^PASS' "$WORK_DIR/install.out"; tail -1 "$WORK_DIR/install.out"
    pass "installed managed-lib (imported + settled to A=B fixpoint + locked)"
else
    fail "install managed-lib"; cat "$WORK_DIR/install.out" >&2; exit 1
fi
MAT_REL="$(e4helper materialized_rel "$VAULT" managed-lib)"
[ -n "$MAT_REL" ] && [ -d "$VAULT/$MAT_REL" ] &&
    pass "managed-lib materialized on disk ($MAT_REL)" ||
    { fail "managed-lib materialized path unresolved ($MAT_REL)"; exit 1; }

# ---------------------------------------------------------------------------
# (2) headless-browser deep-link leg (routes-gate style, bundled offline).
# ---------------------------------------------------------------------------
echo "== (2) gallery card deep-links to its EXACT /#/workspace URL =="
# Wait for the package row to be searchable first.
e4helper search_wait "$BASE" 1 "$SYNC_TIMEOUT" >/dev/null 2>&1 || true
NAV_OUT="$(PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" ROUTES_GATE_PLAYWRIGHT="$PLAYWRIGHT" \
    "$NODE_BIN" "$NAV_DRIVER" "$BASE" managed-lib 2>"$WORK_DIR/nav.err")"
if echo "$NAV_OUT" | grep -q '"ok":true'; then
    pass "(2) /__packages card click landed on the exact /#/workspace deep link (from the payload)"
    echo "   nav: $NAV_OUT"
else
    fail "(2) gallery deep-link navigation failed: $NAV_OUT"
    cat "$WORK_DIR/nav.err" >&2 || true
fi

# ---------------------------------------------------------------------------
# (3) surface-don't-apply: minor + major bumps, consumer file BYTE-UNCHANGED.
# ---------------------------------------------------------------------------
echo "== (3) update channel surfaces minor/major while the file stays byte-unchanged =="
MAT_HASH_BEFORE="$(raw_hash "$VAULT/$MAT_REL")"
pass "(3) snapshot the materialized consumer file bytes (hash=${MAT_HASH_BEFORE:0:12})"

# --- minor: grow the source's exported-color surface -----------------------
if e4helper source_add_color "$VAULT" managed-lib "E4 Accent Coral" >/dev/null 2>&1; then
    pass "(3) edited managed-lib SOURCE: added an exported color (a minor grow)"
else
    fail "(3) could not add a color to the source"
fi
if e4helper updates_wait "$BASE" managed-lib minor "$UPDATE_WINDOW"; then :; else fail "(3) minor bump not surfaced"; fi

# --- major: remove an exported element from the source ---------------------
if e4helper source_remove_color "$VAULT" managed-lib "Brand Teal" >/dev/null 2>&1; then
    pass "(3) edited managed-lib SOURCE: removed the Brand Teal color (a major removal)"
else
    fail "(3) could not remove a color from the source"
fi
if e4helper updates_wait "$BASE" managed-lib major "$UPDATE_WINDOW"; then :; else fail "(3) major bump not surfaced"; fi

# --- the whole point: the consumer .penpot on disk never moved -------------
MAT_HASH_AFTER="$(raw_hash "$VAULT/$MAT_REL")"
if [ "$MAT_HASH_AFTER" = "$MAT_HASH_BEFORE" ]; then
    pass "(3) consumer materialized .penpot is BYTE-UNCHANGED across both bumps (surfaced, not applied)"
else
    fail "(3) consumer file changed bytes ($MAT_HASH_BEFORE -> $MAT_HASH_AFTER) — the update was APPLIED"
fi

# ---------------------------------------------------------------------------
# (5) delete-index-db + rebuild identical (invariant 1). Done BEFORE the drift
# leg so the package-row snapshot is clean (one real package).
# ---------------------------------------------------------------------------
echo "== (5) delete-index-db + rebuild reproduces the package rows identically =="
e4helper rows_snapshot "$BASE" >"$WORK_DIR/rows-before.json"
ROWS_N="$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))))' "$WORK_DIR/rows-before.json" 2>/dev/null || echo 0)"
if stop_headless; then pass "(5) clean shutdown before index-db wipe"; else fail "(5) shutdown hung"; fi
rm -rf "$INDEX_DB_DIR"    # wipe ONLY the disposable vault-index sqlite (NOT postgres)
pass "(5) deleted the vault-index db ($INDEX_DB_DIR)"
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT"; then pass "(5) reboot after index-db wipe"; else fail "(5) reboot"; tail -25 "$LOG" >&2; exit 1; fi
read_token || { fail "(5) no token after reboot"; exit 1; }
if e4helper search_wait "$BASE" "$ROWS_N" "$SYNC_TIMEOUT" >/dev/null 2>&1; then
    pass "(5) the vault-index rebuilt the package rows from disk alone"
else
    fail "(5) package rows did not come back after the index-db wipe"
fi
e4helper rows_snapshot "$BASE" >"$WORK_DIR/rows-after.json"
if cmp -s "$WORK_DIR/rows-before.json" "$WORK_DIR/rows-after.json"; then
    pass "(5) package rows byte-identical across the index-db delete+rebuild ($ROWS_N package[s])"
else
    fail "(5) package rows differ after rebuild"; diff "$WORK_DIR/rows-before.json" "$WORK_DIR/rows-after.json" >&2 || true
fi

# ---------------------------------------------------------------------------
# (4) drift of a managed package -> a .conflict-<ts> copy, overwrite neither.
# The source has legitimately drifted (the major edit above), so preserving it
# is the exact conflict-rule scenario.
# ---------------------------------------------------------------------------
echo "== (4) drift conflict copy overwrites neither side =="
if e4helper preserve_drift "$BASE" "$VAULT" managed-lib; then :; else fail "(4) drift conflict copy"; fi

# ---------------------------------------------------------------------------
# (1) torture-scale search: N synthetic packages, correct ids, <100ms.
# ---------------------------------------------------------------------------
echo "== (1) torture-scale gallery search: correct ids in <100ms =="
if OUT="$(e4helper seed_synthetic "$VAULT" "$TORTURE_N" "$NONCE" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    PROBES_JSON="$(echo "$OUT" | tail -1)"
else
    fail "(1) seeding synthetic packages"; echo "$OUT" >&2
    PROBES_JSON='{"probes":[]}'
fi
# The index holds TORTURE_N synthetic + 1 real = TORTURE_N+1 package rows, but
# /__api/packages/search caps the RETURNED listing at MAX_LIMIT=200 (server-side).
# Wait until the listing fills to that cap (the index is provably larger — the
# first/middle/last probes below each resolve, spanning the whole seeded range).
WANT="$((TORTURE_N + 1))"; [ "$WANT" -gt 200 ] && WANT=200
if e4helper search_wait "$BASE" "$WANT" "$SYNC_TIMEOUT" >/dev/null 2>&1; then
    pass "(1) gallery filled to the search cap ($WANT rows) under $TORTURE_N synthetic torture packages"
else
    fail "(1) gallery never reached torture scale"
fi
# Warm the FTS path once (mmap / page cache), then assert on the measured tookMs.
curl -fsS "$BASE/__api/packages/search?q=${NONCE}0000" >/dev/null 2>&1 || true
# Assert each probe (first / middle / last synthetic package) returns the correct
# id in <100ms. Process-substitution (not a pipe) so PROBE_FAIL stays in-scope.
PROBE_FAIL=0
while read -r q id; do
    [ -z "$q" ] && continue
    if OUT="$(e4helper search_assert "$BASE" "$q" "$id" 100 2>&1)"; then
        echo "$OUT"
    else
        echo "$OUT"; PROBE_FAIL=1
    fi
done < <(echo "$PROBES_JSON" | python3 -c '
import json, sys
for p in json.load(sys.stdin).get("probes", []):
    print(p["q"], p["id"])
' 2>/dev/null)
[ "$PROBE_FAIL" = "0" ] &&
    pass "(1) every torture-scale probe returned the correct id in <100ms" ||
    fail "(1) a torture-scale probe missed its id or the 100ms budget"

# --- shutdown --------------------------------------------------------------
if stop_headless; then pass "final clean shutdown"; else fail "final shutdown hung"; fi
sleep 1
ports_all_free && pass "all 4 ports freed" || fail "ports still busy after shutdown"

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "E4 GALLERY: ALL PASS"
    exit 0
else
    echo "E4 GALLERY: $FAILURES FAILURE(S)"
    exit 1
fi
