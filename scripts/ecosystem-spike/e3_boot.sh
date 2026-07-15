#!/usr/bin/env bash
# E3 de-risk spike boot harness (THROWAWAY). Boots a dedicated headless stack,
# runs the author probe, wipes the DB + reboots, runs the rebuild probe, tears
# down. Ports: proxy 8974 backend 6435 pg 5509 valkey 6452 (renders OFF).
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT=8974 BACKEND_PORT=6435 POSTGRES_PORT=5509 VALKEY_PORT=6452
FIRST_BOOT_TIMEOUT=900 REBOOT_TIMEOUT=600
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e3-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
PROBE="$ROOT/scripts/ecosystem-spike/e3_probe.py"
HEADLESS_PID=""
FAIL=0
PG_CACHE="${E2_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

export PENPOT_BACKEND="$BACKEND" PENPOT_FRONTEND="$BASE"

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}
wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:"; tail -25 "$LOG"; return 1; }
        sleep 2
    done
    echo "timed out waiting for READY"; return 1
}
stop_headless() {
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || { HEADLESS_PID=""; return 0; }; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""; return 0
}
read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN; [ -n "$PENPOT_TOKEN" ]
}
cleanup() {
    [ -n "$HEADLESS_PID" ] && kill -9 "$HEADLESS_PID" 2>/dev/null
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    echo "kept: data=$DATA_DIR vault=$VAULT work=$WORK_DIR log=$LOG"
}
trap cleanup EXIT

echo "== E3 spike boot (proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT) =="
echo "   work=$WORK_DIR"
[ -x "$BIN" ] || { echo "building headless..."; (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog) || exit 1; }
[ -d "$PG_CACHE" ] && { mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; echo "   (seeded pg cache)"; }

start_headless
wait_ready "$FIRST_BOOT_TIMEOUT" || exit 1
echo "READY (boot 1)"
read_token || { echo "no token"; exit 1; }

echo "== PHASE author =="
python3 "$PROBE" author "$WORK_DIR" >"$WORK_DIR/author.out" 2>&1 || { FAIL=1; echo "author FAILED"; }
cat "$WORK_DIR/author.out"

echo "== delete-DB + reboot (invariant 1) =="
stop_headless
rm -rf "$DATA_DIR/postgres"
: >"$LOG"
[ -d "$PG_CACHE" ] && { mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; }
start_headless
wait_ready "$REBOOT_TIMEOUT" || exit 1
echo "READY (boot 2, DB wiped)"
read_token || { echo "no token after reboot"; exit 1; }

echo "== PHASE rebuild =="
python3 "$PROBE" rebuild "$WORK_DIR" >"$WORK_DIR/rebuild.out" 2>&1 || { FAIL=1; echo "rebuild FAILED"; }
cat "$WORK_DIR/rebuild.out"

stop_headless
echo "== done (FAIL=$FAIL) =="
exit "$FAIL"
