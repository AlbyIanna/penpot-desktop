#!/usr/bin/env bash
# N3 lighttable-home gate (PLAN2.md milestone N3, `just n3`).
#
# Exit criteria implemented verbatim, each a PASS/FAIL block in the house style
# of n1-index.sh / m5-features.sh:
#   (a) torture fixture: the shared N1 seeder plants 100 files / ~1000 boards
#       across 4 projects, mirrored to disk + indexed.
#   (b) /__home lists EVERY fixture board; each card's href is the exact
#       /#/workspace?team-id&file-id&page-id deep link, string-asserted from
#       the /__api/vault/boards payload; projects + counts correct; grid-load
#       time for the full fixture reported.
#   (c) project filter + recency/name sort behave.
#   (d) thumbnails: a board with no render is DEGRADED (thumb null, 404); a
#       planted N2-shape render surfaces a thumb URL AND the thumb route serves
#       the exact PNG bytes (degraded mode + real render both exercised).
#   (e) routes-gate.sh live leg: a headless browser clicks a card and asserts
#       the landed /#/workspace deep link, then the escape hatch → /#/dashboard/
#       recent (this IS scripts/routes-gate.sh; called against this stack).
#   (f) an update-file RPC edit updates the CARD (new board name in /boards) AND
#       the STRIP (lastSyncAt advances) within the poll+debounce window;
#       latencies reported.
#   (g) the strip renders a Conflict{copy_path} state FIRST-CLASS with a working
#       reveal-both-versions action; driven off MockStatusSource windowless
#       (PENPOT_LOCAL_TRAY_DEMO=1) so CI exercises it deterministically; reveal
#       accepts the in-vault copy path (200) and rejects traversal (400).
#   (h) /__home is served same-origin and /__bootstrap now lands the webview on
#       it (Location: /__home).
#
# Dedicated ports (ledger): proxy 8940, backend 6401, postgres 5475, valkey
# 6418 (exporter 6481 reserved; this gate keeps renders OFF and proves the
# thumbnail path with a planted render, so the full 1000-board fixture stays
# fast). Fresh mktemp dirs; ANSI-stripped log greps; pg-install cache seeded;
# dirs kept on failure.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, python3, curl, node + the bundled playwright/chromium
# (scripts/fetch-penpot.sh --with-browsers) for the routes-gate leg.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N3_PROXY_PORT:-8940}"
BACKEND_PORT="${N3_BACKEND_PORT:-6401}"
POSTGRES_PORT="${N3_POSTGRES_PORT:-5475}"
VALKEY_PORT="${N3_VALKEY_PORT:-6418}"
FIRST_BOOT_TIMEOUT="${N3_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
FIXTURE_SYNC_TIMEOUT="${N3_FIXTURE_SYNC_TIMEOUT:-600}"
EDIT_TIMEOUT=120
BASE="http://localhost:${PROXY_PORT}"

export N1_FILES="${N1_FILES:-100}"
export N1_BOARDS="${N1_BOARDS:-10}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n3-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n3-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n3-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
N1HELPER="$ROOT/scripts/n1_index_helper.py"
N3HELPER="$ROOT/scripts/n3_home_helper.py"
HEADLESS_PID=""
FAILURES=0

export N1_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
warn() { echo "WARNING: $1"; }

PG_CACHE="${N3_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
n1helper() { python3 "$N1HELPER" "$@"; }
n3helper() { python3 "$N3HELPER" "$@"; }
index_status() { curl -fsS "$BASE/__api/vault/status" 2>/dev/null; }
status_field() { index_status | json_field "$1"; }

