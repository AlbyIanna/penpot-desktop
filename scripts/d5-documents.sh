#!/usr/bin/env bash
# D5 documents gate (PLAN4 milestone D5, `just d5`, chained into `just e2e` —
# D5 makes the app DOCUMENT-BASED: open a `.penpot` by path, from a second
# launch's argv, or by importing one from outside the vault; the window
# title tracks the open file). This is Task 8, THE GATE for the milestone.
#
# Model: scripts/d4-preferences.sh (header block, port set, `pass`/`fail`,
# STRICTLY PID-SCOPED cleanup — never pkill/killall by name, another gate may
# be running — totals, non-zero exit, SKIPPED distinct from PASS and kept out
# of the pass count) and scripts/d3-menus.sh for the GUI-launch parts (a real
# GUI session is required — same operational constraint as D0/D2/D3, not
# CI-headless). No RPC/manifest/spill code is duplicated here beyond small
# gate-local glue: scripts/d5_docs_helper.py drives the RPC seeding + the
# control server's `GET /windows`, and (assertion d) reuses
# scripts/n5_vaults_helper.py's OWN seed/wait_synced/wait_present AND its
# db_file_ids/boards_file_ids/search_count zero-spill primitives directly
# (imported, not reimplemented) — see that helper's module doc.
#
# Assertions (each MUST be able to fail — see .superpowers/sdd/task-8-brief.md):
#
#   (a) Launch with a path argument opens that file. The GUI binary is
#       booted with an in-vault `.penpot` (seeded + synced to disk via a
#       throwaway headless boot BEFORE the GUI ever starts, so the path is
#       genuinely real) as argv. PROOF OF LOOKING FIRST: the control server
#       (`PENPOT_LOCAL_CONTROL_PORT`) must become reachable AND `GET /windows`
#       must show a non-empty array containing the home window — an
#       unreachable or empty response FAILS here as infrastructure, never
#       silently read as "no document opened" (d5_docs_helper.py's
#       `wait_reachable`). Only then: poll for a window whose `fileId`
#       matches the opened file and whose `title` equals the file's name.
#
#   (b) The window title tracks the open file. Same `/windows` read as (a)
#       proves title-at-open. Then: rename the file live via
#       `POST /__api/vault/manage/rename` (D5 Task 6's wiring —
#       `watch_rename_titles` subscribes to the SAME sync-status channel
#       already driving the tray) and poll until the window's title updates
#       — a real assertion, not the "assert title-at-open only" fallback the
#       brief allows, since the mechanism is live and testable here.
#
#   (c) A second launch forwards instead of double-booting — the
#       M5-cooperation exit criterion. Instance 2 launches with a DIFFERENT
#       in-vault `.penpot` (DocB, seeded alongside DocA in the same headless
#       bootstrap) AND a genuinely free, DISTINCT port block (D5_SECOND_*):
#       if the single-instance guard were ever broken, instance 2 would
#       actually succeed at booting its own independent stack on those free
#       ports — giving it the SAME (already-occupied) primary ports would
#       only prove a bind conflict, not that the guard works. Assert:
#       instance 2 exits promptly; its OWN log never shows "penpot stack
#       ready" / "opening vault" / "READY " (proof of looking: instance 1's
#       OWN log — from the exact same boot() — DOES carry these, so the
#       absence in instance 2's log cannot be an artifact of the strings
#       never appearing at all); the second port block is never bound
#       (`ports_all_free`-style check against D5_SECOND_*); instance 1 still
#       answers `GET /health`; and `GET /windows` on instance 1 now ALSO
#       shows DocB's window — the check that proves it FORWARDED rather than
#       merely refused.
#
#   (d) Import-external keeps zero cross-vault spill (P0). Two vaults, A
#       (already active — reused from a/b/c) and B (freshly switched to via
#       the control server's `/open`, seeded with n5_vaults_helper's own
#       needle-bearing content, and proven LIVE via its own `wait_present`
#       before we ever check an absence against it — never a vacuous empty-
#       vault check). With A active again: the native confirm dialog cannot
#       be clicked headlessly, so per the brief's explicit fallback this
#       drives the UNDERLYING mechanism directly — an RPC-seeded, needle-
#       bearing `.penpot` is copied OUTSIDE any vault (standing in for "a
#       file the user has lying around"), then copied BY THIS SCRIPT into
#       A's vault root under `Imported/` (exactly `copy_into_vault`'s own
#       destination — `docopen::IMPORT_PROJECT_DIR`, pinned by the required
#       unit tests below). The sync daemon's own Direction B — untouched,
#       nothing about it is faked — notices it and assigns a file id. Assert
#       it is present in A's DB/boards/search; switch to B; assert it is in
#       NONE of B's DB/boards/search and its `.penpot` dir never touched
#       B's tree on disk.
#
#   (e) Headless-undrivable legs are SKIPPED with reasons, kept OUT of the
#       pass count: Finder double-click (needs a signed, installed app —
#       scripts/d5a-finder-spike.sh's own spike covers the LaunchServices
#       mechanism, scripts/d5-plist-check.sh covers the packaged Info.plist)
#       and drag-drop (no headless drag primitive here). The pure resolver
#       + import-path-decision logic every one of those UI legs funnels
#       through is instead REQUIRED by name — `cargo test -p penpot-desktop
#       -- docopen::` — so a renamed or weakened test cannot leave this gate
#       green (the D1/D2/D3 precedent).
#
# Dedicated ports: proxy 9056, backend 6518, postgres 5591, valkey 6534,
# control 9057 (per the brief). A SECOND, would-be port block
# (D5_SECOND_PROXY_PORT=9060/BACKEND=6520/POSTGRES=5595/VALKEY=6538/
# CONTROL=9061 — all otherwise-unused against every other gate's port
# ledger, checked at write time) exists ONLY as assertion (c)'s double-boot
# trap; a correctly-guarded instance 2 never binds any of them.
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on a
# different port block. We kill ONLY the PIDs this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D5_PROXY_PORT:-9056}"
BACKEND_PORT="${D5_BACKEND_PORT:-6518}"
POSTGRES_PORT="${D5_POSTGRES_PORT:-5591}"
VALKEY_PORT="${D5_VALKEY_PORT:-6534}"
CONTROL_PORT="${D5_CONTROL_PORT:-9057}"
BASE="http://localhost:${PROXY_PORT}"
CONTROL="http://127.0.0.1:${CONTROL_PORT}"

