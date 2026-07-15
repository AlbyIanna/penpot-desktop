#!/usr/bin/env bash
# M4 artifact test: FRESH-MACHINE APPROXIMATION for the macOS dmg.
#
# Mounts the dmg, copies "Penpot Local.app" to a scratch install dir, and runs
# the app binary directly with a sanitized environment (env -i, fresh HOME,
# system-only PATH, poisoned http proxies) — approximating a machine that has
# no Homebrew, no repo checkout, no dev toolchain, and no network.
#
# N2: the whole test now runs with RENDERS ON (PENPOT_LOCAL_EXPORTS=1) — the
# packaged artifact must carry its own node + exporter app + chromium headless
# shell, offline.
#
# Asserts:
#   (a) full boot: SPA through the proxy, /__bootstrap sets the auth cookie,
#       authenticated get-profile works, PNG media round-trips through
#       /assets/** (exercises the bundled relocated ImageMagick identify);
#   (b) OFFLINE first boot: all runtime-layout components (incl. the N2
#       exporter/exporter-node/exporter-browsers trio) resolve source=bundle,
#       no <data>/postgres/install download dir appears, zero download log
#       lines, boot-time bound — with http_proxy/https_proxy/ALL_PROXY poisoned
#       to a dead port so anything that needed the network would fail loudly;
#   (c) provenance: while running, lsof over every stack pid shows 0 opens under
#       /opt/homebrew and 0 under the repo checkout; the watchdog is armed from
#       the bundle inside the installed .app;
#   (d) the sync-status tray is created (GUI run — a window appears);
#   (f) RENDERS ON: a seeded board renders to <name>.exports/*.{svg,png}
#       through the BUNDLED node + headless chromium — no host node on PATH,
#       poisoned proxies (N2 exit criterion);
#   (g) PACKAGED SEARCH: the bundled N1 vault-index service indexes the seeded
#       designs offline and /__api/vault/search returns the 'Cover' board hit
#       (kind=board, alpha fileId, /#/workspace deep link) — the surface N3's
#       board grid depends on, proven present in the shipped artifact;
#   (e) SIGTERM -> clean exit, no orphans; then a SIGKILL run -> the watchdog
#       reaps every child (incl. the exporter node child — the M5 orphan gap).
#
# Ports (test-port ledger): proxy 8906, backend 6381, postgres 5455, valkey
# 6398, exporter 6468.
# Usage: scripts/m4-artifact-test.sh [path-to-dmg]   (default: newest in
#        target/release/bundle/dmg/)
# NOT concurrency-safe (system-wide lsof/pgrep scans): run solo.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PROXY_PORT="${M4_PROXY_PORT:-8906}"
BACKEND_PORT="${M4_BACKEND_PORT:-6381}"
POSTGRES_PORT="${M4_POSTGRES_PORT:-5455}"
VALKEY_PORT="${M4_VALKEY_PORT:-6398}"
EXPORTER_PORT="${M4_EXPORTER_PORT:-6468}"
FIRST_BOOT_TIMEOUT="${M4_FIRST_BOOT_TIMEOUT:-420}"   # offline: no download allowed
SECOND_BOOT_TIMEOUT=240
BASE="http://localhost:${PROXY_PORT}"
POISON="http://127.0.0.1:1"

DMG="${1:-$(ls -t "$ROOT/target/release/bundle/dmg/"*.dmg 2>/dev/null | head -1)}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m4-artifact.XXXXXX")"
MNT="$WORK/mnt"
INSTALL="$WORK/install"
FRESH_HOME="$WORK/home"
DATA_DIR="$WORK/data"
LOG="$WORK/app.log"
APP_NAME="Penpot Local.app"
APP="$INSTALL/$APP_NAME"
APP_BIN="$APP/Contents/MacOS/penpot-desktop"
APP_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
    if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
        kill -TERM "$APP_PID" 2>/dev/null
        for _ in $(seq 1 20); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$APP_PID" 2>/dev/null
    fi
    # Belt and braces: nothing of ours may outlive the test.
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    hdiutil detach "$MNT" -quiet 2>/dev/null
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$WORK"
    else
        echo "kept for debugging: $WORK (log: $LOG)"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

