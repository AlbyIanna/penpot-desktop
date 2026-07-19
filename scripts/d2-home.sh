#!/usr/bin/env bash
# D2 front-door gate (PLAN4 milestone D2, `just d2`, chained into `just e2e` —
# D2 shipped product code: `/__home` can create/rename/duplicate/move/delete
# projects and files, and Penpot's own dashboard is closed off).
#
# Five things this proves, PASS/FAIL like the sibling gates:
#
#   (a) THE LIFECYCLE, end to end, through OUR OWN SURFACES ONLY: new project
#       -> new file in it -> rename -> duplicate -> move the duplicate to a
#       second project -> delete the original. Every step via
#       POST /__api/vault/manage/* (scripts/d2_home_helper.py's `lifecycle`
#       subcommand), never via Penpot's dashboard.
#   (b) THE FOLDER TREE REFLECTS EVERY OPERATION. Never asserted on the first
#       read — the sync daemon polls every ~2s and the vault index lags one
#       further poll behind that (CLAUDE.md gotcha) — every check in
#       `lifecycle` polls with a timeout. The rename check asserts BOTH
#       halves (new name exists AND the old path is gone): the brief calls
#       this out as a real bug this milestone fixed.
#   (c) DELETE LANDS, AND STAYS DELETED ACROSS A RESTART — the load-bearing
#       assertion. After delete: gone from the live tree, present under
#       .trash/, absent from the manifest (checked synchronously, inside
#       `lifecycle`, since manage.rs's delete handler awaits the trash move +
#       manifest save before responding — a poll here would forgive a
#       handler that returned before finishing its own work). Then the WHOLE
#       STACK IS RESTARTED against the SAME data dir + vault, and
#       `assert-disk deleted-stays-deleted` samples continuously over a
#       window for any sign the file came back — on disk, in the manifest,
#       or in the DB (`get-project-files`). `wait-present` runs first as the
#       proof-of-looking gate: if the RPC/disk plumbing can't even see the
#       file that's SUPPOSED to still be there, an absence reading for the
#       deleted one would be vacuous. The startup-reconciliation log line
#       (`sync_daemon::engine`'s `"startup reconciliation done"`, tracing's
#       key=value format) is also checked for `imports=0` — the core
#       invariant re-imports anything on disk but missing from the DB (that
#       is how a wiped database rebuilds); a delete that leaves a live
#       directory with no manifest entry would show up here as `imports=1`
#       on the very next boot.
#   (d) `/dashboard` IS NEVER LOADED IN THE WHOLE SESSION. Two independent
#       legs:
#         * navwatch leg (D0's own mechanism — see d0-navigation-spike.sh):
#           boots the REAL penpot-desktop GUI binary (navwatch's
#           cancel-and-redirect policy is Rust code wired to Tauri's
#           `on_navigation` hook — it is not exercised by a bare browser)
#           with PENPOT_LOCAL_NAVWATCH_LOG set, lets the default boot flow
#           settle at /__home, and asserts no logged navigation URL contains
#           `#/dashboard`. REQUIRES A GUI SESSION, same operational
#           constraint D0 already carries (not CI-headless).
#         * scripts/d2_home_nav.cjs (bundled, offline chromium): proof of
#           looking, then asserts `#escape-hatch` is ABSENT from /__home AND
#           the action controls (button#new-project, button#new-file,
#           button.card-action[data-action][data-file-id]) ARE present — an
#           empty or broken page must never read as "no dashboard" (the
#           d1_surfaces.cjs tri-state + proof-of-render discipline).
#   (e) D0's DEFERRED CAVEAT, discharged: D0 proved the vault survives a
#       mid-session redirect but only ever measured a hand-seeded canary with
#       NO WORKSPACE OPEN. scripts/d2_home_nav.cjs (bundled browser) opens a
#       REAL file at the exact /#/workspace?team-id&file-id&page-id deep
#       link read off a live board card (never hand-constructed), lets it
#       fully render (a route-identifying CSS marker + a generous settle — a
#       short settle silently lies, the D1 finding), attempts a #/dashboard
#       navigation, and re-opens the same file. This script hashes the vault
#       tree (n5_vaults_helper.py's tree_hash — reused, not a third hasher)
#       immediately before and after that whole excursion and asserts it is
#       byte-identical.
#
# Dedicated ports: proxy 9048, backend 6510, postgres 5583, valkey 6526.
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on a
# different port block. We kill ONLY the PID this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT="$ROOT"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D2_PROXY_PORT:-9048}"
BACKEND_PORT="${D2_BACKEND_PORT:-6510}"
POSTGRES_PORT="${D2_POSTGRES_PORT:-5583}"
VALKEY_PORT="${D2_VALKEY_PORT:-6526}"
BASE="http://localhost:${PROXY_PORT}"

