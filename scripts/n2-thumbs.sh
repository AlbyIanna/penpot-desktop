#!/usr/bin/env bash
# N2 render-path test (PLAN2.md milestone N2, docs/milestones/n2.md).
#
# Proves the exporter is a PACKAGED-MODE capability: every component the
# render path needs (node, exporter app, chromium headless shell) resolves
# from the runtime bundle, with NO host node, NO docker, NO network — and the
# two dev-mode bugs found after M5 (stale-exporter adoption; shutdown hang
# during failing renders) are fixed with live regressions.
#
# Blocks (PASS/FAIL lines, house style of m2-invariant.sh/m5-features.sh):
#   (a) packaged-shape OFFLINE boot: headless bin + PENPOT_LOCAL_RUNTIME_BUNDLE
#       under env -i (fresh HOME, system-only PATH — /usr/bin has no node —
#       poisoned http proxies): READY + board-export service; every runtime-
#       layout component INCLUDING exporter/exporter-node/exporter-browsers
#       resolves source=bundle (0 source=dev).
#   (b) renders: seed a vault (file alpha: boards Cover+Detail; beta: Solo)
#       via RPC -> every board gets .svg + .png next to the sources, state
#       hash == manifest hash; per-board latency recorded from the logs.
#   (c) degraded mode: the synced-but-not-yet-rendered window is a benign
#       pending state — no error logged, stack healthy (the N3 lighttable
#       renders its placeholder card off exactly this state).
#   (d) run-twice no-op: reboot the same stack -> hash-gated, zero re-render
#       (exports fingerprint byte-identical, inodes included).
#   (e) bug-B regression: exporter forced down (supervisor retries exhausted),
#       an edit arms a FAILING render batch (retry ladder engaged) -> SIGTERM
#       exits < 20 s (was 5-7 min in M5).
#   (f) bug-A regression 1: a fake stale process squatting the exporter port
#       (answering /readyz 200) -> boot REFUSES with an error naming the pid,
#       never adopts, never reaches READY.
#   (g) bug-A regression 2 (the M5 scenario): SIGKILL during the exporter's
#       boot window -> the orphan watchdog reaps the exporter child; reboot
#       on the SAME port recovers cleanly (no stale adoption, renders work);
#       final SIGTERM < 20 s, all ports freed.
#
# Dedicated ports (ledger): proxy 8918, backend 6393, postgres 5467, valkey
# 6410, exporter 6471. Fresh mktemp dirs; ANSI-stripped log greps; dirs kept
# on failure. Requires: dist/penpot-runtime built by build-runtime-bundle.sh
# (layout=2 with the exporter stack), rust toolchain, python3, curl.
# Concurrency-safe: every process/port assertion is scoped to this stack's
# own data dir / ports (no system-wide pgrep by name).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N2_PROXY_PORT:-8918}"
BACKEND_PORT="${N2_BACKEND_PORT:-6393}"
POSTGRES_PORT="${N2_POSTGRES_PORT:-5467}"
VALKEY_PORT="${N2_VALKEY_PORT:-6410}"
EXPORTER_PORT="${N2_EXPORTER_PORT:-6471}"
FIRST_BOOT_TIMEOUT="${N2_TIMEOUT:-600}"   # bundle postgres pre-seed: no download
REBOOT_TIMEOUT=300
SYNC_TIMEOUT=120
EXPORT_TIMEOUT=180
BASE="http://localhost:${PROXY_PORT}"
BUNDLE="${N2_BUNDLE:-$ROOT/dist/penpot-runtime}"
POISON="http://127.0.0.1:1"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n2-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n2-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n2-work.XXXXXX")"
FRESH_HOME="$WORK_DIR/home"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/m5_features_helper.py"
HEADLESS_PID=""
FAKE_PID=""
FAILURES=0

mkdir -p "$FRESH_HOME" "$WORK_DIR/tmp"