stack_pids() { # every live pid of the running stack (app + children + watchdog)
    {
        [ -n "$APP_PID" ] && echo "$APP_PID"
        [ -n "$APP_PID" ] && pgrep -P "$APP_PID"
        pgrep -f "$DATA_DIR"
        pgrep -f "penpot-watchdog"
    } 2>/dev/null | sort -un
}

start_app() { # sanitized environment: the fresh-machine approximation
    (
        cd "$WORK" || exit 1
        exec env -i \
            HOME="$FRESH_HOME" \
            PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
            TMPDIR="$WORK/tmp/" \
            http_proxy="$POISON" https_proxy="$POISON" \
            HTTP_PROXY="$POISON" HTTPS_PROXY="$POISON" ALL_PROXY="$POISON" \
            PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
            PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
            PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
            PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
            PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
            PENPOT_LOCAL_EXPORTS=1 \
            PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" \
            "$APP_BIN"
    ) >>"$LOG" 2>&1 &
    APP_PID=$!
}

wait_ready() { # poll GET / until the proxy serves the SPA; $1 = timeout s
    # NOT /__bootstrap: that route is single-use per boot and the GUI webview
    # consumes it milliseconds after the proxy binds — polling it would both
    # race the webview and burn the auto-login.
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        local code
        code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$BASE/" 2>/dev/null)"
        [ "$code" = "200" ] && return 0
        if ! kill -0 "$APP_PID" 2>/dev/null; then
            echo "app process died; last log lines:" >&2
            tail -25 "$LOG" >&2
            return 1
        fi
        sleep 2
    done
    echo "timed out waiting for the proxy ($1s); last log lines:" >&2
    tail -25 "$LOG" >&2
    return 1
}

rpc() { # rpc <command> <json-body> <auth-header>
    curl -sS -X POST "$BASE/api/rpc/command/$1" \
        -H "$3" -H 'Content-Type: application/json' \
        -H 'Accept: application/json' -d "$2"
}

echo "== M4 artifact test: dmg=$DMG"
echo "== scratch: $WORK"
mkdir -p "$MNT" "$INSTALL" "$FRESH_HOME" "$WORK/tmp"

# --- preflight ----------------------------------------------------------------
if [ -z "$DMG" ] || [ ! -f "$DMG" ]; then
    fail "dmg exists (run scripts/build-dmg.sh first)"
    exit 1
fi
pass "dmg exists ($(du -sh "$DMG" | cut -f1))"
for port in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$EXPORTER_PORT"; do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
        fail "port $port is free"
        exit 1
    fi
done
pass "test ports free ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"

# --- mount + install ------------------------------------------------------------
if hdiutil attach -nobrowse -readonly -mountpoint "$MNT" "$DMG" >/dev/null; then
    pass "dmg mounts"
else
    fail "dmg mounts"
    exit 1
fi
if ditto "$MNT/$APP_NAME" "$APP" && [ -x "$APP_BIN" ]; then
    pass "app copied to scratch install dir ($(du -sh "$APP" | cut -f1))"
else
    fail "app copied to scratch install dir"
    exit 1
fi
hdiutil detach "$MNT" -quiet || true
if [ -f "$APP/Contents/Resources/penpot-runtime/backend/penpot.jar" ]; then
    pass "bundled penpot-runtime present in Contents/Resources"
else
    fail "bundled penpot-runtime present in Contents/Resources"
    exit 1
fi

# --- first boot: sanitized env, poisoned proxies, fresh everything --------------
BOOT_START="$(date +%s)"
start_app
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    BOOT_SECS=$(($(date +%s) - BOOT_START))
    pass "(b4) OFFLINE first boot reached READY in ${BOOT_SECS}s (poisoned proxies, bound ${FIRST_BOOT_TIMEOUT}s)"
else
    fail "(b4) offline first boot"
    exit 1
fi

