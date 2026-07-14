#!/usr/bin/env bash
# N1 vault-index gate (PLAN2.md milestone N1, `just n1`).
#
# Exit criteria implemented verbatim, each a PASS/FAIL block in the house
# style of m2-invariant.sh / m5-features.sh:
#   (a) torture fixture: 100 files / ~1000 boards seeded via RPC with
#       deterministic content (the shared seeder N3/N5 reuse:
#       scripts/n1_index_helper.py seed), fully mirrored to disk by the sync
#       daemon and fully indexed.
#   (b) correctness + latency: 23 deterministic queries (per-board tokens,
#       color, typography, board name) each return the exact
#       file/page/board/shape ids + the verified /#/workspace deep link.
#       Correctness is HARD; the <100ms latency criterion is SOFT-asserted
#       (loud WARNING on miss, per reviewer note) and reported.
#   (c) needle: a text planted via update-file RPC in a live board appears
#       in search results (correct ids) within one sync window.
#   (d) unicode needle: same with diacritics + CJK; diacritic-folded query
#       matches too.
#   (e) hash-gated no-op: idle cycles perform ZERO index writes (mutation
#       counter frozen); a full reboot with the db kept reindexes NOTHING.
#   (f) invariant 1: delete the index db + reboot -> byte-identical results
#       for a 9-query snapshot, rebuilt from disk alone.
#   (g) rename: an edit rewriting the needle's text removes the stale hit
#       within the sync+debounce window (index rows swapped atomically).
#   (h) /__search page served same-origin; bad requests answer 400.
#
# Dedicated ports (ledger): proxy 8916, backend 6391, postgres 5465, valkey
# 6408. Fresh mktemp dirs; ANSI-stripped log greps; pg-install cache seeded
# from ~/.cache/penpot-local/pg-install; dirs kept on failure.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, python3, curl. No exporter/node needed (index-only).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N1_PROXY_PORT:-8916}"
BACKEND_PORT="${N1_BACKEND_PORT:-6391}"
POSTGRES_PORT="${N1_POSTGRES_PORT:-5465}"
VALKEY_PORT="${N1_VALKEY_PORT:-6408}"
FIRST_BOOT_TIMEOUT="${N1_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
# fixture mirror: 2s DB poll + ~100 sequential export-binfile cycles
FIXTURE_SYNC_TIMEOUT="${N1_FIXTURE_SYNC_TIMEOUT:-600}"
NEEDLE_TIMEOUT=120         # 2s DB poll + export + 1s index poll
BASE="http://localhost:${PROXY_PORT}"

export N1_FILES="${N1_FILES:-100}"
export N1_BOARDS="${N1_BOARDS:-10}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n1-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n1-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n1-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/n1_index_helper.py"
INDEX_DB="$DATA_DIR/vault-index/index.sqlite3"
HEADLESS_PID=""
FAILURES=0

export N1_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
warn() { echo "WARNING: $1"; }

PG_CACHE="${N1_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
        for _ in $(seq 1 20); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR designs=$DESIGNS_DIR log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() { # wait_ready <timeout-seconds>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died; last log lines:" >&2
            tail -20 "$LOG" >&2
            return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2
    return 1
}

stop_headless() {
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 25); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            HEADLESS_PID=""
            return 0
        fi
        sleep 1
    done
    return 1
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

helper() { python3 "$HELPER" "$@"; }

index_status() { curl -fsS "$BASE/__api/vault/status" 2>/dev/null; }

status_field() { # status_field <field>  (from a fresh /__api/vault/status)
    index_status | json_field "$1"
}

wait_indexed() { # wait_indexed <timeout-s> <expected-files>
    local deadline=$(($(date +%s) + $1)) got=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        got="$(status_field filesIndexed 2>/dev/null || echo '?')"
        [ "$got" = "$2" ] && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless died while waiting for filesIndexed=$2" >&2
            tail -20 "$LOG" >&2
            return 1
        fi
        sleep 2
    done
    echo "timed out: filesIndexed=$got, want $2 ($1s)" >&2
    index_status >&2 || true
    return 1
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            return 1
        fi
    done
    return 0
}