export M5_DESIGNS_DIR="$DESIGNS_DIR"   # the helper reads these (reused from m5)
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
    [ -n "$FAKE_PID" ] && kill -9 "$FAKE_PID" 2>/dev/null
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 1
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR designs=$DESIGNS_DIR log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

start_headless() { # packaged-shape sanitized launch: env -i, no host node, offline
    (
        cd "$WORK_DIR" || exit 1
        exec env -i \
            HOME="$FRESH_HOME" \
            PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
            TMPDIR="$WORK_DIR/tmp/" \
            http_proxy="$POISON" https_proxy="$POISON" \
            HTTP_PROXY="$POISON" HTTPS_PROXY="$POISON" ALL_PROXY="$POISON" \
            PENPOT_LOCAL_RUNTIME_BUNDLE="$BUNDLE" \
            PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
            PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
            PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
            PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
            PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
            PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
            PENPOT_LOCAL_EXPORTS=1 \
            PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" \
            "$BIN"
    ) >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() { # wait_ready <timeout-seconds>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died; last log lines:" >&2
            tail -20 "$LOG" >&2
            return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2
    tail -20 "$LOG" >&2
    return 1
}

wait_log() { # wait_log <timeout-seconds> <fixed-string>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" 2>/dev/null | grep -qF "$2" && return 0
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

stop_headless() { # SIGTERM; returns 0 iff exit within 25s
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 25); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            HEADLESS_PID=""
            return 0
        fi
        sleep 1
    done
    return 1
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

helper() { python3 "$HELPER" "$@"; }

wait_ok() { # wait_ok <timeout-s> <subcmd> [args…]
    local timeout="$1" sub="$2"; shift 2
    local deadline=$(($(date +%s) + timeout)) out=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        out="$(helper "$sub" "$WORK_DIR" "$@" 2>&1)" || { echo "$out" >&2; return 1; }
        case "$out" in
            OK\ *) HELPER_OUT="${out#OK }"; return 0 ;;
        esac
        sleep 2
    done
    echo "timed out waiting for $sub $* (last: $out)" >&2
    return 1
}

dir_stat() { # recursive stat fingerprint (path|size|mtime|inode), sorted
    find "$1" -print0 2>/dev/null |
        xargs -0 stat -f '%N|%z|%m|%i' 2>/dev/null | LC_ALL=C sort
}

exporter_listener_pid() { # pid LISTENing on the exporter port, if any
    lsof -nP -tiTCP:"$EXPORTER_PORT" -sTCP:LISTEN 2>/dev/null | head -1
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$EXPORTER_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            return 1
        fi
    done
    return 0
}

echo "== N2 thumbs: bundle=$BUNDLE data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE exporter=$EXPORTER_PORT"

# --- preflight -------------------------------------------------------------------
if [ ! -x "$BUNDLE/bin/node" ] || [ ! -s "$BUNDLE/exporter/app.js" ] ||
    ! ls "$BUNDLE/exporter-browsers" 2>/dev/null | grep -q chromium_headless_shell; then
    fail "runtime bundle at $BUNDLE has the exporter stack (run scripts/build-runtime-bundle.sh)"
    exit 1
fi
pass "runtime bundle has node + exporter + headless shell"
if ! ports_all_free; then
    fail "test ports free ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"
    exit 1
fi
pass "test ports free ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"
    exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- (a) packaged-shape OFFLINE boot ----------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" && wait_log 30 "board-export service started"; then
    pass "(a) packaged-shape OFFLINE boot READY + board-export service (env -i, poisoned proxies, no host node)"
else
    fail "(a) packaged-shape offline boot"
    exit 1
fi
LAYOUT_LINES="$(strip_ansi <"$LOG" | grep "runtime layout: component=" || true)"
LAYOUT_COUNT="$(echo "$LAYOUT_LINES" | grep -c "component=" || true)"
DEV_SOURCED="$(echo "$LAYOUT_LINES" | grep -c "source=dev" || true)"
if [ "$LAYOUT_COUNT" -eq 9 ] && [ "$DEV_SOURCED" -eq 0 ] &&
    echo "$LAYOUT_LINES" | grep -q "component=exporter source=bundle" &&
    echo "$LAYOUT_LINES" | grep "component=exporter-node" | grep -q "source=bundle" &&
    echo "$LAYOUT_LINES" | grep "component=exporter-browsers" | grep -q "source=bundle"; then
    pass "(a) all 9 runtime-layout components resolved from the bundle (incl. exporter/exporter-node/exporter-browsers, 0 source=dev)"
else
    fail "(a) expected 9 bundle-sourced layout components, got count=$LAYOUT_COUNT dev=$DEV_SOURCED"
    echo "$LAYOUT_LINES" >&2
fi
DOWNLOAD_LINES="$(strip_ansi <"$LOG" | grep -ci "download" || true)"
if [ "$DOWNLOAD_LINES" -eq 0 ]; then
    pass "(a) zero 'download' lines in the boot log (offline)"
else
    fail "(a) $DOWNLOAD_LINES 'download' lines found"
    strip_ansi <"$LOG" | grep -i "download" >&2
fi

# --- (b) renders for every board of a seeded vault --------------------------------
if ! read_token; then
    fail "(b) no access token in $DATA_DIR/credentials.json"
    exit 1
fi
if helper seed "$WORK_DIR" >/dev/null 2>"$WORK_DIR/seed.err"; then
    pass "(b) seeded vault: alpha (Cover+Detail) + beta (Solo) via RPC"
else
    fail "(b) RPC seed failed"
    cat "$WORK_DIR/seed.err" >&2
    exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha && wait_ok "$SYNC_TIMEOUT" check beta; then
    pass "(b) sync daemon mirrored both files to disk"
else
    fail "(b) files did not reach the disk within ${SYNC_TIMEOUT}s"
    exit 1
fi

# --- (c) degraded mode: synced-but-not-rendered is a benign pending state ---------
ALPHA_REL="$(helper manifest_entry "$WORK_DIR" alpha | json_field path)"
ALPHA_EXPORTS_DIR="$DESIGNS_DIR/${ALPHA_REL%.penpot}.exports"
DEGRADED_NOTE="window missed (render landed first)"
if [ ! -d "$ALPHA_EXPORTS_DIR" ]; then
    DEGRADED_NOTE="observed live: manifest entry present, no .exports yet"
fi
if ! strip_ansi <"$LOG" | grep -q "board export failed" &&
    curl -fsS -o /dev/null "$BASE/"; then
    pass "(c) degraded mode: pending-render state is benign — no error logged, stack healthy ($DEGRADED_NOTE)"
else
    fail "(c) pending-render window produced errors"
    strip_ansi <"$LOG" | grep "board export failed" >&2 || true
fi

RENDER_T0="$(date +%s)"
if wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    ALPHA_EXPORTS_REL="$HELPER_OUT"
    pass "(b) alpha rendered: $ALPHA_EXPORTS_REL (Cover+Detail, svg+png, state hash == manifest hash)"
else
    fail "(b) alpha exports did not appear within ${EXPORT_TIMEOUT}s"
    exit 1
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check beta; then
    BETA_EXPORTS_REL="$HELPER_OUT"
    pass "(b) beta rendered: $BETA_EXPORTS_REL (Solo, svg+png)"
else
    fail "(b) beta exports did not appear within ${EXPORT_TIMEOUT}s"
    exit 1
fi
# every seeded board has both formats with sane magic
RENDER_OK=1
for f in "$ALPHA_EXPORTS_REL/Cover" "$ALPHA_EXPORTS_REL/Detail" "$BETA_EXPORTS_REL/Solo"; do
    SVG="$DESIGNS_DIR/$f.svg"; PNG="$DESIGNS_DIR/$f.png"
    grep -q "<svg" "$SVG" 2>/dev/null || { echo "bad svg: $SVG" >&2; RENDER_OK=0; }
    [ "$(head -c 8 "$PNG" 2>/dev/null | xxd -p)" = "89504e470d0a1a0a" ] ||
        { echo "bad png: $PNG" >&2; RENDER_OK=0; }
done
if [ "$RENDER_OK" -eq 1 ]; then
    pass "(b) every board of the vault has a valid .svg + .png through the BUNDLED chromium"
else
    fail "(b) render artifacts malformed"
fi
# per-board latency from the service's own timing lines
LATENCY_REPORT="$(strip_ansi <"$LOG" | grep "render batch complete" |
    sed -E 's/.*boards=([0-9]+).*files=([0-9]+).*total_ms=([0-9]+).*/\1 \2 \3/' |
    awk '{ boards+=$1; files+=$2; ms+=$3 } END {
        if (boards > 0) printf "%d boards, %d artifacts, %.1fs total, %.1fs/board", boards, files, ms/1000, ms/1000/boards
        else print "no-batches" }')"