FIRST_BOOT_TIMEOUT="${D2_FIRST_BOOT_TIMEOUT:-900}"   # fresh data dir, pg-cache seeded
REBOOT_TIMEOUT="${D2_REBOOT_TIMEOUT:-300}"            # data dir already provisioned
GUI_BOOT_TIMEOUT="${D2_GUI_BOOT_TIMEOUT:-300}"        # data dir already provisioned
DISK_POLL_TIMEOUT="${D2_DISK_POLL_TIMEOUT:-90}"       # per-step: daemon poll (~2s) + index lag
RESTART_SETTLE="${D2_RESTART_SETTLE:-20}"             # window sampled for "did it come back"
GUI_SETTLE_SECS="${D2_GUI_SETTLE_SECS:-10}"           # generous — a short settle silently lies (D1 finding)

HEADLESS_BIN="$ROOT/target/debug/headless"
GUI_BIN="$ROOT/target/debug/penpot-desktop"
HELPER="$ROOT/scripts/d2_home_helper.py"
NAV_CJS="$ROOT/scripts/d2_home_nav.cjs"
N5_HELPER="$ROOT/scripts/n5_vaults_helper.py"
PLAYWRIGHT="${D2_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
NODE_BIN="${D2_NODE:-node}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d2-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d2-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d2-work.XXXXXX")"
LOG1="$WORK_DIR/headless-1.log"
LOG2="$WORK_DIR/headless-2.log"
LOG3="$WORK_DIR/gui-navwatch.jsonl"

HEADLESS_PID=""
GUI_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

PG_CACHE="${D2_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY — never pkill/killall by name.
cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    if [ -n "$GUI_PID" ] && kill -0 "$GUI_PID" 2>/dev/null; then
        kill -TERM "$GUI_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$GUI_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$GUI_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
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

ports_all_free() {
    local p ok=0
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            ok=1
        fi
    done
    return "$ok"
}

wait_ready() {
    local log="$1" timeout="$2"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$log" 2>/dev/null | grep -q "^READY " && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:" >&2; tail -25 "$log" >&2; return 1; }
        sleep 2
    done
    echo "timed out waiting for READY in $log (${timeout}s)" >&2; return 1
}

stop_headless() {
    [ -n "$HEADLESS_PID" ] || return 0
    kill -TERM "$HEADLESS_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || { HEADLESS_PID=""; return 0; }; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""
}

# Mirrors d0-navigation-spike.sh's helper: the app reaching $needle is the
# readiness signal (main.rs only navigates once the full stack is up, and
# on_navigation records it immediately).
wait_for_log_line() {
    local log="$1" needle="$2" timeout="$3"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -qF "$needle" "$log" 2>/dev/null && return 0
        kill -0 "$GUI_PID" 2>/dev/null || { echo "GUI app exited before reaching '$needle'" >&2; return 1; }
        sleep 1
    done
    echo "timed out waiting for '$needle' in $log (${timeout}s)" >&2
    return 1
}

