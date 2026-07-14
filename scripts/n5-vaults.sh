#!/usr/bin/env bash
# N5 vaults / zero-spill gate (PLAN2.md milestone N5, `just n5`).
#
# The switch mechanism IS the core invariant (PLAN.md), pointed at a different
# tree: stop the sync daemon → wipe the disposable Penpot DB → repoint at the
# new vault root → reconcile from the new tree (re-provision, re-import each
# file under its ORIGINAL id, rebuild the index). Invariant 2 (P0): a switch
# must NEVER surface, import or write a file of another vault, and the user
# disk stays byte-untouched.
#
# Exit criteria (PLAN2.md N5), each a PASS/FAIL block in the house style:
#   (a) two vaults A and B with OVERLAPPING project names ("Shared"/"Studio")
#       and a file each of DISTINCT content (a searchable needle token); one
#       file per vault ALSO embeds a DISTINCT uploaded raster (a PNG referenced
#       by an image fill) so media hygiene is provable.
#   (b) switch A→B→A driven headlessly via the localhost control endpoint
#       (POST /open {path}) — NO GUI dialog.
#   (c) after EVERY switch: ZERO spill — no file of the other vault appears in
#       the DB (get-projects/get-project-files), the /__home lighttable
#       (/__api/vault/boards) or the index (/__api/vault/search); and MEDIA
#       zero-spill + survival — this vault's embedded image is served with its
#       EXACT bytes (re-materialized from the .penpot), the other vault's media
#       is NOT served, and the other vault's blob bytes are GONE from the shared
#       objects storage (<data>/assets is wiped on switch).
#   (d) every file keeps its ORIGINAL Penpot id across the round trip
#       (sameIds, get-project-files).
#   (e) both vault trees are byte-identical before/after (recursive raw-byte
#       hash of the `.penpot` dirs — user disk untouched), no conflict copies.
#   (f) per-vault M2-style DB-wipe→rebuild-same-ids proven on BOTH vaults
#       (the switch IS that wipe→rebuild; asserted on A and on B).
#   (g) a mid-switch SIGKILL (killed during the wipe/reconcile window) + reboot
#       recovers to a consistent SINGLE vault (the target), with no
#       cross-contamination and no orphans.
#
# Dedicated ports (ledger): proxy 8948, backend 6409, postgres 5483, valkey
# 6426 (exporter 6489 reserved; renders OFF), control 8949. Fresh mktemp dirs;
# ANSI-stripped log greps; pg-install cache seeded; dirs kept on failure.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N5_PROXY_PORT:-8948}"
BACKEND_PORT="${N5_BACKEND_PORT:-6409}"
POSTGRES_PORT="${N5_POSTGRES_PORT:-5483}"
VALKEY_PORT="${N5_VALKEY_PORT:-6426}"
CONTROL_PORT="${N5_CONTROL_PORT:-8949}"
FIRST_BOOT_TIMEOUT="${N5_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
SWITCH_TIMEOUT="${N5_SWITCH_TIMEOUT:-600}"   # a switch = full teardown+reboot
SYNC_TIMEOUT=300
CRASH_DELAY_MS="${N5_CRASH_DELAY_MS:-6000}"  # mid-switch pause to land the kill
BASE="http://localhost:${PROXY_PORT}"
CONTROL="http://127.0.0.1:${CONTROL_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n5-data.XXXXXX")"
VAULT_A="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n5-vaultA.XXXXXX")"
VAULT_B="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n5-vaultB.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n5-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/n5_vaults_helper.py"
HEADLESS_PID=""
FAILURES=0

NEEDLE_A="alphaneedle"
NEEDLE_B="bravoneedle"

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

PG_CACHE="${N5_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
        for _ in $(seq 1 20); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    # belt-and-braces: reap any stray stack children under this data dir
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT_A" "$VAULT_B" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vaultA=$VAULT_A vaultB=$VAULT_B log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
n5helper() { python3 "$HELPER" "$@"; }

