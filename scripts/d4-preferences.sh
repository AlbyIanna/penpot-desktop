#!/usr/bin/env bash
# D4 Preferences gate (PLAN4 milestone D4, `just d4`, chained into `just e2e` —
# D4 shipped the native Preferences window that replaces Penpot's `/settings`).
#
# Model: scripts/d3-menus.sh / scripts/d2-home.sh (header block documenting the
# port set, `pass`/`fail` helpers, STRICTLY PID-SCOPED cleanup — never
# `pkill`/`killall` by name, another gate may be running — totals line,
# non-zero exit on any failure). No RPC/hashing code is duplicated here: the
# exports leg drives `scripts/m5_features_helper.py`'s own
# seed/check/exports_check/edit_alpha subcommands directly (the SAME
# `exports_check` helper `scripts/n2-thumbs.sh` reuses — this gate is the
# THIRD caller of that one implementation, never a second hasher), and the
# zero-cross-vault-spill leg drives `scripts/n5_vaults_helper.py`'s own
# seed/wait_synced/wait_present/assert_state — this gate never reimplements
# either.
#
# Assertions (PLAN4 D4 exit criteria), each a PASS/FAIL block:
#   (a) preferences PERSIST across a restart: set one, kill+restart the
#       headless process against the SAME data dir, read it back from BOTH
#       preferences.json on disk AND `GET /__api/prefs`.
#   (b) a LIVE setting actually takes effect: `POST /__api/prefs` with
#       `syncEnabled:false` -> the daemon reports paused (not just that the
#       file changed); flip it back -> resumes. `needsReboot` is asserted
#       false both times (sync is never boot-time — prefs.rs).
#   (c) the exporter toggle actually stops renders — PLAN4's stated exit
#       criterion. POSITIVE FIRST (prove we were looking): with renders ON,
#       an RPC edit produces a fresh render (state hash advances to match the
#       edit). Only then: turn renders OFF, edit AGAIN (proven to land on
#       disk — the sync daemon is untouched by this toggle), and assert NO
#       new render appears within a window at least as long as the positive
#       leg's own measured latency — an absence check with no positive
#       baseline and no window would pass trivially against a broken
#       exporter.
#   (d) sync-off SURVIVES a restart — the specific failure this milestone
#       must not ship: `SyncControl` is constructed unpaused on every boot,
#       so without the boot-time re-apply, "sync off" silently turns itself
#       back on. Proven as part of the SAME restart as (a): `GET /__api/prefs`
#       after the restart must show `syncPaused:true` — the LIVE daemon fact,
#       not merely the persisted file value a missing re-apply could still
#       satisfy.
#   (e) a Preferences-initiated vault switch keeps ZERO cross-vault spill
#       (P0): `POST /__api/prefs/vault` between two freshly-seeded vaults,
#       then `n5_vaults_helper.py assert_state` (the N5 gate's own assertion,
#       reused verbatim) proves the DB / `/__api/vault/boards` / the search
#       index hold ONLY the new vault's files.
#   (f) a reboot-in-place does NOT wipe the vault's DB state: change a
#       boot-time setting, `POST /__api/prefs/reboot`, then re-run the SAME
#       `assert_state` against the vault that was already active — it holds
#       zero-spill AND (its `sameIds` field) that every file kept its
#       ORIGINAL id. A reboot that silently re-imported everything would be
#       correct but the wrong cost for changing a checkbox, and a re-import
#       would show up here as a fresh id.
#   (g) the boot-time toggle REALLY applies after the reboot, on the SERVED
#       artifact (not the file we already know saved correctly from (a)):
#       BEFORE the reboot, `/js/config.js` still carries the `enable-plugins`
#       token and `/index.html` still carries the CSP header (proof the save
#       alone did nothing live — these two are boot-time by design). AFTER
#       `POST /__api/prefs/reboot`: `enable-plugins` is gone (exact
#       whitespace-split token match, not a substring) and the CSP response
#       header is absent.
#
# Dedicated ports: proxy 9054, backend 6516, postgres 5589, valkey 6532. A
# fifth, D4-local port is NOT needed (unlike D3/N5's control-server addition)
# because every D4 route under test — including the vault switch — is served
# same-origin off the proxy itself (`/__api/prefs/*`). The dev-mode exporter
# (assertion c) needs its own local port; 6533 (D4_VALKEY+1) is free against
# every other gate's port ledger (checked against scripts/*.sh at write time).
#
# Requirements: rust toolchain, JDK 26, valkey-server, python3, curl — plus,
# for assertion (c) only, the dev-mode exporter prerequisites scripts/m5-
# features.sh already documents: runtime/exporter (scripts/fetch-penpot.sh
# --with-browsers) and host node (default /opt/homebrew/bin/node, override
# PENPOT_LOCAL_NODE).
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on a
# different port block. We kill ONLY the PIDs this script recorded; never
# pkill/killall by name.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D4_PROXY_PORT:-9054}"
BACKEND_PORT="${D4_BACKEND_PORT:-6516}"
POSTGRES_PORT="${D4_POSTGRES_PORT:-5589}"
VALKEY_PORT="${D4_VALKEY_PORT:-6532}"
EXPORTER_PORT="${D4_EXPORTER_PORT:-6533}"
BASE="http://localhost:${PROXY_PORT}"

