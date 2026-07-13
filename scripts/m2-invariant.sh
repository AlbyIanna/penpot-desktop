#!/usr/bin/env bash
# M2 core-invariant test (docs/milestones/m2.md, `just invariant`).
#
# THE milestone exit criterion — PLAN.md calls a failure here a P0:
#   delete the entire Postgres data dir, restart, and every project/file is
#   rebuilt from the folder tree with no data loss.
#
# Steps (each reported PASS/FAIL like m1-smoke.sh):
#   (a) boot headless against a fresh data dir + fresh designs dir; create
#       2 projects / 3 files via RPC, each with a shape; one file also gets an
#       uploaded PNG referenced by an image-filled rect (media coverage);
#   (b) wait until all 3 .penpot dirs are on disk, manifest caught up
#       (revn == DB revn, lastSyncedHash == recomputed disk semantic hash);
#       record the per-file semantic hashes;
#   (c) clean shutdown, no orphans;
#   (d) rm -rf <data_dir>/postgres ONLY (designs, manifest, secret.key,
#       credentials.json survive);
#   (e) boot again: provisioning re-registers, startup reconciliation must
#       re-import everything from disk (imports=3 exports=0 failed=0);
#   (f) RPC: both projects exist, all 3 files exist WITH THE SAME fileIds,
#       shapes intact, media blob byte-identical through /assets/**;
#   (g) disk untouched: semantic hashes identical to (b), no .conflict-* dirs;
#   (h) third boot: reconciliation is a pure no-op — recursive stat of the
#       designs dir (incl. the manifest) unchanged, DB fingerprint unchanged;
#   (i) clean shutdown, no orphans, all 4 ports freed.
#
# Requirements: rust toolchain, runtime/ artifacts, JDK 26 at
# /opt/homebrew/opt/openjdk, valkey-server, ImageMagick, python3, curl.
# First run may download embedded postgres binaries (network, once).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Dedicated ports (never collide with dev 8686/6161/5433/6380,
# m0 9001/6060, m1 smoke 8788/6263/5435/6382).
PROXY_PORT="${M2_INV_PROXY_PORT:-8892}"
BACKEND_PORT="${M2_INV_BACKEND_PORT:-6367}"
POSTGRES_PORT="${M2_INV_POSTGRES_PORT:-5439}"
VALKEY_PORT="${M2_INV_VALKEY_PORT:-6386}"
FIRST_BOOT_TIMEOUT="${M2_INV_TIMEOUT:-900}"   # may embed a postgres download
REBOOT_TIMEOUT=600
SYNC_TIMEOUT=300          # (b) generous: poll 2s + debounce 3s + export ~5s/file
RECONCILE_TIMEOUT=240
BASE="http://localhost:${PROXY_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m2-inv-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m2-inv-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m2-inv-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/m2_invariant_helper.py"
HEADLESS_PID=""
FAILURES=0

export M2_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"      # helper talks through the proxy
export PENPOT_FRONTEND="$BASE"

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
        rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR designs=$DESIGNS_DIR log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { # json_field <key> ; reads JSON on stdin, prints top-level key
    python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"
}

strip_ansi() { # tracing colorizes log fields (ESC[3m etc.) — strip before grepping
    sed -E $'s/\x1b\\[[0-9;]*m//g'
}