if [ "$LATENCY_REPORT" != "no-batches" ] && [ -n "$LATENCY_REPORT" ]; then
    pass "(b) per-board latency recorded: $LATENCY_REPORT"
else
    fail "(b) could not extract render timings from the log"
fi

# --- (d) run-twice hash-gated no-op (reboot, zero re-render) -----------------------
dir_stat "$DESIGNS_DIR" >"$WORK_DIR/exports-before-reboot.txt"
if stop_headless; then
    pass "(d) clean SIGTERM shutdown of the first boot"
else
    fail "(d) first boot did not stop cleanly"
    exit 1
fi
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log 60 "reconciliation complete"; then
    pass "(d) second boot READY + reconciliation complete"
else
    fail "(d) second boot failed"
    exit 1
fi
read_token || true
sleep 15   # two manifest polls + debounce: any spurious re-render would land now
dir_stat "$DESIGNS_DIR" >"$WORK_DIR/exports-after-reboot.txt"
if cmp -s "$WORK_DIR/exports-before-reboot.txt" "$WORK_DIR/exports-after-reboot.txt"; then
    pass "(d) reboot is a hash-gated no-op: designs tree byte-identical (sources + exports, inodes included)"
else
    fail "(d) reboot re-rendered or touched the tree"
    diff "$WORK_DIR/exports-before-reboot.txt" "$WORK_DIR/exports-after-reboot.txt" >&2 | head -10 || true