FIRST_BOOT_TIMEOUT="${D4_FIRST_BOOT_TIMEOUT:-900}"   # fresh data dir, pg install may be uncached
RESTART_TIMEOUT="${D4_RESTART_TIMEOUT:-300}"
SYNC_TIMEOUT="${D4_SYNC_TIMEOUT:-180}"
EXPORT_TIMEOUT="${D4_EXPORT_TIMEOUT:-180}"
SWITCH_TIMEOUT="${D4_SWITCH_TIMEOUT:-600}"            # a vault switch = full teardown+reboot
REBOOT_TIMEOUT="${D4_REBOOT_TIMEOUT:-300}"             # in-place reboot (same vault, no wipe)
LIVE_APPLY_TIMEOUT="${D4_LIVE_APPLY_TIMEOUT:-20}"      # POST /__api/prefs applies live-fields synchronously; short poll is a safety margin, not a real wait

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d4-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d4-vault.XXXXXX")"
VAULT_A="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d4-vaultA.XXXXXX")"
VAULT_B="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d4-vaultB.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d4-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
M5_HELPER="$ROOT/scripts/m5_features_helper.py"
N5_HELPER="$ROOT/scripts/n5_vaults_helper.py"
D4_HELPER="$ROOT/scripts/d4_prefs_helper.py"
HEADLESS_PID=""
FAILURES=0

export M5_DESIGNS_DIR="$VAULT"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

PG_CACHE="${D4_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE for future runs)"
    fi
}

# PID-scoped teardown ONLY — never pkill/killall by name.
cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$VAULT_A" "$VAULT_B" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT vaultA=$VAULT_A vaultB=$VAULT_B work=$WORK_DIR"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

ports_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$EXPORTER_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

ports_all_free() {
    local p ok=0
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$EXPORTER_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            ok=1
        fi
    done
    return "$ok"
}

# start_headless: honors HL_DESIGNS (empty -> omit the env, registry decides
# the active vault — used for the (a)/(d) restart, matching n5-vaults.sh's
# own "reboot on registry-active vault" pattern).
start_headless() {
    local extra=()
    [ -n "${HL_DESIGNS:-}" ] && extra+=(PENPOT_LOCAL_DESIGNS_DIR="$HL_DESIGNS")
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_EXPORTS=1 \
        PENPOT_LOCAL_EXPORTER_PORT="$EXPORTER_PORT" \
        "${extra[@]+"${extra[@]}"}" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then
            echo "headless process died; last log lines:" >&2; tail -25 "$LOG" >&2; return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2; return 1
}

wait_log() { # wait_log <timeout> <fixed-string>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" 2>/dev/null | grep -qF "$2" && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died waiting for '$2'" >&2; return 1; }
        sleep 1
    done
    echo "timed out waiting for '$2' ($1s)" >&2; return 1
}

stop_headless() {
    kill -TERM "$HEADLESS_PID" 2>/dev/null || return 1
    for _ in $(seq 1 25); do
        if ! kill -0 "$HEADLESS_PID" 2>/dev/null; then HEADLESS_PID=""; return 0; fi
        sleep 1
    done
    return 1
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

m5helper() { python3 "$M5_HELPER" "$@"; }
n5helper() { python3 "$N5_HELPER" "$@"; }
d4helper() { python3 "$D4_HELPER" "$@"; }

# wait_ok <timeout-s> <m5-subcmd> [args…]: polls an m5_features_helper.py
# subcommand until it prints "OK …"; HELPER_OUT carries the suffix.
wait_ok() {
    local timeout="$1" sub="$2"; shift 2
    local deadline=$(($(date +%s) + timeout)) out=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        out="$(m5helper "$sub" "$WORK_DIR" "$@" 2>&1)" || { echo "$out" >&2; return 1; }
        case "$out" in
            OK\ *) HELPER_OUT="${out#OK }"; return 0 ;;
        esac
        sleep 2
    done
    echo "timed out waiting for $sub $* (last: $out)" >&2
    return 1
}

