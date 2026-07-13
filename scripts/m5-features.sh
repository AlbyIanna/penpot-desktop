#!/usr/bin/env bash
# M5 freedom-features test (docs/milestones/m5.md, `just m5`).
#
# One feature = one verifiable block, each reported PASS/FAIL like the other
# milestone scripts (m1-smoke / m2-invariant / m3-sync):
#   (a) per-board auto-export: boot with PENPOT_LOCAL_EXPORTS=1, seed a
#       project with file "alpha" (boards Cover+Detail) and "beta" (Solo) ->
#       <name>.exports/<board>.{svg,png} appear next to the sources, SVG is
#       valid XML, PNG has the right magic bytes; an RPC edit re-renders ONLY
#       the changed file (the other file's exports fingerprint is
#       byte-identical, inodes included); idle cycles cause zero churn.
#   (b) OS-side rename: `mv alpha.penpot alpha-renamed.penpot` -> the SAME
#       file id is renamed in Penpot (rename-file), revn continuity, no
#       import-as-new (project file count unchanged), and the exports dir
#       follows to the new path (stale one left at the old path — documented).
#   (c) OS-side move across project folders: `mv` into a brand-new folder ->
#       a project is created and the file's DB project membership changes;
#       same file id; modifiedAt byte-identical (move-files does not bump it).
#   (d) rename while STOPPED: startup reconciliation re-keys the manifest
#       under the same id (no import-as-new duplicate, no orphan re-export
#       at the old path).
#   (e) designs-git-init.sh: fresh dir -> repo + .gitignore + README with the
#       no-overlay lesson + exactly one commit; second run idempotent;
#       pre-existing repo -> files written but NO commit (no-op on history).
#   (f) non-BMP (emoji) data dir -> clean refusal: exit code 1 within
#       seconds, error names the path + offending char, no crash-loop, no
#       processes/listeners left behind.
#   (g) single-instance: a second headless launch against the same data dir
#       fails loudly (postgres postmaster.pid lock, M4-verified) while the
#       first instance stays healthy. NOTE: tauri-plugin-single-instance
#       covers the GUI app only (second GUI launch focuses the first window —
#       verified live in the M5 hardening workstream); the headless bin has
#       no window to focus, so the loud postgres-lock failure IS the intended
#       headless behavior and is what this block asserts.
#
# Dedicated ports (ledger): proxy 8910, backend 6385, postgres 5459, valkey
# 6402, exporter 6467. Fresh mktemp dirs; ANSI-stripped log greps.
#
# Requirements: rust toolchain, runtime/ artifacts INCLUDING the extracted
# exporter (scripts/fetch-penpot.sh) + playwright chromium
# (fetch-penpot.sh --with-browsers), node (default /opt/homebrew/bin/node,
# override PENPOT_LOCAL_NODE), JDK 26, valkey-server, git, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${M5_PROXY_PORT:-8910}"
BACKEND_PORT="${M5_BACKEND_PORT:-6385}"
POSTGRES_PORT="${M5_POSTGRES_PORT:-5459}"
VALKEY_PORT="${M5_VALKEY_PORT:-6402}"
EXPORTER_PORT="${M5_EXPORTER_PORT:-6467}"
FIRST_BOOT_TIMEOUT="${M5_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
SYNC_TIMEOUT=120           # 2s poll + 3s debounce + export
EXPORT_TIMEOUT=180         # + 2s manifest poll + 3s render debounce + renders
RENAME_TIMEOUT=60
BASE="http://localhost:${PROXY_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m5-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m5-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m5-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/m5_features_helper.py"
GIT_HELPER="$ROOT/scripts/designs-git-init.sh"
HEADLESS_PID=""
FAILURES=0

export M5_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
skip() { echo "SKIPPED: $1"; }

PG_CACHE="${M5_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