# (a) full boot ------------------------------------------------------------------
curl -sS -o "$WORK/index.html" "$BASE/" || true
if grep -qi "<html" "$WORK/index.html" && grep -qi "penpot" "$WORK/index.html"; then
    pass "(a1) GET / serves the Penpot SPA through the proxy"
else
    fail "(a1) GET / serves the Penpot SPA through the proxy"
fi

# /__bootstrap is single-use per boot. In the GUI app the webview navigates
# to it as soon as boot completes, so by the time curl gets here the expected
# answer is 403 "bootstrap already used this boot" — positive evidence the
# webview auto-login consumed it. If curl wins the race instead, it must see
# the full 302 + auth cookie.
BOOTSTRAP_BODY="$WORK/bootstrap-body.txt"
BOOTSTRAP_HDRS="$(curl -sS -o "$BOOTSTRAP_BODY" -D - "$BASE/__bootstrap" || true)"
if echo "$BOOTSTRAP_HDRS" | grep -qi "^HTTP/1.1 403" &&
    grep -q "bootstrap already used this boot" "$BOOTSTRAP_BODY"; then
    pass "(a2) /__bootstrap already consumed by the webview (GUI auto-login happened)"
elif echo "$BOOTSTRAP_HDRS" | grep -qi "^HTTP/1.1 302" &&
    echo "$BOOTSTRAP_HDRS" | grep -qi "^set-cookie: auth-token=" &&
    echo "$BOOTSTRAP_HDRS" | grep -qi "^location: /"; then
    pass "(a2) GET /__bootstrap sets auth-token cookie and redirects to / (curl won the race)"
else
    fail "(a2) /__bootstrap neither consumed-by-webview (403) nor a valid 302 auto-login"
    echo "$BOOTSTRAP_HDRS" >&2
fi

TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
CRED_EMAIL="$(json_field email <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
AUTH="Authorization: Token $TOKEN"
PROFILE_JSON="$(rpc get-profile '{}' "$AUTH" || true)"
PROFILE_EMAIL="$(echo "$PROFILE_JSON" | json_field email 2>/dev/null || true)"
if [ -n "$TOKEN" ] && [ -n "$PROFILE_EMAIL" ] && [ "$PROFILE_EMAIL" = "$CRED_EMAIL" ]; then
    pass "(a3) authenticated get-profile returns the provisioned profile ($PROFILE_EMAIL)"
else
    fail "(a3) authenticated get-profile returns the provisioned profile"
    echo "profile response: $PROFILE_JSON" >&2
fi

# (a4) media upload — the backend execs the BUNDLED relocated `identify`
python3 - "$WORK/tiny.png" <<'EOF'
import struct, sys, zlib
def chunk(t, d):
    return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
raw = b"".join(b"\x00" + bytes((x * 30) % 256 for x in range(8)) for _ in range(8))
png = b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
EOF
PROJECT_ID="$(echo "$PROFILE_JSON" | json_field defaultProjectId 2>/dev/null || true)"
FILE_ID="$(rpc create-file "{\"name\":\"m4-artifact\",\"projectId\":\"$PROJECT_ID\"}" "$AUTH" |
    json_field id 2>/dev/null || true)"
MEDIA_JSON="$(curl -sS -X POST "$BASE/api/rpc/command/upload-file-media-object" \
    -H "$AUTH" -H 'Accept: application/json' \
    -F "file-id=$FILE_ID" -F "is-local=true" -F "name=tiny.png" \
    -F "content=@$WORK/tiny.png;type=image/png" || true)"
MEDIA_ID="$(echo "$MEDIA_JSON" | json_field id 2>/dev/null || true)"
if [ -n "$MEDIA_ID" ] &&
    curl -sS -o "$WORK/fetched.png" "$BASE/assets/by-file-media-id/$MEDIA_ID" &&
    cmp -s "$WORK/tiny.png" "$WORK/fetched.png"; then
    pass "(a4) PNG round-trips through /assets/** (bundled identify worked)"
else
    fail "(a4) PNG round-trips through /assets/** (bundled identify worked)"
    echo "upload response: $MEDIA_JSON" >&2
fi