# The would-be SECOND stack's ports (assertion c) — genuinely free, distinct
# from the primary block, so a broken single-instance guard would actually
# succeed at binding them (see header comment for why that matters).
SECOND_PROXY_PORT="${D5_SECOND_PROXY_PORT:-9060}"
SECOND_BACKEND_PORT="${D5_SECOND_BACKEND_PORT:-6520}"
SECOND_POSTGRES_PORT="${D5_SECOND_POSTGRES_PORT:-5595}"
SECOND_VALKEY_PORT="${D5_SECOND_VALKEY_PORT:-6538}"
SECOND_CONTROL_PORT="${D5_SECOND_CONTROL_PORT:-9061}"

FIRST_BOOT_TIMEOUT="${D5_FIRST_BOOT_TIMEOUT:-900}"    # fresh data dir, pg install may be uncached
GUI_BOOT_TIMEOUT="${D5_GUI_BOOT_TIMEOUT:-300}"
DISK_POLL_TIMEOUT="${D5_DISK_POLL_TIMEOUT:-90}"        # sync daemon's own ~2s poll cadence + margin
RENAME_TIMEOUT="${D5_RENAME_TIMEOUT:-90}"              # rename -> export -> relocate -> status tick -> retitle
SECOND_EXIT_TIMEOUT="${D5_SECOND_EXIT_TIMEOUT:-45}"    # instance 2 should exit almost instantly
SWITCH_TIMEOUT="${D5_SWITCH_TIMEOUT:-300}"             # a vault switch = full teardown + wipe + reconcile

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d5-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d5-vaultA.XXXXXX")"
VAULT_B="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d5-vaultB.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d5-work.XXXXXX")"
LOG_HEADLESS="$WORK_DIR/headless.log"
LOG_GUI1="$WORK_DIR/gui1.log"
LOG_GUI2="$WORK_DIR/gui2.log"
CARGO_TEST_LOG="$WORK_DIR/cargo-test.log"

HEADLESS_BIN="$ROOT/target/debug/headless"
GUI_BIN="$ROOT/target/debug/penpot-desktop"
HELPER="$ROOT/scripts/d5_docs_helper.py"
N5_HELPER="$ROOT/scripts/n5_vaults_helper.py"

