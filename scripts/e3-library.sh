#!/usr/bin/env bash
# E3 component-library linking gate (PLAN3.md milestone E3, `just e3`).
#
# Chapter 3's linking half: a component-library package published shared
# (`set-file-shared`), a consumer package LINKED to it (`link-file-to-library`)
# that places a component instance referencing the library by its vault-local
# file-id, and the surgical linked-file export that keeps the reference on disk
# WITHOUT inlining the library. The disposable `file_library_rel` is re-derived
# from the per-vault lockfile on a delete-DB rebuild (invariant 1); library
# change severity is surfaced through the E1 contract-diff channel, never `revn`.
#
# Seeded from the proven live probe (scripts/ecosystem-spike/e3_probe.py) — the
# same RPC sequence, promoted to a house-style PASS/FAIL gate driving the real
# package verbs (/__api/packages/{publish,link}) + the sync daemon's automatic
# M2 resurrect + boot re-link reconcile.
#
# Exit criteria (PLAN3 E3, verbatim), each a PASS/FAIL block:
#   (1) install+publish a library, author a consumer that links it and places an
#       instance -> the instance's componentFile resolves to the library's
#       vault-local id (DB + on disk).
#   (2) delete-DB + reboot rebuilds BOTH files under their ORIGINAL ids and the
#       instance STILL resolves (invariant 1), with file_library_rel re-derived
#       from the lockfile.
#   (3) a patch edit surfaces NO bump while minor/major edits surface the correct
#       bump via the lockfile contract-diff channel (E1 `contract diff`), NOT
#       `revn` (revn advances on the patch too, yet the bump stays PATCH).
#   (4) file_library_rel is re-established from the lockfile on rebuild
#       (derived/disposable — the boot reconcile does it, no manual re-link).
#   (5) include-libraries is unused as storage: the linked consumer's on-disk
#       `.penpot` tree is single-file, componentFile=<libId>, library NOT inlined.
#   (6) unlink clears the relation (`unlink-file-from-library`).
#
# Dedicated ports (ledger): proxy 8974, backend 6435, postgres 5509, valkey
# 6452, control 8975 (exporter 6515 reserved; renders OFF). Fresh mktemp dirs;
# ANSI-stripped log greps; pg-install cache seeded; dirs kept on failure.
# Run-twice idempotent (fresh dirs each run).
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, git, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E3_PROXY_PORT:-8974}"
BACKEND_PORT="${E3_BACKEND_PORT:-6435}"
POSTGRES_PORT="${E3_POSTGRES_PORT:-5509}"
VALKEY_PORT="${E3_VALKEY_PORT:-6452}"
CONTROL_PORT="${E3_CONTROL_PORT:-8975}"
FIRST_BOOT_TIMEOUT="${E3_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
MATERIALIZE_TIMEOUT="${E3_MATERIALIZE_TIMEOUT:-180}"
SYNC_TIMEOUT="${E3_SYNC_TIMEOUT:-240}"
RELINK_TIMEOUT="${E3_RELINK_TIMEOUT:-180}"
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
CONTRACT_BIN="$ROOT/target/debug/contract"
E2HELPER="$ROOT/scripts/e2_packages_helper.py"
E3HELPER="$ROOT/scripts/e3_library_helper.py"
HEADLESS_PID=""
FAILURES=0

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
e2helper() { python3 "$E2HELPER" "$@"; }
e3helper() { python3 "$E3HELPER" "$@"; }

PG_CACHE="${E3_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
# last-line JSON field from a helper's stdout capture
last_json_field() { tail -1 | json_field "$1"; }

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

post_json() { # post_json <path> <json-body>
    curl -fsS -X POST "$BASE$1" -H 'Content-Type: application/json' -d "$2" 2>/dev/null
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
  "name": "$id"
}
EOF
}

echo "== E3 library-linking gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   vault (empty): $VAULT"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"
if ! (cd "$ROOT" && cargo build -q -p vault-index --bin contract); then
    fail "build (contract CLI)"; exit 1