save_pg_cache() {
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
        for _ in $(seq 1 20); do
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

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

start_headless() { # start_headless [extra env as VAR=VAL words]
    env "$@" \
        PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_EXPORTS=1 \
        PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
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

stop_headless() {
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

# helper <subcommand> <workdir> [args…]
helper() { python3 "$HELPER" "$@"; }

wait_ok() { # wait_ok <timeout-s> <subcmd> [args…]  (subcmd prints OK …/WAIT …)
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

echo "== M5 features: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE exporter=$EXPORTER_PORT"

# --- build ---------------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"
    exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- exporter prerequisites (dev-mode; documented host requirements) ------------
NODE_BIN="${PENPOT_LOCAL_NODE:-/opt/homebrew/bin/node}"
if [ ! -f "$ROOT/runtime/exporter/app.js" ] || [ ! -x "$NODE_BIN" ] ||
    ! ls "$ROOT/runtime/exporter-browsers" 2>/dev/null | grep -q chromium; then
    fail "exporter prerequisites missing — run scripts/fetch-penpot.sh --with-browsers and install node"
    exit 1
fi
pass "exporter prerequisites present (runtime/exporter, node, playwright chromium)"

# --- (a) auto-export -------------------------------------------------------------
if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE — no download needed)"
fi
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" && wait_log 30 "board-export service started"; then
    pass "(a) boot READY with the exporter child + board-export service (PENPOT_LOCAL_EXPORTS=1)"
else
    fail "(a) boot with exports enabled"
    exit 1
fi
if ! read_token; then
    fail "(a) no access token in $DATA_DIR/credentials.json"
    exit 1
fi
if helper seed "$WORK_DIR" >/dev/null 2>"$WORK_DIR/seed.err"; then
    pass "(a) seeded project 'ProjectA': alpha (boards Cover+Detail) + beta (board Solo)"
else
    fail "(a) RPC seed failed"
    cat "$WORK_DIR/seed.err" >&2
    exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha && wait_ok "$SYNC_TIMEOUT" check beta; then
    pass "(a) sync daemon mirrored both files to disk (manifest consistent)"
else
    fail "(a) files did not reach the disk within ${SYNC_TIMEOUT}s"
    exit 1
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    ALPHA_EXPORTS_REL="$HELPER_OUT"
    pass "(a) alpha exports rendered: $ALPHA_EXPORTS_REL (Cover+Detail, svg+png, state hash == manifest hash)"
else
    fail "(a) alpha exports did not appear within ${EXPORT_TIMEOUT}s"
    exit 1
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check beta; then
    BETA_EXPORTS_REL="$HELPER_OUT"
    pass "(a) beta exports rendered: $BETA_EXPORTS_REL (Solo, svg+png)"
else
    fail "(a) beta exports did not appear within ${EXPORT_TIMEOUT}s"
    exit 1
fi
ALPHA_SVG="$DESIGNS_DIR/$ALPHA_EXPORTS_REL/Cover.svg"
ALPHA_PNG="$DESIGNS_DIR/$ALPHA_EXPORTS_REL/Cover.png"

# Well-formedness probe: expat with entity declarations rejected (guards
# against XXE/entity-expansion even though the SVG is our own local render).
xml_well_formed() {
    python3 - "$1" <<'PYEOF'
import sys
import xml.parsers.expat as expat
p = expat.ParserCreate()
def deny(*_a):
    sys.exit(3)  # any entity/DTD declaration -> reject
p.EntityDeclHandler = deny
p.ExternalEntityRefHandler = lambda *a: 0
try:
    with open(sys.argv[1], "rb") as fh:
        p.ParseFile(fh)
except expat.ExpatError:
    sys.exit(1)
PYEOF
}
if xml_well_formed "$ALPHA_SVG" &&
    grep -q "<svg" "$ALPHA_SVG"; then
    pass "(a) Cover.svg is valid XML with an <svg> root"
else
    fail "(a) Cover.svg is not valid SVG/XML"
fi
if [ "$(head -c 8 "$ALPHA_PNG" | xxd -p)" = "89504e470d0a1a0a" ]; then
    pass "(a) Cover.png has the PNG magic bytes"
else
    fail "(a) Cover.png magic bytes wrong: $(head -c 8 "$ALPHA_PNG" | xxd -p)"
fi
# idle -> no churn (whole designs tree incl. exports dirs; size/mtime/inode)
dir_stat "$DESIGNS_DIR" >"$WORK_DIR/idle-before.txt"
sleep 10
dir_stat "$DESIGNS_DIR" >"$WORK_DIR/idle-after.txt"
if cmp -s "$WORK_DIR/idle-before.txt" "$WORK_DIR/idle-after.txt"; then
    pass "(a) idle: designs tree (sources + exports) byte-stable across 10s (zero churn)"
else
    fail "(a) idle churn detected"
    diff "$WORK_DIR/idle-before.txt" "$WORK_DIR/idle-after.txt" >&2 | head -10 || true
fi
# edit alpha -> ONLY alpha re-renders
ALPHA_STATE_BEFORE="$(json_field renderedFromHash <"$DESIGNS_DIR/$ALPHA_EXPORTS_REL/.exports-state.json")"
dir_stat "$DESIGNS_DIR/$BETA_EXPORTS_REL" >"$WORK_DIR/beta-exports-before.txt"
if helper edit_alpha "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit.err"; then
    pass "(a) RPC edit applied to alpha (rect added inside Cover)"
else
    fail "(a) RPC edit failed"
    cat "$WORK_DIR/edit.err" >&2
    exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha && wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    ALPHA_STATE_AFTER="$(json_field renderedFromHash <"$DESIGNS_DIR/$ALPHA_EXPORTS_REL/.exports-state.json")"
    if [ "$ALPHA_STATE_AFTER" != "$ALPHA_STATE_BEFORE" ]; then
        pass "(a) alpha re-rendered from the new hash after the edit"
    else
        fail "(a) alpha exports state hash did not move after the edit"
    fi
else
    fail "(a) alpha did not re-render within ${EXPORT_TIMEOUT}s of the edit"
fi
# The renderer emits shape GEOMETRY (fills as rgb()/hex), not shape names —
# assert on the edit rect's unique fill color #12B886 = rgb(18,184,134).
if grep -Eiq "12B886|rgb\( *18, *184, *134 *\)" "$ALPHA_SVG"; then
    pass "(a) the edited shape's fill is present in the fresh Cover.svg"
else
    fail "(a) edited shape's fill missing from the fresh Cover.svg"
fi
dir_stat "$DESIGNS_DIR/$BETA_EXPORTS_REL" >"$WORK_DIR/beta-exports-after.txt"
if cmp -s "$WORK_DIR/beta-exports-before.txt" "$WORK_DIR/beta-exports-after.txt"; then
    pass "(a) ONLY the changed file re-rendered (beta exports fingerprint identical, inodes included)"
else
    fail "(a) beta exports changed although only alpha was edited"
    diff "$WORK_DIR/beta-exports-before.txt" "$WORK_DIR/beta-exports-after.txt" >&2 || true
fi

# --- (b) OS-side rename of a .penpot dir -----------------------------------------
ALPHA_BEFORE="$(helper info "$WORK_DIR" alpha)"
ALPHA_ID="$(echo "$ALPHA_BEFORE" | json_field id)"
ALPHA_REVN="$(echo "$ALPHA_BEFORE" | json_field revn)"
PROJECT_A_ID="$(helper manifest_entry "$WORK_DIR" alpha | json_field projectId)"
FILES_BEFORE_N="$(helper project_files "$WORK_DIR" "$PROJECT_A_ID" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ALPHA_REL="$(helper manifest_entry "$WORK_DIR" alpha | json_field path)"
mv "$DESIGNS_DIR/$ALPHA_REL" "$DESIGNS_DIR/${ALPHA_REL%.penpot}-renamed.penpot"
if RENAME_LATENCY="$(helper wait_name "$WORK_DIR" alpha "alpha-renamed" "$RENAME_TIMEOUT" 2>"$WORK_DIR/rename.err")"; then
    pass "(b) mv -> rename-file visible in Penpot in ${RENAME_LATENCY}s (same file id $ALPHA_ID)"
else
    fail "(b) DB rename did not happen within ${RENAME_TIMEOUT}s"
    cat "$WORK_DIR/rename.err" >&2
    exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha; then
    NEW_ALPHA_REL="$HELPER_OUT"; NEW_ALPHA_REL="${NEW_ALPHA_REL#* }"
    pass "(b) manifest re-keyed + name-refresh export settled (path: $NEW_ALPHA_REL)"
else
    fail "(b) manifest did not settle after the rename"
fi
ALPHA_AFTER="$(helper info "$WORK_DIR" alpha)"
if [ "$(echo "$ALPHA_AFTER" | json_field id)" = "$ALPHA_ID" ] &&
    [ "$(echo "$ALPHA_AFTER" | json_field revn)" = "$ALPHA_REVN" ]; then
    pass "(b) id + revn continuity: same id, revn untouched ($ALPHA_REVN) — no reimport"
else
    fail "(b) id/revn moved: before=$ALPHA_BEFORE after=$ALPHA_AFTER"
fi
FILES_AFTER_N="$(helper project_files "$WORK_DIR" "$PROJECT_A_ID" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
if [ "$FILES_AFTER_N" = "$FILES_BEFORE_N" ]; then
    pass "(b) project file count unchanged ($FILES_AFTER_N) — no import-as-new duplicate"
else
    fail "(b) project file count moved: $FILES_BEFORE_N -> $FILES_AFTER_N"
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    pass "(b) exports follow the rename: fresh $HELPER_OUT rendered (stale old .exports left in place — documented)"
else
    fail "(b) no exports at the renamed path within ${EXPORT_TIMEOUT}s"
fi

# --- (c) OS-side move across project folders --------------------------------------
ALPHA_BEFORE="$(helper info "$WORK_DIR" alpha)"
ALPHA_MODIFIED_BEFORE="$(echo "$ALPHA_BEFORE" | json_field modifiedAt)"
ALPHA_REL="$(helper manifest_entry "$WORK_DIR" alpha | json_field path)"
mkdir -p "$DESIGNS_DIR/ClientB"
mv "$DESIGNS_DIR/$ALPHA_REL" "$DESIGNS_DIR/ClientB/"
MOVE_DEADLINE=$(($(date +%s) + RENAME_TIMEOUT))
MOVED=""
while [ "$(date +%s)" -lt "$MOVE_DEADLINE" ]; do
    ENTRY="$(helper manifest_entry "$WORK_DIR" alpha 2>/dev/null || true)"
    case "$ENTRY" in
        *'"path": "ClientB/'*) MOVED=1; break ;;
    esac
    sleep 1
done
if [ -n "$MOVED" ]; then
    pass "(c) manifest re-keyed to ClientB/ after the cross-folder mv"
else
    fail "(c) manifest did not re-key to ClientB/ within ${RENAME_TIMEOUT}s (last: $ENTRY)"
    exit 1
fi
NEW_PROJECT_ID="$(helper manifest_entry "$WORK_DIR" alpha | json_field projectId)"
NEW_PROJECT_NAME="$(helper manifest_entry "$WORK_DIR" alpha | json_field projectName)"
if [ "$NEW_PROJECT_ID" != "$PROJECT_A_ID" ] && [ "$NEW_PROJECT_NAME" = "ClientB" ]; then
    pass "(c) a new project 'ClientB' owns the file (projectId changed)"
else
    fail "(c) project mapping wrong: id=$NEW_PROJECT_ID name=$NEW_PROJECT_NAME"
fi
# DB membership: file listed in the new project, gone from the old one.
IN_NEW="$(helper project_files "$WORK_DIR" "$NEW_PROJECT_ID" | python3 -c "import json,sys; print(any(f['id']==sys.argv[1] for f in json.load(sys.stdin)))" "$ALPHA_ID")"
IN_OLD="$(helper project_files "$WORK_DIR" "$PROJECT_A_ID" | python3 -c "import json,sys; print(any(f['id']==sys.argv[1] for f in json.load(sys.stdin)))" "$ALPHA_ID")"
if [ "$IN_NEW" = "True" ] && [ "$IN_OLD" = "False" ]; then
    pass "(c) DB project membership changed (move-files): in ClientB, not in ProjectA"
else
    fail "(c) DB membership wrong: in_new=$IN_NEW in_old=$IN_OLD"
fi
ALPHA_AFTER="$(helper info "$WORK_DIR" alpha)"
if [ "$(echo "$ALPHA_AFTER" | json_field id)" = "$ALPHA_ID" ] &&
    [ "$(echo "$ALPHA_AFTER" | json_field modifiedAt)" = "$ALPHA_MODIFIED_BEFORE" ]; then
    pass "(c) same file id, modifiedAt byte-identical (move does not bump it — bounce-free)"
else
    fail "(c) id/modifiedAt moved across the project move: before=$ALPHA_BEFORE after=$ALPHA_AFTER"
fi

# --- (g) single-instance (needs the first instance still running) -----------------
# GUI: tauri-plugin-single-instance (second launch focuses the existing
# window; verified live in the M5 hardening workstream — not drivable
# headlessly here). Headless: the postgres postmaster.pid lock makes a second
# launch fail LOUDLY while the first instance is unharmed — asserted here.
SECOND_LOG="$WORK_DIR/second.log"
env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
    PENPOT_LOCAL_EXPORTS=1 \
    PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" \
    "$BIN" >"$SECOND_LOG" 2>&1 &
SECOND_PID=$!
SECOND_EXIT=""
SECOND_DEADLINE=$(($(date +%s) + 120))
while [ "$(date +%s)" -lt "$SECOND_DEADLINE" ]; do
    if ! kill -0 "$SECOND_PID" 2>/dev/null; then
        wait "$SECOND_PID"; SECOND_EXIT=$?
        break
    fi
    sleep 1
done
if [ -z "$SECOND_EXIT" ]; then
    kill -9 "$SECOND_PID" 2>/dev/null
    fail "(g) second headless launch did not exit within 120s"
elif [ "$SECOND_EXIT" -ne 0 ]; then
    pass "(g) second headless launch against the same data dir failed loudly (exit $SECOND_EXIT, no silent corruption)"
else
    fail "(g) second headless launch unexpectedly succeeded (exit 0)"
fi
if grep -q "READY" "$SECOND_LOG"; then
    fail "(g) second instance reached READY — double boot!"
else
    pass "(g) second instance never reached READY"
fi
if curl -fsS -o /dev/null "$BASE/" && helper info "$WORK_DIR" alpha >/dev/null 2>&1; then
    pass "(g) first instance unharmed (GET / 200 + authenticated RPC still work)"
else
    fail "(g) first instance degraded after the second launch"
fi
echo "     note: the GUI app uses tauri-plugin-single-instance (second launch focuses"
echo "     the existing window). That path needs a real window server and is covered"
echo "     by the M5 hardening live proof; this block asserts the headless behavior."

# --- (d) rename while STOPPED -> startup reconciliation re-keys --------------------
BETA_BEFORE="$(helper info "$WORK_DIR" beta)"
BETA_ID="$(echo "$BETA_BEFORE" | json_field id)"
BETA_REL="$(helper manifest_entry "$WORK_DIR" beta | json_field path)"
if stop_headless; then
    pass "(d) first instance stopped cleanly (SIGTERM)"
else
    fail "(d) headless did not exit within 25s of SIGTERM"
    exit 1
fi
mv "$DESIGNS_DIR/$BETA_REL" "$DESIGNS_DIR/${BETA_REL%.penpot}2.penpot"
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log 120 "reconciliation complete"; then
    pass "(d) reboot after the offline rename reached READY + reconciliation complete"
else
    fail "(d) reboot did not reach READY/reconciliation"
    exit 1
fi
read_token || true
BETA_ENTRY="$(helper manifest_entry "$WORK_DIR" beta 2>&1)"
case "$BETA_ENTRY" in
    *"${BETA_REL%.penpot}2.penpot"*)
        pass "(d) manifest re-keyed to the new path under the SAME file id ($BETA_ID)" ;;
    *)
        fail "(d) manifest not re-keyed: $BETA_ENTRY" ;;
