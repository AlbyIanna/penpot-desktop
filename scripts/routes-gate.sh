#!/usr/bin/env bash
# routes-gate — the SPA hash-route version-bump gate (PLAN2.md risk 2, born in
# milestone N3). The whole chapter adds exactly ONE new upstream coupling: the
# hash-route shapes the lighttable deep-links into. This gate makes that
# coupling loud on any Penpot bump — run it (with roundtrip.py) before anything
# else after a version change.
#
# Two legs, house style (PASS/FAIL, ANSI-stripped greps, dirs kept on failure):
#   (STATIC) the route strings the app depends on are present in the compiled
#     bundle runtime/frontend/js/*.js (grep, no stack needed): workspace,
#     dashboard/recent, team-id, file-id, page-id.
#   (LIVE)  a headless browser (the BUNDLED chromium via N2's playwright,
#     offline) loads /__home, clicks ONE board card and asserts the landed URL
#     is the exact /#/workspace?team-id&file-id&page-id deep link the card
#     advertised, then triggers the escape hatch and asserts /#/dashboard/recent.
#
# Reuse: if ROUTES_GATE_BASE is set (the caller — e.g. n3-home.sh — already has
# a running stack WITH boards) the live leg drives that stack and boots nothing.
# Otherwise this script boots its own throwaway stack (dedicated ports) and
# seeds one file / two boards via the N1 seeder.
#
# Dedicated ports (only used for the self-boot path): proxy 8942, backend 6403,
# postgres 5477, valkey 6420.
#
# Browser: the bundled chromium headless-shell + playwright (recorded in the
# headline). No host Chrome, no network.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

FRONTEND_JS="${ROUTES_GATE_FRONTEND_JS:-$ROOT/runtime/frontend/js}"
NAV_DRIVER="$ROOT/scripts/routes_gate_nav.cjs"
# The bundled render browsers (N2). Prefer the repo runtime/, fall back to dist.
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
PLAYWRIGHT="${ROUTES_GATE_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
NODE_BIN="${ROUTES_GATE_NODE:-node}"

PROXY_PORT="${RG_PROXY_PORT:-8942}"
BACKEND_PORT="${RG_BACKEND_PORT:-6403}"
POSTGRES_PORT="${RG_POSTGRES_PORT:-5477}"
VALKEY_PORT="${RG_VALKEY_PORT:-6420}"
FIRST_BOOT_TIMEOUT="${RG_TIMEOUT:-900}"

FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

# Self-boot bookkeeping (only set when we boot our own stack).
BOOTED=0
HEADLESS_PID=""
DATA_DIR=""; DESIGNS_DIR=""; WORK_DIR=""; LOG=""
PG_CACHE="${RG_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

cleanup() {
    if [ "$BOOTED" = "1" ] && [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 20); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    if [ "$BOOTED" = "1" ]; then
        if [ "$FAILURES" -eq 0 ]; then
            rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR"
        else
            echo "kept for debugging: data=$DATA_DIR designs=$DESIGNS_DIR log=$LOG"
        fi
    fi
}
trap cleanup EXIT

echo "== routes-gate: frontend-js=$FRONTEND_JS browsers=$BROWSERS"

# --- pre-flight ------------------------------------------------------------
[ -f "$NAV_DRIVER" ] || { fail "nav driver missing: $NAV_DRIVER"; exit 1; }
if [ ! -d "$FRONTEND_JS" ]; then
    fail "frontend bundle not found at $FRONTEND_JS — run scripts/fetch-penpot.sh"
    exit 1
fi
if [ ! -e "$PLAYWRIGHT" ]; then
    fail "bundled playwright missing at $PLAYWRIGHT — run scripts/fetch-penpot.sh --with-browsers"
    exit 1
fi