stop_gui() {
    [ -n "$GUI_PID" ] || return 0
    kill -TERM "$GUI_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$GUI_PID" 2>/dev/null || { GUI_PID=""; return 0; }; sleep 1; done
    kill -9 "$GUI_PID" 2>/dev/null; GUI_PID=""
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

# Turns `{"steps":[{"name","ok","detail"}, ...]}` on stdin into tab-separated
# PASS|FAIL/name/detail lines. Never used with a bare pipe-into-while below
# (that runs the loop body in a subshell in bash, and $FAILURES incremented
# there would never reach the parent shell) — always fed via process
# substitution instead.
steps_to_lines() {
    # NOTE: no backslashes inside any f-string {} expression below — Python
    # 3.9 (in use on the dev machine) rejects that with a SyntaxError, unlike
    # 3.12+. Every value is pulled into a plain variable first.
    python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception as e:
    print("FAIL\t(unparseable)\t" + str(e))
    sys.exit(0)
for s in data.get("steps", []):
    verdict = "PASS" if s.get("ok") else "FAIL"
    name = s.get("name", "?")
    detail = s.get("detail", "")
    print(f"{verdict}\t{name}\t{detail}")
'
}

echo "== D2 front-door gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT"

# --- pre-flight --------------------------------------------------------------
[ -f "$HELPER" ] || { fail "lifecycle helper missing: $HELPER"; exit 1; }
[ -f "$NAV_CJS" ] || { fail "nav script missing: $NAV_CJS"; exit 1; }
[ -f "$N5_HELPER" ] || { fail "n5 tree-hash helper missing: $N5_HELPER (reused, not duplicated)"; exit 1; }
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing: $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }
[ -d "$BROWSERS" ] || { fail "bundled browsers missing: $BROWSERS"; exit 1; }
ports_free || { fail "one of the D2 ports is busy"; exit 1; }
if pgrep -f "$GUI_BIN" >/dev/null 2>&1; then
    fail "a penpot-desktop GUI process is already running (single-instance guard would swallow our launch) — quit it first"
    exit 1
fi
pass "pre-flight: ports free, no existing GUI instance, helper + nav script + n5 helper + playwright + browsers present"

if "$NODE_BIN" "$NAV_CJS" selftest >"$WORK_DIR/nav-selftest.log" 2>&1; then
    pass "preflight: node d2_home_nav.cjs selftest"
else
    fail "preflight: node d2_home_nav.cjs selftest failed"
    cat "$WORK_DIR/nav-selftest.log" >&2
fi
if python3 -m py_compile "$HELPER"; then
    pass "preflight: python3 -m py_compile d2_home_helper.py"
else
    fail "preflight: d2_home_helper.py does not compile"
fi

# --- build ---------------------------------------------------------------
echo "-- build (penpot-desktop package builds both the headless and GUI bins)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop); then
    fail "cargo build -p penpot-desktop"; exit 1
fi
[ -x "$HEADLESS_BIN" ] || { fail "built binary missing: $HEADLESS_BIN"; exit 1; }
[ -x "$GUI_BIN" ] || { fail "built binary missing: $GUI_BIN"; exit 1; }
pass "cargo build -p penpot-desktop (headless + penpot-desktop GUI)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "   (seeded postgres binaries from $PG_CACHE)"
fi

BOOT_ENV=(
    PENPOT_LOCAL_DATA_DIR="$DATA_DIR"
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT"
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT"
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT"
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT"
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT"
)

# =========================================================================
# PHASE 1 — headless boot #1: drive the lifecycle (a) + disk assertions (b)
# =========================================================================
echo "-- boot #1 (headless, first boot — provisions postgres)"
env "${BOOT_ENV[@]}" "$HEADLESS_BIN" >"$LOG1" 2>&1 &
HEADLESS_PID=$!
if ! wait_ready "$LOG1" "$FIRST_BOOT_TIMEOUT"; then fail "boot #1 reaches READY"; exit 1; fi
pass "boot #1 reaches READY"
read_token || { fail "no access token in $DATA_DIR/credentials.json after boot #1"; exit 1; }