HEADLESS_PID=""
GUI1_PID=""
GUI2_PID=""
FAILURES=0
SKIPPED=0

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
skip() { echo "SKIPPED: $1"; SKIPPED=$((SKIPPED + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

PG_CACHE="${D5_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE for future runs)"
    fi
}

# PID-scoped teardown ONLY — never pkill/killall by name. Another gate may be
# running concurrently on a different port block.
cleanup() {
    for pid_var in HEADLESS_PID GUI1_PID GUI2_PID; do
        pid="${!pid_var}"
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null
            for _ in $(seq 1 25); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
            kill -9 "$pid" 2>/dev/null
        fi
    done
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$VAULT_B" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vaultA=$VAULT vaultB=$VAULT_B work=$WORK_DIR"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

ports_free_list() { # ports_free_list <port>...
    local p
    for p in "$@"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

ports_all_free_list() { # ports_all_free_list <port>...
    local p ok=0
    for p in "$@"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            ok=1
        fi
    done
    return "$ok"
}

wait_ready() { # wait_ready <log> <timeout>
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

stop_gui1() {
    [ -n "$GUI1_PID" ] || return 0
    kill -TERM "$GUI1_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$GUI1_PID" 2>/dev/null || { GUI1_PID=""; return 0; }; sleep 1; done
    kill -9 "$GUI1_PID" 2>/dev/null; GUI1_PID=""
}

d5helper() { python3 "$HELPER" "$@"; }
n5helper() { python3 "$N5_HELPER" "$@"; }

# switch_vault <target_path>: drive the control server's `/open` — the EXACT
# `VaultRunner::switch_to` the GUI's File > Open Vault callback calls (same
# technique n5-vaults.sh / d3-menus.sh already established). Re-reads the
# token afterward (provisioning mints a fresh one — see n5_vaults_helper's
# module doc).
switch_vault() {
    local target="$1"
    local resp
    resp="$(curl -fsS --max-time "$SWITCH_TIMEOUT" -X POST "$CONTROL/open" \
        -H 'Content-Type: application/json' -d "{\"path\": \"$target\"}" 2>"$WORK_DIR/switch.err")"
    if ! echo "$resp" | grep -qE '"ok":[[:space:]]*true'; then
        echo "switch to $target failed: $resp $(cat "$WORK_DIR/switch.err" 2>/dev/null)" >&2
        return 1
    fi
    read_token
}

echo "== D5 documents gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   second (would-be) ports: proxy=$SECOND_PROXY_PORT backend=$SECOND_BACKEND_PORT pg=$SECOND_POSTGRES_PORT valkey=$SECOND_VALKEY_PORT control=$SECOND_CONTROL_PORT"
echo "   data:   $DATA_DIR"
echo "   vaultA: $VAULT"
echo "   vaultB: $VAULT_B"

# --- pre-flight --------------------------------------------------------------
for h in "$HELPER" "$N5_HELPER"; do
    [ -f "$h" ] || { fail "helper missing: $h"; exit 1; }
done
if PENPOT_BACKEND="$BASE" PENPOT_TOKEN=x python3 -c "
import sys
sys.path.insert(0, '$ROOT/scripts')
import d5_docs_helper
"; then
    pass "preflight: d5_docs_helper.py imports cleanly (and, transitively, n5_vaults_helper.py)"
else
    fail "preflight: d5_docs_helper.py failed to import"; exit 1
fi
ports_free_list "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT" ||
    { fail "one of the primary D5 ports is busy"; exit 1; }
ports_free_list "$SECOND_PROXY_PORT" "$SECOND_BACKEND_PORT" "$SECOND_POSTGRES_PORT" "$SECOND_VALKEY_PORT" "$SECOND_CONTROL_PORT" ||
    { fail "one of the SECOND (would-be) D5 ports is busy — assertion (c) needs it genuinely free"; exit 1; }
if pgrep -f "$GUI_BIN" >/dev/null 2>&1; then
    fail "a penpot-desktop GUI process is already running (the single-instance guard would swallow our launch) — quit it first"
    exit 1
fi
pass "pre-flight: ports free (primary + second block), no existing GUI instance, helpers present"

# =========================================================================
# LEG (e), pure half — the resolver + import-target-path + poll-outcome
# logic every headless-undrivable UI leg (Finder double-click, drag-drop,
# the native confirm dialog) funnels through. Run FIRST, no live stack
# needed, so a broken contract fails fast (D3's own ordering rationale).
# =========================================================================
echo "-- (e) cargo test -p penpot-desktop -- docopen::"
(cd "$ROOT" && cargo test -p penpot-desktop -- docopen::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
CARGO_TEST_STRIPPED="$WORK_DIR/cargo-test.stripped.log"
strip_ansi <"$CARGO_TEST_LOG" >"$CARGO_TEST_STRIPPED"

REQUIRED_TESTS=(
    "docopen::tests::a_known_in_vault_penpot_resolves_to_its_file_id|the core resolve() happy path: an in-vault, already-manifested .penpot opens by id"
    "docopen::tests::a_non_penpot_path_is_rejected|a directory that isn't a .penpot must never be treated as one"
    "docopen::tests::an_external_penpot_is_flagged_for_import|outside-the-vault -> offer-to-import, never a silent open"
    "docopen::tests::a_vault_internal_penpot_with_no_manifest_entry_yet_is_pending_not_external|a freshly-created in-vault dir must NOT be offered for import (would duplicate it)"
    "docopen::tests::an_unresolvable_vault_root_fails_closed_not_open|the in/out-of-vault boundary is P0: an uncanonicalizable vault_root must refuse to classify, never fall back to raw-string comparison"
    "docopen::tests::dotdot_cannot_make_an_in_vault_path_look_external|a .. traversal must not misclassify an in-vault file as external (spill vector)"
    "docopen::tests::a_fresh_name_lands_directly_under_the_imported_folder|import_target_rel_path's base case — this is the EXACT destination this gate's assertion (d) also copies into by hand"
    "docopen::tests::a_taken_name_gets_a_numeric_suffix_deterministically|a name collision on import gets a suffix, never an overwrite"
    "docopen::tests::the_suffix_walk_skips_every_already_taken_candidate|the suffix walk is exhaustive, not off-by-one"
    "docopen::tests::a_name_without_the_penpot_suffix_is_still_handled|import_target_rel_path stays defensive even off its only real caller's guarantee"
    "docopen::tests::a_path_separator_in_the_source_name_cannot_escape_the_imported_folder|a maliciously-named source can never escape Imported/ via a smuggled path separator"
    "docopen::tests::a_found_id_is_ready_regardless_of_elapsed_time|poll_outcome: an id found on the FIRST read is Ready immediately, no needless waiting"
    "docopen::tests::no_id_yet_and_time_left_means_keep_waiting|poll_outcome: not found + time left -> Waiting, not a premature give-up"
    "docopen::tests::no_id_and_the_timeout_reached_gives_up|poll_outcome: the poll is BOUNDED — it must eventually give up, never spin forever"
)

for entry in "${REQUIRED_TESTS[@]}"; do
    name="${entry%%|*}"
    why="${entry#*|}"
    line="$(grep -F "test ${name} " "$CARGO_TEST_STRIPPED" || true)"
    if [ -z "$line" ]; then
        fail "(e) required test NOT FOUND in the run: $name — $why (a rename or removal would hide behind this gate)"
    elif echo "$line" | grep -q '\.\.\. ok$'; then
        pass "(e) $name — $why"
    else
        fail "(e) $name FAILED — $why : $line"
    fi
done

if [ "$CARGO_TEST_EXIT" -eq 0 ]; then
    pass "(e) cargo test -p penpot-desktop -- docopen:: exits 0 (no unlisted regression hiding behind the named checks alone)"
else
    fail "(e) cargo test -p penpot-desktop -- docopen:: exited $CARGO_TEST_EXIT — see $CARGO_TEST_LOG"
fi

# =========================================================================
# BUILD — one command builds both the headless and GUI bins.
# =========================================================================
echo "-- build (penpot-desktop package: headless + penpot-desktop GUI)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop); then
    fail "cargo build -p penpot-desktop"; exit 1
fi
[ -x "$HEADLESS_BIN" ] || { fail "built binary missing: $HEADLESS_BIN"; exit 1; }
[ -x "$GUI_BIN" ] || { fail "built binary missing: $GUI_BIN"; exit 1; }
pass "cargo build -p penpot-desktop (headless + penpot-desktop GUI)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
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
# BOOTSTRAP — a throwaway headless boot to seed DocA + DocB as GENUINE,
# already-synced-to-disk .penpot dirs BEFORE the GUI ever launches: (a)
# requires the CLI argument to already resolve in-vault, which needs a real
# manifest entry, which needs a real sync-daemon poll cycle — nothing this
# gate can shortcut. Shut down cleanly before the GUI boots (M4 finding:
# postgres refuses a shared data dir from two processes at once).
# =========================================================================
echo "-- bootstrap (headless): seed DocA + DocB"
env "${BOOT_ENV[@]}" "$HEADLESS_BIN" >"$LOG_HEADLESS" 2>&1 &
HEADLESS_PID=$!
if wait_ready "$LOG_HEADLESS" "$FIRST_BOOT_TIMEOUT"; then
    pass "bootstrap: headless reaches READY"
else
    fail "bootstrap: headless never reached READY"; exit 1
fi
read_token || { fail "bootstrap: no access token in $DATA_DIR/credentials.json"; exit 1; }

DOCS_JSON="$(d5helper seed_docs "$WORK_DIR" "$VAULT" "$DISK_POLL_TIMEOUT" 2>"$WORK_DIR/seed-docs.err")"
if [ -n "$DOCS_JSON" ] && echo "$DOCS_JSON" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["docs"]["DocA"]["fileId"] and d["docs"]["DocB"]["fileId"] else 1)' 2>/dev/null; then
    pass "bootstrap: DocA + DocB created via RPC and synced to disk as real .penpot dirs"
else
    fail "bootstrap: seed_docs failed: $(cat "$WORK_DIR/seed-docs.err" 2>/dev/null)"; exit 1
fi
DOCA_ID="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocA"]["fileId"])')"
DOCA_TITLE="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocA"]["title"])')"
DOCA_ABS="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocA"]["absPath"])')"
DOCB_ID="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocB"]["fileId"])')"
DOCB_TITLE="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocB"]["title"])')"
DOCB_ABS="$(echo "$DOCS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["docs"]["DocB"]["absPath"])')"
echo "   DocA: id=$DOCA_ID title=$DOCA_TITLE path=$DOCA_ABS"
echo "   DocB: id=$DOCB_ID title=$DOCB_TITLE path=$DOCB_ABS"