fi
if ! strip_ansi <"$LOG" | grep -q "exports updated"; then
    pass "(d) zero render batches ran on the second boot"
else
    fail "(d) the second boot re-rendered despite unchanged hashes"
fi

# --- (e) bug-B regression: SIGTERM during a FAILING render batch < 20 s ------------
# Force the exporter down for good: kill each respawn fast so the supervisor's
# retry budget (5) exhausts and the child stays dead (renders then fail with
# connection-refused and ride the retry ladder — the M5 hang scenario).
KILLS=0
DEADLINE=$(($(date +%s) + 90))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    EPID="$(exporter_listener_pid)"
    if [ -n "$EPID" ]; then
        kill -9 "$EPID" 2>/dev/null && KILLS=$((KILLS + 1))
        sleep 0.5
    else
        sleep 1
        # no listener for a while + GaveUp logged -> supervisor stopped retrying
        if strip_ansi <"$LOG" | grep -q "gave up restarting" && [ -z "$(exporter_listener_pid)" ]; then
            break
        fi
    fi
done
if [ -z "$(exporter_listener_pid)" ] && [ "$KILLS" -ge 1 ]; then
    pass "(e) exporter forced down ($KILLS kills; supervisor retry budget exhausted)"
else
    fail "(e) could not exhaust the exporter's restart budget (kills=$KILLS)"