echo "-- (a)+(b) lifecycle: create -> rename -> duplicate -> move -> delete, through /__api/vault/manage/* only"
export PENPOT_BACKEND="$BASE"
LIFECYCLE_OUT="$(python3 "$HELPER" lifecycle "$WORK_DIR" "$VAULT" "$DISK_POLL_TIMEOUT" 2>"$WORK_DIR/lifecycle.err")"
if echo "$LIFECYCLE_OUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("steps") else 1)' 2>/dev/null; then
    while IFS=$'\t' read -r verdict name detail; do
        if [ "$verdict" = "PASS" ]; then
            pass "(lifecycle) $name: $detail"
        else
            fail "(lifecycle) $name: $detail"
        fi
    done < <(echo "$LIFECYCLE_OUT" | steps_to_lines)
else
    fail "(lifecycle) d2_home_helper.py produced no parseable step list — INFRASTRUCTURE FAILURE: $(cat "$WORK_DIR/lifecycle.err" 2>/dev/null)"
fi

echo "-- clean shutdown of boot #1"
stop_headless
sleep 1
if ports_all_free; then
    pass "boot #1: clean shutdown, all 4 D2 ports freed"
else
    fail "boot #1: ports still busy after shutdown"
fi

# =========================================================================
# PHASE 1b — headless boot #2 (restart): THE load-bearing assertion (c)
# =========================================================================
STATE_FILE="$WORK_DIR/d2-state.json"
if [ ! -f "$STATE_FILE" ]; then
    fail "(c) SKIPPED — no d2-state.json from the lifecycle leg, so the restart/delete-stays-deleted check has nothing to verify against (INFRASTRUCTURE FAILURE carried over from the lifecycle leg above)"