# (b) offline-first-boot evidence -------------------------------------------------
# N2: renders ON adds three exporter components (exporter, exporter-node,
# exporter-browsers) to the M4 six.
LAYOUT_LINES="$(grep "runtime layout: component=" "$LOG" || true)"
LAYOUT_COUNT="$(echo "$LAYOUT_LINES" | grep -c "component=" || true)"
DEV_SOURCED="$(echo "$LAYOUT_LINES" | grep -c "source=dev" || true)"
if [ "$LAYOUT_COUNT" -eq 9 ] && [ "$DEV_SOURCED" -eq 0 ] &&
    echo "$LAYOUT_LINES" | grep -q "component=penpot-watchdog source=bundle" &&
    echo "$LAYOUT_LINES" | grep "component=exporter " | grep -q "source=bundle" &&
    echo "$LAYOUT_LINES" | grep "component=exporter-node" | grep -q "source=bundle" &&
    echo "$LAYOUT_LINES" | grep "component=exporter-browsers" | grep -q "source=bundle"; then
    pass "(b1) all 9 runtime-layout components resolved from the bundle (incl. exporter trio, 0 source=dev)"
else
    fail "(b1) all 9 runtime-layout components resolved from the bundle"
    echo "$LAYOUT_LINES" >&2
fi
if [ ! -e "$DATA_DIR/postgres/install" ]; then
    pass "(b2) no <data>/postgres/install download dir was created"
else
    fail "(b2) no <data>/postgres/install download dir was created"
fi
DOWNLOAD_LINES="$(grep -ci "download" "$LOG" || true)"
if [ "$DOWNLOAD_LINES" -eq 0 ]; then
    pass "(b3) zero 'download' lines in the boot log"
else
    fail "(b3) zero 'download' lines in the boot log ($DOWNLOAD_LINES found)"
    grep -i "download" "$LOG" >&2
fi

# (c) provenance audit -------------------------------------------------------------
PIDS="$(stack_pids | tr '\n' ',' | sed 's/,$//')"
LSOF_OUT="$WORK/lsof.txt"
lsof -p "$PIDS" >"$LSOF_OUT" 2>/dev/null
HOMEBREW_LEAKS="$(grep -c "/opt/homebrew" "$LSOF_OUT" || true)"
REPO_LEAKS="$(grep -c "$ROOT" "$LSOF_OUT" || true)"
if [ "$HOMEBREW_LEAKS" -eq 0 ]; then
    pass "(c1) lsof: 0 open files under /opt/homebrew (pids: $PIDS)"
else
    fail "(c1) lsof: $HOMEBREW_LEAKS open files under /opt/homebrew"
    grep "/opt/homebrew" "$LSOF_OUT" >&2
fi
if [ "$REPO_LEAKS" -eq 0 ]; then
    pass "(c2) lsof: 0 open files under the repo checkout"
else
    fail "(c2) lsof: $REPO_LEAKS open files under the repo checkout"
    grep "$ROOT" "$LSOF_OUT" >&2
fi
# The layout resolver hands the supervisor a non-canonicalized path
# (<.app>/Contents/MacOS/../Resources/penpot-runtime/...), so match on the
# .app prefix + the bundle-relative suffix rather than one literal string.
WATCHDOG_CMD="$(ps -o args= -p "$(pgrep -f penpot-watchdog | head -1)" 2>/dev/null || true)"
if echo "$WATCHDOG_CMD" | grep -qF "$APP_NAME/Contents" &&
    echo "$WATCHDOG_CMD" | grep -qF "Resources/penpot-runtime/bin/penpot-watchdog"; then
    pass "(c3) watchdog armed from the bundle inside the installed .app"
else
    fail "(c3) watchdog armed from the bundle inside the installed .app"
    echo "watchdog cmd: $WATCHDOG_CMD" >&2
fi

# (d) tray ---------------------------------------------------------------------------
if grep -q "sync-status tray icon created" "$LOG"; then
    pass "(d) sync-status tray icon created (GUI run)"
else
    fail "(d) sync-status tray icon created (GUI run)"
fi

