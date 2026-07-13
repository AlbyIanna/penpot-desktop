#!/usr/bin/env bash
# M1 smoke test (docs/milestones/m1.md, `just smoke`).
#
# Boots the FULL stack headless against a FRESH temp data dir and asserts:
#   (a) the Penpot SPA is served through the proxy;
#   (b) GET /__bootstrap answers Set-Cookie auth-token + redirect to /;
#   (c) an authenticated RPC (get-profile with the persisted access token)
#       returns the provisioned profile;
#   (d) a PNG uploaded via upload-file-media-object comes back byte-identical
#       through /assets/** (the X-Accel path end-to-end);
#   (e) SIGTERM -> clean exit within the grace period, no orphan
#       java/valkey/postgres processes;
#   (f) a second boot with the SAME data dir reaches READY and the persisted
#       token still authenticates (PENPOT_SECRET_KEY persistence proof).
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# /opt/homebrew/opt/openjdk (JDK 26), valkey-server, ImageMagick (`identify` —
# the backend shells out to it for media uploads), python3, curl.
# First run needs network once (embedded postgres binaries download).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Dedicated ports so the smoke run never collides with a dev instance on the
# default 8686/6161/5433/6380.
PROXY_PORT="${M1_SMOKE_PROXY_PORT:-8788}"
BACKEND_PORT="${M1_SMOKE_BACKEND_PORT:-6263}"
POSTGRES_PORT="${M1_SMOKE_POSTGRES_PORT:-5435}"
VALKEY_PORT="${M1_SMOKE_VALKEY_PORT:-6382}"
FIRST_BOOT_TIMEOUT="${M1_SMOKE_TIMEOUT:-900}"   # embeds a postgres download
SECOND_BOOT_TIMEOUT=300
BASE="http://localhost:${PROXY_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m1-smoke-data.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m1-smoke-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HEADLESS_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 15); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { # json_field <key> ; reads JSON on stdin, prints top-level key
    python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"
}

start_headless() {
    PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() { # wait_ready <timeout-seconds> <marker>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if tail -50 "$LOG" 2>/dev/null | grep -q "^READY "; then
            return 0
        fi
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

stop_headless() { # SIGTERM + wait; returns 0 on clean exit within 15s
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 15); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            HEADLESS_PID=""
            return 0
        fi
        sleep 1
    done
    return 1
}

rpc() { # rpc <command> <json-body> [auth-header]
    local cmd="$1" body="$2" auth="${3:-}"
    if [ -n "$auth" ]; then
        curl -sS -X POST "$BASE/api/rpc/command/$cmd" \
            -H "$auth" -H 'Content-Type: application/json' \
            -H 'Accept: application/json' -d "$body"
    else
        curl -sS -X POST "$BASE/api/rpc/command/$cmd" \
            -H 'Content-Type: application/json' \
            -H 'Accept: application/json' -d "$body"
    fi
}

echo "== M1 smoke: data dir $DATA_DIR, proxy $BASE"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless); then
    fail "build (cargo build -p penpot-desktop --bin headless)"
    exit 1
fi
pass "build (cargo build -p penpot-desktop --bin headless)"

# --- first boot --------------------------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "first boot reaches READY (fresh data dir)"
else
    fail "first boot reaches READY (fresh data dir)"
    exit 1
fi

# (a) SPA through the proxy ---------------------------------------------------
curl -sS -o "$WORK_DIR/index.html" "$BASE/" || true
if grep -qi "<html" "$WORK_DIR/index.html" && grep -qi "penpot" "$WORK_DIR/index.html"; then
    pass "(a) GET / serves the Penpot SPA through the proxy"
else
    fail "(a) GET / serves the Penpot SPA through the proxy"
fi

# (b) bootstrap auto-login ------------------------------------------------------
BOOTSTRAP_HDRS="$(curl -sS -o /dev/null -D - "$BASE/__bootstrap" || true)"
if echo "$BOOTSTRAP_HDRS" | grep -qi "^HTTP/1.1 302" &&
    echo "$BOOTSTRAP_HDRS" | grep -qi "^set-cookie: auth-token=" &&
    echo "$BOOTSTRAP_HDRS" | grep -qi "^location: /"; then
    pass "(b) GET /__bootstrap sets auth-token cookie and redirects to /"
