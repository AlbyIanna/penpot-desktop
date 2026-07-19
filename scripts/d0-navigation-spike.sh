#!/usr/bin/env bash
# D0 navigation-control SPIKE gate (PLAN4 milestone D0, `just d0`).
#
# Answers: can the webview observe + redirect Penpot's SPA HASH navigation
# WITHOUT touching the SPA (invariant 3)? Assembles Tasks 1-5 (all merged):
#   - GET /__navprobe?run=<case> (apps/desktop/src/navprobe.{rs,html})
#   - PENPOT_LOCAL_NAVWATCH_LOG / _REDIRECT (apps/desktop/src/navwatch.rs)
#   - PENPOT_LOCAL_START_URL (apps/desktop/src/main.rs)
#   - scripts/d0_navprobe.py (reads the navwatch JSONL)
#   - scripts/d0_penpot_nav.cjs (read-only DOM inspection of Penpot's SPA)
#
# CONTROLLER DECISION (2026-07-19, recorded in .superpowers/sdd/progress.md):
# the central hash-observation leg asserts a CONCLUSIVE MEASUREMENT, not a
# direction. The control case (full document nav) MUST be observed or the
# harness itself is broken. The hash case passes iff the probe produced a
# definite, parseable true/false — a NO-GO (not observed) is a legitimate
# result that lowers PLAN4 chapter 4's ceiling; it still passes the gate. The
# GO/NO-GO verdict is written to findings.json as DATA, not asserted as a
# direction. This task (6) does NOT compute `redirectWorks`/`workspaceIntact`
# (Task 7, gated on hashObserved=True) — both are written as null here.
#
# REQUIRES A GUI SESSION — it launches the real Tauri binary (penpot-desktop,
# not headless) so `on_navigation` can be attached to a real window. Not
# CI-headless. Also requires no other Penpot Local GUI instance already
# running: tauri_plugin_single_instance is keyed on the app identifier, not
# our dedicated ports, so a second launch would just focus the existing
# window and exit before ever reaching our probe (pre-flight checked below).
#
# Dedicated ports: proxy 9034, backend 6496, postgres 5569, valkey 6512.
# Control 9037 is reserved for this port block (not bound here — this GUI
# binary has no localhost control server; headless.rs is the one that reads
# PENPOT_LOCAL_CONTROL_PORT) so a sibling gate never lands on it either.
#
# KNOWN RISK: scripts/d0_penpot_nav.cjs waits a fixed 4s before scraping
# anchors. A slow-rendering SPA yields a FALSE NEGATIVE on usesAnchorHref.
# The raw node output is recorded verbatim in findings.json; see the caveat
# line printed next to that result below.
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on
# other port blocks. We kill ONLY the PID this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT="$ROOT"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D0_PROXY_PORT:-9034}"
BACKEND_PORT="${D0_BACKEND_PORT:-6496}"
POSTGRES_PORT="${D0_POSTGRES_PORT:-5569}"
VALKEY_PORT="${D0_VALKEY_PORT:-6512}"
BASE="http://localhost:${PROXY_PORT}"

FIRST_BOOT_TIMEOUT="${D0_FIRST_BOOT_TIMEOUT:-600}"   # fresh data dir, pg-cache seeded
REBOOT_TIMEOUT="${D0_REBOOT_TIMEOUT:-300}"            # data dir already provisioned
SETTLE_SECS=4                                          # covers the probe page's 300ms timer

APP_BIN="$ROOT/target/debug/penpot-desktop"
HEADLESS_BIN="$ROOT/target/debug/headless"
NAVPROBE_PY="$ROOT/scripts/d0_navprobe.py"
PENPOT_NAV_CJS="$ROOT/scripts/d0_penpot_nav.cjs"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-work.XXXXXX")"
FINDINGS="$WORK_DIR/findings.json"

PLAYWRIGHT="${D0_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
NODE_BIN="${D0_NODE:-node}"

APP_PID=""
PASS=0; FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

PG_CACHE="${D0_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY. Never pkill/killall by name (another gate may
# run concurrently on a different port block).
cleanup() {
    if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
        kill -TERM "$APP_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$APP_PID" 2>/dev/null
        wait "$APP_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAIL" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT work=$WORK_DIR"
    fi
}
trap cleanup EXIT

ports_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

echo "== D0 navigation-control spike =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT"
echo "   work:  $WORK_DIR"

# --- pre-flight --------------------------------------------------------------
[ -f "$NAVPROBE_PY" ] || { fail "probe runner missing: $NAVPROBE_PY"; exit 1; }
[ -f "$PENPOT_NAV_CJS" ] || { fail "penpot nav inspector missing: $PENPOT_NAV_CJS"; exit 1; }
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing: $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }
[ -d "$BROWSERS" ] || { fail "bundled browsers missing: $BROWSERS"; exit 1; }
ports_free || { fail "one of the D0 ports is busy"; exit 1; }
# tauri_plugin_single_instance is keyed on the app identifier, not our ports:
# a second GUI launch would silently focus that window and exit immediately,
# never reaching our probe. Refuse to run into that trap.
if pgrep -f "$APP_BIN" >/dev/null 2>&1; then
    fail "a penpot-desktop GUI process is already running (single-instance guard would swallow our launch) — quit it first"
    exit 1