esac
if BETA_RENAME_S="$(helper wait_name "$WORK_DIR" beta "beta2" 30 2>"$WORK_DIR/beta-rename.err")"; then
    pass "(d) DB rename mirrored (name 'beta2' ${BETA_RENAME_S}s after probe start)"
else
    fail "(d) DB name not updated"
    cat "$WORK_DIR/beta-rename.err" >&2
fi
BETA_PROJECT_ID="$(helper manifest_entry "$WORK_DIR" beta | json_field projectId)"
N_IN_PROJECT="$(helper project_files "$WORK_DIR" "$BETA_PROJECT_ID" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
if [ "$N_IN_PROJECT" = "1" ]; then
    pass "(d) exactly 1 file in the project — no import-as-new duplicate"
else
    fail "(d) expected 1 file in the project, found $N_IN_PROJECT"
fi
if [ ! -e "$DESIGNS_DIR/$BETA_REL" ]; then
    pass "(d) old path not re-exported as an orphan"
else
    fail "(d) old path re-appeared: $BETA_REL"
fi
if [ -z "$(find "$DESIGNS_DIR" -type d -name '*.conflict-*.penpot' 2>/dev/null)" ]; then
    pass "(d) no conflict copies produced by the offline rename"
else
    fail "(d) unexpected conflict copies appeared"