fi
pass "build (contract CLI)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ---------------------------------------------------------------------------
# Boot + author a library package + a consumer package from a builtin template.
# ---------------------------------------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then pass "boot READY on an empty vault"; else fail "boot"; exit 1; fi
read_token || { fail "no access token"; exit 1; }

echo "== authoring a library + a consumer package tree from a builtin template =="
if OUT="$(e2helper new_template "$BASE" "$VAULT" welcome "$MATERIALIZE_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    SRC_REL="$(echo "$OUT" | tail -1 | json_field relPath)"
    pass "materialized a source template tree ($SRC_REL)"
else
    fail "authoring source template"; echo "$OUT" >&2; exit 1
fi
author_package button-library component-library 1.0.0 "$SRC_REL"
author_package app-screens    design           1.0.0 "$SRC_REL"
pass "dropped a library .penpot AND a consumer .penpot under .penpot-packages/"
sleep 6  # let the source template finish syncing

# ---------------------------------------------------------------------------
# (1) publish a library, author its component, link a consumer, place instance.
# ---------------------------------------------------------------------------
echo "== (1) publish library + link consumer + place an instance =="
PUB="$(post_json /__api/packages/publish '{"id":"button-library"}')"
LIB_ID="$(echo "$PUB" | json_field fileId 2>/dev/null || true)"
LIB_SHARED="$(echo "$PUB" | json_field libraryShared 2>/dev/null || true)"
if [ -n "$LIB_ID" ] && [ "$LIB_SHARED" = "True" -o "$LIB_SHARED" = "true" ]; then
    pass "(1) published button-library shared (fileId=$LIB_ID, libraryShared=$LIB_SHARED)"
else
    fail "(1) publish button-library: $PUB"; exit 1
fi

# author the library's contract INTO the installed library file
if OUT="$(e3helper add_library_content "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$LIB_ID" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    COMP_ID="$(echo "$OUT" | last_json_field componentId)"
    MAIN_INST="$(echo "$OUT" | last_json_field mainInstanceId)"
    BRAND_TEAL="$(echo "$OUT" | last_json_field brandTealColorId)"
    PATCH_TARGET="$(echo "$OUT" | last_json_field patchTargetId)"
    pass "(1) authored the library contract (component $COMP_ID + Brand Teal + typography)"
else
    fail "(1) authoring library contract"; echo "$OUT" >&2; exit 1
fi

LINK="$(post_json /__api/packages/link '{"consumerId":"app-screens","libraryId":"button-library"}')"
CONS_ID="$(echo "$LINK" | json_field consumerFileId 2>/dev/null || true)"
LINK_LIB_ID="$(echo "$LINK" | json_field libraryFileId 2>/dev/null || true)"
if [ -n "$CONS_ID" ] && [ "$LINK_LIB_ID" = "$LIB_ID" ]; then
    pass "(1) linked app-screens -> button-library (consumerFileId=$CONS_ID, libraryFileId=$LINK_LIB_ID)"
else
    fail "(1) link app-screens -> button-library: $LINK"; exit 1
fi

# place an instance of the library component into the consumer
if OUT="$(e3helper place_instance "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_ID" "$LIB_ID" "$COMP_ID" "$MAIN_INST" 2>&1)"; then
    echo "$OUT" | grep -E '^(PASS|FAIL)' || true
    INST_ID="$(echo "$OUT" | last_json_field instanceId)"
    echo "$OUT" | grep -q '^PASS' && pass "(1) instance componentFile resolves to the library vault-local id (DB)" \
        || fail "(1) instance componentFile did not resolve"
else
    fail "(1) place instance"; echo "$OUT" >&2; exit 1
fi

# ---------------------------------------------------------------------------
# (5) linked consumer on disk is single-file, componentFile=libId, no inline.
# ---------------------------------------------------------------------------
echo "== (5) linked consumer on disk: single-file, library NOT inlined =="
if OUT="$(e3helper wait_consumer_ondisk "$BASE" "$VAULT" "$CONS_ID" "$LIB_ID" "$INST_ID" "$SYNC_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    CONS_REL="$(echo "$OUT" | last_json_field relPath)"
    pass "(1)(5) surgical linked export landed on disk ($CONS_REL)"