fi
pass "pre-flight: ports free, no existing GUI instance, playwright + browsers present"

# --- build ---------------------------------------------------------------
echo "-- build the GUI binary + headless (penpot-desktop package builds both bins)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop); then
    fail "cargo build -p penpot-desktop"; exit 1
fi
[ -x "$APP_BIN" ] || { fail "built binary missing: $APP_BIN"; exit 1; }
[ -x "$HEADLESS_BIN" ] || { fail "built binary missing: $HEADLESS_BIN"; exit 1; }
pass "cargo build -p penpot-desktop (penpot-desktop + headless)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "   (seeded postgres binaries from $PG_CACHE)"
fi

# --- launch helper -------------------------------------------------------
# Waits for a line containing $needle to appear in $log (the app reaching
# $needle is our readiness signal: main.rs only calls window.navigate(url)
# AFTER the full stack -- postgres/backend/valkey/proxy -- is up, and
# on_navigation records that navigation immediately). Returns 0 on success.
wait_for_log_line() {
    local log="$1" needle="$2" timeout="$3"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -qF "$needle" "$log" 2>/dev/null && return 0
        kill -0 "$APP_PID" 2>/dev/null || { echo "app exited before reaching '$needle'" >&2; return 1; }
        sleep 1
    done
    echo "timed out waiting for '$needle' in $log (${timeout}s)" >&2
    return 1
}

stop_app() {
    [ -n "$APP_PID" ] || return 0
    kill -TERM "$APP_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$APP_PID" 2>/dev/null || { wait "$APP_PID" 2>/dev/null; APP_PID=""; return 0; }; sleep 1; done
    kill -9 "$APP_PID" 2>/dev/null
    wait "$APP_PID" 2>/dev/null
    APP_PID=""
}

# --- probe one navigation case ------------------------------------------------
# Launches the app pointed at /__navprobe?run=<case> against the SHARED
# DATA_DIR/VAULT (so only the very first boot pays full provisioning cost;
# every later boot in this run starts against an already-initialized
# Postgres cluster, same pattern as the sibling gates' second-boot legs).
probe_case() {
    local case="$1" boot_timeout="$2"
    local log="$WORK_DIR/nav-$case.jsonl" applog="$WORK_DIR/app-$case.log"
    : >"$log"
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_NAVWATCH_LOG="$log" \
        PENPOT_LOCAL_START_URL="${BASE}/__navprobe?run=${case}" \
        "$APP_BIN" >"$applog" 2>&1 &
    APP_PID=$!
    if wait_for_log_line "$log" "/__navprobe?run=${case}" "$boot_timeout"; then
        sleep "$SETTLE_SECS"
    else
        echo "   -- boot/nav log tail --" >&2; tail -25 "$applog" >&2 || true
    fi
    stop_app
    python3 "$NAVPROBE_PY" observe "$log" "$case"
}

# (a) CONTROL: a full document navigation MUST be observed. If this fails the
#     harness itself is broken and every other result is meaningless.
echo "-- case: full (control)"
FULL=$(probe_case full "$FIRST_BOOT_TIMEOUT")
echo "     full: $FULL"
FULL_OBSERVED=$(echo "$FULL" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")
if [ "$FULL_OBSERVED" = "True" ]; then
    pass "(control) a full document navigation is observed by on_navigation"
else
    fail "(control) full navigation NOT observed ($FULL_OBSERVED) — harness is broken, results below are meaningless"
fi

# (b) THE CENTRAL QUESTION: is a same-document HASH change observed? Both
#     outcomes measure conclusively; only an inconclusive reading (probe page
#     never loaded / unparseable output) fails this leg.
echo "-- case: hash (central)"
HASH=$(probe_case hash "$REBOOT_TIMEOUT")
echo "     hash: $HASH"
HASH_OBSERVED=$(echo "$HASH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")
if [ "$HASH_OBSERVED" = "True" ] || [ "$HASH_OBSERVED" = "False" ]; then
    pass "(central) conclusive measurement obtained: same-document hash change observed=$HASH_OBSERVED"
    if [ "$HASH_OBSERVED" = "True" ]; then
        echo "     -> DATA: hash change IS observed; redirect would be possible (Task 7)"
    else
        echo "     -> DATA: hash change is NOT observed; ceiling is 'not the default' (NO-GO is legitimate)"
    fi
else
    fail "(central) INCONCLUSIVE — no definite true/false reading ($HASH_OBSERVED); probe page likely never loaded"
fi

# (c) pushState (expected NOT observed on most engines — recorded, not
#     asserted; the brief only wants this echoed as data).
echo "-- case: pushstate (recorded, not asserted)"
PUSH=$(probe_case pushstate "$REBOOT_TIMEOUT")
echo "     pushstate: $PUSH"
PUSH_OBSERVED=$(echo "$PUSH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")

# (d) REALITY CHECK: does Penpot actually navigate via anchor hrefs? This leg
#     only needs a normally reachable stack for the bundled offline chromium
#     to auto-login and inspect the real dashboard DOM — it never touches
#     on_navigation, so it uses the HEADLESS binary (same one every sibling
#     gate boots) rather than the GUI one: no window overhead, and no risk of
#     tripping the single-instance guard.
echo "-- reality check: does Penpot's SPA use anchor hrefs for its routes?"
BOOTLOG="$WORK_DIR/headless-bootstrap.log"
env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
    "$HEADLESS_BIN" >"$BOOTLOG" 2>&1 &
APP_PID=$!
READY=0
DEADLINE=$(($(date +%s) + REBOOT_TIMEOUT))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    strip_ansi <"$BOOTLOG" 2>/dev/null | grep -q "^READY " && { READY=1; break; }
    kill -0 "$APP_PID" 2>/dev/null || break
    sleep 1
done
if [ "$READY" = "1" ]; then
    pass "(reality-check pre-flight) headless stack reached READY at $BASE"
    PENPOT="$(PLAYWRIGHT_MODULE="$PLAYWRIGHT" PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" BASE="$BASE" REPO_ROOT="$ROOT" "$NODE_BIN" "$PENPOT_NAV_CJS" 2>"$WORK_DIR/penpot-nav.err" || true)"