fi

# --- shutdown of the main stack ----------------------------------------------------
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
STACK_PIDS="$(pgrep -f "$DATA_DIR" || true)"
if stop_headless; then
    ORPHANS=""
    for pid in $JAVA_PIDS $STACK_PIDS; do
        kill -0 "$pid" 2>/dev/null && ORPHANS="$ORPHANS $pid"
    done
    if [ -z "$ORPHANS" ]; then
        pass "clean shutdown, no orphan java/valkey/postgres/exporter processes"
    else
        fail "orphan processes left behind:$ORPHANS"
        ps -o pid,command -p ${ORPHANS} >&2 || true
    fi
else
    fail "headless did not exit within 25s of SIGTERM"
fi
sleep 1
if ports_all_free; then
    pass "all 5 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"
else
    fail "ports still busy after clean shutdown"
fi

# --- (e) designs-git-init.sh --------------------------------------------------------
GITDIR_FRESH="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m5-git-fresh.XXXXXX")"
mkdir -p "$GITDIR_FRESH/ProjectX/file.penpot"
echo '{"k":1}' >"$GITDIR_FRESH/ProjectX/file.penpot/manifest.json"
if bash "$GIT_HELPER" "$GITDIR_FRESH" >"$WORK_DIR/git1.out" 2>&1; then
    pass "(e) designs-git-init.sh ran on a fresh dir"