else
    echo "-- boot #2 (restart, SAME data dir + vault — postgres already provisioned)"
    env "${BOOT_ENV[@]}" "$HEADLESS_BIN" >"$LOG2" 2>&1 &
    HEADLESS_PID=$!
    if ! wait_ready "$LOG2" "$REBOOT_TIMEOUT"; then
        fail "boot #2 (restart) reaches READY"
    else
        pass "boot #2 (restart) reaches READY"
        read_token || { fail "no access token in $DATA_DIR/credentials.json after boot #2"; }

        RECON_LINE="$(strip_ansi <"$LOG2" | grep "startup reconciliation done" | tail -1)"
        if [ -z "$RECON_LINE" ]; then
            fail "(c) no 'startup reconciliation done' log line in boot #2 — cannot verify imports=0 (INFRASTRUCTURE FAILURE)"
        elif echo "$RECON_LINE" | grep -q "imports=0 "; then
            pass "(c) startup reconciliation reported imports=0: $RECON_LINE"
        else
            fail "(c) startup reconciliation did NOT report imports=0 — something on disk needed importing that should not have (the deleted file resurrecting looks exactly like this): $RECON_LINE"
        fi

        if python3 "$HELPER" wait-present "$WORK_DIR" "$VAULT" "$DISK_POLL_TIMEOUT" >"$WORK_DIR/wait-present.log" 2>&1; then
            pass "(c) proof-of-looking: the surviving file (the moved duplicate) reconciled correctly after the restart"
        else
            fail "(c) proof-of-looking FAILED — the surviving file did not come back after the restart; an absence reading for the deleted file would be meaningless: $(cat "$WORK_DIR/wait-present.log")"
        fi

        DELETED_OUT="$(python3 "$HELPER" assert-disk "$WORK_DIR" "$VAULT" "$RESTART_SETTLE" deleted-stays-deleted 2>"$WORK_DIR/assert-disk.err")"
        if echo "$DELETED_OUT" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("ok") else 1)' 2>/dev/null; then
            pass "(c) THE LOAD-BEARING CHECK: the deleted file stayed deleted across a restart (sampled continuously over ${RESTART_SETTLE}s — disk, manifest, and DB): $DELETED_OUT"
        else
            fail "(c) THE LOAD-BEARING CHECK FAILED: the deleted file came back (or the check itself errored): $DELETED_OUT $(cat "$WORK_DIR/assert-disk.err" 2>/dev/null)"
        fi

        # --- (d, browser leg) + (e): one authenticated Playwright session ---
        if python3 "$HELPER" wait-board-indexed "$WORK_DIR" "$DISK_POLL_TIMEOUT" >"$WORK_DIR/wait-board.log" 2>&1; then
            pass "(e) proof-of-looking: the surviving board is indexed (vault-index has caught up) ahead of the browser leg"
        else
            fail "(e) the surviving board never got indexed — the browser leg below would find no board card to open: $(cat "$WORK_DIR/wait-board.log")"
        fi

        HASH_BEFORE="$(python3 "$N5_HELPER" tree_hash "$VAULT")"
        NAV_OUT="$(BASE="$BASE" REPO_ROOT="$ROOT" PLAYWRIGHT_MODULE="$PLAYWRIGHT" PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" \
            "$NODE_BIN" "$NAV_CJS" 2>"$WORK_DIR/nav.err")"
        HASH_AFTER="$(python3 "$N5_HELPER" tree_hash "$VAULT")"
        echo "     nav: $NAV_OUT"

        NAV_OK="$(echo "$NAV_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))' 2>/dev/null || echo "ERROR")"
        if [ "$NAV_OK" != "True" ]; then
            fail "(d/e) d2_home_nav.cjs did not complete successfully — INFRASTRUCTURE FAILURE, no browser-leg measurement below is trustworthy: $NAV_OUT $(cat "$WORK_DIR/nav.err" 2>/dev/null)"
            HATCH="inconclusive"; ACTIONS="inconclusive"; WS="inconclusive"; REOPEN="inconclusive"
        else
            HATCH="$(echo "$NAV_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("escapeHatch"))')"
            ACTIONS="$(echo "$NAV_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("actionsPresent"))')"
            WS="$(echo "$NAV_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("workspaceRendered"))')"
            REOPEN="$(echo "$NAV_OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("reopenRendered"))')"
        fi

        case "$HATCH" in
            gone) pass "(d) #escape-hatch is ABSENT from /__home — the upstream dashboard link is gone, not just hidden" ;;
            present) fail "(d) #escape-hatch is PRESENT on /__home — the upstream dashboard is still one click away" ;;
            *) fail "(d) #escape-hatch check is INCONCLUSIVE ($HATCH) — /__home never demonstrably rendered, so absence proves nothing" ;;
        esac
        case "$ACTIONS" in
            yes) pass "(d) proof of looking: /__home's action controls (new-project, new-file, card-action) are present" ;;
            no) fail "(d) /__home rendered but the action controls are MISSING — see actionCounts in the nav output above" ;;
            *) fail "(d) action-controls check is INCONCLUSIVE ($ACTIONS) — /__home never demonstrably rendered" ;;
        esac
        case "$WS" in
            yes) pass "(e) D0's deferred caveat: a REAL workspace (deep link read off a live board card) fully rendered" ;;
            *) fail "(e) the real workspace never demonstrably rendered ($WS) — the deep-link/render mechanism is broken" ;;
        esac
        case "$REOPEN" in
            yes) pass "(e) the file still opens after the attempted #/dashboard navigation" ;;
            *) fail "(e) re-opening the same file after the #/dashboard excursion did NOT demonstrably render ($REOPEN)" ;;
        esac

        if [ -n "$HASH_BEFORE" ] && [ -n "$HASH_AFTER" ] && [ "$HASH_BEFORE" = "$HASH_AFTER" ]; then
            pass "(e) vault tree hash unchanged across the real-workspace-open + #/dashboard-nav excursion (before=$HASH_BEFORE)"
        else
            fail "(e) vault tree hash CHANGED across the excursion (before=$HASH_BEFORE after=$HASH_AFTER) — a mere navigation attempt touched the user's files"
        fi
    fi

    echo "-- clean shutdown of boot #2"
    stop_headless
    sleep 1
    if ports_all_free; then
        pass "boot #2: clean shutdown, all 4 D2 ports freed"
    else
        fail "boot #2: ports still busy after shutdown"
    fi
