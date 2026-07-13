#!/usr/bin/env bash
# ci-bundle-smoke.sh — headless smoke test of a built penpot-runtime bundle.
#
# Boots the `headless` bin with PENPOT_LOCAL_RUNTIME_BUNDLE pointed at the
# bundle EXTRACTED FROM THE PACKAGED ARTIFACT (AppImage / .app), under
# poisoned proxies (anything touching the network through an env-honoring
# client fails loudly) and a fresh data dir, then asserts:
#   (1) READY within the timeout;
#   (2) every runtime-layout component resolves source=bundle (0 source=dev);
#   (3) no postgres download (no <data>/postgres/install, no 'download' log
#       lines) — the offline-first-boot invariant;
#   (4) GET / serves the Penpot SPA through the proxy;
#   (5) authenticated get-profile with the persisted access token;
#   (6) PNG upload round-trips byte-identical through /assets/** — this execs
#       the BUNDLED `identify` via the backend child PATH;
#   (7) SIGTERM -> clean exit, no orphans.
#
# Usage: ci-bundle-smoke.sh <bundle-dir> <headless-bin>
# Ports (new ledger entries): 8907/6266/5457/6383.

set -u

BUNDLE="${1:?usage: ci-bundle-smoke.sh <bundle-dir> <headless-bin>}"
BIN="${2:?usage: ci-bundle-smoke.sh <bundle-dir> <headless-bin>}"

PROXY_PORT="${SMOKE_PROXY_PORT:-8907}"
BACKEND_PORT="${SMOKE_BACKEND_PORT:-6266}"
POSTGRES_PORT="${SMOKE_POSTGRES_PORT:-5457}"
VALKEY_PORT="${SMOKE_VALKEY_PORT:-6383}"
BOOT_TIMEOUT="${SMOKE_TIMEOUT:-420}"
BASE="http://localhost:${PROXY_PORT}"

[ -f "$BUNDLE/backend/penpot.jar" ] || { echo "FATAL: $BUNDLE is not a bundle (no backend/penpot.jar)"; exit 1; }
[ -x "$BIN" ] || { echo "FATAL: headless bin $BIN missing/not executable"; exit 1; }

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-ci-smoke-data.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-ci-smoke-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
HEADLESS_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 20); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    if [ "$FAILURES" -ne 0 ]; then
        echo "==== last 80 log lines ===="
        tail -80 "$LOG" 2>/dev/null
        echo "kept for debugging: data=$DATA_DIR log=$LOG"
    else
        rm -rf "$DATA_DIR" "$WORK_DIR"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

rpc() { # rpc <command> <json-body> [auth-header]
    local cmd="$1" body="$2" auth="${3:-}"
    if [ -n "$auth" ]; then
        curl --noproxy '*' -sS -X POST "$BASE/api/rpc/command/$cmd" \
            -H "$auth" -H 'Content-Type: application/json' \
            -H 'Accept: application/json' -d "$body"
    else
        curl --noproxy '*' -sS -X POST "$BASE/api/rpc/command/$cmd" \
            -H 'Content-Type: application/json' \
            -H 'Accept: application/json' -d "$body"
    fi
}

echo "== CI bundle smoke: bundle=$BUNDLE bin=$BIN data=$DATA_DIR proxy=$BASE"

# --- boot (poisoned proxies: dead port 1 — offline for env-honoring clients) --
PENPOT_LOCAL_RUNTIME_BUNDLE="$BUNDLE" \
PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
http_proxy="http://127.0.0.1:1" https_proxy="http://127.0.0.1:1" \
HTTP_PROXY="http://127.0.0.1:1" HTTPS_PROXY="http://127.0.0.1:1" \
ALL_PROXY="http://127.0.0.1:1" \
    "$BIN" >>"$LOG" 2>&1 &
HEADLESS_PID=$!

READY=0
DEADLINE=$(($(date +%s) + BOOT_TIMEOUT))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if grep -q "^READY " "$LOG" 2>/dev/null; then READY=1; break; fi
    if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then break; fi
    sleep 2
done
if [ "$READY" -eq 1 ]; then
    pass "(1) first boot reaches READY under poisoned proxies (fresh data dir)"
else
    fail "(1) first boot reaches READY within ${BOOT_TIMEOUT}s"
    exit 1
fi

# --- (2) every component from the bundle ---------------------------------------
DEV_LINES="$(grep -c "source=dev" "$LOG" 2>/dev/null || true)"
BUNDLE_LINES="$(grep -c "source=bundle" "$LOG" 2>/dev/null || true)"
if [ "${DEV_LINES:-0}" -eq 0 ] && [ "${BUNDLE_LINES:-0}" -ge 5 ]; then
    pass "(2) runtime layout: $BUNDLE_LINES source=bundle components, 0 source=dev"