# --- (STATIC) route strings present in the compiled bundle -----------------
# Each fragment must appear in at least one non-.map .js file.
STATIC_OK=1
# N3 shapes (workspace/dashboard) + N4 viewer shape (view/frame-id/section).
for frag in "workspace" "dashboard/recent" "view" "team-id" "file-id" "page-id" "frame-id" "section"; do
    if grep -rlq --include='*.js' -- "$frag" "$FRONTEND_JS" 2>/dev/null; then
        :
    else
        fail "(static) route string '$frag' not found in the SPA bundle — upstream route drift?"
        STATIC_OK=0
    fi
done
[ "$STATIC_OK" = "1" ] && pass "(static) all 8 hash-route strings present in runtime/frontend/js/*.js (incl. N4 viewer view/frame-id/section)"

# --- resolve/boot the live stack -------------------------------------------
BASE="${ROUTES_GATE_BASE:-}"
if [ -n "$BASE" ]; then
    pass "(live) using caller stack at $BASE (no boot)"
else
    echo "   (no ROUTES_GATE_BASE: booting a throwaway stack + seeding 1 file/2 boards)"
    BIN="$ROOT/target/debug/headless"
    DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-rg-data.XXXXXX")"
    DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-rg-designs.XXXXXX")"
    WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-rg-work.XXXXXX")"
    LOG="$WORK_DIR/headless.log"
    BASE="http://localhost:${PROXY_PORT}"
    export N1_DESIGNS_DIR="$DESIGNS_DIR" PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE"
    export N1_FILES=1 N1_BOARDS=2

    if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
        fail "build (headless + penpot-watchdog)"; exit 1
    fi
    if [ -d "$PG_CACHE" ]; then
        mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    fi
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!; BOOTED=1
    deadline=$(($(date +%s) + FIRST_BOOT_TIMEOUT))
    ready=0
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && { ready=1; break; }
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:"; tail -20 "$LOG"; break; }
        sleep 2
    done
    [ "$ready" = "1" ] || { fail "(live) self-boot never became READY"; exit 1; }
    export PENPOT_TOKEN="$(python3 -c "import json;print(json.load(open('$DATA_DIR/credentials.json'))['access_token'])" 2>/dev/null || true)"
    python3 "$ROOT/scripts/n1_index_helper.py" seed "$WORK_DIR" >/dev/null 2>&1 || { fail "(live) seed failed"; exit 1; }
    # wait for the 1 file to be indexed so /__home has a card
    for _ in $(seq 1 60); do
        got="$(curl -fsS "$BASE/__api/vault/status" 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)["filesIndexed"])' 2>/dev/null || echo 0)"
        [ "$got" = "1" ] && break; sleep 2
    done
    pass "(live) self-booted stack seeded + indexed at $BASE"
fi

# --- (LIVE) headless-browser navigation asserts ----------------------------
NAV_OUT="$(PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" ROUTES_GATE_PLAYWRIGHT="$PLAYWRIGHT" \
    "$NODE_BIN" "$NAV_DRIVER" "$BASE" 2>"${WORK_DIR:-/tmp}/nav.err")"
NAV_RC=$?
if [ "$NAV_RC" = "0" ] && echo "$NAV_OUT" | grep -q '"ok":true'; then
    pass "(live) card-click landed on the exact /#/workspace deep link"
    pass "(live) escape hatch landed on /#/dashboard/recent"
    pass "(live) viewer link (Peek Present) landed on /#/view?file-id&page-id&frame-id&section"
    echo "   nav: $NAV_OUT"
else
    fail "(live) navigation asserts failed (rc=$NAV_RC): $NAV_OUT"
    cat "${WORK_DIR:-/tmp}/nav.err" 2>/dev/null >&2 || true
fi

echo
echo "headline: browser=bundled-chromium-headless-shell(playwright) ; $NAV_OUT"
if [ "$FAILURES" -eq 0 ]; then
    echo "ROUTES-GATE: ALL PASS"
    exit 0
else
    echo "ROUTES-GATE: $FAILURES FAILURE(S)"
    exit 1
fi