stop_headless
sleep 1
if ports_all_free_list "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; then
    pass "bootstrap: clean shutdown, primary ports freed before the GUI boots"
else
    fail "bootstrap: ports still busy after headless shutdown"; exit 1
fi

# =========================================================================
# (a)+(b) — GUI instance 1, launched WITH DocA's path as argv.
# =========================================================================
echo "-- (a) GUI instance 1: launch with DocA as a CLI argument (REQUIRES A GUI SESSION)"
env "${BOOT_ENV[@]}" PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" "$GUI_BIN" "$DOCA_ABS" >"$LOG_GUI1" 2>&1 &
GUI1_PID=$!

if d5helper wait_reachable "$CONTROL" "$GUI_BOOT_TIMEOUT" >"$WORK_DIR/wait-reachable.out" 2>"$WORK_DIR/wait-reachable.err"; then
    pass "(a) proof of looking: the control server is reachable and GET /windows shows a non-empty array with the home window — an unreachable/empty response would have FAILED this, not read as 'no document'"
else
    fail "(a) control server never became reachable with a real /windows body: $(cat "$WORK_DIR/wait-reachable.err" 2>/dev/null)"
    kill -0 "$GUI1_PID" 2>/dev/null || { echo "   GUI instance 1 died; log tail:" >&2; tail -40 "$LOG_GUI1" >&2; }
    exit 1