# (f) RENDERS ON: seeded board -> svg+png via the bundled node + headless shell -----
# Reuses the m5 RPC helper (seed alpha/beta with boards; exports_check waits
# for state hash == manifest hash). Designs root = the packaged default
# (<data>/designs — PENPOT_LOCAL_DESIGNS_DIR is unset in this test).
export M5_DESIGNS_DIR="$DATA_DIR/designs"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"
export PENPOT_TOKEN="$TOKEN"
HELPER="$ROOT/scripts/m5_features_helper.py"
if python3 "$HELPER" seed "$WORK" >/dev/null 2>"$WORK/seed.err"; then
    pass "(f1) seeded boards via RPC (alpha: Cover+Detail, beta: Solo)"
else
    fail "(f1) RPC seed failed"
    cat "$WORK/seed.err" >&2
fi
EXPORTS_REL=""
F_DEADLINE=$(($(date +%s) + 240))
while [ "$(date +%s)" -lt "$F_DEADLINE" ]; do
    OUT="$(python3 "$HELPER" exports_check "$WORK" alpha 2>/dev/null || true)"
    case "$OUT" in
        OK\ *) EXPORTS_REL="${OUT#OK }"; break ;;
    esac
    sleep 3
done
if [ -n "$EXPORTS_REL" ]; then
    pass "(f2) packaged render path works: $EXPORTS_REL rendered (svg+png per board, hash-gated state)"
else
    fail "(f2) exports did not appear within 240s (renders on the packaged artifact broken)"
fi
F_SVG="$DATA_DIR/designs/$EXPORTS_REL/Cover.svg"
F_PNG="$DATA_DIR/designs/$EXPORTS_REL/Cover.png"
if grep -q "<svg" "$F_SVG" 2>/dev/null &&
    [ "$(head -c 8 "$F_PNG" 2>/dev/null | xxd -p)" = "89504e470d0a1a0a" ]; then
    pass "(f3) Cover.svg is SVG and Cover.png has PNG magic (bundled chromium output)"
else
    fail "(f3) render artifacts malformed"
fi
# the render must NOT have used a host node: the exporter child is bundle node
EXPORTER_NODE_CMD="$(ps -o args= -p "$(lsof -nP -tiTCP:"$EXPORTER_PORT" -sTCP:LISTEN 2>/dev/null | head -1)" 2>/dev/null || true)"
if echo "$EXPORTER_NODE_CMD" | grep -qF "Resources/penpot-runtime/bin/node"; then
    pass "(f4) exporter child runs on the BUNDLED node inside the .app"
else
    fail "(f4) exporter child not on the bundled node: $EXPORTER_NODE_CMD"
fi

# (g) PACKAGED SEARCH: the N1 offline vault index must ship in the artifact -------------
# N3's board grid + the /__search surface both consume /__api/vault/search; that
# route only exists if the bundled vault-index service booted, watched the
# packaged designs dir, and indexed the seeded boards — all offline (proxies
# poisoned). Prove a seeded board name ("Cover") is searchable through the
# installed .app. Depends on the (f1) RPC seed + the sync daemon mirroring the
# boards to <data>/designs, which the exports checks above already confirmed.
ALPHA_FILE_ID="$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['files']['alpha']['fileId'])" "$WORK/expect.json" 2>/dev/null || true)"
SEARCH_RESP=""
SEARCH_OK=0
G_DEADLINE=$(($(date +%s) + 120))
while [ "$(date +%s)" -lt "$G_DEADLINE" ]; do
    SEARCH_RESP="$(curl -sS "$BASE/__api/vault/search?q=Cover&kind=board&limit=10" 2>/dev/null || true)"
    if echo "$SEARCH_RESP" | python3 -c '
import json, sys
want = sys.argv[1]
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
hits = d.get("hits", [])
for h in hits:
    if h.get("kind") == "board" and "Cover" in h.get("name", ""):
        # deep link must be a same-origin workspace navigation
        if not str(h.get("deepLink", "")).startswith("/#/workspace"):
            continue
        # if we know alpha, insist the hit belongs to it
        if want and h.get("fileId") != want:
            continue
        sys.exit(0)