start_headless() { # start_headless [extra env KEY=VAL ...]
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$@" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died; last log lines:" >&2; tail -20 "$LOG" >&2; return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2; return 1
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

wait_indexed() { # wait_indexed <timeout-s> <expected-files>
    local deadline=$(($(date +%s) + $1)) got=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        got="$(status_field filesIndexed 2>/dev/null || echo '?')"
        [ "$got" = "$2" ] && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died waiting for filesIndexed=$2" >&2; tail -20 "$LOG" >&2; return 1; }
        sleep 2
    done
    echo "timed out: filesIndexed=$got want $2 ($1s)" >&2; return 1
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2; lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true; return 1
        fi
    done
    return 0
}

echo "== N3 home: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE fixture=${N1_FILES}x${N1_BOARDS}"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- boot ------------------------------------------------------------------
if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" && strip_ansi <"$LOG" | grep -q "vault-index service started"; then
    pass "boot READY with the vault-index service"
else
    fail "boot"; exit 1
fi
read_token || { fail "no access token"; exit 1; }

# --- (a) torture fixture ---------------------------------------------------
SEED_OUT="$(n1helper seed "$WORK_DIR" 2>"$WORK_DIR/seed.err")"
if [ -n "$SEED_OUT" ]; then
    pass "(a) torture fixture seeded: $SEED_OUT"
else
    fail "(a) seed failed"; cat "$WORK_DIR/seed.err" >&2; exit 1
fi
SYNC_T0=$(date +%s)
if wait_indexed "$FIXTURE_SYNC_TIMEOUT" "$N1_FILES"; then
    SYNC_ELAPSED=$(($(date +%s) - SYNC_T0))
    pass "(a) all $N1_FILES files mirrored + indexed in ${SYNC_ELAPSED}s"
else
    fail "(a) fixture did not finish sync+index"; exit 1
fi

# --- (b) board grid + exact deep links -------------------------------------
if BOARDS_OUT="$(n3helper assert_boards "$WORK_DIR" 2>"$WORK_DIR/boards.err")"; then
    GRID_MS="$(echo "$BOARDS_OUT" | json_field gridLoadMs)"
    BOARD_COUNT="$(echo "$BOARDS_OUT" | json_field count)"
    pass "(b) /__home lists all $BOARD_COUNT boards; every card href string-asserted as the exact /#/workspace deep link"
    echo "     grid load: full-fixture /__api/vault/boards in ${GRID_MS}ms ($BOARD_COUNT boards)"
else
    fail "(b) boards listing wrong"; cat "$WORK_DIR/boards.err" >&2
fi

# --- (c) sort + filter -----------------------------------------------------
if n3helper assert_sort "$WORK_DIR" >/dev/null 2>"$WORK_DIR/sort.err"; then
    pass "(c) recency sort newest-first + name sort A→Z"
else
    fail "(c) sort wrong"; cat "$WORK_DIR/sort.err" >&2
fi
if FOUT="$(n3helper assert_filter "$WORK_DIR" "Torture-1" 2>"$WORK_DIR/filter.err")"; then
    pass "(c) project filter: $FOUT"
else
    fail "(c) filter wrong"; cat "$WORK_DIR/filter.err" >&2
fi

# --- (d) thumbnails: degraded + a planted real render ----------------------
PLANT_OUT="$(n3helper plant_thumb "$WORK_DIR" 2>"$WORK_DIR/plant.err")"
if [ -n "$PLANT_OUT" ]; then
    pass "(d) planted an N2-shape render on disk: $PLANT_OUT"
else
    fail "(d) plant_thumb failed"; cat "$WORK_DIR/plant.err" >&2
fi
if THUMB_OUT="$(n3helper assert_thumb "$WORK_DIR" 2>"$WORK_DIR/thumb.err")"; then
    pass "(d) $THUMB_OUT — real render served; boards with no render stay degraded (404)"
else
    fail "(d) thumbnail path broken"; cat "$WORK_DIR/thumb.err" >&2
fi

# --- (e) routes-gate: live headless-browser navigation ---------------------
if ROUTES_GATE_BASE="$BASE" bash "$ROOT/scripts/routes-gate.sh" >"$WORK_DIR/routes-gate.log" 2>&1; then
    pass "(e) routes-gate.sh live nav: card→/#/workspace + escape→/#/dashboard/recent"
    grep -E "^PASS: \(live\)|^   nav:|browser=" "$WORK_DIR/routes-gate.log" | sed 's/^/     /'
else
    fail "(e) routes-gate.sh failed"; sed 's/^/     /' "$WORK_DIR/routes-gate.log" >&2
fi

# --- (f) an edit updates the card AND the strip ----------------------------
STRIP_BEFORE="$(n3helper strip_last_sync "$WORK_DIR" 2>/dev/null || echo null)"
NEWNAME="n3-renamed-$(date +%s)"
if n3helper edit_board "$WORK_DIR" "$NEWNAME" >/dev/null 2>"$WORK_DIR/edit.err"; then
    pass "(f) board renamed via update-file RPC (mod-obj set name → $NEWNAME)"
else
    fail "(f) edit_board failed"; cat "$WORK_DIR/edit.err" >&2
fi
if CARD_LAT="$(n3helper wait_card "$WORK_DIR" "$NEWNAME" "$EDIT_TIMEOUT" 2>"$WORK_DIR/card.err")"; then
    pass "(f) CARD updated: new board name in /__api/vault/boards ${CARD_LAT}s after the edit"
else
    fail "(f) card never updated"; cat "$WORK_DIR/card.err" >&2
fi
if STRIP_LAT="$(n3helper wait_strip_advance "$WORK_DIR" "$STRIP_BEFORE" "$EDIT_TIMEOUT" 2>"$WORK_DIR/strip.err")"; then
    pass "(f) STRIP updated: lastSyncAt advanced ${STRIP_LAT}s after the edit (real sync-daemon feed)"
else
    fail "(f) strip never advanced"; cat "$WORK_DIR/strip.err" >&2
fi

# --- (h) /__home served + bootstrap lands on it ----------------------------
if curl -fsS "$BASE/__home" | grep -q 'board-card\|id="grid"'; then
    pass "(h) /__home served same-origin (plain HTML/vanilla JS grid page)"
else
    fail "(h) /__home missing"
fi
LOC="$(curl -sI "$BASE/__bootstrap" | strip_ansi | tr -d '\r' | awk 'tolower($1)=="location:"{print $2}')"
if [ "$LOC" = "/__home" ]; then
    pass "(h) /__bootstrap lands the webview on /__home (Location: /__home)"
else
    fail "(h) /__bootstrap Location was '$LOC', expected /__home"
fi

# --- (g) conflict strip + reveal, driven off MockStatusSource --------------
# Reboot with the strip fed by the mock (drivable windowless) so the Conflict
# state + reveal action are exercised deterministically.
if stop_headless; then
    pass "(g) stopped main leg for the mock-driven strip leg"
else
    fail "(g) stop failed"; exit 1
fi
: >"$LOG"
start_headless PENPOT_LOCAL_TRAY_DEMO=1
if wait_ready "$REBOOT_TIMEOUT" && strip_ansi <"$LOG" | grep -q "serving MockStatusSource demo frames"; then
    pass "(g) reboot: strip SSE running off MockStatusSource (PENPOT_LOCAL_TRAY_DEMO)"
else
    fail "(g) mock strip not engaged"; tail -20 "$LOG" >&2; exit 1
fi
if COPY_PATH="$(n3helper wait_strip_conflict "$WORK_DIR" 40 2>"$WORK_DIR/conflict.err")"; then
    pass "(g) Conflict{copy_path} rendered first-class in the strip: $COPY_PATH"
else
    fail "(g) conflict never appeared in the strip"; cat "$WORK_DIR/conflict.err" >&2
fi
if [ -n "${COPY_PATH:-}" ]; then
    REV="$(n3helper reveal "$WORK_DIR" "$COPY_PATH" 2>/dev/null)"
    if echo "$REV" | grep -q "^200 True"; then
        pass "(g) reveal-both-versions action works: reveal($COPY_PATH) → $REV"
    else
        fail "(g) reveal of the conflict copy failed: $REV"
    fi
fi
REV_BAD="$(n3helper reveal "$WORK_DIR" "../../etc/passwd" 2>/dev/null)"
if echo "$REV_BAD" | grep -q "^400 "; then
    pass "(g) reveal rejects path traversal (../../etc/passwd → 400)"
else
    fail "(g) reveal did not reject traversal: $REV_BAD"
fi

# --- shutdown --------------------------------------------------------------
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
STACK_PIDS="$(pgrep -f "$DATA_DIR" || true)"
if stop_headless; then
    ORPHANS=""
    for pid in $JAVA_PIDS $STACK_PIDS; do kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"; done
    if [ -z "$ORPHANS" ]; then pass "clean shutdown, no orphan processes"; else fail "orphans left:$ORPHANS"; fi
else
    fail "headless did not exit within 25s of SIGTERM"
fi
sleep 1
if ports_all_free; then
    pass "all 4 ports freed"
else
    fail "ports still busy after shutdown"
fi

echo
echo "headline: fixture ${N1_FILES}x${N1_BOARDS} ; grid-load ${GRID_MS:-?}ms/${BOARD_COUNT:-?} boards ; edit→card ${CARD_LAT:-?}s ; edit→strip ${STRIP_LAT:-?}s"
if [ "$FAILURES" -eq 0 ]; then
    echo "N3 HOME: ALL PASS"
    exit 0
else
    echo "N3 HOME: $FAILURES FAILURE(S)"
    exit 1
fi