fi

RESP_A="$(d5helper wait_file_window "$CONTROL" "$DOCA_ID" "$DOCA_TITLE" "$GUI_BOOT_TIMEOUT" 2>"$WORK_DIR/wait-docA.err")"
if echo "$RESP_A" | grep -qE '"ok":[[:space:]]*true'; then
    DOCA_LABEL="$(echo "$RESP_A" | python3 -c 'import json,sys; print(json.load(sys.stdin)["label"])')"
    pass "(a) launching with a path argument opened that file: /windows shows fileId=$DOCA_ID title='$DOCA_TITLE' (label=$DOCA_LABEL)"
else
    fail "(a) DocA's window never appeared with the right title: $RESP_A $(cat "$WORK_DIR/wait-docA.err" 2>/dev/null)"
    exit 1
fi

# =========================================================================
# (b) — the window title tracks the open file: rename it live.
# =========================================================================
echo "-- (b) rename DocA live and assert the open window's title updates"
RENAME_RESP="$(d5helper rename_file "$DOCA_ID" "DocA Renamed" 2>"$WORK_DIR/rename.err")"
if echo "$RENAME_RESP" | grep -qE '"ok":[[:space:]]*true'; then
    pass "(b) POST /__api/vault/manage/rename DocA -> 'DocA Renamed': ok:true"