sys.exit(1)
' "$ALPHA_FILE_ID" 2>/dev/null; then
        SEARCH_OK=1
        break
    fi
    sleep 3
done
SEARCH_COUNT="$(echo "$SEARCH_RESP" | json_field count 2>/dev/null || echo '?')"
if [ "$SEARCH_OK" -eq 1 ]; then
    pass "(g1) /__api/vault/search served OFFLINE through the .app: seeded 'Cover' board hit (kind=board, alpha fileId, /#/workspace deep link; count=$SEARCH_COUNT)"
else
    fail "(g1) /__api/vault/search returned no seeded 'Cover' board hit within 120s (packaged N1 index surface)"
    echo "last search response: $SEARCH_RESP" >&2
fi

# (g2) PACKAGED PALETTE: N4's quick-open surface must ship + rank offline in the .app ----
# Two things N4 adds to the artifact: the /__palette overlay page (served by the
# desktop router) and the ranked /__api/vault/palette API (served by the bundled
# vault-index). Both must work OFFLINE through the installed .app (proxies still
# poisoned). Assert the page serves and a fuzzy "Cover" query ranks the seeded
# board first with kind=board + an exact /#/workspace Enter payload.
PALETTE_PAGE_CODE="$(curl -s -o "$WORK/palette.html" -w '%{http_code}' "$BASE/__palette" 2>/dev/null || echo 000)"
if [ "$PALETTE_PAGE_CODE" = "200" ] && grep -qi "Quick open" "$WORK/palette.html" && grep -q 'id="q"' "$WORK/palette.html"; then
    pass "(g2a) /__palette overlay page served OFFLINE through the .app (HTTP 200)"
else
    fail "(g2a) /__palette page not served (code=$PALETTE_PAGE_CODE)"
fi
PAL_RESP="$(curl -sS "$BASE/__api/vault/palette?q=Cover&limit=10" 2>/dev/null || true)"
if echo "$PAL_RESP" | python3 -c '
import json, sys
want = sys.argv[1]
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
hits = d.get("hits", [])
if not hits:
    sys.exit(1)
# The seeded "Cover" board must be the top-ranked hit for a fuzzy "Cover" query.
top = hits[0]
if top.get("kind") != "board":
    sys.exit(1)
if "Cover" not in top.get("label", ""):
    sys.exit(1)
if not str(top.get("deepLink", "")).startswith("/#/workspace"):
    sys.exit(1)
if want and top.get("fileId") != want:
    sys.exit(1)
sys.exit(0)
' "$ALPHA_FILE_ID" 2>/dev/null; then
    PAL_MS="$(echo "$PAL_RESP" | json_field tookMs 2>/dev/null || echo '?')"
    pass "(g2b) /__api/vault/palette ranked the seeded 'Cover' board first OFFLINE (kind=board, exact /#/workspace Enter payload; tookMs=$PAL_MS)"
else
    fail "(g2b) /__api/vault/palette did not rank the seeded 'Cover' board first (packaged N4 palette surface)"
    echo "last palette response: $PAL_RESP" >&2
fi

# (g3) PACKAGED NEW-FROM-TEMPLATE: N6's offline template gallery must ship in the artifact
# and create a REAL on-disk .penpot file in the packaged app, offline (templates are
# bundled in penpot-runtime/backend/builtin-templates; import is loopback RPC only).
TPL_CODE="$(curl -s -o "$WORK/templates.json" -w '%{http_code}' "$BASE/__api/templates" 2>/dev/null || echo 000)"
TPL_COUNT="$(python3 -c "import json;print(json.load(open('$WORK/templates.json')).get('count',0))" 2>/dev/null || echo 0)"
if [ "$TPL_CODE" = "200" ] && [ "${TPL_COUNT:-0}" -ge 4 ]; then
    pass "(g3a) /__api/templates listed $TPL_COUNT templates OFFLINE through the .app"
else
    fail "(g3a) /__api/templates not served offline (code=$TPL_CODE count=$TPL_COUNT)"