echo "== N1 vault index: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE fixture=${N1_FILES}x${N1_BOARDS}"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"
    exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- boot ------------------------------------------------------------------
if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE — no download needed)"
fi
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" &&
    strip_ansi <"$LOG" | grep -q "vault-index service started"; then
    pass "boot READY with the vault-index service"
else
    fail "boot with vault-index"
    exit 1
fi
if ! read_token; then
    fail "no access token in $DATA_DIR/credentials.json"
    exit 1
fi

# --- (a) torture fixture ------------------------------------------------------
SEED_OUT="$(helper seed "$WORK_DIR" 2>"$WORK_DIR/seed.err")"
if [ -n "$SEED_OUT" ]; then
    pass "(a) torture fixture seeded via RPC: $SEED_OUT"
else
    fail "(a) RPC seed failed"
    cat "$WORK_DIR/seed.err" >&2
    exit 1
fi
SYNC_T0=$(date +%s)
if wait_indexed "$FIXTURE_SYNC_TIMEOUT" "$N1_FILES"; then
    SYNC_ELAPSED=$(($(date +%s) - SYNC_T0))
    pass "(a) all $N1_FILES files mirrored to disk AND indexed in ${SYNC_ELAPSED}s"
else
    fail "(a) fixture did not finish sync+index within ${FIXTURE_SYNC_TIMEOUT}s"
    exit 1
fi
DOCS_TOTAL="$(status_field docsTotal)"
# ≥ boards×2 (frame + text) per file + color + typography per file
MIN_DOCS=$((N1_FILES * (N1_BOARDS * 2 + 2)))
if [ "$DOCS_TOTAL" -ge "$MIN_DOCS" ]; then
    pass "(a) index holds $DOCS_TOTAL docs (≥ $MIN_DOCS expected for ${N1_FILES}x${N1_BOARDS} + assets)"
else
    fail "(a) docsTotal=$DOCS_TOTAL < expected $MIN_DOCS"
fi

# --- (b) correctness + latency -------------------------------------------------
if QUERIES_OUT="$(helper queries "$WORK_DIR" 2>"$WORK_DIR/queries.err")"; then
    pass "(b) 23/23 queries returned the exact file/page/board/shape ids + verified deep links"
    P50="$(echo "$QUERIES_OUT" | json_field p50Ms)"
    MAXMS="$(echo "$QUERIES_OUT" | json_field maxMs)"
    echo "     latency: $QUERIES_OUT"
    if python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) < 100 else 1)" "$MAXMS"; then
        pass "(b) all queries under 100ms (p50=${P50}ms max=${MAXMS}ms)"
    else
        warn "(b) SOFT LATENCY CRITERION MISSED: max query latency ${MAXMS}ms >= 100ms (p50=${P50}ms) — correctness unaffected, investigate before N3"
    fi
else
    fail "(b) query battery failed: $QUERIES_OUT"
    cat "$WORK_DIR/queries.err" >&2
fi

# --- (c) needle via update-file -------------------------------------------------
NEEDLE_TOKEN="xylophone-n1-needle"
if helper plant_needle "$WORK_DIR" ascii "the $NEEDLE_TOKEN hides in plain sight" >/dev/null 2>"$WORK_DIR/needle.err"; then
    pass "(c) needle planted via update-file RPC in file-000/board-000-00"
else
    fail "(c) plant_needle failed"
    cat "$WORK_DIR/needle.err" >&2
    exit 1
