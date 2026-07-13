#!/usr/bin/env bash
# M3 two-way-sync + conflict test (docs/milestones/m3.md, `just m3`).
#
# THE milestone exit criteria as an executable PASS/FAIL test:
#   1. `git checkout` an older version of a file directory -> it appears in
#      Penpot within seconds (latency measured and reported);
#   2. a simultaneous-edit test produces a conflict copy — NEVER data loss.
#
# Steps (each reported PASS/FAIL like m1-smoke.sh / m2-invariant.sh):
#   (a) boot headless on fresh dirs; seed 1 project + 1 file with a shape via
#       RPC; wait until the daemon mirrors it to disk; record semantic hash v1;
#   (b) `git init` the designs root, commit everything (v1);
#   (c) RPC-edit the file (add a second shape) -> daemon exports v2 ->
#       git commit v2; assert v1 hash != v2 hash;
#   (d) EXIT CRITERION 1: `git checkout --no-overlay <v1> -- <file dir>`
#       (--no-overlay so the v2-only shape's JSON is actually DELETED; plain
#       pathspec checkout is overlay-mode and leaves files added since v1) ->
#       the watcher imports it; poll get-file until the v2-only shape is gone;
#       measure the latency; assert a fresh export's semantic hash == v1 hash;
#   (e) loop prevention around (d): idle poll cycles produce ZERO further
#       imports/exports (ANSI-stripped log op counters frozen), the designs
#       dir stat fingerprint is byte-stable, DB (revn,modifiedAt) frozen;
#   (f) EXIT CRITERION 2 (simultaneous edit): pause the daemon (SIGUSR1
#       test hook in the headless bin -> SyncControl) -> edit the file ON DISK
#       (rename a shape, renormalized) AND via RPC (add a different shape) ->
#       resume -> exactly ONE <name>.conflict-<ts>.penpot dir appears; it holds
#       the DB version (hash == pre-resume fresh-export hash); the DISK version
#       is in the DB (get-file shows the renamed shape, not the RPC one); the
#       manifest is consistent; NEITHER version's content lost;
#   (g) the conflict copy is inert: more idle cycles -> no churn, conflict dir
#       untouched, no new conflict dirs;
#   (h) clean shutdown, no orphans, all 4 ports freed; then a SIGKILL-variant
#       boot: SIGKILL after the two-way daemon is active -> the orphan
#       watchdog reaps every child within its grace, ports freed.
#
# Requirements: rust toolchain, runtime/ artifacts, JDK 26 at
# /opt/homebrew/opt/openjdk, valkey-server, git, python3, curl.
#
# Embedded-postgres cache: a FRESH data dir means postgresql_embedded would
# re-download the binaries every run via the GitHub *API* (rate-limited to
# 60/h unauthenticated — a second back-to-back run can 403). The script keeps
# a persistent copy of <data>/postgres/install under M3_SYNC_PG_CACHE
# (default ~/.cache/penpot-local/pg-install) and seeds each fresh data dir
# from it: the crate sees install/<version>/ and goes fully offline.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Dedicated ports (ledger in docs/milestones/m3.md; never collide with dev
# 8686/6161/5433/6380 or the other tests' ports).
PROXY_PORT="${M3_SYNC_PROXY_PORT:-8900}"
BACKEND_PORT="${M3_SYNC_BACKEND_PORT:-6375}"
POSTGRES_PORT="${M3_SYNC_POSTGRES_PORT:-5451}"
VALKEY_PORT="${M3_SYNC_VALKEY_PORT:-6394}"
FIRST_BOOT_TIMEOUT="${M3_SYNC_TIMEOUT:-900}"   # may embed a postgres download
REBOOT_TIMEOUT=600
SYNC_TIMEOUT=120          # poll 2s + DB debounce 3s + export ~5s
REVERT_BOUND=30           # exit criterion 1 upper bound (real number reported)
CONFLICT_BOUND=30         # conflict copy must appear within this after resume
BASE="http://localhost:${PROXY_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m3-sync-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m3-sync-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m3-sync-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/m3_sync_helper.py"
HEADLESS_PID=""
FAILURES=0
GIT=(git -C "$DESIGNS_DIR" -c user.email=m3@test.local -c user.name="M3 Sync Test" -c commit.gpgsign=false)

export M3_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"      # helper talks through the proxy
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

PG_CACHE="${M3_SYNC_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

save_pg_cache() { # persist the downloaded postgres binaries for future runs
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE for future runs)"
    fi
}

cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 15); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    save_pg_cache
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