else
    fail "(b) rename RPC failed: $RENAME_RESP $(cat "$WORK_DIR/rename.err" 2>/dev/null)"
fi
if d5helper wait_window_title "$CONTROL" "$DOCA_LABEL" "DocA Renamed" "$RENAME_TIMEOUT" >/dev/null 2>"$WORK_DIR/wait-rename.err"; then
    pass "(b) the open window's title tracks the rename: /windows now shows title='DocA Renamed' for label=$DOCA_LABEL (D5 Task 6's watch_rename_titles wiring)"
else
    fail "(b) the window title never updated after the rename: $(cat "$WORK_DIR/wait-rename.err" 2>/dev/null)"
fi

# =========================================================================
# (c) — a second launch forwards instead of double-booting.
# =========================================================================
echo "-- (c) GUI instance 2: launch with DocB, a DISTINCT free port block"
WINDOWS_BEFORE="$(d5helper window_count "$CONTROL" 2>/dev/null || echo '?')"
: >"$LOG_GUI2"
env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
    PENPOT_LOCAL_PROXY_PORT="$SECOND_PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$SECOND_BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$SECOND_POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$SECOND_VALKEY_PORT" \
    PENPOT_LOCAL_CONTROL_PORT="$SECOND_CONTROL_PORT" \
    "$GUI_BIN" "$DOCB_ABS" >"$LOG_GUI2" 2>&1 &
GUI2_PID=$!

EXIT_DEADLINE=$(($(date +%s) + SECOND_EXIT_TIMEOUT))
while kill -0 "$GUI2_PID" 2>/dev/null && [ "$(date +%s)" -lt "$EXIT_DEADLINE" ]; do sleep 1; done
if kill -0 "$GUI2_PID" 2>/dev/null; then
    fail "(c) instance 2 did not exit within ${SECOND_EXIT_TIMEOUT}s — killing it"
    kill -9 "$GUI2_PID" 2>/dev/null
else
    pass "(c) instance 2 exited promptly (the single-instance guard intercepted it before .setup())"
fi
GUI2_PID=""

LOG_GUI1_STRIPPED="$WORK_DIR/gui1.stripped.log"
LOG_GUI2_STRIPPED="$WORK_DIR/gui2.stripped.log"
strip_ansi <"$LOG_GUI1" >"$LOG_GUI1_STRIPPED"
strip_ansi <"$LOG_GUI2" >"$LOG_GUI2_STRIPPED"
if grep -qF "penpot stack ready" "$LOG_GUI1_STRIPPED"; then
    pass "(c) proof of looking: instance 1's OWN log (same boot() code path) DOES carry 'penpot stack ready' — the string genuinely appears for a real boot, so its absence below cannot be an artifact"
else
    fail "(c) instance 1's log never shows 'penpot stack ready' either — this leg's absence check below would be meaningless"
fi
BOOT_LEAK=0
for needle in "penpot stack ready" "opening vault" "READY "; do
    if grep -qF "$needle" "$LOG_GUI2_STRIPPED"; then
        fail "(c) DOUBLE-BOOT: instance 2's log contains '$needle' — a second stack tried to boot"
        BOOT_LEAK=1
    fi
done
[ "$BOOT_LEAK" -eq 0 ] && pass "(c) instance 2's log shows NONE of 'penpot stack ready' / 'opening vault' / 'READY ' — no second stack ever tried to boot"

if ports_all_free_list "$SECOND_PROXY_PORT" "$SECOND_BACKEND_PORT" "$SECOND_POSTGRES_PORT" "$SECOND_VALKEY_PORT" "$SECOND_CONTROL_PORT"; then
    pass "(c) the SECOND (would-be) port block was never bound — nothing double-booted a stack on it"
else
    fail "(c) something bound one of the SECOND port block's ports — a second stack DID boot"
fi

if curl -fsS --max-time 10 "$CONTROL/health" >/dev/null 2>&1; then
    pass "(c) instance 1 still answers GET /health after the second launch"
else
    fail "(c) instance 1 stopped answering /health after the second launch"
fi