fi
if HIT_OUT="$(helper wait_hit "$WORK_DIR" ascii "$NEEDLE_TOKEN" "$NEEDLE_TIMEOUT" 2>"$WORK_DIR/hit.err")"; then
    E2E_S="${HIT_OUT%% *}"; QLAT_MS="${HIT_OUT##* }"
    pass "(c) needle searchable ${E2E_S}s after update-file (query latency ${QLAT_MS}ms), ids verified"
else
    fail "(c) needle not found within ${NEEDLE_TIMEOUT}s"
    cat "$WORK_DIR/hit.err" >&2
fi

# --- (d) unicode needle ----------------------------------------------------------
if helper plant_needle "$WORK_DIR" unicode "Überschrift Diseño 検索テスト n1-ünïcode" >/dev/null 2>"$WORK_DIR/uneedle.err"; then
    pass "(d) unicode needle planted (diacritics + CJK)"
else
    fail "(d) unicode plant_needle failed"
    cat "$WORK_DIR/uneedle.err" >&2
fi
if UHIT="$(helper wait_hit "$WORK_DIR" unicode "検索テスト" "$NEEDLE_TIMEOUT" 2>"$WORK_DIR/uhit.err")"; then
    pass "(d) CJK query '検索テスト' finds the needle (${UHIT%% *}s)"
else
    fail "(d) CJK needle not found"
    cat "$WORK_DIR/uhit.err" >&2
fi
if UHIT2="$(helper wait_hit "$WORK_DIR" unicode "uberschrift diseno" 10 2>"$WORK_DIR/uhit2.err")"; then
    pass "(d) diacritic-folded query 'uberschrift diseno' matches (unicode61 remove_diacritics)"
else
    fail "(d) diacritic folding broken"
    cat "$WORK_DIR/uhit2.err" >&2
fi

# --- (e) hash-gated no-op: idle + reboot-with-db-kept ------------------------------
MUT_BEFORE="$(status_field mutations)"
sleep 6   # ≥6 poll cycles
MUT_AFTER="$(status_field mutations)"
if [ "$MUT_BEFORE" = "$MUT_AFTER" ]; then
    pass "(e) idle: zero index writes across 6s of poll cycles (mutations=$MUT_AFTER frozen)"
else
    fail "(e) idle churn: mutations moved $MUT_BEFORE -> $MUT_AFTER"
fi
helper snapshot "$WORK_DIR" "$WORK_DIR/snapshot-before.json" >/dev/null || fail "(e) snapshot failed"
if stop_headless; then
    pass "(e) clean SIGTERM stop"
else
    fail "(e) headless did not stop"
    exit 1
fi
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_indexed 120 "$N1_FILES"; then
    MUT_REBOOT="$(status_field mutations)"
    if [ "$MUT_REBOOT" = "0" ]; then
        pass "(e) reboot with db kept: all $N1_FILES files recognized, ZERO reindexes (run-twice hash-gated no-op)"
    else
        fail "(e) reboot reindexed $MUT_REBOOT files although nothing changed"
    fi
else
    fail "(e) reboot with kept db did not settle"
    exit 1
fi
read_token || true

# --- (f) invariant 1: delete the index db + reboot -> identical results -------------
if stop_headless; then
    pass "(f) stopped for index-db deletion"
else
    fail "(f) stop failed"
    exit 1
fi
rm -f "$INDEX_DB" "$INDEX_DB-wal" "$INDEX_DB-shm"
if [ ! -f "$INDEX_DB" ]; then
    pass "(f) index db deleted ($INDEX_DB + WAL sidecars)"
fi
: >"$LOG"
start_headless
REBUILD_T0=$(date +%s)
if wait_ready "$REBOOT_TIMEOUT" && wait_indexed 300 "$N1_FILES"; then
    REBUILD_S=$(($(date +%s) - REBUILD_T0))
    MUT_REBUILD="$(status_field mutations)"
    if [ "$MUT_REBUILD" -ge "$N1_FILES" ]; then
        pass "(f) full rebuild from disk alone: $MUT_REBUILD reindexes, settled ${REBUILD_S}s after boot"
    else
        fail "(f) expected >= $N1_FILES reindexes after db deletion, saw $MUT_REBUILD"
    fi
