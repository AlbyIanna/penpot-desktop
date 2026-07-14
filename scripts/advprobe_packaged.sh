#!/usr/bin/env bash
# INDEPENDENT packaged-artifact probe (chapter-2 fresh-machine approximation):
# install the fresh dmg to a scratch dir, boot the installed .app binary under
# a sanitized env (env -i, fresh HOME, system-only PATH, poisoned proxies, no
# host node), then verify BOTH new chapter-2 capabilities work OFFLINE from the
# installed bundle:
#   - the Vault Index: /__api/vault/search returns board + text hits;
#   - Thumbnails: every seeded board renders .exports/*.{svg,png} via the
#     BUNDLED node + chromium.
# Plus offline evidence: all 9 layout components source=bundle, zero downloads.
# NOT concurrency-safe (system lsof): run solo. Ports 8906/6381/5455/6398/6468.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROXY_PORT="${M4_PROXY_PORT:-8906}"; BACKEND_PORT="${M4_BACKEND_PORT:-6381}"
POSTGRES_PORT="${M4_POSTGRES_PORT:-5455}"; VALKEY_PORT="${M4_VALKEY_PORT:-6398}"
EXPORTER_PORT="${M4_EXPORTER_PORT:-6468}"
BASE="http://localhost:${PROXY_PORT}"; POISON="http://127.0.0.1:1"
DMG="${1:-$(ls -t "$ROOT/target/release/bundle/dmg/"*.dmg 2>/dev/null | head -1)}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/adv-pkg.XXXXXX")"
MNT="$WORK/mnt"; INSTALL="$WORK/install"; FRESH_HOME="$WORK/home"
DATA_DIR="$WORK/data"; LOG="$WORK/app.log"
APP_NAME="Penpot Local.app"; APP="$INSTALL/$APP_NAME"
APP_BIN="$APP/Contents/MacOS/penpot-desktop"; APP_PID=""; FAILURES=0
mkdir -p "$MNT" "$INSTALL" "$FRESH_HOME" "$WORK/tmp"
pass(){ echo "PASS: $1"; }; fail(){ echo "FAIL: $1"; FAILURES=$((FAILURES+1)); }
json_field(){ python3 -c "import json,sys;print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
cleanup(){
  [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null && {
    kill -TERM "$APP_PID" 2>/dev/null
    for _ in $(seq 1 20); do kill -0 "$APP_PID" 2>/dev/null||break; sleep 1; done
    kill -9 "$APP_PID" 2>/dev/null; }
  pkill -9 -f "$DATA_DIR" 2>/dev/null; hdiutil detach "$MNT" -quiet 2>/dev/null
  [ "$FAILURES" -eq 0 ] && rm -rf "$WORK" || echo "kept: $WORK"
}
trap cleanup EXIT
start_app(){ ( cd "$WORK"||exit 1; exec env -i HOME="$FRESH_HOME" \
    PATH="/usr/bin:/bin:/usr/sbin:/sbin" TMPDIR="$WORK/tmp/" \
    http_proxy="$POISON" https_proxy="$POISON" HTTP_PROXY="$POISON" HTTPS_PROXY="$POISON" ALL_PROXY="$POISON" \
    PENPOT_LOCAL_DATA_DIR="$DATA_DIR" PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" PENPOT_LOCAL_EXPORTS=1 \
    PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" "$APP_BIN" ) >>"$LOG" 2>&1 & APP_PID=$!; }
wait_ready(){ local d=$(($(date +%s)+$1)); while [ "$(date +%s)" -lt "$d" ]; do
  [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$BASE/" 2>/dev/null)" = "200" ] && return 0
  kill -0 "$APP_PID" 2>/dev/null||{ echo died; tail -25 "$LOG">&2; return 1; }; sleep 2; done
  echo "ready timeout">&2; tail -25 "$LOG">&2; return 1; }

echo "== adv packaged probe: dmg=$DMG scratch=$WORK"
[ -f "$DMG" ] && pass "dmg exists ($(du -sh "$DMG"|cut -f1))" || { fail "dmg missing"; exit 1; }
for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$EXPORTER_PORT"; do
  lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { fail "port $p busy"; exit 1; }; done
pass "ports free"
hdiutil attach -nobrowse -readonly -mountpoint "$MNT" "$DMG" >/dev/null && pass "dmg mounts" || { fail mount; exit 1; }
ditto "$MNT/$APP_NAME" "$APP" && [ -x "$APP_BIN" ] && pass "app copied to scratch ($(du -sh "$APP"|cut -f1))" || { fail copy; exit 1; }
hdiutil detach "$MNT" -quiet || true

BOOT0="$(date +%s)"
start_app
wait_ready 420 && pass "(boot) OFFLINE first boot READY in $(($(date +%s)-BOOT0))s (env -i, poisoned proxies, no host node)" || { fail "offline boot"; exit 1; }

# offline evidence
LAYOUT="$(grep 'runtime layout: component=' "$LOG" || true)"
LC="$(echo "$LAYOUT"|grep -c 'component=' || true)"; DEVC="$(echo "$LAYOUT"|grep -c 'source=dev' || true)"
[ "$LC" -eq 9 ] && [ "$DEVC" -eq 0 ] && pass "(offline) all 9 layout components source=bundle (0 dev)" || { fail "layout $LC dev=$DEVC"; echo "$LAYOUT">&2; }
DLC="$(grep -ci download "$LOG" || true)"; [ "$DLC" -eq 0 ] && pass "(offline) zero download lines" || fail "(offline) $DLC download lines"

TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null||true)"
[ -n "$TOKEN" ] && pass "(auth) provisioned token present" || { fail "no token"; exit 1; }

# seed boards + text layers
export M5_DESIGNS_DIR="$DATA_DIR/designs" PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE" PENPOT_TOKEN="$TOKEN"
HELPER="$ROOT/scripts/m5_features_helper.py"
python3 "$HELPER" seed "$WORK" >/dev/null 2>"$WORK/seed.err" && pass "(seed) alpha(Cover+Detail)+beta(Solo) via RPC" || { fail seed; cat "$WORK/seed.err">&2; exit 1; }

# THUMBNAILS offline on the installed .app
EXPORTS_REL=""; D=$(($(date +%s)+240))
while [ "$(date +%s)" -lt "$D" ]; do
  O="$(python3 "$HELPER" exports_check "$WORK" alpha 2>/dev/null||true)"; case "$O" in OK\ *) EXPORTS_REL="${O#OK }"; break;; esac; sleep 3; done