else
    fail "(5) consumer never exported to disk"; echo "$OUT" >&2; exit 1
fi
if e3helper assert_consumer_singlefile "$VAULT" "$CONS_REL" "$CONS_ID" "$LIB_ID" "$INST_ID"; then
    :
else
    fail "(5) consumer on-disk representation"
fi

# ---------------------------------------------------------------------------
# linked-state witness: GET /__api/packages surfaces libraryShared + links[].
# ---------------------------------------------------------------------------
echo "== linked-state witness (GET /__api/packages) =="
curl -fsS "$BASE/__api/packages" >"$WORK_DIR/packages.json" 2>/dev/null || true
if python3 - "$LIB_ID" "$WORK_DIR/packages.json" <<'PY'
import json, sys
lib_id = sys.argv[1]
d = json.load(open(sys.argv[2]))
pkgs = {p["id"]: p for p in d.get("packages", [])}
lib = pkgs.get("button-library", {})
cons = pkgs.get("app-screens", {})
ok = True
def check(c, m):
    global ok
    print(("PASS: " if c else "FAIL: ") + m); ok = ok and c
check(lib.get("libraryShared") is True and lib.get("live") is True,
      f"library shows libraryShared=true + live=true (libraryShared={lib.get('libraryShared')}, live={lib.get('live')})")
check(any(l.get("libraryFileId") == lib_id for l in cons.get("links", [])),
      f"consumer links[] contains the library fileId ({[l.get('libraryFileId') for l in cons.get('links', [])]})")
sys.exit(0 if ok else 1)
PY
then :; else fail "linked-state witness"; fi

# ---------------------------------------------------------------------------
# (2)(4) delete-DB + reboot: rebuild both files under original ids, re-derive.
# ---------------------------------------------------------------------------
echo "== (2)(4) delete-DB + reboot: rebuild + re-derive file_library_rel =="
cp "$VAULT/lock.json" "$WORK_DIR/lock-before.json"
if stop_headless; then pass "(2) clean shutdown before DB wipe"; else fail "(2) shutdown hung"; fi
rm -rf "$DATA_DIR/postgres"     # wipe ONLY the disposable DB (M2)
pass "(2) deleted the Penpot database (rm -rf <data>/postgres)"
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log "$SYNC_TIMEOUT" "startup reconciliation done"; then
    pass "(2) reboot + startup reconciliation from disk"
else
    fail "(2) reboot/reconcile"; tail -25 "$LOG" >&2; exit 1
fi
read_token || { fail "(2) no token after reboot"; exit 1; }
if e3helper assert_reboot "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_ID" "$LIB_ID" "$INST_ID" "$RELINK_TIMEOUT"; then
    :
else
    fail "(2)(4) post-reboot rebuild/re-derive"
fi
if cmp -s "$VAULT/lock.json" "$WORK_DIR/lock-before.json"; then
    pass "(2) lock.json byte-identical across the delete-DB + reboot"
else
    fail "(2) lock.json changed across the wipe"
fi

# ---------------------------------------------------------------------------
# (3) contract-diff channel: patch -> no bump; minor/major -> correct bump.
# ---------------------------------------------------------------------------
echo "== (3) contract-diff channel (E1) surfaces bumps, NOT revn =="
# anchor snapshot of the (post-reboot, pristine) library on-disk tree
SIG_ANCHOR="$(e3helper ondisk_sig "$BASE" "$VAULT" "$LIB_ID" | json_field sig)"
LIB_REL="$(e3helper ondisk_sig "$BASE" "$VAULT" "$LIB_ID" | json_field relPath)"
cp -R "$VAULT/$LIB_REL" "$WORK_DIR/lib-anchor"

contract_bump() { "$CONTRACT_BIN" diff "$1" "$2" | strip_ansi | sed -n 's/^OVERALL BUMP: //p' | head -1; }