fi
# Create a new file from the smallest template (fastest import) and confirm a real .penpot
# directory materialises on disk in the active vault (<data>/designs).
TID="$(python3 -c "import json;ts=json.load(open('$WORK/templates.json')).get('templates',[]);ts.sort(key=lambda t:t.get('sizeBytes',0));print(ts[0]['id'] if ts else '')" 2>/dev/null || echo '')"
BEFORE_N="$(find "$DATA_DIR/designs" -type d -name '*.penpot' 2>/dev/null | wc -l | tr -d ' ')"
curl -sS -X POST "$BASE/__api/templates/new" -H 'Content-Type: application/json' \
    -d "{\"templateId\":\"$TID\"}" -o "$WORK/newtpl.json" 2>/dev/null || true
NEW_OK="$(python3 -c "import json;print(json.load(open('$WORK/newtpl.json')).get('ok'))" 2>/dev/null || echo None)"
AFTER_N="$BEFORE_N"
for _ in $(seq 1 45); do
    AFTER_N="$(find "$DATA_DIR/designs" -type d -name '*.penpot' 2>/dev/null | wc -l | tr -d ' ')"
    [ "${AFTER_N:-0}" -gt "${BEFORE_N:-0}" ] && break
    sleep 2
done
if [ "$NEW_OK" = "True" ] && [ "${AFTER_N:-0}" -gt "${BEFORE_N:-0}" ]; then
    pass "(g3b) new-from-template '$TID' materialised a real on-disk .penpot dir OFFLINE (dirs $BEFORE_N -> $AFTER_N)"
else
    fail "(g3b) new-from-template did not materialise an on-disk .penpot dir (ok=$NEW_OK id=$TID before=$BEFORE_N after=$AFTER_N)"
    cat "$WORK/newtpl.json" >&2 2>/dev/null || true
fi

# (e) SIGTERM: clean exit, no orphans ---------------------------------------------------
CHILL_PIDS="$(stack_pids)"
kill -TERM "$APP_PID" 2>/dev/null
CLEAN_EXIT=1
for _ in $(seq 1 20); do
    if ! kill -0 "$APP_PID" 2>/dev/null; then CLEAN_EXIT=0; break; fi
    sleep 1
done
if [ "$CLEAN_EXIT" -eq 0 ]; then
    sleep 2
    ORPHANS=""
    for pid in $CHILL_PIDS; do
        kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"
    done
    if [ -z "$ORPHANS" ]; then
        pass "(e1) SIGTERM -> clean exit within 20s, no orphans"
        APP_PID=""
    else
        fail "(e1) orphan processes after SIGTERM:$ORPHANS"
        ps -o pid,args -p ${ORPHANS} >&2 || true
    fi
else
    fail "(e1) app did not exit within 20s of SIGTERM"
fi

# (e2) SIGKILL: the watchdog must reap the children --------------------------------------
: >"$LOG"
start_app
if wait_ready "$SECOND_BOOT_TIMEOUT"; then
    pass "(e2a) second boot (same data dir) reached READY"
    KILL_PIDS="$(stack_pids | grep -v "^$APP_PID\$" || true)"
    kill -9 "$APP_PID" 2>/dev/null
    APP_PID=""
    REAPED=1
    for _ in $(seq 1 30); do
        LEFT=""
        for pid in $KILL_PIDS; do
            kill -0 "$pid" 2>/dev/null && LEFT="$LEFT $pid"
        done
        if [ -z "$LEFT" ]; then REAPED=0; break; fi
        sleep 1
    done
    if [ "$REAPED" -eq 0 ]; then
        pass "(e2b) SIGKILL -> watchdog reaped every child within 30s"
    else
        fail "(e2b) children still alive 30s after SIGKILL:$LEFT"
        ps -o pid,args -p ${LEFT} >&2 || true
    fi
else
    fail "(e2a) second boot (same data dir) reached READY"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "M4 ARTIFACT TEST: ALL PASS"
    exit 0
else
    echo "M4 ARTIFACT TEST: $FAILURES FAILURE(S)"
    exit 1
fi