RESP_B="$(d5helper wait_file_window "$CONTROL" "$DOCB_ID" "$DOCB_TITLE" "$GUI_BOOT_TIMEOUT" 2>"$WORK_DIR/wait-docB.err")"
if echo "$RESP_B" | grep -qE '"ok":[[:space:]]*true'; then
    pass "(c) THE forwarding proof: instance 1's /windows now ALSO shows DocB (fileId=$DOCB_ID title='$DOCB_TITLE') — instance 2 forwarded its document instead of merely refusing to launch"
else
    fail "(c) DocB's window never appeared on instance 1 — the second launch's document was NOT forwarded: $RESP_B $(cat "$WORK_DIR/wait-docB.err" 2>/dev/null)"
fi
WINDOWS_AFTER="$(d5helper window_count "$CONTROL" 2>/dev/null || echo '?')"
echo "   window count: before=$WINDOWS_BEFORE after=$WINDOWS_AFTER"
if [ "$WINDOWS_BEFORE" != "?" ] && [ "$WINDOWS_AFTER" != "?" ] && [ "$WINDOWS_AFTER" -gt "$WINDOWS_BEFORE" ]; then
    pass "(c) a NEW window appeared on instance 1 ($WINDOWS_BEFORE -> $WINDOWS_AFTER) — corroborates the forwarding, not a no-op refusal"
else
    fail "(c) window count did not grow on instance 1 ($WINDOWS_BEFORE -> $WINDOWS_AFTER)"
fi

# =========================================================================
# (d) — import-external keeps zero cross-vault spill.
# =========================================================================
echo "-- (d) vault B: switch, seed, prove it's genuinely live"
if switch_vault "$VAULT_B"; then
    pass "(d) control /open -> vault B: ok:true"
else
    fail "(d) switch to vault B failed"; exit 1
fi
if n5helper seed "$WORK_DIR" B d5needleB >/dev/null 2>"$WORK_DIR/seedB.err"; then
    pass "(d) seeded vault B with n5_vaults_helper's own overlapping-project content + needle 'd5needleB'"
else
    fail "(d) seed B failed"; cat "$WORK_DIR/seedB.err" >&2; exit 1
fi
if n5helper wait_synced "$WORK_DIR" B "$VAULT_B" "$SWITCH_TIMEOUT" >/dev/null 2>"$WORK_DIR/syncB.err" &&
    n5helper wait_present "$WORK_DIR" B "$SWITCH_TIMEOUT" >/dev/null 2>"$WORK_DIR/presentB.err"; then
    pass "(d) proof of looking: vault B is genuinely live — its own files are on disk AND present in its DB/search (never a vacuous empty-vault absence check below)"
else
    fail "(d) vault B never settled"; cat "$WORK_DIR/syncB.err" "$WORK_DIR/presentB.err" >&2 2>/dev/null; exit 1
fi

echo "-- (d) vault A: switch back, seed an external-import source"
if switch_vault "$VAULT"; then
    pass "(d) control /open -> vault A: ok:true"
else
    fail "(d) switch back to vault A failed"; exit 1
fi
NEEDLE_JSON="$(d5helper seed_needle_file "$WORK_DIR" "$VAULT" ImportSeed d5importneedle "$DISK_POLL_TIMEOUT" 2>"$WORK_DIR/seed-needle.err")"
if [ -n "$NEEDLE_JSON" ] && echo "$NEEDLE_JSON" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["fileId"] else 1)' 2>/dev/null; then
    pass "(d) seeded a needle-bearing .penpot in vault A (RPC-created, synced to disk) to serve as the external-import source"
else
    fail "(d) seed_needle_file failed: $(cat "$WORK_DIR/seed-needle.err" 2>/dev/null)"; exit 1
fi
SRC_ABS="$(echo "$NEEDLE_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["absPath"])')"

EXTERNAL_DIR="$WORK_DIR/external-src"
EXTERNAL_SRC="$EXTERNAL_DIR/Imported-From-Outside.penpot"
mkdir -p "$EXTERNAL_DIR"
if cp -R "$SRC_ABS" "$EXTERNAL_SRC"; then
    pass "(d) copied the seeded .penpot to $EXTERNAL_SRC — physically OUTSIDE both vault A and vault B (\"a file the user has lying around\")"
else
    fail "(d) could not copy the seed file out to an external scratch path"; exit 1
fi