else
    fail "(e) designs-git-init.sh failed on a fresh dir"
    cat "$WORK_DIR/git1.out" >&2
fi
GITQ=(git -C "$GITDIR_FRESH")
if [ -d "$GITDIR_FRESH/.git" ] && [ -f "$GITDIR_FRESH/.gitignore" ] && [ -f "$GITDIR_FRESH/DESIGNS-README.md" ]; then
    pass "(e) repo + .gitignore + DESIGNS-README.md created"
else
    fail "(e) missing repo/.gitignore/README"
fi
if grep -q -- "--no-overlay" "$GITDIR_FRESH/DESIGNS-README.md" &&
    grep -q -- "git restore --source" "$GITDIR_FRESH/DESIGNS-README.md" &&
    grep -q "overlay mode" "$GITDIR_FRESH/DESIGNS-README.md"; then
    pass "(e) README teaches the no-overlay lesson (git checkout --no-overlay / git restore --source)"
else
    fail "(e) README missing the no-overlay lesson"
fi
if grep -q '\.penpot-sync\.json\.tmp-\*' "$GITDIR_FRESH/.gitignore" &&
    ! grep -qE '^\s*\*?\.?penpot-sync\.json\s*$' "$GITDIR_FRESH/.gitignore" &&
    ! grep -qE '^\s*\*\.exports/\s*$' "$GITDIR_FRESH/.gitignore"; then
    pass "(e) .gitignore ignores only transient noise (manifest + exports stay tracked)"