wait_log() { # wait_log <timeout-seconds> <grep-pattern> (fixed string, ANSI-stripped)
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if strip_ansi <"$LOG" 2>/dev/null | grep -qF "$2"; then
            return 0
        fi
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died waiting for '$2'" >&2
            tail -20 "$LOG" >&2
            return 1
        fi
        sleep 1
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
    # false-positive on unrelated processes (same pattern as m1/m2 scripts).
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

read_token() { # refresh PENPOT_TOKEN from credentials.json
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

designs_stat() { # recursive stat fingerprint of the designs dir (incl. manifest + .git)
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

# Total count of actual sync OPERATIONS in the log (imports + exports, both
# directions, ANSI-stripped). Own-write/no-op skips log at debug and never
# match — this is the loop-prevention counter.
sync_op_count() {
    strip_ansi <"$LOG" | grep -cE \
        "imported disk → DB|imported unknown disk dir → DB|exported DB → disk|sync FS→DB complete|sync DB→FS complete" || true
}

conflict_dirs() { # newline-separated conflict-copy dirs under the designs root
    find "$DESIGNS_DIR" -type d -name '*.conflict-*.penpot' 2>/dev/null
}

wait_check_ok() { # wait_check_ok <timeout-s>; sets CHECK_HASH / CHECK_PATH
    local deadline=$(($(date +%s) + $1)) out=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        out="$(python3 "$HELPER" check "$WORK_DIR" 2>&1)" || { echo "$out" >&2; return 1; }
        case "$out" in
            OK\ *)
                CHECK_HASH="$(echo "$out" | cut -d' ' -f2)"
                CHECK_PATH="$(echo "$out" | cut -d' ' -f3-)"
                return 0 ;;
        esac
        sleep 2
    done
    echo "timed out waiting for the daemon to catch up (last: $out)" >&2
    return 1
}

echo "== M3 sync: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE"

# --- build -------------------------------------------------------------------
# Also build the SIGKILL orphan watchdog sibling: `cargo build -p
# penpot-desktop` does NOT build dependency-crate bins, and without
# target/debug/penpot-watchdog the boot proceeds watchdog-less.
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"
    exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- (a) boot + seed + first sync to disk -------------------------------------
if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE — no download needed)"
fi
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "(a) boot reaches READY (fresh data dir + fresh designs dir)"
else
    fail "(a) boot reaches READY"
    exit 1
fi
if ! read_token; then
    fail "(a) no access token in $DATA_DIR/credentials.json"
    exit 1
fi
if SETUP_OUT="$(python3 "$HELPER" setup "$WORK_DIR" 2>"$WORK_DIR/setup.err")"; then
    pass "(a) created project 'M3 Sync' + file 'homepage' with a shape via RPC"
else
    fail "(a) RPC content setup failed"
    cat "$WORK_DIR/setup.err" >&2
    exit 1
fi
if wait_check_ok "$SYNC_TIMEOUT"; then
    V1_HASH="$CHECK_HASH"; FILE_REL="$CHECK_PATH"
    pass "(a) daemon mirrored the file to disk; manifest consistent (v1 hash recorded)"
    echo "     file dir: $FILE_REL"
    echo "     v1 hash : $V1_HASH"
else
    fail "(a) daemon did not mirror the file to disk within ${SYNC_TIMEOUT}s"
    exit 1
fi

# --- (b) git init + commit v1 --------------------------------------------------
if "${GIT[@]}" init -q &&
    "${GIT[@]}" add -A &&
    "${GIT[@]}" commit -q -m "v1: one shape" &&
    V1_COMMIT="$("${GIT[@]}" rev-parse HEAD)"; then
    pass "(b) git init + commit v1 in the designs root ($V1_COMMIT)"
else
    fail "(b) git init/commit v1"
    exit 1
fi

# --- (c) RPC edit -> daemon exports v2 -> commit v2 ----------------------------
if python3 "$HELPER" edit2 "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit2.err"; then
    pass "(c) RPC edit applied (second shape added)"
else
    fail "(c) RPC edit failed"
    cat "$WORK_DIR/edit2.err" >&2
    exit 1
fi
if wait_check_ok "$SYNC_TIMEOUT"; then
    V2_HASH="$CHECK_HASH"
    pass "(c) daemon exported v2 to disk; manifest consistent"
    echo "     v2 hash : $V2_HASH"
else
    fail "(c) daemon did not export v2 within ${SYNC_TIMEOUT}s"
    exit 1
fi
if [ "$V1_HASH" != "$V2_HASH" ]; then
    pass "(c) v1 hash != v2 hash (the edit is semantically visible on disk)"
else
    fail "(c) v1 and v2 hashes are identical — edit not visible"
    exit 1
fi
if "${GIT[@]}" add -A && "${GIT[@]}" commit -q -m "v2: two shapes"; then
    pass "(c) git commit v2"
else
    fail "(c) git commit v2"
    exit 1
fi

# --- (d) EXIT CRITERION 1: git checkout v1 -> appears in Penpot ----------------
# --no-overlay: pathspec checkout defaults to overlay mode, which does NOT
# delete files added since v1 (the v2 shape's JSON would survive and the
# "revert" would be a merge). --no-overlay makes it a true revert of the dir.
if "${GIT[@]}" checkout -q --no-overlay "$V1_COMMIT" -- "$FILE_REL"; then
    pass "(d) git checkout --no-overlay v1 -- '$FILE_REL'"
else
    fail "(d) git checkout of the v1 file dir"
    exit 1
fi
if REVERT_LATENCY="$(python3 "$HELPER" wait_revert "$WORK_DIR" "$REVERT_BOUND" 2>"$WORK_DIR/revert.err")"; then
    pass "(d) EXIT CRITERION 1: v1 content visible in Penpot ${REVERT_LATENCY}s after checkout (bound ${REVERT_BOUND}s)"
else
    fail "(d) v1 content did not appear in Penpot within ${REVERT_BOUND}s"
    cat "$WORK_DIR/revert.err" >&2
    exit 1
fi
if wait_check_ok "$SYNC_TIMEOUT" && [ "$CHECK_HASH" = "$V1_HASH" ]; then
    pass "(d) manifest consistent after the import; disk hash == v1 hash"
else
    fail "(d) manifest/disk did not settle back to v1 (got ${CHECK_HASH:-?})"
fi
EXPORT_AFTER_REVERT="$(python3 "$HELPER" export_hash "$WORK_DIR" 2>&1)"
if [ "$EXPORT_AFTER_REVERT" = "$V1_HASH" ]; then
    pass "(d) fresh DB export semantic hash == v1 hash (full content revert, ids preserved)"
else
    fail "(d) fresh DB export hash != v1 hash (export=$EXPORT_AFTER_REVERT v1=$V1_HASH)"
fi

# --- (e) loop prevention: idle cycles are pure no-ops --------------------------
OPS_BEFORE="$(sync_op_count)"
designs_stat >"$WORK_DIR/stat-e-before.txt"
DBSTATE_BEFORE="$(python3 "$HELPER" dbstate "$WORK_DIR" 2>&1)"
sleep 10   # ~5 poll cycles + many fs ticks
OPS_AFTER="$(sync_op_count)"
DBSTATE_AFTER="$(python3 "$HELPER" dbstate "$WORK_DIR" 2>&1)"
designs_stat >"$WORK_DIR/stat-e-after.txt"
if [ "$OPS_BEFORE" = "$OPS_AFTER" ]; then
    pass "(e) zero further imports/exports across 10s of idle polling (op count frozen at $OPS_AFTER)"
else
    fail "(e) sync op count moved during idle: $OPS_BEFORE -> $OPS_AFTER"
    strip_ansi <"$LOG" | grep -E "imported disk → DB|exported DB → disk|sync (FS→DB|DB→FS) complete" | tail -5 >&2
fi
if cmp -s "$WORK_DIR/stat-e-before.txt" "$WORK_DIR/stat-e-after.txt"; then
    pass "(e) designs dir stat fingerprint stable across idle (size/mtime/ctime, incl. manifest)"
else
    fail "(e) designs dir changed during idle"
    diff "$WORK_DIR/stat-e-before.txt" "$WORK_DIR/stat-e-after.txt" >&2 || true
fi
if [ "$DBSTATE_BEFORE" = "$DBSTATE_AFTER" ]; then
    pass "(e) DB (revn, modifiedAt) frozen across idle — no import bounce"
else
    fail "(e) DB state moved during idle: $DBSTATE_BEFORE -> $DBSTATE_AFTER"
fi

# --- (f) EXIT CRITERION 2: simultaneous edit -> conflict copy ------------------
kill -USR1 "$HEADLESS_PID"
if wait_log 15 "SIGUSR1: sync paused"; then
    pass "(f) daemon paused via the SIGUSR1 test hook"
else
    fail "(f) pause did not take effect"
    exit 1
fi
if DISK_HASH="$(python3 "$HELPER" disk_edit "$WORK_DIR" 2>"$WORK_DIR/diskedit.err")"; then
    pass "(f) on-disk edit applied while paused (shape renamed, renormalized)"
    echo "     disk hash: $DISK_HASH"
else
    fail "(f) on-disk edit failed"
    cat "$WORK_DIR/diskedit.err" >&2
    exit 1
fi
if python3 "$HELPER" edit_db "$WORK_DIR" >/dev/null 2>"$WORK_DIR/editdb.err"; then
    pass "(f) RPC edit applied while paused (different shape added in the DB)"
else
    fail "(f) RPC edit failed"
    cat "$WORK_DIR/editdb.err" >&2
    exit 1
fi
# Prove the pause is real: several poll periods with both sides dirty and the
# daemon must touch NEITHER (no import of the disk edit, no conflict copy).
DBSTATE_PAUSED="$(python3 "$HELPER" dbstate "$WORK_DIR" 2>&1)"
sleep 6
DBSTATE_PAUSED2="$(python3 "$HELPER" dbstate "$WORK_DIR" 2>&1)"
if [ "$DBSTATE_PAUSED" = "$DBSTATE_PAUSED2" ] && [ -z "$(conflict_dirs)" ]; then
    pass "(f) paused daemon touched neither side for 6s (DB frozen, no conflict copies yet)"
else
    fail "(f) paused daemon acted: db '$DBSTATE_PAUSED' -> '$DBSTATE_PAUSED2', conflicts: $(conflict_dirs | wc -l | tr -d ' ')"
fi
# The DB version that MUST survive in the conflict copy (RPC edit included).
CONFLICT_EXPECTED="$(python3 "$HELPER" export_hash "$WORK_DIR" 2>&1)"
if [ "$CONFLICT_EXPECTED" != "$DISK_HASH" ]; then
    pass "(f) DB version and disk version are semantically distinct (real divergence staged)"
    echo "     db (expected conflict-copy) hash: $CONFLICT_EXPECTED"
else
    fail "(f) staged edits are semantically identical — test would be vacuous"
    exit 1
fi

kill -USR1 "$HEADLESS_PID"
if wait_log 15 "SIGUSR1: sync resumed"; then
    pass "(f) daemon resumed via the SIGUSR1 test hook"
else
    fail "(f) resume did not take effect"
    exit 1
fi
CONFLICT_DEADLINE=$(($(date +%s) + CONFLICT_BOUND))
CONFLICT_DIR=""
CONFLICT_T0="$(python3 -c 'import time; print(f"{time.time():.2f}")')"
while [ "$(date +%s)" -lt "$CONFLICT_DEADLINE" ]; do
    CONFLICT_DIR="$(conflict_dirs | head -1)"
    [ -n "$CONFLICT_DIR" ] && break
    sleep 0.5
done
CONFLICT_LATENCY="$(python3 -c "import time; print(f'{time.time() - $CONFLICT_T0:.2f}')")"
N_CONFLICTS="$(conflict_dirs | grep -c . || true)"
if [ -n "$CONFLICT_DIR" ] && [ "$N_CONFLICTS" = "1" ]; then
    pass "(f) EXIT CRITERION 2: exactly one conflict copy appeared ${CONFLICT_LATENCY}s after resume"
    echo "     conflict dir: ${CONFLICT_DIR#"$DESIGNS_DIR"/}"
else
    fail "(f) expected exactly 1 conflict dir within ${CONFLICT_BOUND}s, found ${N_CONFLICTS}"
    exit 1
fi
CONFLICT_HASH="$(python3 "$HELPER" dir_hash "$WORK_DIR" "$CONFLICT_DIR" 2>&1)"
if [ "$CONFLICT_HASH" = "$CONFLICT_EXPECTED" ]; then
    pass "(f) conflict copy holds the DB version (semantic hash == pre-resume DB export)"
else
    fail "(f) conflict copy hash mismatch (got $CONFLICT_HASH want $CONFLICT_EXPECTED)"
fi
if wait_check_ok "$SYNC_TIMEOUT" && [ "$CHECK_HASH" = "$DISK_HASH" ]; then
    pass "(f) live file dir untouched (== disk edit) and manifest consistent (revn + lastSyncedHash)"
else
    fail "(f) live dir / manifest not settled on the disk version (got ${CHECK_HASH:-?} want $DISK_HASH)"
fi
SHAPES="$(python3 "$HELPER" shapes "$WORK_DIR" "M3 Rect One (disk)" "M3 Rect DB" "M3 Rect One" 2>&1)"
if [ "$(echo "$SHAPES" | json_field "M3 Rect One (disk)")" = "True" ] &&
    [ "$(echo "$SHAPES" | json_field "M3 Rect DB")" = "False" ]; then
    pass "(f) DB now holds the DISK version (renamed shape present, RPC-only shape gone from the live file)"
else
    fail "(f) DB does not reflect the disk version: $SHAPES"
fi
# No data loss: both versions' content is findable — the DB version in the
# conflict copy (hash-verified above), the disk version live (hash-verified
# above). Belt-and-braces: the RPC-only shape's name is present inside the
# conflict copy's JSON.
if grep -rqF '"M3 Rect DB"' "$CONFLICT_DIR"; then
    pass "(f) NEVER data loss: the RPC edit survives inside the conflict copy; the disk edit is live"
else
    fail "(f) the RPC-only shape is missing from the conflict copy"
fi

# --- (g) the conflict copy is inert --------------------------------------------
OPS_BEFORE="$(sync_op_count)"
stat -f '%N|%z|%m|%c' "$CONFLICT_DIR" >"$WORK_DIR/conflict-stat-before.txt"
find "$CONFLICT_DIR" -print0 | xargs -0 stat -f '%N|%z|%m|%c' | LC_ALL=C sort >>"$WORK_DIR/conflict-stat-before.txt"
sleep 8    # ~4 poll cycles + fs ticks
OPS_AFTER="$(sync_op_count)"
stat -f '%N|%z|%m|%c' "$CONFLICT_DIR" >"$WORK_DIR/conflict-stat-after.txt"
find "$CONFLICT_DIR" -print0 | xargs -0 stat -f '%N|%z|%m|%c' | LC_ALL=C sort >>"$WORK_DIR/conflict-stat-after.txt"
N_CONFLICTS="$(conflict_dirs | grep -c . || true)"
if [ "$OPS_BEFORE" = "$OPS_AFTER" ] && [ "$N_CONFLICTS" = "1" ] &&
    cmp -s "$WORK_DIR/conflict-stat-before.txt" "$WORK_DIR/conflict-stat-after.txt"; then
    pass "(g) conflict copy is inert: no churn over 8s idle, dir untouched, no new conflict dirs"
else
    fail "(g) post-conflict churn: ops $OPS_BEFORE->$OPS_AFTER, conflicts=$N_CONFLICTS"
    diff "$WORK_DIR/conflict-stat-before.txt" "$WORK_DIR/conflict-stat-after.txt" >&2 || true
fi

# --- (h) clean shutdown, then the SIGKILL/watchdog variant ---------------------
check_shutdown "(h)"
sleep 1
if ports_all_free; then
    pass "(h) all 4 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT)"
else
    fail "(h) ports still busy after clean shutdown"
fi

: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log 120 "reconciliation complete"; then
    pass "(h) SIGKILL-variant boot: READY with the two-way sync daemon active (watcher + poll loop)"
else
    fail "(h) SIGKILL-variant boot did not reach READY + active daemon"
    exit 1
fi
if strip_ansi <"$LOG" | grep -qF "orphan watchdog armed"; then
    pass "(h) orphan watchdog armed on this boot"
else
    fail "(h) orphan watchdog was NOT armed (missing penpot-watchdog binary?)"
fi
kill -9 "$HEADLESS_PID"
HEADLESS_PID=""
KILL_DEADLINE=$(($(date +%s) + 30))
REAPED=""
while [ "$(date +%s)" -lt "$KILL_DEADLINE" ]; do
    if [ -z "$(pgrep -f "$DATA_DIR" || true)" ] && ports_all_free 2>/dev/null; then
        REAPED=1
        break
    fi
    sleep 1
done
if [ -n "$REAPED" ]; then
    pass "(h) SIGKILL: watchdog reaped every child within 30s (no process references the data dir, all 4 ports freed)"
else
    fail "(h) orphans survived SIGKILL:"
    pgrep -lf "$DATA_DIR" >&2 || true
    lsof -nP -iTCP:"$PROXY_PORT" -iTCP:"$BACKEND_PORT" -iTCP:"$POSTGRES_PORT" -iTCP:"$VALKEY_PORT" -sTCP:LISTEN >&2 || true
fi

echo
echo "headline latencies: git-checkout->Penpot ${REVERT_LATENCY:-?}s ; resume->conflict-copy ${CONFLICT_LATENCY:-?}s"
if [ "$FAILURES" -eq 0 ]; then
    echo "M3 SYNC: ALL PASS"
    exit 0
else
    echo "M3 SYNC: $FAILURES FAILURE(S)"
    exit 1
fi
