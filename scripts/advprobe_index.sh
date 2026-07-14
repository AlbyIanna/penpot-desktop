#!/usr/bin/env bash
# INDEPENDENT adversarial probes for the vault index (N1), index-only stack.
#   - FTS hostility: quotes/hyphens/CJK/emoji/boolean-words/fts-syntax/SQLish/
#     control chars/very-long strings + a ~10MB text layer.
#   - Index vs sync race (PLAN2.md risk 6): plant a needle + rename the file
#     dir in the same sync window; rapid-fire 10 edits -> converge with zero
#     stale hits and zero missed hits, stable deepLink file-id.
# Dedicated ports (mine): proxy 8920 backend 6395 postgres 5469 valkey 6412.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
PROXY_PORT="${N1_PROXY_PORT:-8920}"; BACKEND_PORT="${N1_BACKEND_PORT:-6395}"
POSTGRES_PORT="${N1_POSTGRES_PORT:-5469}"; VALKEY_PORT="${N1_VALKEY_PORT:-6412}"
BASE="http://localhost:${PROXY_PORT}"
DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-idx-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-idx-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-idx-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"; BIN="$ROOT/target/debug/headless"
HEADLESS_PID=""; FAILURES=0
export N1_FILES=2 N1_BOARDS=2 N1_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE"
PG_CACHE="$HOME/.cache/penpot-local/pg-install"
pass(){ echo "PASS: $1"; }; fail(){ echo "FAIL: $1"; FAILURES=$((FAILURES+1)); }
json_field(){ python3 -c "import json,sys;print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
strip_ansi(){ sed -E $'s/\x1b\\[[0-9;]*m//g'; }
cleanup(){
  [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null && {
    kill -TERM "$HEADLESS_PID" 2>/dev/null
    for _ in $(seq 1 20); do kill -0 "$HEADLESS_PID" 2>/dev/null||break; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; }
  pkill -9 -f "$DATA_DIR" 2>/dev/null
  [ "$FAILURES" -eq 0 ] && rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR" || echo "kept: $DATA_DIR $DESIGNS_DIR $LOG"
}
trap cleanup EXIT
start(){ env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
  PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
  PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
  "$BIN" >>"$LOG" 2>&1 & HEADLESS_PID=$!; }
wait_ready(){ local d=$(($(date +%s)+$1)); while [ "$(date +%s)" -lt "$d" ]; do
  grep -q "^READY " "$LOG" 2>/dev/null && return 0
  kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "died"; tail -20 "$LOG">&2; return 1; }; sleep 2; done; return 1; }
read_token(){ PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null||true)"; export PENPOT_TOKEN; [ -n "$PENPOT_TOKEN" ]; }
wait_indexed(){ local d=$(($(date +%s)+$1)) g; while [ "$(date +%s)" -lt "$d" ]; do
  g="$(curl -fsS "$BASE/__api/vault/status" 2>/dev/null|json_field filesIndexed 2>/dev/null||echo '?')"
  [ "$g" = "$2" ] && return 0; sleep 2; done; echo "idx timeout got=$g want=$2">&2; return 1; }

echo "== adv index probes: proxy=$BASE data=$DATA_DIR"
(cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog) || { fail build; exit 1; }
[ -d "$PG_CACHE" ] && { mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; }
start
wait_ready 900 && strip_ansi <"$LOG"|grep -q "vault-index service started" && pass "boot + vault-index" || { fail boot; exit 1; }
read_token || { fail token; exit 1; }
python3 "$ROOT/scripts/n1_index_helper.py" seed "$WORK_DIR" >/dev/null 2>"$WORK_DIR/seed.err" && pass "seed 2x2 fixture" || { fail seed; cat "$WORK_DIR/seed.err">&2; exit 1; }
wait_indexed 120 2 && pass "fixture indexed" || { fail "fixture index"; exit 1; }

echo "--- PROBE 1: FTS hostility ---"
if OUT="$(python3 "$ROOT/scripts/adv_helper.py" fts_hostility "$WORK_DIR" 2>"$WORK_DIR/fts.err")"; then
  pass "FTS hostility: every marker findable, no search crash"; echo "$OUT"
else
  fail "FTS hostility"; echo "$OUT"; cat "$WORK_DIR/fts.err">&2
fi

echo "--- PROBE 2: 10MB text layer ---"
if OUT="$(python3 "$ROOT/scripts/adv_helper.py" bigtext "$WORK_DIR" 2>"$WORK_DIR/big.err")"; then
  pass "10MB text layer indexed + marker findable"; echo "$OUT"
else
  fail "10MB text layer"; echo "$OUT"; cat "$WORK_DIR/big.err">&2
fi

echo "--- PROBE 3: index vs sync race (rename + 10 rapid edits) ---"
if OUT="$(python3 "$ROOT/scripts/adv_helper.py" race "$WORK_DIR" 2>"$WORK_DIR/race.err")"; then
  pass "race converged: zero stale hits, stable deepLink file-id"; echo "$OUT"
else
  fail "index/sync race left stale or missed hits"; echo "$OUT"; cat "$WORK_DIR/race.err">&2
fi

# index health after the storm: idle cycles must be write-free
M1="$(curl -fsS "$BASE/__api/vault/status"|json_field mutations)"; sleep 6
M2="$(curl -fsS "$BASE/__api/vault/status"|json_field mutations)"
[ "$M1" = "$M2" ] && pass "post-storm idle: zero index writes (mutations=$M2 frozen)" || fail "post-storm idle churn $M1->$M2"

kill -TERM "$HEADLESS_PID" 2>/dev/null
for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null||{ HEADLESS_PID=""; break; }; sleep 1; done
echo
[ "$FAILURES" -eq 0 ] && { echo "ADV INDEX: ALL PASS"; exit 0; } || { echo "ADV INDEX: $FAILURES FAILURE(S)"; exit 1; }