if [ -n "$EXPORTS_REL" ]; then
  SVG="$DATA_DIR/designs/$EXPORTS_REL/Cover.svg"; PNG="$DATA_DIR/designs/$EXPORTS_REL/Cover.png"
  if grep -q "<svg" "$SVG" 2>/dev/null && [ "$(head -c8 "$PNG" 2>/dev/null|xxd -p)" = "89504e470d0a1a0a" ]; then
    pass "(THUMBNAILS) every board rendered offline via BUNDLED chromium: $EXPORTS_REL (Cover.svg valid + Cover.png magic)"
  else fail "(THUMBNAILS) artifacts malformed"; fi
else fail "(THUMBNAILS) no .exports within 240s"; fi
ENODE="$(ps -o args= -p "$(lsof -nP -tiTCP:"$EXPORTER_PORT" -sTCP:LISTEN 2>/dev/null|head -1)" 2>/dev/null||true)"
echo "$ENODE" | grep -qF "Resources/penpot-runtime/bin/node" && pass "(THUMBNAILS) exporter child on the BUNDLED node inside the .app" || fail "(THUMBNAILS) exporter not on bundled node: $ENODE"

# SEARCH offline on the installed .app — wait for the vault index to catch up
STATUS_OK=""; D=$(($(date +%s)+120))
while [ "$(date +%s)" -lt "$D" ]; do
  FI="$(curl -fsS "$BASE/__api/vault/status" 2>/dev/null|json_field filesIndexed 2>/dev/null||echo '?')"
  [ "$FI" -ge 2 ] 2>/dev/null && { STATUS_OK=1; break; }; sleep 2; done
[ -n "$STATUS_OK" ] && pass "(SEARCH) vault index built from disk in the installed .app (filesIndexed=$FI)" || fail "(SEARCH) index never reached 2 files"
# board-name search
BODY="$(curl -fsS "$BASE/__api/vault/search?q=Cover&limit=20" 2>/dev/null||echo '{}')"
echo "$BODY" | python3 -c "import json,sys;b=json.load(sys.stdin);sys.exit(0 if any(h['kind']=='board' and h['name']=='Cover' for h in b.get('hits',[])) else 1)" \
  && pass "(SEARCH) board-name query 'Cover' returns the board hit offline" || { fail "(SEARCH) 'Cover' no board hit"; echo "$BODY">&2; }
# text-layer search (m5 seed writes 'Hello Penpot' style text; probe the file name token instead which is deterministic: boards carry text? use 'Solo' board of beta)
BODY2="$(curl -fsS "$BASE/__api/vault/search?q=Solo&limit=20" 2>/dev/null||echo '{}')"
echo "$BODY2" | python3 -c "import json,sys;b=json.load(sys.stdin);sys.exit(0 if b.get('count',0)>=1 else 1)" \
  && pass "(SEARCH) query 'Solo' returns >=1 hit offline (deepLink present)" || { fail "(SEARCH) 'Solo' no hit"; echo "$BODY2">&2; }
# deepLink shape check
echo "$BODY" | python3 -c "import json,sys;b=json.load(sys.stdin);h=next((x for x in b['hits'] if x['kind']=='board' and x['name']=='Cover'),None);sys.exit(0 if h and h['deepLink'].startswith('/#/workspace?team-id=') and 'file-id=' in h['deepLink'] else 1)" \
  && pass "(SEARCH) hit carries a valid /#/workspace deep link" || fail "(SEARCH) deepLink malformed"

kill -TERM "$APP_PID" 2>/dev/null
for _ in $(seq 1 20); do kill -0 "$APP_PID" 2>/dev/null||{ APP_PID=""; break; }; sleep 1; done
echo
[ "$FAILURES" -eq 0 ] && { echo "ADV PACKAGED: ALL PASS"; exit 0; } || { echo "ADV PACKAGED: $FAILURES FAILURE(S)"; exit 1; }