else
    fail "(e) .gitignore contents wrong"
    cat "$GITDIR_FRESH/.gitignore" >&2
fi
N_COMMITS="$("${GITQ[@]}" rev-list --count HEAD 2>/dev/null || echo 0)"
if [ "$N_COMMITS" = "1" ]; then
    pass "(e) exactly one initial commit"
else
    fail "(e) expected 1 commit, found $N_COMMITS"
fi
if bash "$GIT_HELPER" "$GITDIR_FRESH" >"$WORK_DIR/git2.out" 2>&1 &&
    [ "$("${GITQ[@]}" rev-list --count HEAD)" = "1" ] &&
    "${GITQ[@]}" diff --quiet; then
    pass "(e) second run is idempotent (still 1 commit, working tree clean)"
else
    fail "(e) second run not idempotent"
    cat "$WORK_DIR/git2.out" >&2
fi
GITDIR_PRE="$(mktemp -d "${TMPDIR:-/tmp}/penpot-m5-git-pre.XXXXXX")"
git -C "$GITDIR_PRE" init -q
echo hello >"$GITDIR_PRE/existing.txt"
if bash "$GIT_HELPER" "$GITDIR_PRE" >"$WORK_DIR/git3.out" 2>&1 &&
    [ "$(git -C "$GITDIR_PRE" rev-list --count HEAD 2>/dev/null || echo 0)" = "0" ] &&
    [ -f "$GITDIR_PRE/.gitignore" ] &&
    grep -q "no commit made" "$WORK_DIR/git3.out"; then
    pass "(e) pre-existing repo: files written, NO commit made (history untouched)"