# start_headless: honors HL_DESIGNS (omit env when empty → use registry active)
# and HL_DELAY (mid-switch test delay, ms).
start_headless() {
    local extra=()
    [ -n "${HL_DESIGNS:-}" ] && extra+=(PENPOT_LOCAL_DESIGNS_DIR="$HL_DESIGNS")
    [ -n "${HL_DELAY:-}" ] && extra+=(PENPOT_LOCAL_SWITCH_TEST_DELAY_MS="$HL_DELAY")
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
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

wait_log() { # wait_log <timeout> <pattern>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" | grep -q "$2" && return 0
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

control_active_path() { curl -fsS "$CONTROL/active" 2>/dev/null | json_field path 2>/dev/null; }

# switch_vault <abs-path>: drive `File > Open Vault` headlessly (blocks until
# the target stack is up). Returns 0 on a 200 with ok=true.
switch_vault() {
    local target="$1" out
    out="$(curl -fsS --max-time "$SWITCH_TIMEOUT" -X POST "$CONTROL/open" \
        -H 'Content-Type: application/json' -d "{\"path\": \"$target\"}" 2>/dev/null)" || return 1
    echo "$out" | grep -q '"ok":true' || echo "$out" | grep -q '"ok": true'
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2; lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true; return 1
        fi
    done
    return 0
}

no_conflicts() { # no_conflicts <vault_root>
    [ -z "$(find "$1" -name '*.conflict-*' -print -quit 2>/dev/null)" ]
}

# blob_on_disk <png_file>: 0 if some file under <data>/assets is byte-identical
# to <png_file> (the objects-storage backend stores the raw upload). Used to
# prove media hygiene: the inactive vault's blob must NOT linger in the shared
# cache after a switch.
blob_on_disk() { # blob_on_disk <png_file>
    local png="$1" f
    [ -d "$DATA_DIR/assets" ] || return 1
    while IFS= read -r f; do
        cmp -s "$f" "$png" && return 0
    done < <(find "$DATA_DIR/assets" -type f 2>/dev/null)
    return 1
}

# assert_media <this> <other> <ctx>: after a switch, prove MEDIA zero-spill +
# survival. <this> is the active vault, <other> the one switched away from.
#   served-level: this vault's embedded image is served with its EXACT uploaded
#     bytes (re-materialized from the .penpot); the other vault's media id is
#     NOT served (its DB rows are wiped).
#   disk-level:  the other vault's blob bytes are GONE from <data>/assets (the
#     objects storage is wiped on switch), this vault's blob is present.
assert_media() {
    local this="$1" other="$2" ctx="$3"
    local this_png="$WORK_DIR/img-$this.png" other_png="$WORK_DIR/img-$other.png"
    if n5helper media_assert "$WORK_DIR" "$this" "$other" >"$WORK_DIR/media-$this-$ctx.json" 2>&1; then
        pass "(c) $ctx media zero-spill+survival: $this image served (exact bytes), $other media not served"
    else
        fail "(c) $ctx media spill/loss"; cat "$WORK_DIR/media-$this-$ctx.json" >&2
    fi
    if blob_on_disk "$other_png"; then
        fail "(c) $ctx MEDIA SPILL on disk: $other's blob bytes still under <data>/assets"
    else
        pass "(c) $ctx $other's blob bytes absent from <data>/assets (objects storage wiped on switch)"
    fi
    if blob_on_disk "$this_png"; then
        pass "(c) $ctx $this's blob re-materialized on disk from the .penpot"
    else
        fail "(c) $ctx $this's blob missing from <data>/assets after reconcile"
    fi
}

check_no_orphans() { # check_no_orphans <label>  (after stop_headless has run)
    local label="$1"
    if [ -n "$(pgrep -f "$DATA_DIR" || true)" ]; then
        fail "$label orphan stack processes survived"; pgrep -fl "$DATA_DIR" >&2 || true
    else
        pass "$label no orphan java/valkey/postgres processes"
    fi
}

echo "== N5 vaults: data=$DATA_DIR A=$VAULT_A B=$VAULT_B proxy=$BASE control=$CONTROL"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ---------------------------------------------------------------------------
# Phase 1 — round trip A→B→A (+ B rebuild), no crash delay
# ---------------------------------------------------------------------------
HL_DESIGNS="$VAULT_A" HL_DELAY="" start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "(a) first boot opens vault A"
else
    fail "(a) first boot"; exit 1
fi
read_token || { fail "(a) no access token"; exit 1; }
[ "$(control_active_path)" = "$VAULT_A" ] &&
    pass "(a) control /active reports vault A" ||
    fail "(a) control /active not A: $(control_active_path)"

# Seed A (2 overlapping-named projects + a needle each).
if IDS_A="$(n5helper seed "$WORK_DIR" A "$NEEDLE_A" 2>"$WORK_DIR/seedA.err")"; then
    pass "(a) seeded vault A (overlapping project names + needle '$NEEDLE_A'): $IDS_A"
else
    fail "(a) seed A"; cat "$WORK_DIR/seedA.err" >&2; exit 1
fi
if n5helper wait_synced "$WORK_DIR" A "$VAULT_A" "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/syncA.err"; then
    pass "(a) vault A mirrored to disk"
else
    fail "(a) vault A did not sync"; cat "$WORK_DIR/syncA.err" >&2; exit 1
fi
HASH_A="$(n5helper tree_hash "$VAULT_A")"
echo "     vault A .penpot tree hash: ${HASH_A:0:16}…"

# --- switch A→B ------------------------------------------------------------
SWITCH_T0=$(date +%s)
if switch_vault "$VAULT_B"; then
    SWITCH_MS=$((($(date +%s) - SWITCH_T0)))
    pass "(b) switch A→B via control endpoint (POST /open) in ${SWITCH_MS}s"
else
    fail "(b) switch A→B failed"; tail -20 "$LOG" >&2; exit 1
fi
read_token || { fail "(b) no token after switch to B"; exit 1; }
[ "$(control_active_path)" = "$VAULT_B" ] &&
    pass "(b) active vault is now B" || fail "(b) active not B: $(control_active_path)"

if IDS_B="$(n5helper seed "$WORK_DIR" B "$NEEDLE_B" 2>"$WORK_DIR/seedB.err")"; then
    pass "(a) seeded vault B (same project names, distinct needle '$NEEDLE_B'): $IDS_B"
else
    fail "(a) seed B"; cat "$WORK_DIR/seedB.err" >&2; exit 1
fi
n5helper wait_synced "$WORK_DIR" B "$VAULT_B" "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/syncB.err" ||
    { fail "(a) vault B did not sync"; cat "$WORK_DIR/syncB.err" >&2; exit 1; }
HASH_B="$(n5helper tree_hash "$VAULT_B")"
echo "     vault B .penpot tree hash: ${HASH_B:0:16}…"

# ZERO SPILL after switch to B.
if n5helper assert_state "$WORK_DIR" B A >"$WORK_DIR/stateB.json" 2>&1; then
    pass "(c) after A→B: zero spill — DB/boards/search hold only B; no A file or needle"
else
    fail "(c) SPILL after A→B"; cat "$WORK_DIR/stateB.json" >&2
fi
# MEDIA zero-spill: B's image served (exact bytes); A's media not served + A's
# blob bytes gone from the shared objects storage.
assert_media B A "A→B"
no_conflicts "$VAULT_A" && no_conflicts "$VAULT_B" &&
    pass "(e) no conflict copies after A→B" || fail "(e) conflict copy appeared after A→B"
[ "$(n5helper tree_hash "$VAULT_A")" = "$HASH_A" ] &&
    pass "(e) vault A .penpot tree byte-untouched while B is open" ||
    fail "(e) vault A tree changed while B open"

# --- switch B→A ------------------------------------------------------------
if switch_vault "$VAULT_A"; then
    pass "(b) switch B→A via control endpoint"
else
    fail "(b) switch B→A failed"; tail -20 "$LOG" >&2; exit 1
fi
read_token || { fail "(b) no token after switch to A"; exit 1; }
[ "$(control_active_path)" = "$VAULT_A" ] &&
    pass "(b) active vault is A again" || fail "(b) active not A: $(control_active_path)"

# Wait for startup reconciliation (re-import from disk) + index rebuild.
if n5helper wait_present "$WORK_DIR" A "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/reconA.err"; then
    pass "(d) vault A reconciled from disk after B→A (files back in the DB + index)"
else
    fail "(d) vault A did not reconcile after B→A"; cat "$WORK_DIR/reconA.err" >&2
fi

# ZERO SPILL + SAME IDS after switch back to A.
if n5helper assert_state "$WORK_DIR" A B >"$WORK_DIR/stateA.json" 2>&1; then
    SAME_A="$(json_field sameIds <"$WORK_DIR/stateA.json")"
    pass "(c)(d) after B→A: zero spill; A files rebuilt from disk with ORIGINAL ids (sameIds=$SAME_A)"
else
    fail "(c)(d) SPILL or id-mismatch after B→A"; cat "$WORK_DIR/stateA.json" >&2
fi
# MEDIA SURVIVES the A→B→A round trip: A's image is served again with its exact
# uploaded bytes (re-materialized from the .penpot); B's media is gone.
assert_media A B "B→A"
[ "$(n5helper tree_hash "$VAULT_A")" = "$HASH_A" ] &&
    pass "(e) vault A .penpot tree byte-identical across the A→B→A round trip" ||
    fail "(e) vault A tree changed across the round trip"
[ "$(n5helper tree_hash "$VAULT_B")" = "$HASH_B" ] &&
    pass "(e) vault B .penpot tree byte-untouched while A is open" ||
    fail "(e) vault B tree changed while A open"
no_conflicts "$VAULT_A" && no_conflicts "$VAULT_B" &&
    pass "(e) no conflict copies after B→A" || fail "(e) conflict copy appeared after B→A"

# --- per-vault rebuild proof for B (switch A→B again; B reconciles from disk)
if switch_vault "$VAULT_B"; then pass "(f) switch A→B again (per-vault B rebuild)"; else fail "(f) switch A→B (2) failed"; exit 1; fi
read_token || { fail "(f) no token"; exit 1; }
n5helper wait_present "$WORK_DIR" B "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/reconB.err" ||
    { fail "(f) vault B did not reconcile after A→B (2)"; cat "$WORK_DIR/reconB.err" >&2; }
if n5helper assert_state "$WORK_DIR" B A >"$WORK_DIR/stateB2.json" 2>&1; then
    SAME_B="$(json_field sameIds <"$WORK_DIR/stateB2.json")"
    pass "(f) M2-style wipe→rebuild on B: files re-imported under ORIGINAL ids (sameIds=$SAME_B), zero A spill"
else
    fail "(f) B rebuild spill/id-mismatch"; cat "$WORK_DIR/stateB2.json" >&2
fi
# MEDIA SURVIVES B's wipe→rebuild too: B's image re-materialized, A's gone.
assert_media B A "A→B(2)"
[ "$(n5helper tree_hash "$VAULT_B")" = "$HASH_B" ] &&
    pass "(f) vault B tree byte-identical across its wipe→rebuild" ||
    fail "(f) vault B tree changed across rebuild"

# clean stop before the crash-recovery phase
JAVA_PIDS="$(pgrep -P "$HEADLESS_PID" -f "penpot.jar" || true)"
if stop_headless; then pass "phase-1 clean shutdown"; else fail "phase-1 shutdown hung"; fi
check_no_orphans "phase-1"

# ---------------------------------------------------------------------------
# Phase 2 — mid-switch SIGKILL + reboot recovery
# ---------------------------------------------------------------------------
# Reboot on the CURRENT DB owner (registry active = B; env unset so the
# registry decides — matches the DB, no wipe). Then switch B→A with a widened
# crash window and kill during the wipe/reconcile.
: >"$LOG"
HL_DESIGNS="" HL_DELAY="$CRASH_DELAY_MS" start_headless
if wait_ready "$REBOOT_TIMEOUT"; then
    pass "(g) reboot on registry-active vault B (env unset)"
else
    fail "(g) reboot before crash test"; exit 1
fi
[ "$(control_active_path)" = "$VAULT_B" ] &&
    pass "(g) active vault before crash is B" || fail "(g) pre-crash active not B: $(control_active_path)"

# Fire the switch to A in the background; it will pause mid-wipe.
( curl -fsS --max-time "$SWITCH_TIMEOUT" -X POST "$CONTROL/open" \
    -H 'Content-Type: application/json' -d "{\"path\": \"$VAULT_A\"}" >/dev/null 2>&1 ) &
SWITCH_BG=$!
if wait_log 60 "vault switch: wiping DB"; then
    pass "(g) switch B→A reached the wipe/reconcile window"
else
    fail "(g) switch never reached the wipe window"; kill "$SWITCH_BG" 2>/dev/null; exit 1
fi
sleep 1  # squarely inside the widened window (DB wiped, target not yet booted)
KILLED_PID="$HEADLESS_PID"
kill -9 "$HEADLESS_PID" 2>/dev/null
wait "$SWITCH_BG" 2>/dev/null
for _ in $(seq 1 15); do kill -0 "$KILLED_PID" 2>/dev/null || break; sleep 1; done
HEADLESS_PID=""
if [ -f "$DATA_DIR/vault-switch.json" ]; then
    pass "(g) SIGKILL mid-switch: switch-in-progress marker left on disk (target recorded)"
else
    fail "(g) no switch marker after mid-switch SIGKILL"
fi
sleep 2  # let the watchdog reap any stack children (none expected mid-wipe)
if ports_all_free; then
    pass "(g) no orphan stack after the SIGKILL (ports free)"
else
    fail "(g) orphan stack after SIGKILL"; pkill -9 -f "$DATA_DIR" 2>/dev/null
fi

# Reboot → recovery completes the switch forward to the target (A).
: >"$LOG"
HL_DESIGNS="" HL_DELAY="" start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log 30 "vault switch recovery: complete"; then
    pass "(g) reboot: interrupted switch recovered forward to a single consistent vault"
else
    fail "(g) recovery reboot did not complete the switch"; tail -25 "$LOG" >&2; exit 1
fi
read_token || { fail "(g) no token after recovery"; exit 1; }
[ ! -f "$DATA_DIR/vault-switch.json" ] &&
    pass "(g) switch marker cleared after recovery" || fail "(g) switch marker still present after recovery"
RECOVERED="$(control_active_path)"
if [ "$RECOVERED" = "$VAULT_A" ]; then
    pass "(g) recovered to the switch target — active vault is A"
else
    fail "(g) recovered to '$RECOVERED', expected A ($VAULT_A)"
fi
n5helper wait_present "$WORK_DIR" A "$SYNC_TIMEOUT" >/dev/null 2>"$WORK_DIR/reconR.err" ||
    { fail "(g) recovered vault A did not reconcile from disk"; cat "$WORK_DIR/reconR.err" >&2; }
if n5helper assert_state "$WORK_DIR" A B >"$WORK_DIR/recovered.json" 2>&1; then
    pass "(g) recovered vault: zero cross-contamination — only A's files, original ids, no B spill/orphans"
else
    fail "(g) recovered vault has spill / id-mismatch"; cat "$WORK_DIR/recovered.json" >&2
fi
# MEDIA survives the crash+recovery: A's image served, B's media gone on disk.
assert_media A B "recovery"
[ "$(n5helper tree_hash "$VAULT_A")" = "$HASH_A" ] &&
    pass "(g) vault A tree byte-identical after the crash+recovery" || fail "(g) vault A tree changed after crash"
[ "$(n5helper tree_hash "$VAULT_B")" = "$HASH_B" ] &&
    pass "(g) vault B tree byte-untouched through the crash+recovery" || fail "(g) vault B tree changed after crash"
no_conflicts "$VAULT_A" && no_conflicts "$VAULT_B" &&
    pass "(g) no conflict copies after crash recovery" || fail "(g) conflict copy after recovery"

# --- final shutdown --------------------------------------------------------
if stop_headless; then pass "final clean shutdown"; else fail "final shutdown hung"; fi
check_no_orphans "final"
sleep 1
ports_all_free && pass "all 4 ports freed" || fail "ports still busy after shutdown"

echo
echo "headline: A→B→A round trip zero-spill ; sameIds A=${SAME_A:-?} B=${SAME_B:-?} ; switch≈${SWITCH_MS:-?}s ; SIGKILL mid-wipe → recovered forward to A"
if [ "$FAILURES" -eq 0 ]; then
    echo "N5 VAULTS: ALL PASS"
    exit 0
else
    echo "N5 VAULTS: $FAILURES FAILURE(S)"
    exit 1
fi