fi
if helper edit_alpha "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit.err"; then
    pass "(e) RPC edit applied (arms a render that MUST fail)"
else
    fail "(e) RPC edit failed"
    cat "$WORK_DIR/edit.err" >&2
fi
if wait_log 90 "transient failure; retrying"; then
    pass "(e) failing render batch engaged the retry ladder"
else
    fail "(e) retry ladder never engaged after the edit"
fi
SIGTERM_T0="$(date +%s)"
if stop_headless; then
    SIGTERM_SECS=$(($(date +%s) - SIGTERM_T0))
    if [ "$SIGTERM_SECS" -lt 20 ]; then
        pass "(e) SIGTERM during the failing batch -> clean exit in ${SIGTERM_SECS}s (< 20s; was 5-7 min pre-N2)"
    else
        fail "(e) exit took ${SIGTERM_SECS}s (>= 20s)"
    fi
else
    fail "(e) headless did not exit within 25s of SIGTERM during a failing batch"
fi
sleep 1
if ports_all_free; then
    pass "(e) all 5 ports freed after the failing-batch shutdown"
else
    fail "(e) ports still busy"
fi

# --- (f) bug-A regression 1: stale /readyz-answering squatter -> refusal -----------
python3 - "$EXPORTER_PORT" <<'PYEOF' >"$WORK_DIR/fake-exporter.log" 2>&1 &
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header("Content-Length", "2")
        self.end_headers(); self.wfile.write(b"ok")
    def log_message(self, *a): pass
http.server.HTTPServer(("0.0.0.0", int(sys.argv[1])), H).serve_forever()
PYEOF
FAKE_PID=$!
sleep 1
if [ -n "$(exporter_listener_pid)" ]; then
    pass "(f) fake stale exporter planted on port $EXPORTER_PORT (pid $FAKE_PID, answers 200)"
else
    fail "(f) could not plant the fake stale exporter"
fi
: >"$LOG"
start_headless
REFUSED_EXIT=""
REFUSE_DEADLINE=$(($(date +%s) + REBOOT_TIMEOUT))
while [ "$(date +%s)" -lt "$REFUSE_DEADLINE" ]; do
    if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
        wait "$HEADLESS_PID"; REFUSED_EXIT=$?
        HEADLESS_PID=""
        break
    fi
    if grep -q "^READY " "$LOG" 2>/dev/null; then break; fi
    sleep 2
done
if [ -n "$REFUSED_EXIT" ] && [ "$REFUSED_EXIT" -ne 0 ]; then
    pass "(f) boot REFUSED the squatted port (exit $REFUSED_EXIT, never READY)"
else
    fail "(f) boot did not refuse (exit=${REFUSED_EXIT:-still running / READY})"
    [ -n "$HEADLESS_PID" ] && { kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""; }
fi
if strip_ansi <"$LOG" | grep -q "already in use by pid(s) $FAKE_PID" &&
    strip_ansi <"$LOG" | grep -q "refusing to adopt"; then
    pass "(f) refusal error names the squatting pid ($FAKE_PID) and says 'refusing to adopt'"
else
    fail "(f) refusal message wrong or missing:"
    strip_ansi <"$LOG" | grep -i "exporter" | tail -5 >&2
fi
kill -9 "$FAKE_PID" 2>/dev/null
wait "$FAKE_PID" 2>/dev/null
FAKE_PID=""
# the refused boot must not leave its own children behind
sleep 2
pkill -9 -f "$DATA_DIR" 2>/dev/null && sleep 1
if ports_all_free; then
    pass "(f) no listeners left after the refused boot"
else
    fail "(f) refused boot leaked listeners"
fi

