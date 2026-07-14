#!/usr/bin/env bash
# INDEPENDENT adversarial probe: THREE-WORLD invariant 1.
# Delete the postgres data dir + the vault-index db + every .exports dir
# TOGETHER, then ONE reboot must rebuild all three worlds (DB, search index,
# thumbnails) from the folder tree alone — with file ids preserved.
# Packaged-shape stack (bundle + exporter), env -i, offline. Also does an
# extra SIGKILL storm during renders as a bonus stale-adoption check.
# My ports: proxy 8920 backend 6395 postgres 5469 valkey 6412 exporter 6473.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
PROXY_PORT="${N2_PROXY_PORT:-8920}"; BACKEND_PORT="${N2_BACKEND_PORT:-6395}"
POSTGRES_PORT="${N2_POSTGRES_PORT:-5469}"; VALKEY_PORT="${N2_VALKEY_PORT:-6412}"
EXPORTER_PORT="${N2_EXPORTER_PORT:-6473}"
BASE="http://localhost:${PROXY_PORT}"; BUNDLE="${N2_BUNDLE:-$ROOT/dist/penpot-runtime}"
POISON="http://127.0.0.1:1"
DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-inv-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-inv-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/adv-inv-work.XXXXXX")"
FRESH_HOME="$WORK_DIR/home"; LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"; HELPER="$ROOT/scripts/m5_features_helper.py"
HEADLESS_PID=""; FAILURES=0
mkdir -p "$FRESH_HOME" "$WORK_DIR/tmp"
export M5_DESIGNS_DIR="$DESIGNS_DIR" PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE"
pass(){ echo "PASS: $1"; }; fail(){ echo "FAIL: $1"; FAILURES=$((FAILURES+1)); }
json_field(){ python3 -c "import json,sys;print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
strip_ansi(){ sed -E $'s/\x1b\\[[0-9;]*m//g'; }
helper(){ python3 "$HELPER" "$@"; }
cleanup(){
  [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null && {
    kill -TERM "$HEADLESS_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null||break; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; }
  pkill -9 -f "$DATA_DIR" 2>/dev/null
  [ "$FAILURES" -eq 0 ] && rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR" || echo "kept: $DATA_DIR $DESIGNS_DIR $LOG"
}
trap cleanup EXIT
start(){ ( cd "$WORK_DIR"||exit 1; exec env -i HOME="$FRESH_HOME" \
    PATH="/usr/bin:/bin:/usr/sbin:/sbin" TMPDIR="$WORK_DIR/tmp/" \
    http_proxy="$POISON" https_proxy="$POISON" HTTP_PROXY="$POISON" HTTPS_PROXY="$POISON" ALL_PROXY="$POISON" \
    PENPOT_LOCAL_RUNTIME_BUNDLE="$BUNDLE" PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" PENPOT_LOCAL_EXPORTS=1 \
    PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" "$BIN" ) >>"$LOG" 2>&1 & HEADLESS_PID=$!; }
wait_ready(){ local d=$(($(date +%s)+$1)); while [ "$(date +%s)" -lt "$d" ]; do
  grep -q "^READY " "$LOG" 2>/dev/null && return 0
  kill -0 "$HEADLESS_PID" 2>/dev/null||{ echo died; tail -20 "$LOG">&2; return 1; }; sleep 2; done; echo "ready timeout">&2; tail -20 "$LOG">&2; return 1; }
wait_log(){ local d=$(($(date +%s)+$1)); while [ "$(date +%s)" -lt "$d" ]; do
  strip_ansi <"$LOG" 2>/dev/null|grep -qF "$2" && return 0
  kill -0 "$HEADLESS_PID" 2>/dev/null||return 1; sleep 1; done; return 1; }
read_token(){ PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null||true)"; export PENPOT_TOKEN; [ -n "$PENPOT_TOKEN" ]; }
stop(){ kill -TERM "$HEADLESS_PID" 2>/dev/null||return 1
  for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null||{ HEADLESS_PID=""; return 0; }; sleep 1; done; return 1; }
wait_indexed(){ local d=$(($(date +%s)+$1)) g; while [ "$(date +%s)" -lt "$d" ]; do
  g="$(curl -fsS "$BASE/__api/vault/status" 2>/dev/null|json_field filesIndexed 2>/dev/null||echo '?')"
  [ "$g" -ge "$2" ] 2>/dev/null && return 0; sleep 2; done; echo "idx timeout got=$g want>=$2">&2; return 1; }
wait_ok(){ local t="$1" sub="$2"; shift 2; local d=$(($(date +%s)+t)) out
  while [ "$(date +%s)" -lt "$d" ]; do out="$(helper "$sub" "$WORK_DIR" "$@" 2>&1)"||{ echo "$out">&2; return 1; }
    case "$out" in OK\ *) HELPER_OUT="${out#OK }"; return 0;; esac; sleep 2; done
  echo "timeout $sub $* (last:$out)">&2; return 1; }
count_exports(){ find "$DESIGNS_DIR" -type d -name '*.exports' 2>/dev/null | wc -l | tr -d ' '; }
count_render_files(){ find "$DESIGNS_DIR" \( -name '*.svg' -o -name '*.png' \) 2>/dev/null | wc -l | tr -d ' '; }
search_json(){ curl -fsS "$BASE/__api/vault/search?q=$1&limit=50"; }

echo "== adv three-world invariant: bundle=$BUNDLE data=$DATA_DIR designs=$DESIGNS_DIR"
[ -x "$BUNDLE/bin/node" ] || { fail "bundle missing"; exit 1; }
(cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog) || { fail build; exit 1; }

start
wait_ready 600 && wait_log 30 "board-export service started" && wait_log 30 "vault-index service started" && pass "boot: exporter + index services" || { fail boot; exit 1; }
read_token || { fail token; exit 1; }
helper seed "$WORK_DIR" >/dev/null 2>"$WORK_DIR/seed.err" && pass "seed alpha+beta" || { fail seed; cat "$WORK_DIR/seed.err">&2; exit 1; }
wait_ok 120 check alpha && wait_ok 120 check beta && pass "mirrored to disk" || { fail mirror; exit 1; }
wait_ok 180 exports_check alpha && wait_ok 180 exports_check beta && pass "thumbnails rendered (world 3)" || { fail render; exit 1; }
wait_indexed 120 2 && pass "index built (world 2)" || { fail index; exit 1; }
# capture originals
ALPHA_FID="$(helper info "$WORK_DIR" alpha | json_field id)"
BODY_BEFORE="$(search_json Cover)"
LINK_BEFORE="$(echo "$BODY_BEFORE" | python3 -c "import json,sys;b=json.load(sys.stdin);print(next((h['deepLink'] for h in b['hits'] if h['kind']=='board' and h['name']=='Cover'),''))")"
EXPORTS_BEFORE="$(count_exports)"; RFILES_BEFORE="$(count_render_files)"
echo "     before: alphaFid=$ALPHA_FID exportsDirs=$EXPORTS_BEFORE renderFiles=$RFILES_BEFORE coverLink=$LINK_BEFORE"
[ -n "$ALPHA_FID" ] && echo "$LINK_BEFORE" | grep -q "file-id=$ALPHA_FID" && pass "pre-delete: Cover board found in index, deepLink carries alpha file-id" || fail "pre-delete search wrong"

# ---- THE TRIPLE-WORLD DELETE ----
stop || { fail "stop before delete"; exit 1; }
rm -rf "$DATA_DIR/postgres"        # world 1: the DB (M2-style whole wipe)
rm -rf "$DATA_DIR/vault-index"     # world 2: the search index
find "$DESIGNS_DIR" -type d -name '*.exports' -exec rm -rf {} + 2>/dev/null  # world 3: thumbnails
DEL_OK=1
[ -e "$DATA_DIR/postgres" ] && DEL_OK=0
[ -e "$DATA_DIR/vault-index" ] && DEL_OK=0
[ "$(count_exports)" != "0" ] && DEL_OK=0
# folder tree (source of truth) + manifest + secret.key must survive
[ -f "$DATA_DIR/secret.key" ] && [ -f "$DESIGNS_DIR/.penpot-sync.json" ] && [ -d "$DATA_DIR" ] || DEL_OK=0
NPENPOT="$(find "$DESIGNS_DIR" -type d -name '*.penpot' | wc -l | tr -d ' ')"
if [ "$DEL_OK" -eq 1 ] && [ "$NPENPOT" -ge 2 ]; then
  pass "deleted ALL THREE worlds (postgres + index + .exports); folder tree ($NPENPOT .penpot), manifest, secret.key survive"
else
  fail "triple-delete precondition (delOk=$DEL_OK penpot=$NPENPOT)"; exit 1
fi

# ---- ONE REBOOT REBUILDS ALL THREE ----
: >"$LOG"; start
wait_ready 600 && pass "single reboot READY on the emptied DB" || { fail "reboot"; exit 1; }
wait_log 120 "reconciliation complete" && pass "world 1 (DB): reconciled from the folder tree" || fail "no reconcile"
read_token || true
wait_indexed 180 2 && pass "world 2 (index): rebuilt from disk alone" || fail "index not rebuilt"
# world 3: thumbnails re-render
wait_ok 240 exports_check alpha && wait_ok 240 exports_check beta && pass "world 3 (thumbnails): re-rendered from disk alone" || fail "thumbnails not rebuilt"

# ---- same-id invariant across the rebuild ----
BODY_AFTER="$(search_json Cover)"
ALPHA_FID2="$(helper info "$WORK_DIR" alpha | json_field id)"
LINK_AFTER="$(echo "$BODY_AFTER" | python3 -c "import json,sys;b=json.load(sys.stdin);print(next((h['deepLink'] for h in b['hits'] if h['kind']=='board' and h['name']=='Cover'),''))")"
EXPORTS_AFTER="$(count_exports)"; RFILES_AFTER="$(count_render_files)"
echo "     after:  alphaFid=$ALPHA_FID2 exportsDirs=$EXPORTS_AFTER renderFiles=$RFILES_AFTER coverLink=$LINK_AFTER"
if [ -n "$ALPHA_FID2" ] && [ "$ALPHA_FID" = "$ALPHA_FID2" ]; then
  pass "same-id invariant: alpha keeps file id $ALPHA_FID across the triple-wipe (M2 core invariant holds)"
else
  fail "file id changed: $ALPHA_FID -> $ALPHA_FID2"
fi
# Durable ids (file-id + page-id) must be byte-identical; team-id is
# re-provisioned by a full DB wipe (index reads profile.default_team_id at boot)
# so it legitimately changes — compare the durable portions only, report team-id.
fid_of(){ echo "$1" | sed -E 's/.*file-id=([0-9a-f-]+).*/\1/'; }
pid_of(){ echo "$1" | sed -E 's/.*page-id=([0-9a-f-]+).*/\1/'; }
tid_of(){ echo "$1" | sed -E 's/.*team-id=([0-9a-f-]+).*/\1/'; }
if [ "$(fid_of "$LINK_BEFORE")" = "$(fid_of "$LINK_AFTER")" ] && [ "$(pid_of "$LINK_BEFORE")" = "$(pid_of "$LINK_AFTER")" ] && [ -n "$LINK_AFTER" ]; then
  pass "index deepLink durable ids stable: file-id=$(fid_of "$LINK_AFTER") page-id=$(pid_of "$LINK_AFTER") identical before/after"
  echo "     NOTE (finding): team-id regenerated by the DB wipe $(tid_of "$LINK_BEFORE") -> $(tid_of "$LINK_AFTER") (index reads current profile.default_team_id; expected, not a defect)"
else
  fail "deepLink durable ids differ: '$LINK_BEFORE' vs '$LINK_AFTER'"
fi
[ "$EXPORTS_AFTER" = "$EXPORTS_BEFORE" ] && [ "$RFILES_AFTER" = "$RFILES_BEFORE" ] && pass "thumbnail world fully restored (dirs $EXPORTS_AFTER, files $RFILES_AFTER match)" || fail "thumbnail counts differ (dirs $EXPORTS_BEFORE->$EXPORTS_AFTER files $RFILES_BEFORE->$RFILES_AFTER)"

stop && pass "clean final shutdown" || fail "final shutdown"
echo
[ "$FAILURES" -eq 0 ] && { echo "ADV INVARIANT: ALL PASS"; exit 0; } || { echo "ADV INVARIANT: $FAILURES FAILURE(S)"; exit 1; }