exports_state_hash() { # exports_state_hash <abs-exports-dir>
    python3 -c "
import json, sys
try:
    d = json.load(open(sys.argv[1] + '/.exports-state.json'))
    print(d.get('renderedFromHash', ''))
except FileNotFoundError:
    print('')
" "$1"
}

render_batch_count() { strip_ansi <"$LOG" | grep -c "render batch complete" || true; }

echo "== D4 preferences gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT exporter=$EXPORTER_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT   vaultA: $VAULT_A   vaultB: $VAULT_B"

# --- pre-flight --------------------------------------------------------------
for h in "$M5_HELPER" "$N5_HELPER" "$D4_HELPER"; do
    [ -f "$h" ] || { fail "helper missing: $h"; exit 1; }
    python3 -m py_compile "$h" || { fail "helper does not compile: $h"; exit 1; }
done
pass "preflight: all three helpers present and compile"

NODE_BIN="${PENPOT_LOCAL_NODE:-/opt/homebrew/bin/node}"
if [ ! -f "$ROOT/runtime/exporter/app.js" ] || [ ! -x "$NODE_BIN" ] ||
    ! ls "$ROOT/runtime/exporter-browsers" 2>/dev/null | grep -q chromium; then
    fail "exporter prerequisites missing — run scripts/fetch-penpot.sh --with-browsers and install node (assertion c needs real renders)"
    exit 1
fi
pass "preflight: dev-mode exporter prerequisites present (runtime/exporter, node, playwright chromium)"

ports_free || { fail "one of the D4 ports is busy"; exit 1; }
pass "preflight: ports free"

if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# =========================================================================
# BOOT 1 — fresh data dir (no preferences.json yet -> defaults, everything
# ON), vault, renders enabled via PENPOT_LOCAL_EXPORTS=1.
# =========================================================================
echo "-- boot 1 (fresh data dir + vault, exports enabled)"
HL_DESIGNS="$VAULT" start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" && wait_log 30 "board-export service started"; then
    pass "boot: READY + board-export service started (defaults: everything enabled)"
else
    fail "boot 1"; exit 1
fi
read_token || { fail "no access token in $DATA_DIR/credentials.json"; exit 1; }

if m5helper seed "$WORK_DIR" >/dev/null 2>"$WORK_DIR/seed.err"; then
    pass "seeded vault: project 'ProjectA', file alpha (boards Cover+Detail)"
else
    fail "RPC seed failed"; cat "$WORK_DIR/seed.err" >&2; exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha; then
    pass "alpha mirrored to disk"
else
    fail "alpha did not reach disk within ${SYNC_TIMEOUT}s"; exit 1
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    ALPHA_EXPORTS_REL="$HELPER_OUT"
    ALPHA_EXPORTS_ABS="$VAULT/$ALPHA_EXPORTS_REL"
    pass "initial render present: $ALPHA_EXPORTS_REL"
else
    fail "alpha did not render at all within ${EXPORT_TIMEOUT}s — nothing further to test"; exit 1
fi

# =========================================================================
# (c) POSITIVE LEG — renders ON, an edit produces a fresh render. Timed, so
# the (c) NEGATIVE leg below can wait at least as long before declaring
# absence.
# =========================================================================
echo "-- (c) positive: renders ON, edit -> fresh render"
T_EDIT_POS=$(date +%s)
if m5helper edit_alpha "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit-pos.err"; then
    pass "(c) positive: RPC edit applied to alpha/Cover"
else
    fail "(c) positive: RPC edit failed"; cat "$WORK_DIR/edit-pos.err" >&2; exit 1
fi
if wait_ok "$SYNC_TIMEOUT" check alpha; then
    read -r HASH_POS _ <<<"$HELPER_OUT"
    pass "(c) positive: edit landed on disk (hash ${HASH_POS:0:12}…)"