else
    fail "(b) GET /__bootstrap sets auth-token cookie and redirects to /"
    echo "$BOOTSTRAP_HDRS" >&2
fi

# (c) authenticated RPC with the persisted access token -----------------------
TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
CRED_EMAIL="$(json_field email <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
AUTH="Authorization: Token $TOKEN"
PROFILE_JSON="$(rpc get-profile '{}' "$AUTH" || true)"
PROFILE_EMAIL="$(echo "$PROFILE_JSON" | json_field email 2>/dev/null || true)"
if [ -n "$TOKEN" ] && [ -n "$PROFILE_EMAIL" ] && [ "$PROFILE_EMAIL" = "$CRED_EMAIL" ]; then
    pass "(c) get-profile with the stored access token returns the provisioned profile ($PROFILE_EMAIL)"
else
    fail "(c) get-profile with the stored access token returns the provisioned profile"
    echo "profile response: $PROFILE_JSON" >&2
fi

# (d) media upload + X-Accel asset round-trip ---------------------------------
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
FILE_ID="$(rpc create-file "{\"name\":\"m1-smoke\",\"projectId\":\"$PROJECT_ID\"}" "$AUTH" |
    json_field id 2>/dev/null || true)"
MEDIA_JSON="$(curl -sS -X POST "$BASE/api/rpc/command/upload-file-media-object" \
    -H "$AUTH" -H 'Accept: application/json' \
    -F "file-id=$FILE_ID" -F "is-local=true" -F "name=tiny.png" \
    -F "content=@$WORK_DIR/tiny.png;type=image/png" || true)"
MEDIA_ID="$(echo "$MEDIA_JSON" | json_field id 2>/dev/null || true)"
if [ -n "$MEDIA_ID" ] &&
    curl -sS -o "$WORK_DIR/fetched.png" "$BASE/assets/by-file-media-id/$MEDIA_ID" &&
    cmp -s "$WORK_DIR/tiny.png" "$WORK_DIR/fetched.png"; then
    pass "(d) uploaded PNG comes back byte-identical through /assets/** (X-Accel)"
else
    fail "(d) uploaded PNG comes back byte-identical through /assets/** (X-Accel)"
    echo "upload response: $MEDIA_JSON" >&2
fi

# (e) clean shutdown, no orphans ----------------------------------------------
# Record the exact child pids before shutdown so the orphan check cannot
# false-positive on unrelated processes. Scope the java lookup to children of
# OUR headless instance — a global 'pgrep -f penpot.jar' matches concurrently
# running dev instances (observed false positive during M1 verification).
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
STACK_PIDS="$(pgrep -f "$DATA_DIR" || true)"
if stop_headless; then
    ORPHANS=""
    for pid in $JAVA_PIDS $STACK_PIDS; do
        kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"
    done
    if [ -z "$ORPHANS" ]; then
        pass "(e) SIGTERM -> clean exit, no orphan java/valkey/postgres processes"
    else
        fail "(e) orphan processes left behind:$ORPHANS"
        ps -o pid,command -p ${ORPHANS} >&2 || true
    fi
else
    fail "(e) headless did not exit within 15s of SIGTERM"
fi

# (f) second boot with the SAME data dir --------------------------------------
: >"$LOG"
start_headless
if wait_ready "$SECOND_BOOT_TIMEOUT"; then
    PROFILE2_EMAIL="$(rpc get-profile '{}' "$AUTH" | json_field email 2>/dev/null || true)"
    if [ -n "$PROFILE2_EMAIL" ] && [ "$PROFILE2_EMAIL" = "$CRED_EMAIL" ]; then
        pass "(f) second boot READY and the persisted token still authenticates (secret-key persistence)"
    else
        fail "(f) persisted token rejected after restart (secret key not stable?)"
    fi
else
    fail "(f) second boot with the same data dir did not reach READY"
fi
if [ -n "$HEADLESS_PID" ]; then
    if stop_headless; then
        pass "(f2) second instance shut down cleanly"
    else
        fail "(f2) second instance did not exit within 15s of SIGTERM"
    fi
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "M1 SMOKE: ALL PASS"
    exit 0
else
    echo "M1 SMOKE: $FAILURES FAILURE(S)"
    exit 1
fi