else
    fail "(reality-check pre-flight) headless stack never reached READY within ${REBOOT_TIMEOUT}s"
    tail -25 "$BOOTLOG" >&2 || true
    PENPOT='{"ok":false,"error":"stack not reachable"}'
fi
stop_app
echo "     penpot: $PENPOT"
echo "     CAVEAT: d0_penpot_nav.cjs waits a fixed 4s before scraping anchors —"
echo "             a slower SPA render would read as a FALSE NEGATIVE here."
PENPOT_OK=$(echo "$PENPOT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))' 2>/dev/null || echo "False")
USES_ANCHOR=$(echo "$PENPOT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("usesAnchorHref"))' 2>/dev/null || echo "null")
if [ "$PENPOT_OK" = "True" ]; then
    pass "(reality-check) inspector ran successfully — usesAnchorHref=$USES_ANCHOR (see caveat above)"
else
    fail "(reality-check) d0_penpot_nav.cjs did not complete successfully: $PENPOT"
fi

# --- findings.json -----------------------------------------------------------
# redirectWorks / workspaceIntact are Task 7's job (gated on hashObserved =
# True); this task writes them as null so the shape matches the plan's
# interface even though the measurement hasn't run yet.
python3 - "$FINDINGS" "$FULL_OBSERVED" "$HASH_OBSERVED" "$PUSH_OBSERVED" "$USES_ANCHOR" "$PENPOT" <<'PY'
import json, sys

def to_bool_or_none(s):
    if s == "True":
        return True
    if s == "False":
        return False
    return None

path, full_o, hash_o, push_o, anchor_o, penpot_raw = sys.argv[1:7]
try:
    penpot_json = json.loads(penpot_raw)
except Exception:
    penpot_json = {"ok": False, "raw": penpot_raw}

hash_observed = to_bool_or_none(hash_o)
verdict = "NO-VERDICT (inconclusive)"
if hash_observed is True:
    verdict = "GO (hash change IS observed by on_navigation)"
elif hash_observed is False:
    verdict = "NO-GO (hash change is NOT observed by on_navigation)"

findings = {
    "fullObserved": to_bool_or_none(full_o),
    "hashObserved": hash_observed,
    "pushstateObserved": to_bool_or_none(push_o),
    "usesAnchorHref": to_bool_or_none(anchor_o),
    "redirectWorks": None,
    "workspaceIntact": None,
    "verdict": verdict,
    "notes": {
        "redirectWorks": "not measured by task 6 — task 7 runs only if hashObserved is True",
        "workspaceIntact": "not measured by task 6 — task 7 runs only if hashObserved is True",
        "usesAnchorHrefCaveat": "d0_penpot_nav.cjs uses a fixed 4s post-load wait; a slower SPA render reads as a false negative here",
    },
    "penpotNavRaw": penpot_json,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(findings, f, indent=2, sort_keys=True)
    f.write("\n")
print(json.dumps(findings, indent=2, sort_keys=True))
PY

echo
echo "== findings written to $FINDINGS =="
echo "D0 VERDICT (data, not an assertion of direction): hashObserved=$HASH_OBSERVED"

echo
if [ "$FAIL" -eq 0 ]; then
    echo "D0 NAVIGATION: ALL PASS ($PASS passed)"
else
    echo "D0 NAVIGATION: $FAIL FAILURE(S) ($PASS passed)"
fi
exit "$FAIL"