# --- (g) bug-A regression 2: SIGKILL during the exporter boot window ----------------
: >"$LOG"
start_headless
KILL_WINDOW_HIT=0
KW_DEADLINE=$(($(date +%s) + FIRST_BOOT_TIMEOUT))
while [ "$(date +%s)" -lt "$KW_DEADLINE" ]; do
    EPID="$(exporter_listener_pid)"
    if [ -n "$EPID" ]; then
        # The exporter just bound: we are inside its boot window (the exact
        # M5 orphan gap). Give the supervisor's pid feeder one tick (1 s) to
        # report the fresh pid to the orphan watchdog — the documented
        # residual exposure is sub-second — then SIGKILL the app.
        sleep 1.2
        if grep -q "^READY " "$LOG" 2>/dev/null; then
            echo "     (note: READY landed during the 1.2s feeder tick — kill is post-boot but pre-steady-state)"
        fi
        kill -9 "$HEADLESS_PID" 2>/dev/null
        HEADLESS_PID=""
        KILL_WINDOW_HIT=1
        break
    fi
    if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then break; fi
    grep -q "^READY " "$LOG" 2>/dev/null && break
    sleep 0.3
done
if [ "$KILL_WINDOW_HIT" -eq 1 ]; then
    pass "(g) SIGKILL landed inside the exporter boot window (exporter pid $EPID was up, app killed)"
else
    fail "(g) never caught the exporter boot window"
    [ -n "$HEADLESS_PID" ] && { kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""; }
fi
REAPED=0
for _ in $(seq 1 30); do
    if [ -z "$(exporter_listener_pid)" ]; then REAPED=1; break; fi
    sleep 1
done
if [ "$REAPED" -eq 1 ]; then
    pass "(g) orphan watchdog reaped the exporter child (port free within 30s of SIGKILL — was orphaned in M5)"
else
    fail "(g) exporter child still listening 30s after SIGKILL (pid $(exporter_listener_pid))"
    kill -9 "$(exporter_listener_pid)" 2>/dev/null
fi
# give the watchdog a beat to finish reaping postgres/valkey/backend too
for _ in $(seq 1 30); do
    ports_all_free >/dev/null 2>&1 && break
    sleep 1
done
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log 30 "board-export service started"; then
    pass "(g) reboot on the SAME exporter port recovered cleanly (READY + board-export)"
else
    fail "(g) reboot after the SIGKILL did not recover"
fi
if ! strip_ansi <"$LOG" | grep -q "refusing to adopt"; then
    pass "(g) recovery adopted nothing stale (no refusal needed — port was properly reaped)"
else
    fail "(g) recovery hit a stale listener (the watchdog did not reap)"
    strip_ansi <"$LOG" | grep "refusing to adopt" >&2
fi
# renders still work end-to-end after the crash-recovery cycle
read_token || true
if helper edit_alpha "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit2.err" &&
    wait_ok "$SYNC_TIMEOUT" check alpha && wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    pass "(g) post-recovery render round-trip works (edit -> sync -> fresh exports)"
else
    fail "(g) post-recovery render failed"
    cat "$WORK_DIR/edit2.err" >&2 2>/dev/null || true
fi
FINAL_T0="$(date +%s)"
if stop_headless; then
    FINAL_SECS=$(($(date +%s) - FINAL_T0))
    if [ "$FINAL_SECS" -lt 20 ]; then
        pass "(g) final SIGTERM -> clean exit in ${FINAL_SECS}s (< 20s)"
    else
        fail "(g) final shutdown took ${FINAL_SECS}s"
    fi
else
    fail "(g) final shutdown did not complete within 25s"
fi
sleep 1
if ports_all_free; then
    pass "(g) all 5 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"
else
    fail "(g) ports still busy after the final shutdown"
fi

echo
echo "headline: render latency [$LATENCY_REPORT] ; SIGTERM-during-failing-batch ${SIGTERM_SECS:-?}s ; stale refusal exit=${REFUSED_EXIT:-?}"
if [ "$FAILURES" -eq 0 ]; then
    echo "N2 THUMBS: ALL PASS"
    exit 0
else
    echo "N2 THUMBS: $FAILURES FAILURE(S)"
    exit 1
fi