else
    fail "(e) pre-existing repo handling wrong"
    cat "$WORK_DIR/git3.out" >&2
fi
rm -rf "$GITDIR_FRESH" "$GITDIR_PRE"

# --- (f) non-BMP (emoji) data dir -> clean refusal -----------------------------------
EMOJI_DATA="$WORK_DIR/dati 🎨"
mkdir -p "$EMOJI_DATA"
EMOJI_LOG="$WORK_DIR/emoji.log"
env PENPOT_LOCAL_DATA_DIR="$EMOJI_DATA" \
    PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
    "$BIN" >"$EMOJI_LOG" 2>&1 &
EMOJI_PID=$!
EMOJI_EXIT=""
EMOJI_DEADLINE=$(($(date +%s) + 20))
while [ "$(date +%s)" -lt "$EMOJI_DEADLINE" ]; do
    if ! kill -0 "$EMOJI_PID" 2>/dev/null; then
        wait "$EMOJI_PID"; EMOJI_EXIT=$?
        break
    fi
    sleep 1
done
if [ -n "$EMOJI_EXIT" ] && [ "$EMOJI_EXIT" -ne 0 ]; then
    pass "(f) emoji data dir refused with exit code $EMOJI_EXIT within 20s (no crash-loop)"
else
    [ -z "$EMOJI_EXIT" ] && kill -9 "$EMOJI_PID" 2>/dev/null
    fail "(f) expected a fast non-zero exit, got '${EMOJI_EXIT:-still running}'"
fi
if strip_ansi <"$EMOJI_LOG" | grep -q "U+1F3A8" &&
    strip_ansi <"$EMOJI_LOG" | grep -qi "data directory" &&
    strip_ansi <"$EMOJI_LOG" | grep -qi "emoji"; then
    pass "(f) error names the offending path, the character (U+1F3A8) and says 'emoji'"
else
    fail "(f) refusal message unclear:"
    strip_ansi <"$EMOJI_LOG" | tail -5 >&2
fi
if [ -z "$(pgrep -f "dati 🎨" || true)" ] && ports_all_free; then
    pass "(f) zero processes spawned, zero listeners — nothing to clean up"
else
    fail "(f) orphans/listeners left behind by the refused boot"
    pgrep -lf "dati 🎨" >&2 || true
fi
if [ ! -d "$EMOJI_DATA/postgres" ] && [ ! -f "$EMOJI_DATA/secret.key" ]; then
    pass "(f) refused boot wrote nothing into the emoji data dir"
else
    fail "(f) the refused boot left files in the emoji data dir"
    ls -la "$EMOJI_DATA" >&2
fi

echo
echo "headline: rename->Penpot ${RENAME_LATENCY:-?}s ; single-instance second exit=$SECOND_EXIT ; emoji refusal exit=${EMOJI_EXIT:-?}"
if [ "$FAILURES" -eq 0 ]; then
    echo "M5 FEATURES: ALL PASS"
    exit 0
else
    echo "M5 FEATURES: $FAILURES FAILURE(S)"
    exit 1
fi