else
    fail "(c) positive: edit never reached disk"; exit 1
fi
if wait_ok "$EXPORT_TIMEOUT" exports_check alpha; then
    T_RENDER_POS=$(( $(date +%s) - T_EDIT_POS ))
    STATE_HASH_POS="$(exports_state_hash "$ALPHA_EXPORTS_ABS")"
    if [ "$STATE_HASH_POS" = "$HASH_POS" ]; then
        pass "(c) positive: a FRESH render appeared for the edited state in ${T_RENDER_POS}s (renders ON works — proof of looking before we trust any absence)"
    else
        fail "(c) positive: exports state hash ($STATE_HASH_POS) != the edit's manifest hash ($HASH_POS)"
    fi
else
    fail "(c) positive: renders ON but no fresh render appeared within ${EXPORT_TIMEOUT}s — the exit criterion has nothing to test an OFF toggle against"
    exit 1
fi

# =========================================================================
# (b) LIVE setting takes effect: sync off -> daemon paused; on -> resumes.
# =========================================================================
echo "-- (b) live sync pause/resume via POST /__api/prefs"
RESP="$(d4helper post '{"syncEnabled":false,"exportsEnabled":true,"pluginsEnabled":true,"cspEnabled":true}' 2>"$WORK_DIR/post-b1.err")"
if echo "$RESP" | grep -q '"ok": true' && echo "$RESP" | grep -q '"needsReboot": false'; then
    pass "(b) POST syncEnabled:false -> ok:true, needsReboot:false (sync is never boot-time)"
else
    fail "(b) unexpected POST response: $RESP $(cat "$WORK_DIR/post-b1.err" 2>/dev/null)"
fi
if d4helper wait_bool syncPaused true "$LIVE_APPLY_TIMEOUT" >"$WORK_DIR/wait-b1.out" 2>"$WORK_DIR/wait-b1.err"; then
    pass "(b) GET /__api/prefs reports syncPaused:true — the DAEMON, not just the file"
else
    fail "(b) daemon never reported paused"; cat "$WORK_DIR/wait-b1.err" >&2
fi
RESP="$(d4helper post '{"syncEnabled":true,"exportsEnabled":true,"pluginsEnabled":true,"cspEnabled":true}' 2>"$WORK_DIR/post-b2.err")"
if echo "$RESP" | grep -q '"ok": true' && echo "$RESP" | grep -q '"needsReboot": false'; then
    pass "(b) POST syncEnabled:true -> ok:true, needsReboot:false"
else
    fail "(b) unexpected POST response: $RESP $(cat "$WORK_DIR/post-b2.err" 2>/dev/null)"
fi
if d4helper wait_bool syncPaused false "$LIVE_APPLY_TIMEOUT" >"$WORK_DIR/wait-b2.out" 2>"$WORK_DIR/wait-b2.err"; then
    pass "(b) GET /__api/prefs reports syncPaused:false — resumed"
else
    fail "(b) daemon never reported resumed"; cat "$WORK_DIR/wait-b2.err" >&2
fi

# =========================================================================
# (c) NEGATIVE LEG — renders OFF: the same kind of edit must NOT produce a
# new render within a window at least as long as the positive leg's own
# measured latency.
# =========================================================================
echo "-- (c) negative: renders OFF, edit must NOT re-render"
RESP="$(d4helper post '{"syncEnabled":true,"exportsEnabled":false,"pluginsEnabled":true,"cspEnabled":true}' 2>"$WORK_DIR/post-c.err")"
if echo "$RESP" | grep -q '"ok": true' && echo "$RESP" | grep -q '"needsReboot": false'; then
    pass "(c) POST exportsEnabled:false -> ok:true, needsReboot:false (OFF is live)"
else
    fail "(c) unexpected POST response: $RESP $(cat "$WORK_DIR/post-c.err" 2>/dev/null)"
fi
if d4helper wait_bool rendersRunning false "$LIVE_APPLY_TIMEOUT" >"$WORK_DIR/wait-c.out" 2>"$WORK_DIR/wait-c.err"; then
    pass "(c) GET /__api/prefs reports rendersRunning:false — the poll loop actually stopped"
else
    fail "(c) rendersRunning never went false"; cat "$WORK_DIR/wait-c.err" >&2
fi
BATCHES_AT_OFF="$(render_batch_count)"
if m5helper edit_alpha "$WORK_DIR" >/dev/null 2>"$WORK_DIR/edit-neg.err"; then
    pass "(c) negative: a second RPC edit applied to alpha/Cover (renders OFF)"