start_headless() {
    PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() { # wait_ready <timeout-seconds>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -q "^READY " "$LOG" 2>/dev/null; then
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

wait_log() { # wait_log <timeout-seconds> <grep-pattern>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if grep -q "$2" "$LOG" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died waiting for '$2'" >&2
            tail -20 "$LOG" >&2
            return 1
        fi
        sleep 2
    done
    echo "timed out waiting for log pattern '$2' ($1s)" >&2
    return 1
}

stop_headless() { # SIGTERM + wait; returns 0 on clean exit within 20s
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 20); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            HEADLESS_PID=""
            return 0
        fi
        sleep 1
    done
    return 1
}

check_shutdown() { # check_shutdown <step-label>
    # Record child pids before shutdown so the orphan check cannot
    # false-positive on unrelated processes (same pattern as m1-smoke).
    local label="$1"
    local java_pids stack_pids orphans=""
    java_pids="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
    stack_pids="$(pgrep -f "$DATA_DIR" || true)"
    if stop_headless; then
        for pid in $java_pids $stack_pids; do
            kill -0 "$pid" 2>/dev/null && orphans="$orphans $pid"
        done
        if [ -z "$orphans" ]; then
            pass "$label clean shutdown, no orphan java/valkey/postgres processes"
        else
            fail "$label orphan processes left behind:$orphans"
            ps -o pid,command -p ${orphans} >&2 || true
        fi
    else
        fail "$label headless did not exit within 20s of SIGTERM"
    fi
}

read_token() { # refresh PENPOT_TOKEN from credentials.json (rewritten on re-provision)
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

designs_stat() { # recursive stat fingerprint of the designs dir (incl. manifest)
    find "$DESIGNS_DIR" -print0 2>/dev/null |
        xargs -0 stat -f '%N|%z|%m|%c' 2>/dev/null | LC_ALL=C sort
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            return 1
        fi
    done
    return 0
}

echo "== M2 invariant: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE"

# --- build -------------------------------------------------------------------
# Also build the SIGKILL orphan watchdog sibling (crates/supervisor bin):
# `cargo build -p penpot-desktop` does NOT build dependency-crate bins, and
# without target/debug/penpot-watchdog the boot proceeds watchdog-less (M3).
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"
    exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- (a) first boot + seed content -------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "(a) first boot reaches READY (fresh data dir + fresh designs dir)"
else
    fail "(a) first boot reaches READY"
    exit 1
fi
if ! read_token; then
    fail "(a) no access token in $DATA_DIR/credentials.json"
    exit 1
fi
if SETUP_OUT="$(python3 "$HELPER" setup "$WORK_DIR" 2>"$WORK_DIR/setup.err")"; then
    pass "(a) created 2 projects / 3 files with shapes + uploaded PNG via RPC"
    echo "     $SETUP_OUT"
else
    fail "(a) RPC content setup failed"
    cat "$WORK_DIR/setup.err" >&2
    exit 1
fi

# --- (b) wait for the daemon to mirror everything to disk ---------------------
SYNC_DEADLINE=$(($(date +%s) + SYNC_TIMEOUT))
SYNCED=""
while [ "$(date +%s)" -lt "$SYNC_DEADLINE" ]; do
    CHECK="$(python3 "$HELPER" check "$WORK_DIR" 2>&1)" || { echo "$CHECK" >&2; break; }
    if [ "$CHECK" = "OK" ]; then SYNCED=1; break; fi
    sleep 2
done
if [ -n "$SYNCED" ]; then
    pass "(b) all 3 .penpot dirs on disk, manifest caught up (revn + lastSyncedHash == disk), media blob present"
    cp "$WORK_DIR/hashes.json" "$WORK_DIR/hashes-before.json"
else
    fail "(b) daemon did not mirror all 3 files to disk within ${SYNC_TIMEOUT}s (last: ${CHECK:-?})"
    exit 1
fi

# --- (c) clean shutdown --------------------------------------------------------
check_shutdown "(c)"

# --- (d) surgical DB wipe ------------------------------------------------------
if [ -d "$DATA_DIR/postgres" ] && rm -rf "$DATA_DIR/postgres" &&
    [ ! -e "$DATA_DIR/postgres" ] &&
    [ -f "$DESIGNS_DIR/.penpot-sync.json" ] &&
    [ -f "$DATA_DIR/secret.key" ] &&
    [ -f "$DATA_DIR/credentials.json" ] &&
    [ "$(find "$DESIGNS_DIR" -type d -name '*.penpot' | wc -l | tr -d ' ')" = "3" ]; then
    pass "(d) rm -rf <data>/postgres — manifest, 3 .penpot dirs, secret.key, credentials survive"
else
    fail "(d) surgical postgres wipe (or survivors missing)"
    exit 1
fi

# --- (e) reboot: re-provision + reconciliation re-imports from disk -----------
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT"; then
    pass "(e) second boot reaches READY on the wiped DB (re-provisioning path)"
else
    fail "(e) second boot on wiped DB did not reach READY"
    exit 1
fi
if wait_log "$RECONCILE_TIMEOUT" "startup reconciliation done"; then
    RECON_LINE="$(grep "startup reconciliation done" "$LOG" | tail -1 | strip_ansi)"
    if echo "$RECON_LINE" | grep -q "imports=3" &&
        echo "$RECON_LINE" | grep -q "exports=0" &&
        echo "$RECON_LINE" | grep -q "failed=0"; then
        pass "(e) startup reconciliation re-imported all 3 files from disk (imports=3 exports=0 failed=0)"
    else
        fail "(e) reconciliation counts wrong: $RECON_LINE"
    fi
else
    fail "(e) startup reconciliation did not complete within ${RECONCILE_TIMEOUT}s"
    exit 1
fi
if ! read_token; then
    fail "(e) no access token in credentials.json after re-provisioning"
    exit 1
fi

# --- (f) DB verification: same ids, shapes, media ------------------------------
if VERIFY_OUT="$(python3 "$HELPER" verify "$WORK_DIR" 2>"$WORK_DIR/verify.err")"; then
    pass "(f) both projects + all 3 files rebuilt WITH THE SAME fileIds; shapes and media intact"
else
    fail "(f) post-wipe DB verification"
    cat "$WORK_DIR/verify.err" >&2
fi
echo "$VERIFY_OUT" >"$WORK_DIR/verify.json"
SAME_IDS="$(echo "$VERIFY_OUT" | json_field sameIds 2>/dev/null || echo false)"
echo "     sameIds=$SAME_IDS"

# --- (g) disk untouched by the re-import (idempotence) -------------------------
python3 "$HELPER" hashes "$WORK_DIR" >"$WORK_DIR/hashes-after.json" 2>&1
if cmp -s "$WORK_DIR/hashes-before.json" "$WORK_DIR/hashes-after.json" &&
    [ -z "$(find "$DESIGNS_DIR" -name '*.conflict-*' -print -quit)" ]; then
    pass "(g) on-disk semantic hashes identical to (b); no .conflict-* dirs (reconciliation did not rewrite disk)"
else
    fail "(g) disk changed after the wipe-reimport (or conflict dirs appeared)"
    diff "$WORK_DIR/hashes-before.json" "$WORK_DIR/hashes-after.json" >&2 || true
fi

# --- (h) third boot: pure no-op ------------------------------------------------
python3 "$HELPER" dbstate "$WORK_DIR" >"$WORK_DIR/dbstate-before.json" 2>&1 ||
    fail "(h) dbstate snapshot before the no-op restart"
designs_stat >"$WORK_DIR/stat-before.txt"
check_shutdown "(h-pre)"
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log "$RECONCILE_TIMEOUT" "startup reconciliation done"; then
    RECON_LINE="$(grep "startup reconciliation done" "$LOG" | tail -1 | strip_ansi)"
    if echo "$RECON_LINE" | grep -q "imports=0" &&
        echo "$RECON_LINE" | grep -q "exports=0" &&
        echo "$RECON_LINE" | grep -q "noops=3" &&
        echo "$RECON_LINE" | grep -q "failed=0"; then
        pass "(h) third-boot reconciliation is a pure no-op (imports=0 exports=0 noops=3 failed=0)"
    else
        fail "(h) third-boot reconciliation not a no-op: $RECON_LINE"
    fi
    sleep 8   # let a few poll cycles run — they must not export either
    designs_stat >"$WORK_DIR/stat-after.txt"
    if cmp -s "$WORK_DIR/stat-before.txt" "$WORK_DIR/stat-after.txt"; then
        pass "(h) recursive stat of the designs dir unchanged (mtime/ctime/size, incl. manifest)"
    else
        fail "(h) designs dir stat fingerprint changed across the no-op restart"
        diff "$WORK_DIR/stat-before.txt" "$WORK_DIR/stat-after.txt" >&2 || true
    fi
    read_token || true
    python3 "$HELPER" dbstate "$WORK_DIR" >"$WORK_DIR/dbstate-after.json" 2>&1 ||
        fail "(h) dbstate snapshot after the no-op restart"
    if cmp -s "$WORK_DIR/dbstate-before.json" "$WORK_DIR/dbstate-after.json"; then
        pass "(h) DB fingerprint unchanged (same projects, file revn + modifiedAt)"
    else
        fail "(h) DB changed across the no-op restart"
        diff "$WORK_DIR/dbstate-before.json" "$WORK_DIR/dbstate-after.json" >&2 || true
    fi
else
    fail "(h) third boot did not reach READY + reconciliation"
fi

# --- (i) final shutdown, ports freed -------------------------------------------
if [ -n "$HEADLESS_PID" ]; then
    check_shutdown "(i)"
fi
sleep 1
if ports_all_free; then
    pass "(i) all 4 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT)"
else
    fail "(i) ports still busy after shutdown"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "M2 INVARIANT: ALL PASS (sameIds=$SAME_IDS)"
    exit 0
else
    echo "M2 INVARIANT: $FAILURES FAILURE(S) (sameIds=$SAME_IDS)"
    exit 1
fi