else
    fail "(2) runtime layout: expected 0 source=dev / >=5 source=bundle, got dev=$DEV_LINES bundle=$BUNDLE_LINES"
    grep "component=" "$LOG" | head -10
fi

# --- (3) offline first boot: no postgres download ------------------------------
if [ ! -e "$DATA_DIR/postgres/install" ] && ! grep -qi "download" "$LOG"; then
    pass "(3) no postgres download dir, zero 'download' log lines (offline first boot)"
else
    fail "(3) offline first boot violated"
    ls "$DATA_DIR/postgres" 2>/dev/null; grep -i "download" "$LOG" | head -5
fi

# --- (4) SPA through the proxy --------------------------------------------------
curl --noproxy '*' -sS -o "$WORK_DIR/index.html" "$BASE/" || true
if grep -qi "<html" "$WORK_DIR/index.html" && grep -qi "penpot" "$WORK_DIR/index.html"; then
    pass "(4) GET / serves the Penpot SPA through the proxy"
else
    fail "(4) GET / serves the Penpot SPA through the proxy"
fi

# --- (5) authenticated get-profile ---------------------------------------------
TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
CRED_EMAIL="$(json_field email <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
AUTH="Authorization: Token $TOKEN"
PROFILE_JSON="$(rpc get-profile '{}' "$AUTH" || true)"
PROFILE_EMAIL="$(echo "$PROFILE_JSON" | json_field email 2>/dev/null || true)"
if [ -n "$TOKEN" ] && [ -n "$PROFILE_EMAIL" ] && [ "$PROFILE_EMAIL" = "$CRED_EMAIL" ]; then
    pass "(5) get-profile with the persisted access token ($PROFILE_EMAIL)"
else
    fail "(5) get-profile with the persisted access token"
    echo "profile response: $PROFILE_JSON"
fi

# --- (6) PNG upload round-trip (exercises the bundled identify) ------------------
python3 - "$WORK_DIR/tiny.png" <<'EOF'
import struct, sys, zlib
def chunk(t, d):
    return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
ihdr = chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 0, 0, 0, 0))
raw = b"".join(b"\x00" + bytes((x * 30) % 256 for x in range(8)) for _ in range(8))
png = b"\x89PNG\r\n\x1a\n" + ihdr + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
EOF
PROJECT_ID="$(echo "$PROFILE_JSON" | json_field defaultProjectId 2>/dev/null || true)"
FILE_ID="$(rpc create-file "{\"name\":\"ci-smoke\",\"projectId\":\"$PROJECT_ID\"}" "$AUTH" |
    json_field id 2>/dev/null || true)"
MEDIA_JSON="$(curl --noproxy '*' -sS -X POST "$BASE/api/rpc/command/upload-file-media-object" \
    -H "$AUTH" -H 'Accept: application/json' \
    -F "file-id=$FILE_ID" -F "is-local=true" -F "name=tiny.png" \
    -F "content=@$WORK_DIR/tiny.png;type=image/png" || true)"
MEDIA_ID="$(echo "$MEDIA_JSON" | json_field id 2>/dev/null || true)"
if [ -n "$MEDIA_ID" ] &&
    curl --noproxy '*' -sS -o "$WORK_DIR/fetched.png" "$BASE/assets/by-file-media-id/$MEDIA_ID" &&
    cmp -s "$WORK_DIR/tiny.png" "$WORK_DIR/fetched.png"; then
    pass "(6) uploaded PNG round-trips byte-identical (bundled identify exec'd by the backend)"
else
    fail "(6) PNG upload round-trip"
    echo "upload response: $MEDIA_JSON"
fi

# --- (7) clean shutdown, no orphans ---------------------------------------------
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
STACK_PIDS="$(pgrep -f "$DATA_DIR" || true)"
kill -TERM "$HEADLESS_PID" 2>/dev/null
CLEAN=0
for _ in $(seq 1 20); do
    if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then CLEAN=1; break; fi
    sleep 1
done
if [ "$CLEAN" -eq 1 ]; then
    HEADLESS_PID=""
    ORPHANS=""
    for pid in $JAVA_PIDS $STACK_PIDS; do
        kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"
    done
    if [ -z "$ORPHANS" ]; then
        pass "(7) SIGTERM -> clean exit, no orphan java/valkey/postgres processes"
    else
        fail "(7) orphan processes left behind:$ORPHANS"
        ps -o pid,command -p ${ORPHANS} 2>/dev/null || ps aux | grep -E "$(echo $ORPHANS | tr ' ' '|')" || true
    fi
else
    fail "(7) headless did not exit within 20s of SIGTERM"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "CI BUNDLE SMOKE: ALL PASS"
    exit 0
else
    echo "CI BUNDLE SMOKE: $FAILURES FAILURE(S)"
    exit 1
fi