else
    fail "(c) negative: RPC edit failed"; cat "$WORK_DIR/edit-neg.err" >&2
fi
if wait_ok "$SYNC_TIMEOUT" check alpha; then
    read -r HASH_NEG _ <<<"$HELPER_OUT"
    if [ "$HASH_NEG" != "$HASH_POS" ]; then
        pass "(c) proof of looking: the edit DID land on disk (manifest hash advanced: ${HASH_NEG:0:12}…) — sync is untouched by the renders toggle, so an unchanged .exports-state.json below means a suppressed render, not a sync that never happened"
    else
        fail "(c) negative: edit produced the SAME hash as before — the edit itself didn't take, this leg proves nothing"
    fi
else
    fail "(c) negative: edit never reached disk — cannot test render suppression against it"
    exit 1   # HASH_NEG is unset past this point (set -u) and the absence check below is meaningless without it
fi
ABSENCE_WINDOW=$((T_RENDER_POS * 3))
[ "$ABSENCE_WINDOW" -lt 45 ] && ABSENCE_WINDOW=45
echo "   waiting ${ABSENCE_WINDOW}s (>= 3x the positive leg's ${T_RENDER_POS}s) before checking for absence"
sleep "$ABSENCE_WINDOW"
STATE_HASH_AFTER="$(exports_state_hash "$ALPHA_EXPORTS_ABS")"
BATCHES_AFTER="$(render_batch_count)"
if [ "$STATE_HASH_AFTER" = "$STATE_HASH_POS" ] && [ "$STATE_HASH_AFTER" != "$HASH_NEG" ]; then
    pass "(c) negative: exports state hash UNCHANGED (${STATE_HASH_AFTER:0:12}…) after ${ABSENCE_WINDOW}s with renders OFF — no new render"
else
    fail "(c) negative: exports state hash moved to $STATE_HASH_AFTER (expected unchanged $STATE_HASH_POS) — a render happened despite the toggle"
fi
if [ "$BATCHES_AFTER" -eq "$BATCHES_AT_OFF" ]; then
    pass "(c) negative: zero new 'render batch complete' log lines since the toggle ($BATCHES_AT_OFF -> $BATCHES_AFTER)"
else
    fail "(c) negative: render batch count grew ($BATCHES_AT_OFF -> $BATCHES_AFTER) despite renders being OFF"
fi

# =========================================================================
# (a)+(d) preferences persist across a restart; sync-off survives it (the
# LIVE daemon fact after a cold boot_active_vault() -> boot(), not merely
# the file).
# =========================================================================
echo "-- (a)+(d) restart: preferences.json + GET /__api/prefs + live daemon state"
RESP="$(d4helper post '{"syncEnabled":false,"exportsEnabled":false,"pluginsEnabled":true,"cspEnabled":true}' 2>"$WORK_DIR/post-ad.err")"
if echo "$RESP" | grep -q '"ok": true' && echo "$RESP" | grep -q '"needsReboot": false'; then
    pass "(a)/(d) POST syncEnabled:false (exports already off) -> ok:true, needsReboot:false"
else
    fail "(a)/(d) unexpected POST response: $RESP $(cat "$WORK_DIR/post-ad.err" 2>/dev/null)"
fi
if d4helper wait_bool syncPaused true "$LIVE_APPLY_TIMEOUT" >/dev/null 2>"$WORK_DIR/wait-ad.err"; then
    pass "(a)/(d) daemon paused before the restart (setting up the load-bearing check)"
else
    fail "(a)/(d) daemon never paused before restart"; cat "$WORK_DIR/wait-ad.err" >&2
fi

if stop_headless; then
    pass "(a)/(d) clean shutdown before restart"
else
    fail "(a)/(d) shutdown did not complete within 25s"; exit 1
fi
: >"$LOG"   # reset: wait_ready must see the SECOND process's own READY line
HL_DESIGNS="" start_headless   # registry decides -> resolves back to $VAULT
if wait_ready "$RESTART_TIMEOUT"; then
    pass "(a)/(d) second boot reached READY (same data dir, registry-resolved vault)"
else
    fail "(a)/(d) second boot never reached READY"; exit 1
fi
read_token || true