else
    fail "(f) rebuild did not settle"
    exit 1
fi
read_token || true
helper snapshot "$WORK_DIR" "$WORK_DIR/snapshot-after.json" >/dev/null || fail "(f) snapshot failed"
if cmp -s "$WORK_DIR/snapshot-before.json" "$WORK_DIR/snapshot-after.json"; then
    pass "(f) rebuilt index returns BYTE-IDENTICAL results (9-query snapshot incl. ranks/snippets)"
else
    fail "(f) rebuilt results differ from the pre-deletion index:"
    diff "$WORK_DIR/snapshot-before.json" "$WORK_DIR/snapshot-after.json" | head -20 >&2
fi

# --- (g) rename removes the stale hit ------------------------------------------------
if helper rename_needle "$WORK_DIR" ascii "now it says trombone-n1 instead" >/dev/null 2>"$WORK_DIR/rename.err"; then
    pass "(g) needle text rewritten via update-file (mod-obj set content)"
else
    fail "(g) rename_needle failed"
    cat "$WORK_DIR/rename.err" >&2
fi
if GONE_S="$(helper wait_no_hit "$WORK_DIR" "$NEEDLE_TOKEN" "$NEEDLE_TIMEOUT" 2>"$WORK_DIR/gone.err")"; then
    pass "(g) stale hit gone ${GONE_S}s after the edit (within the sync+debounce window)"
else
    fail "(g) stale hit still present after ${NEEDLE_TIMEOUT}s"
    cat "$WORK_DIR/gone.err" >&2
fi
if HIT2="$(helper wait_hit "$WORK_DIR" ascii "trombone-n1" 30 2>"$WORK_DIR/hit2.err")"; then
    pass "(g) the rewritten text is searchable (${HIT2%% *}s) — rows swapped, not just dropped"
else
    fail "(g) rewritten text not searchable"
    cat "$WORK_DIR/hit2.err" >&2
fi

# --- (h) /__search page + API validation ----------------------------------------------
if curl -fsS "$BASE/__search" | grep -q "Vault search"; then
    pass "(h) /__search page served same-origin (plain HTML/vanilla JS)"
else
    fail "(h) /__search page missing"
fi
HTTP_CODE="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/__api/vault/search")"
if [ "$HTTP_CODE" = "400" ]; then
    pass "(h) search without q answers 400 (no FTS syntax leakage)"
else
    fail "(h) expected 400 for missing q, got $HTTP_CODE"
fi

# --- shutdown ---------------------------------------------------------------------------
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
STACK_PIDS="$(pgrep -f "$DATA_DIR" || true)"
if stop_headless; then
    ORPHANS=""
    for pid in $JAVA_PIDS $STACK_PIDS; do
        kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"
    done
    if [ -z "$ORPHANS" ]; then
        pass "clean shutdown, no orphan processes"
    else
        fail "orphan processes left behind:$ORPHANS"
        ps -o pid,command -p ${ORPHANS} >&2 || true
    fi
else
    fail "headless did not exit within 25s of SIGTERM"
fi
sleep 1
if ports_all_free; then
    pass "all 4 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT)"
else
    fail "ports still busy after clean shutdown"
fi

echo
echo "headline: fixture ${N1_FILES}x${N1_BOARDS} sync+index ${SYNC_ELAPSED:-?}s ; query p50 ${P50:-?}ms max ${MAXMS:-?}ms ; needle e2e ${E2E_S:-?}s ; rebuild ${REBUILD_S:-?}s ; rename-stale-gone ${GONE_S:-?}s"
if [ "$FAILURES" -eq 0 ]; then
    echo "N1 INDEX: ALL PASS"
    exit 0
else
    echo "N1 INDEX: $FAILURES FAILURE(S)"
    exit 1
fi