# --- patch edit (impl-only move) -> revn advances, contract bump PATCH ---
EDIT="$(e3helper contract_edit "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$LIB_ID" patch "$PATCH_TARGET")"
REVN_B="$(echo "$EDIT" | json_field revnBefore)"; REVN_A="$(echo "$EDIT" | json_field revnAfter)"
SIG_PATCH="$(e3helper wait_ondisk_change "$BASE" "$VAULT" "$LIB_ID" "$SIG_ANCHOR" "$SYNC_TIMEOUT" | json_field sig)"
if [ -n "$SIG_PATCH" ] && [ "$SIG_PATCH" != "None" ]; then
    cp -R "$VAULT/$LIB_REL" "$WORK_DIR/lib-patch"
    BUMP_PATCH="$(contract_bump "$WORK_DIR/lib-anchor" "$WORK_DIR/lib-patch")"
    [ "$BUMP_PATCH" = "PATCH" ] &&
        pass "(3) patch edit surfaces NO bump (contract diff = PATCH)" ||
        fail "(3) patch edit contract diff = $BUMP_PATCH (expected PATCH)"
    if [ "${REVN_A:-0}" -gt "${REVN_B:-0}" ] 2>/dev/null && [ "$BUMP_PATCH" = "PATCH" ]; then
        pass "(3) revn advanced on the patch ($REVN_B->$REVN_A) yet bump stayed PATCH — channel is contract, NOT revn"
    else
        fail "(3) revn/patch invariant (revn $REVN_B->$REVN_A, bump $BUMP_PATCH)"
    fi
else
    fail "(3) patch edit never synced to disk"
fi

# --- minor edit (add exported color) -> bump MINOR ---
e3helper contract_edit "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$LIB_ID" minor >/dev/null
SIG_MINOR="$(e3helper wait_ondisk_change "$BASE" "$VAULT" "$LIB_ID" "$SIG_PATCH" "$SYNC_TIMEOUT" | json_field sig)"
if [ -n "$SIG_MINOR" ] && [ "$SIG_MINOR" != "None" ]; then
    cp -R "$VAULT/$LIB_REL" "$WORK_DIR/lib-minor"
    BUMP_MINOR="$(contract_bump "$WORK_DIR/lib-anchor" "$WORK_DIR/lib-minor")"
    [ "$BUMP_MINOR" = "MINOR" ] &&
        pass "(3) minor edit (added an exported color) surfaces MINOR" ||
        fail "(3) minor edit contract diff = $BUMP_MINOR (expected MINOR)"
else
    fail "(3) minor edit never synced to disk"
fi

# --- major edit (remove exported color) -> bump MAJOR ---
e3helper contract_edit "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$LIB_ID" major "$BRAND_TEAL" >/dev/null
SIG_MAJOR="$(e3helper wait_ondisk_change "$BASE" "$VAULT" "$LIB_ID" "$SIG_MINOR" "$SYNC_TIMEOUT" | json_field sig)"
if [ -n "$SIG_MAJOR" ] && [ "$SIG_MAJOR" != "None" ]; then
    cp -R "$VAULT/$LIB_REL" "$WORK_DIR/lib-major"
    BUMP_MAJOR="$(contract_bump "$WORK_DIR/lib-anchor" "$WORK_DIR/lib-major")"
    [ "$BUMP_MAJOR" = "MAJOR" ] &&
        pass "(3) major edit (removed an exported color) surfaces MAJOR" ||
        fail "(3) major edit contract diff = $BUMP_MAJOR (expected MAJOR)"
else
    fail "(3) major edit never synced to disk"
fi

# ---------------------------------------------------------------------------
# (6) unlink clears the relation.
# ---------------------------------------------------------------------------
echo "== (6) unlink clears the relation =="
if e3helper unlink "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_ID" "$LIB_ID"; then
    :
else
    fail "(6) unlink"
fi

# --- shutdown --------------------------------------------------------------
if stop_headless; then pass "final clean shutdown"; else fail "final shutdown hung"; fi
sleep 1
ports_all_free && pass "all 4 ports freed" || fail "ports still busy after shutdown"

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "E3 LIBRARY: ALL PASS"
    exit 0
else
    echo "E3 LIBRARY: $FAILURES FAILURE(S)"
    exit 1
fi