FILE_PREFS="$(d4helper prefs_file "$DATA_DIR")"
if echo "$FILE_PREFS" | grep -q '"syncEnabled": false'; then
    pass "(a) FILE leg: preferences.json on disk carries syncEnabled:false — $FILE_PREFS"
else
    fail "(a) FILE leg: preferences.json does not carry syncEnabled:false — $FILE_PREFS"
fi
API_PREFS="$(d4helper get)"
if echo "$API_PREFS" | grep -q '"syncEnabled": false'; then
    pass "(a) API leg: GET /__api/prefs preferences.syncEnabled:false — $API_PREFS"
else
    fail "(a) API leg: GET /__api/prefs missing syncEnabled:false — $API_PREFS"
fi
if echo "$API_PREFS" | grep -q '"syncPaused": true'; then
    pass "(d) THE load-bearing check: GET /__api/prefs syncPaused:true on the FIRST read after a cold restart — the daemon re-applied the persisted pause at boot, it did not silently turn itself back on"
else
    fail "(d) REGRESSION: syncPaused is not true after the restart — sync-off did NOT survive the restart (the exact failure this milestone must not ship) — $API_PREFS"
fi

# =========================================================================
# (e) a Preferences-initiated vault switch keeps ZERO cross-vault spill (P0).
# Two FRESH vaults (never touched by the m5 seed above, so n5's own
# assert_state — which expects the DB to hold EXACTLY its seeded ids — stays
# meaningful) via POST /__api/prefs/vault, then n5_vaults_helper's own
# assertion, reused verbatim.
# =========================================================================
echo "-- (e) Preferences-initiated vault switch: zero cross-vault spill"
NEEDLE_A="d4needleA"; NEEDLE_B="d4needleB"
RESP="$(d4helper vault "$VAULT_A" "$SWITCH_TIMEOUT" 2>"$WORK_DIR/vault-a.err")"
if echo "$RESP" | grep -q '"ok": true'; then
    pass "(e) POST /__api/prefs/vault -> vault A: ok:true"
else
    fail "(e) switch to vault A failed: $RESP $(cat "$WORK_DIR/vault-a.err" 2>/dev/null)"; exit 1
fi
read_token || { fail "(e) no token after switch to A"; exit 1; }
if n5helper seed "$WORK_DIR" A "$NEEDLE_A" >/dev/null 2>"$WORK_DIR/seedA.err"; then
    pass "(e) seeded vault A (overlapping project names + needle '$NEEDLE_A')"
else
    fail "(e) seed A failed"; cat "$WORK_DIR/seedA.err" >&2; exit 1
fi
if n5helper wait_synced "$WORK_DIR" A "$VAULT_A" "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/syncA.err"; then
    pass "(e) vault A mirrored to disk"
else
    fail "(e) vault A did not sync"; cat "$WORK_DIR/syncA.err" >&2; exit 1
fi

RESP="$(d4helper vault "$VAULT_B" "$SWITCH_TIMEOUT" 2>"$WORK_DIR/vault-b.err")"
if echo "$RESP" | grep -q '"ok": true'; then
    pass "(e) POST /__api/prefs/vault -> vault B: ok:true"
else
    fail "(e) switch to vault B failed: $RESP $(cat "$WORK_DIR/vault-b.err" 2>/dev/null)"; exit 1
fi
read_token || { fail "(e) no token after switch to B"; exit 1; }
if n5helper seed "$WORK_DIR" B "$NEEDLE_B" >/dev/null 2>"$WORK_DIR/seedB.err"; then
    pass "(e) seeded vault B (same project names, distinct needle '$NEEDLE_B')"
else
    fail "(e) seed B failed"; cat "$WORK_DIR/seedB.err" >&2; exit 1
fi
if n5helper wait_synced "$WORK_DIR" B "$VAULT_B" "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/syncB.err" &&
    n5helper wait_present "$WORK_DIR" B "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/indexB.err"; then
    pass "(e) vault B mirrored to disk and caught up in the index"
else
    fail "(e) vault B did not settle"; cat "$WORK_DIR/syncB.err" "$WORK_DIR/indexB.err" >&2 2>/dev/null; exit 1
fi
if n5helper assert_state "$WORK_DIR" B A >"$WORK_DIR/stateB.json" 2>&1; then
    pass "(e) zero cross-vault spill after the Preferences-initiated switch — DB/boards/search hold ONLY B; no file/id/needle of A"