# The native confirm dialog cannot be clicked headlessly (brief's explicit
# fallback): copy the external source into A's vault root under Imported/
# ourselves — EXACTLY the destination `docopen::import_target_rel_path`
# would choose for a fresh name (pinned by the required
# `docopen::tests::a_fresh_name_lands_directly_under_the_imported_folder`
# test above) — and let the daemon's OWN, untouched Direction B notice it.
IMPORTED_REL="Imported/Imported-From-Outside.penpot"
mkdir -p "$VAULT/Imported"
if cp -R "$EXTERNAL_SRC" "$VAULT/$IMPORTED_REL"; then
    pass "(d) copied the external .penpot into vault A at $IMPORTED_REL (the same target copy_into_vault would choose)"
else
    fail "(d) could not copy the external .penpot into vault A"; exit 1
fi

IMPORTED_ID="$(d5helper wait_manifest_id "$VAULT" "$IMPORTED_REL" "$DISK_POLL_TIMEOUT" 2>"$WORK_DIR/wait-imported.err")"
if [ -n "$IMPORTED_ID" ]; then
    pass "(d) the sync daemon's Direction B noticed $IMPORTED_REL and assigned it file id $IMPORTED_ID"
else
    fail "(d) the daemon never imported $IMPORTED_REL: $(cat "$WORK_DIR/wait-imported.err" 2>/dev/null)"; exit 1
fi

PRESENT_A="$(d5helper assert_present "$IMPORTED_ID" d5importneedle 2>&1)"
if [ $? -eq 0 ]; then
    pass "(d) the imported file is present in A's DB/boards/search: $PRESENT_A"
else
    fail "(d) the imported file is NOT fully present in A: $PRESENT_A"
fi

echo "-- (d) switch to B and assert zero spill"
if switch_vault "$VAULT_B"; then
    pass "(d) control /open -> vault B (for the absence check): ok:true"
else
    fail "(d) switch to vault B (for the absence check) failed"; exit 1
fi
ABSENT_B="$(d5helper assert_absent "$IMPORTED_ID" d5importneedle 2>&1)"
if [ $? -eq 0 ]; then
    pass "(d) ZERO CROSS-VAULT SPILL: the imported file is in NONE of B's DB/boards/search: $ABSENT_B"
else
    fail "(d) SPILL: the imported file leaked into vault B: $ABSENT_B"
fi

FOUND_ON_DISK_B="$(find "$VAULT_B" -name 'Imported-From-Outside.penpot' 2>/dev/null | wc -l | tr -d ' ')"
if [ "$FOUND_ON_DISK_B" = "0" ]; then
    pass "(d) the imported .penpot dir never touched vault B's folder tree on disk"
else
    fail "(d) the imported .penpot dir appeared under vault B's tree on disk (count=$FOUND_ON_DISK_B)"
fi
if [ -d "$VAULT/$IMPORTED_REL" ]; then
    pass "(d) sanity: the imported .penpot dir still sits under vault A's OWN tree on disk ($IMPORTED_REL) — the vault switch away from A did not touch the folder tree, only the disposable DB/index"
else
    fail "(d) sanity failed: $VAULT/$IMPORTED_REL is gone from disk"
fi

# =========================================================================
# (e), headless-undrivable legs — printed as SKIPPED, kept OUT of the pass
# count. The underlying logic is REQUIRED above via the pinned
# docopen::tests:: names, so a gap here is never a silent one.
# =========================================================================
skip "(e) Finder double-click on a .penpot — needs a SIGNED, INSTALLED .app for LaunchServices to bind as the default handler (scripts/d5a-finder-spike.sh proved the mechanism with a throwaway signed app; scripts/d5-plist-check.sh proves the SHIPPED bundle's Info.plist carries the merged package-document-type declaration). No headless equivalent for either exists."
skip "(e) drag-and-drop of a .penpot onto a window — no headless drag primitive is available to this gate. The drop handler funnels into the exact same open_document() this gate already drives via CLI argv/second-launch (menubar/mod.rs's DragDrop event arm), and the routing decision it calls into is covered by the required docopen::tests:: above."

# --- final shutdown ----------------------------------------------------------
echo "-- final shutdown"
if stop_gui1; then
    pass "final clean shutdown of instance 1"
else
    fail "instance 1 did not shut down within 25s"
fi
sleep 1
if ports_all_free_list "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; then
    pass "all primary D5 ports freed"
else
    fail "primary D5 ports still busy after final shutdown"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D5 DOCUMENTS: ALL PASS ($SKIPPED SKIPPED)"
    exit 0
else
    echo "D5 DOCUMENTS: $FAILURES FAILURE(S) ($SKIPPED SKIPPED)"
    exit 1
fi