fi

# =========================================================================
# PHASE 2 — GUI boot: THE navwatch leg of (d) (D0's own mechanism)
# =========================================================================
echo "-- boot #3 (GUI, penpot-desktop — REQUIRES A GUI SESSION, same operational constraint as D0)"
if pgrep -f "$GUI_BIN" >/dev/null 2>&1; then
    fail "(d/navwatch) a penpot-desktop GUI process is already running — refusing to launch a second one"
else
    : >"$LOG3"
    env "${BOOT_ENV[@]}" PENPOT_LOCAL_NAVWATCH_LOG="$LOG3" "$GUI_BIN" >"$WORK_DIR/gui.log" 2>&1 &
    GUI_PID=$!
    # Wait on the literal URL main.rs's default flow navigates to first
    # (window.navigate({proxy}/__bootstrap)) — guaranteed to be observed and
    # logged the instant it happens (D0's own readiness idiom). NOT
    # "/__home": whether the server-side 302 to /__home produces a SEPARATE
    # on_navigation observation is a webview-engine detail this script does
    # not assume either way. A generous settle AFTER this signal (below)
    # gives that redirect — and anything downstream of it — time to land and
    # be logged before we read the file, rather than chasing a second
    # uncertain string match.
    if wait_for_log_line "$LOG3" "/__bootstrap" "$GUI_BOOT_TIMEOUT"; then
        pass "(d/navwatch) GUI boot reached the default entry point (/__bootstrap)"
        sleep "$GUI_SETTLE_SECS"
    else
        fail "(d/navwatch) GUI never reached /__bootstrap within ${GUI_BOOT_TIMEOUT}s"
        echo "   -- app log tail --" >&2; tail -25 "$WORK_DIR/gui.log" >&2 || true
    fi
    stop_gui

    if [ -s "$LOG3" ]; then
        LOG3_LINES="$(wc -l <"$LOG3" | tr -d ' ')"
        pass "(d/navwatch) proof of life: the navwatch log recorded $LOG3_LINES observation(s)"
        if grep -q '/__home' "$LOG3"; then
            echo "     (the redirect to /__home after bootstrap WAS observed as its own navigation — confirms the settle window covered it)"
        else
            echo "     NOTE: /__home was not observed as a separate navigation line; the #/dashboard check below still covers every line that WAS logged"
        fi
        if grep -qF '#/dashboard' "$LOG3"; then
            fail "(d/navwatch) THE ASSERTION FAILED: a navigation URL containing #/dashboard was logged during the session: $(grep -F '#/dashboard' "$LOG3")"
        else
            pass "(d/navwatch) THE ASSERTION: /dashboard was never loaded anywhere in the session — no logged navigation URL contains #/dashboard"
        fi
    else
        fail "(d/navwatch) the navwatch log is empty — INFRASTRUCTURE FAILURE, no navigation was ever observed, so absence of #/dashboard proves nothing"
    fi

    sleep 1
    if ports_all_free; then
        pass "boot #3 (GUI): clean shutdown, all 4 D2 ports freed"
    else
        fail "boot #3 (GUI): ports still busy after shutdown"
    fi
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D2 HOME: ALL PASS"
else
    echo "D2 HOME: $FAILURES FAILURE(S)"
fi
exit "$FAILURES"