else
    fail "(e) SPILL after Preferences-initiated switch A->B"; cat "$WORK_DIR/stateB.json" >&2
fi

# =========================================================================
# (f)+(g) reboot-in-place: does NOT wipe the DB (original ids preserved,
# zero spill still holds); the boot-time toggle (plugins/CSP) actually
# applies on the SERVED artifact only after the reboot, not before.
# =========================================================================
echo "-- (f)+(g) reboot-in-place: DB preserved; boot-time toggle takes effect on the served artifact"
PRE_FLAGS="$(d4helper config_js_has enable-plugins)"
PRE_CSP="$(d4helper csp_present)"
if [ "$PRE_FLAGS" = "yes" ] && [[ "$PRE_CSP" == yes\|* ]]; then
    pass "(g) baseline BEFORE any change: /js/config.js has enable-plugins, /index.html carries a CSP header (${PRE_CSP#yes|})"
else
    fail "(g) unexpected baseline: config.js enable-plugins=$PRE_FLAGS csp=$PRE_CSP"
fi

RESP="$(d4helper post '{"syncEnabled":false,"exportsEnabled":false,"pluginsEnabled":false,"cspEnabled":false}' 2>"$WORK_DIR/post-fg.err")"
if echo "$RESP" | grep -q '"ok": true' && echo "$RESP" | grep -q '"needsReboot": true'; then
    pass "(f)/(g) POST pluginsEnabled:false,cspEnabled:false -> ok:true, needsReboot:true (boot-time)"
else
    fail "(f)/(g) unexpected POST response: $RESP $(cat "$WORK_DIR/post-fg.err" 2>/dev/null)"
fi

MID_FLAGS="$(d4helper config_js_has enable-plugins)"
MID_CSP="$(d4helper csp_present)"
if [ "$MID_FLAGS" = "yes" ] && [[ "$MID_CSP" == yes\|* ]]; then
    pass "(g) proof of looking: BEFORE the reboot the OLD config.js/CSP are STILL served (saving alone does nothing live for boot-time fields)"
else
    fail "(g) config.js/CSP changed WITHOUT a reboot (should be boot-time-only): flags=$MID_FLAGS csp=$MID_CSP"
fi

RESP="$(d4helper reboot "$REBOOT_TIMEOUT" 2>"$WORK_DIR/reboot.err")"
if echo "$RESP" | grep -q '"ok": true'; then
    pass "(f) POST /__api/prefs/reboot -> ok:true (blocked until reboot_in_place returned)"
else
    fail "(f) reboot-in-place failed: $RESP $(cat "$WORK_DIR/reboot.err" 2>/dev/null)"; exit 1
fi
read_token || true

if n5helper assert_state "$WORK_DIR" B A >"$WORK_DIR/stateB2.json" 2>&1; then
    SAME_B="$(json_field sameIds <"$WORK_DIR/stateB2.json")"
    pass "(f) reboot-in-place did NOT wipe the DB: vault B's files still present under their ORIGINAL ids (sameIds=$SAME_B), zero A spill"
else
    fail "(f) DB state changed across the reboot-in-place (re-import or spill)"; cat "$WORK_DIR/stateB2.json" >&2
fi

POST_FLAGS="$(d4helper config_js_has enable-plugins)"
if [ "$POST_FLAGS" = "no" ]; then
    pass "(g) AFTER the reboot: /js/config.js no longer carries the enable-plugins token (served artifact, whole-token match)"
else
    fail "(g) AFTER the reboot: /js/config.js still has enable-plugins (got '$POST_FLAGS') — pluginsEnabled:false did not take effect"
fi
POST_CSP="$(d4helper csp_present)"
if [ "$POST_CSP" = "no|" ]; then
    pass "(g) AFTER the reboot: /index.html no longer carries a Content-Security-Policy response header"
else
    fail "(g) AFTER the reboot: /index.html still carries a CSP header (got '$POST_CSP') — cspEnabled:false did not take effect"
fi

# --- final shutdown ----------------------------------------------------------
if stop_headless; then
    pass "final clean shutdown"
else
    fail "final shutdown did not complete within 25s"
fi
sleep 1
if ports_all_free; then
    pass "all 5 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT/$EXPORTER_PORT)"
else
    fail "ports still busy after final shutdown"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D4 PREFERENCES: ALL PASS"
    exit 0
else
    echo "D4 PREFERENCES: $FAILURES FAILURE(S)"
    exit 1
fi
