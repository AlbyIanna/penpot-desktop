#!/usr/bin/env bash
# C2 CLI gate (chapter C). A LIVE gate over the third front-end:
#   (a) TIER-1 verbs run with NO stack — the assert that the tier split is real
#       rather than nominal — and the partial ones declare what they lack
#       ("partial": true + a "missing" array), so an offline answer can never be
#       mistaken for a complete one.
#   (b) A TIER-2/3 verb with no daemon running fails and NAMES `penpot daemon`.
#       Attach-or-fail never self-boots: the data dir is exclusive (a second
#       stack dies on the postgres lock in ~2s) and a warm self-boot costs ~11s.
#   (c) With a daemon up, the same verb succeeds, and runtime.json carries a
#       live pid, the proxy port, the control port and a token.
#   (d) The control port REFUSES a tokenless request (401), a wrong-token
#       request (401), and a browser-shaped one carrying a VALID token
#       (403, Sec-Fetch-Site) — POST /open wipes the DB, and any web page can
#       send a cross-origin POST.
#   (e) runtime.json is REMOVED on clean shutdown, so a stale record never
#       makes the CLI attach to a dead port.
# Dedicated ports: proxy 9062, backend 6522, postgres 5597, valkey 6540,
# control 9063. Chained into `just e2e` — chapter C lands product code.
#
# Two properties of the surface under test shape how the legs are written, and
# both have already produced a check that could not fail:
#
#   * **Exit status is the contract.** Since chapter C task 11 a non-2xx from a
#     Tier-2/3 verb is an `Err`: pretty JSON on stdout on success ONLY, the
#     server's own explanation on stderr, exit 1. Before that fix every 4xx/5xx
#     exited 0, so `penpot boards || fail` could never fire. Every leg here
#     asserts on `$?` (and on stdout being EMPTY when a verb fails), and leg (b)
#     is the demonstration that a leg written this way really can go red.
#   * **`202` is success.** `reboot` and the Preferences vault-switch route ack
#     and detach, because the reboot tears down the very router that would have
#     answered. A leg checking for a literal `200` would call a working reboot
#     broken; nothing here compares status codes except the three auth
#     assertions in leg (d), where the code IS the assertion.
#
# Every CLI invocation runs under a hard wall-clock bound (`run_verb_within`),
# and the reboot leg asserts that NOTHING had to be killed. Both halves are
# load-bearing and were learned the hard way here: without the bound, a verb
# that wedges hangs the whole gate (a deadline loop whose body can block forever
# has no deadline); WITH the bound but without the assertion, killing the wedged
# verb releases whatever it was wedged on, so the leg goes green against a
# deadlocked app. The gate did exactly that before the assertion was added.
#
# The vault index is its OWN barrier, separate from disk-sync and from the DB:
# `/__api/vault/boards` and `/__api/vault/search` answer 503 until the indexer
# has created its schema, and task 11 deliberately classifies 503 as a FAILURE
# so a gate can never read "index still building" as "this vault has zero
# boards". `wait_index` below therefore POLLS for the barrier — it never sleeps
# a fixed interval and hopes.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, git, python3, curl. Renders stay OFF (no exporter).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${C2_PROXY_PORT:-9062}"
BACKEND_PORT="${C2_BACKEND_PORT:-6522}"
POSTGRES_PORT="${C2_POSTGRES_PORT:-5597}"
VALKEY_PORT="${C2_VALKEY_PORT:-6540}"
CONTROL_PORT="${C2_CONTROL_PORT:-9063}"
FIRST_BOOT_TIMEOUT="${C2_TIMEOUT:-900}"
INDEX_TIMEOUT="${C2_INDEX_TIMEOUT:-300}"
REBOOT_TIMEOUT="${C2_REBOOT_TIMEOUT:-600}"
STOP_TIMEOUT=60

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-c2-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-c2-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-c2-work.XXXXXX")"
LOG="$WORK_DIR/daemon.log"
CLI="$ROOT/target/debug/penpot"
RECORD="$DATA_DIR/runtime.json"
CONTROL="http://127.0.0.1:${CONTROL_PORT}"
DAEMON_PID=""
FAILURES=0
# Last boards listing seen by `wait_index`: total cards, and how many are real
# indexed boards rather than placeholders. Initialized for `set -u`.
BOARDS_COUNT=0
BOARDS_REAL=0

# The one template this gate imports (Tier 3). `ux-notes` is 48 KB — the
# smallest of the 15 builtins with real boards — and is already one of
# n6-templates.sh's representative set, so the import path it exercises is
# proven elsewhere and cheap to re-run here.
TEMPLATE="${C2_TEMPLATE:-ux-notes}"
PROBE_NAME="c2cliprobe"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

PG_CACHE="${C2_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE for future runs)"
    fi
}

cleanup() {
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null
        for _ in $(seq 1 20); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$DAEMON_PID" 2>/dev/null
    fi
    # belt-and-braces: reap any stray stack children under this data dir
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT log=$LOG"
    fi
}
trap cleanup EXIT

# --- small helpers ---------------------------------------------------------

# jget <dotted.path>: read one value out of the JSON on stdin. Prints the value
# (JSON-encoded for objects/arrays, raw for strings) and returns 0; returns 1
# and prints nothing when the path is absent or stdin is not JSON — so `jget`
# failing is itself an assertable outcome, not a silent empty string.
jget() {
    python3 -c '
import json, sys
try:
    node = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for key in sys.argv[1].split("."):
    if isinstance(node, list):
        try:
            node = node[int(key)]
        except Exception:
            sys.exit(1)
    elif isinstance(node, dict) and key in node:
        node = node[key]
    else:
        sys.exit(1)
print(node if isinstance(node, str) else json.dumps(node))
' "$1"
}

# is_json: 0 iff stdin parses as JSON. Used to prove a verb printed a payload
# and not prose.
is_json() { python3 -c 'import json,sys; json.load(sys.stdin)' >/dev/null 2>&1; }

# boards_stats: "<total-cards> <real-board-cards>" from a boards listing on
# stdin. The two differ — see `wait_index`.
boards_stats() {
    python3 -c '
import json, sys
try:
    listing = json.load(sys.stdin)
except Exception:
    print("0 0"); sys.exit(0)
cards = listing.get("boards", [])
print(len(cards), sum(1 for c in cards if c.get("kind") == "board"))
'
}

file_mode() { stat -f '%Lp' "$1" 2>/dev/null || stat -c '%a' "$1" 2>/dev/null; }

# cli <verb...>: run the CLI exactly as a user would, against THIS gate's data
# dir, vault and port block. One wrapper for every tier — Tier 1 reads the
# tree/data dir from this env, Tier 2/3 read runtime.json out of the same data
# dir, so a verb can never be pointed at the wrong instance by accident.
cli() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
        "$CLI" "$@"
}

# run_verb_within <seconds> <tag> <verb...>: run a verb under a hard wall-clock
# limit, capturing stdout to $WORK_DIR/<tag>.json and stderr to
# $WORK_DIR/<tag>.err. Returns the verb's own exit status — which IS the
# contract under test, so it is never swallowed — or a non-zero kill status if
# the limit was hit.
#
# The limit is not decoration. The CLI's loopback client sets no request
# timeout, so a verb issued while the app is tearing its proxy down blocks
# until the app comes back — and a poll loop whose BODY can block forever has
# no deadline at all: it never returns to its own `while` test. `timeout(1)`
# does not exist on macOS, hence the watchdog subshell.
#
# `LAST_KILLED` is 1 iff the watchdog had to fire. That is not bookkeeping: the
# bound makes a wedged app look like an ordinary slow poll, so a leg that only
# watched the eventual outcome would go GREEN against a deadlocked teardown —
# the watchdog itself breaks the cycle by closing the connection. Observed:
# with the `VaultRunner::shutdown` deadlock present, this gate's reboot leg
# passed. `LAST_KILLED` is what makes the wedge visible.
VERB_TIMEOUT="${C2_VERB_TIMEOUT:-300}"
LAST_OUT=""
LAST_ERR=""
LAST_KILLED=0
run_verb_within() {
    local secs="$1" tag="$2"; shift 2
    LAST_OUT="$WORK_DIR/$tag.json"
    LAST_ERR="$WORK_DIR/$tag.err"
    local killed_flag="$WORK_DIR/$tag.killed"
    rm -f "$killed_flag"
    cli "$@" >"$LAST_OUT" 2>"$LAST_ERR" &
    local pid=$! rc=0
    # `cli` is a shell FUNCTION, so `$pid` is the subshell running it, not the
    # `penpot` binary — bash does not exec a function body the way it execs a
    # bare command. Signalling only `$pid` would reap the subshell and leave
    # the CLI process orphaned, still holding the very connection this bound
    # exists to release. So: descendants first, then the subshell. Verified.
    {
        sleep "$secs"
        kill -0 "$pid" 2>/dev/null || exit 0
        : >"$killed_flag"
        pkill -TERM -P "$pid" 2>/dev/null; kill -TERM "$pid" 2>/dev/null
        sleep 2
        pkill -9 -P "$pid" 2>/dev/null; kill -9 "$pid" 2>/dev/null
    } >/dev/null 2>&1 &
    local killer=$!
    wait "$pid" 2>/dev/null; rc=$?
    kill "$killer" 2>/dev/null; wait "$killer" 2>/dev/null
    LAST_KILLED=0
    [ -f "$killed_flag" ] && LAST_KILLED=1
    return "$rc"
}

# run_verb <tag> <verb...>: the same, under the default limit.
run_verb() { run_verb_within "$VERB_TIMEOUT" "$@"; }

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            return 1
        fi
    done
    return 0
}

start_daemon() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
        "$CLI" daemon >>"$LOG" 2>&1 &
    DAEMON_PID=$!
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            echo "penpot daemon died; last log lines:" >&2; tail -25 "$LOG" >&2; return 1
        fi
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2; return 1
}

stop_daemon() {
    kill -TERM "$DAEMON_PID" 2>/dev/null || return 1
    for _ in $(seq 1 "$STOP_TIMEOUT"); do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then DAEMON_PID=""; return 0; fi
        sleep 1
    done
    return 1
}

# wait_index <timeout> [min-cards] [min-real-boards]: poll `penpot boards` until
# it SUCCEEDS and the listing meets both minimums, leaving it in
# $WORK_DIR/boards.json and its shape in $BOARDS_COUNT / $BOARDS_REAL. This is
# the index barrier — see the header. Polling is
# not politeness here: `/__api/vault/boards` answers 503 while the index is
# still building, that 503 is (correctly) a verb failure, and a leg that ran
# `penpot boards` right after READY would go red for a reason that has nothing
# to do with the CLI.
#
# The `min-real-boards` argument exists because "count >= 1" is NOT the barrier
# it looks like. `boards::assemble_cards` emits a placeholder card of
# `kind == "file"` for a file with zero indexed boards (D2's gap fix), so a
# freshly imported file appears in the listing a poll BEFORE its board rows do.
# Observed: one run crossed `count >= 1` on the placeholder and then failed the
# search leg, because there was no real board to take a query from. A barrier
# that a placeholder satisfies is not a barrier.
wait_index() { # wait_index <timeout> [min-cards] [min-real-boards]
    local deadline=$(($(date +%s) + $1)) min="${2:-0}" min_boards="${3:-0}" stats
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if run_verb_within 60 boards boards; then
            stats="$(boards_stats <"$WORK_DIR/boards.json")"
            BOARDS_COUNT="${stats%% *}"; BOARDS_REAL="${stats##* }"
            if [ "${BOARDS_COUNT:-0}" -ge "$min" ] 2>/dev/null &&
               [ "${BOARDS_REAL:-0}" -ge "$min_boards" ] 2>/dev/null; then
                return 0
            fi
        fi
        kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon died while waiting for the index" >&2; return 1; }
        sleep 2
    done
    echo "index barrier not crossed in ${1}s (want cards>=$min, real boards>=$min_boards); last answer:" >&2
    cat "$WORK_DIR/boards.json" "$WORK_DIR/boards.err" >&2 2>/dev/null
    return 1
}

# Read the per-boot control token fresh on EVERY call, never cached: the record
# is rewritten on each boot, and an empty token (record not yet published) must
# read as a 401 the caller's own poll loop retries, not as a cached stale one.
control_token() { jget token <"$RECORD" 2>/dev/null || true; }
ctl_code() { # ctl_code <curl-args...> — the HTTP status of a control request
    curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$@"
}

echo "== C2 CLI: data=$DATA_DIR vault=$VAULT proxy=:$PROXY_PORT control=$CONTROL"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-cli --bin penpot -p supervisor --bin penpot-watchdog); then
    fail "build (penpot + penpot-watchdog)"; exit 1
fi
pass "build (penpot + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ---------------------------------------------------------------------------
# (a) Tier-1 verbs run with NO stack
# ---------------------------------------------------------------------------
# The premise first: nothing of this gate's port block is listening. Without
# this the whole leg is unfalsifiable — "ran offline" would be indistinguishable
# from "ran against something that happened to be up".
if ports_all_free; then
    pass "(a) premise: no stack — all 5 ports of this gate's block are free"
else
    fail "(a) a port of this gate's block is already in use; leg (a) cannot prove anything"; exit 1
fi

TIER1_TAGS=(vault-list checkpoint open tpl-list pkg-list prefs-get sync-status)
for tag in "${TIER1_TAGS[@]}"; do
    case "$tag" in
        open) run_verb "$tag" open "Designs/nothing.penpot" ;;
        *)    run_verb "$tag" "$tag" ;;
    esac
    rc=$?
    if [ "$rc" -eq 0 ] && is_json <"$LAST_OUT"; then
        pass "(a) penpot $tag ran with no stack and printed JSON on stdout"
    else
        fail "(a) penpot $tag exited $rc / non-JSON stdout"; head -5 "$LAST_ERR" >&2
    fi
done

# The two verbs with real but INCOMPLETE offline answers must SAY SO, naming
# exactly what they cannot see. An offline answer that looks complete is worse
# than no answer: the caller could not otherwise tell it was reading a subset.
if [ "$(jget partial <"$WORK_DIR/pkg-list.json")" = "true" ] &&
   [ "$(jget missing <"$WORK_DIR/pkg-list.json")" = '["live"]' ]; then
    pass "(a) pkg-list declares itself partial, missing exactly [\"live\"]"
else
    fail "(a) pkg-list's partial/missing is not the documented pair: $(jget partial <"$WORK_DIR/pkg-list.json") / $(jget missing <"$WORK_DIR/pkg-list.json")"
fi
if [ "$(jget partial <"$WORK_DIR/sync-status.json")" = "true" ] &&
   [ "$(jget missing <"$WORK_DIR/sync-status.json")" = '["paused", "lastError", "state", "conflict"]' ]; then
    pass "(a) sync-status declares itself partial, missing exactly the four daemon-only fields"
else
    fail "(a) sync-status's partial/missing is not the documented pair: $(jget partial <"$WORK_DIR/sync-status.json") / $(jget missing <"$WORK_DIR/sync-status.json")"
fi

# ...and the other half of the same rule: a COMPLETE offline answer must not
# carry the flag. A blanket `"partial": true` would satisfy the two assertions
# above while telling a caller nothing.
COMPLETE_OK=1
for tag in vault-list checkpoint open tpl-list prefs-get; do
    if jget partial <"$WORK_DIR/$tag.json" >/dev/null 2>&1; then
        fail "(a) penpot $tag flags itself partial, but its offline answer IS complete"
        COMPLETE_OK=0
    fi
done
[ "$COMPLETE_OK" -eq 1 ] &&
    pass "(a) the five complete Tier-1 answers carry no partial flag (the flag discriminates)"

# Two verbs that would be indistinguishable from stubs without a value check:
# tpl-list really read the runtime bundle, and checkpoint really wrote a commit.
TPL_COUNT="$(jget count <"$WORK_DIR/tpl-list.json" 2>/dev/null || echo 0)"
[ "${TPL_COUNT:-0}" -ge 1 ] 2>/dev/null &&
    pass "(a) tpl-list enumerated $TPL_COUNT builtin templates off the runtime bundle, no stack" ||
    fail "(a) tpl-list found no builtin templates (count=$TPL_COUNT)"
if [ "$(jget ok <"$WORK_DIR/checkpoint.json")" = "true" ] &&
   [ -n "$(jget commit <"$WORK_DIR/checkpoint.json" 2>/dev/null)" ]; then
    pass "(a) checkpoint committed the vault offline (decision=$(jget decision <"$WORK_DIR/checkpoint.json"), commit=$(jget commit <"$WORK_DIR/checkpoint.json"))"
else
    fail "(a) checkpoint did not commit offline"; cat "$WORK_DIR/checkpoint.json" >&2
fi

# The leg's real claim: running every Tier-1 verb self-booted NOTHING.
if ports_all_free; then
    pass "(a) after all 7 Tier-1 verbs the ports are still free — no verb self-booted a stack"
else
    fail "(a) a Tier-1 verb left something listening — it booted a stack"
fi

# ---------------------------------------------------------------------------
# (b) Tier-2/3 verbs with no daemon fail, naming `penpot daemon`
# ---------------------------------------------------------------------------
[ ! -f "$RECORD" ] &&
    pass "(b) no runtime.json before any daemon starts" ||
    fail "(b) runtime.json exists with no daemon running"

# One Tier-2 (boards), one Tier-3 (reboot), and the `--live` flip — the same
# verb that just succeeded offline in leg (a), one flag away from needing the
# app. Each must exit NON-ZERO, name the fix, and print NOTHING on stdout: a
# failure object on stdout would be piped into `jq` as though it were a result.
b_attach_fails() { # b_attach_fails <tag> <label> <verb...>
    local tag="$1" label="$2"; shift 2
    run_verb "$tag" "$@"
    local rc=$?
    if [ "$rc" -eq 0 ]; then
        fail "(b) $label exited 0 with no daemon running — attach-or-fail is not failing"
    elif ! grep -q 'penpot daemon' "$LAST_ERR"; then
        fail "(b) $label failed (exit $rc) but never named \`penpot daemon\`: $(head -1 "$LAST_ERR")"
    elif [ -s "$LAST_OUT" ]; then
        fail "(b) $label failed but still wrote to stdout: $(head -1 "$LAST_OUT")"
    else
        pass "(b) $label with no daemon: exit $rc, stdout empty, stderr names \`penpot daemon\`"
    fi
}
b_attach_fails no-daemon-boards  "penpot boards (Tier 2)"        boards
b_attach_fails no-daemon-reboot  "penpot reboot (Tier 3)"        reboot
b_attach_fails no-daemon-live    "penpot prefs-get --live (the Tier-1→2 flip)" prefs-get --live

# ---------------------------------------------------------------------------
# boot the daemon — everything below needs the running app
# ---------------------------------------------------------------------------
start_daemon
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "(c) \`penpot daemon\` booted the stack and printed READY (pid $DAEMON_PID)"
else
    fail "(c) \`penpot daemon\` never became READY"; exit 1
fi

# ---------------------------------------------------------------------------
# (d) the control port refuses tokenless, wrong-token and browser-shaped
# ---------------------------------------------------------------------------
# The control port is unconditional since chapter C, and POST /open wipes the
# disposable DB and reconciles the whole vault from disk. ANY web page the user
# has open can send a cross-origin POST to 127.0.0.1 — the same-origin policy
# blocks READING the response, not SENDING the request. So the server demands
# the per-boot token AND refuses anything browser-shaped even when the token is
# valid.
#
# These four assertions and n5-vaults.sh's two are the ONLY places the auth
# layer is proven to be applied to the router `serve_control` actually serves:
# the unit tests in crates/app-core/src/runtime_record.rs exercise a synthetic
# router they build themselves, and every other control request in this repo's
# gates merely ADDS a header — which an UNGUARDED server would ignore, staying
# green. Delete the `.layer(...)` in `serve_control` and this block is what
# must go red. The POSITIVE control comes first, so a 401/403 below can only
# mean "refused", never "server down".
#
# But first, the record barrier. `READY` does NOT imply `runtime.json` exists:
# the daemon SPAWNS `serve_control` and prints `READY` on the very next
# statement without waiting for it, and it is `serve_control` that binds the
# port and writes the record. Reading the token too early yields an empty
# string, the positive control 401s, and all four assertions below fail
# shouting THE CONTROL PORT IS UNAUTHENTICATED — the chapter's security gate
# raising a false alarm about the chapter's security fix, which is a worse
# failure than the one it is watching for. Every other barrier in this gate is
# a poll; this was the one place relying on statement ordering.
wait_record() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        [ -n "$(control_token)" ] && return 0
        kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon died before publishing runtime.json" >&2; return 1; }
        sleep 1
    done
    echo "runtime.json carried no token within ${1}s" >&2; return 1
}
if wait_record 60; then
    pass "(d) runtime.json carries a token (polled — READY does not imply the record is on disk)"
else
    fail "(d) runtime.json never carried a token; the four auth assertions below cannot be trusted"
fi

AUTH_OK="$(ctl_code -H "x-penpot-control-token: $(control_token)" "$CONTROL/health")"
[ "$AUTH_OK" = "200" ] &&
    pass "(d) positive control: GET /health WITH the runtime.json token -> 200 (the server is up and answering)" ||
    fail "(d) GET /health with a valid token -> '$AUTH_OK', expected 200 — the control server is not answering; the three refusals below prove nothing"
AUTH_NONE="$(ctl_code "$CONTROL/health")"
[ "$AUTH_NONE" = "401" ] &&
    pass "(d) tokenless GET /health -> 401: an unauthenticated caller cannot reach POST /open (the DB wipe)" ||
    fail "(d) tokenless GET /health -> '$AUTH_NONE', expected 401 — THE CONTROL PORT IS UNAUTHENTICATED: anything on this machine can drive POST /open"
AUTH_WRONG="$(ctl_code -H "x-penpot-control-token: not-the-token" "$CONTROL/health")"
[ "$AUTH_WRONG" = "401" ] &&
    pass "(d) wrong-token GET /health -> 401: guessing the header name is not enough" ||
    fail "(d) wrong-token GET /health -> '$AUTH_WRONG', expected 401 — the token is not being COMPARED, only required"
AUTH_BROWSER="$(ctl_code -H "x-penpot-control-token: $(control_token)" \
    -H 'Sec-Fetch-Site: cross-site' "$CONTROL/health")"
[ "$AUTH_BROWSER" = "403" ] &&
    pass "(d) browser-shaped GET /health with a VALID token -> 403: a web page cannot drive the DB wipe even if it learns the token" ||
    fail "(d) browser-shaped GET /health with a valid token -> '$AUTH_BROWSER', expected 403 — ANY WEB PAGE THE USER HAS OPEN CAN REACH POST /open"

# ---------------------------------------------------------------------------
# (c) with a daemon up: the record is well-formed and the same verbs succeed
# ---------------------------------------------------------------------------
if [ -f "$RECORD" ]; then
    pass "(c) runtime.json published at $RECORD"
    REC_PID="$(jget pid <"$RECORD" 2>/dev/null || true)"
    REC_PROXY="$(jget proxy_port <"$RECORD" 2>/dev/null || true)"
    REC_CONTROL="$(jget control_port <"$RECORD" 2>/dev/null || true)"
    REC_TOKEN="$(jget token <"$RECORD" 2>/dev/null || true)"
    [ "$REC_PID" = "$DAEMON_PID" ] && kill -0 "$REC_PID" 2>/dev/null &&
        pass "(c) runtime.json pid $REC_PID is the LIVE daemon (attach's staleness check has something true to check)" ||
        fail "(c) runtime.json pid '$REC_PID' is not the live daemon ($DAEMON_PID)"
    [ "$REC_PROXY" = "$PROXY_PORT" ] &&
        pass "(c) runtime.json proxy_port=$REC_PROXY (the /__api/* surface — Tier 2/3 would 404 without it)" ||
        fail "(c) runtime.json proxy_port='$REC_PROXY', expected $PROXY_PORT"
    [ "$REC_CONTROL" = "$CONTROL_PORT" ] &&
        pass "(c) runtime.json control_port=$REC_CONTROL (the /open surface a vault switch needs)" ||
        fail "(c) runtime.json control_port='$REC_CONTROL', expected $CONTROL_PORT"
    [ "${#REC_TOKEN}" -ge 32 ] &&
        pass "(c) runtime.json carries a ${#REC_TOKEN}-char per-boot token" ||
        fail "(c) runtime.json token is too short to be the per-boot token: '${REC_TOKEN}'"
    REC_MODE="$(file_mode "$RECORD")"
    [ "$REC_MODE" = "600" ] &&
        pass "(c) runtime.json is mode 0600 — the token is not readable by other users" ||
        fail "(c) runtime.json is mode '$REC_MODE', expected 600"
else
    fail "(c) no runtime.json after READY — the CLI has nothing to attach to"
fi

# The index barrier, crossed by POLLING (see wait_index). The elapsed wait is
# printed rather than asserted: on a warm boot it is often 0s, and a leg that
# claimed a long wait it did not perform would be the same kind of check that
# passes without checking. What is asserted is that the verb succeeded — and
# the poll is what makes that assertion about the CLI rather than about how
# fast the indexer happened to be.
IDX_T0=$(date +%s)
if wait_index "$INDEX_TIMEOUT" 0 0; then
    pass "(c) penpot boards (Tier 2) succeeded with the daemon up (waited $(( $(date +%s) - IDX_T0 ))s for the index barrier; 503 while it builds is a verb FAILURE by design)"
else
    fail "(c) penpot boards never succeeded within ${INDEX_TIMEOUT}s"
fi

# The `--live` flip, from the other side: the SAME two verbs leg (a) ran
# offline now answer with the fields their offline forms named as MISSING, and
# no longer flag themselves partial.
if run_verb live-prefs prefs-get --live; then
    if jget syncPaused <"$WORK_DIR/live-prefs.json" >/dev/null &&
       jget preferences <"$WORK_DIR/live-prefs.json" >/dev/null &&
       jget vault.path <"$WORK_DIR/live-prefs.json" >/dev/null; then
        pass "(c) prefs-get --live carries the live facts the file cannot (syncPaused, vault) alongside the preferences"
    else
        fail "(c) prefs-get --live is missing the live-only fields"; cat "$WORK_DIR/live-prefs.json" >&2
    fi
else
    fail "(c) prefs-get --live failed with the daemon up"; head -3 "$WORK_DIR/live-prefs.err" >&2
fi
if run_verb live-sync sync-status --live; then
    if jget syncPaused <"$WORK_DIR/live-sync.json" >/dev/null &&
       ! jget partial <"$WORK_DIR/live-sync.json" >/dev/null 2>&1; then
        pass "(c) sync-status --live answers syncPaused — the field its offline form named as missing — and drops the partial flag"
    else
        fail "(c) sync-status --live still partial / still missing syncPaused"; cat "$WORK_DIR/live-sync.json" >&2
    fi
else
    fail "(c) sync-status --live failed with the daemon up"; head -3 "$WORK_DIR/live-sync.err" >&2
fi

# A Tier-3 verb: `new --from-template` needs the supervised BACKEND, not just
# the running app. It also gives the index something real to hold, so the
# boards/search assertions below cannot pass vacuously on an empty vault.
if run_verb new-tpl new --from-template "$TEMPLATE" --name "$PROBE_NAME"; then
    NEW_FILE_ID="$(jget fileId <"$WORK_DIR/new-tpl.json" 2>/dev/null || true)"
    if [ "$(jget ok <"$WORK_DIR/new-tpl.json")" = "true" ] && [ -n "$NEW_FILE_ID" ]; then
        pass "(c) penpot new --from-template $TEMPLATE (Tier 3) created $NEW_FILE_ID, settled in $(jget settleCycles <"$WORK_DIR/new-tpl.json") cycle(s)"
    else
        fail "(c) new --from-template answered 2xx with no fileId"; cat "$WORK_DIR/new-tpl.json" >&2
    fi
else
    fail "(c) penpot new --from-template $TEMPLATE failed"; head -5 "$WORK_DIR/new-tpl.err" >&2
    NEW_FILE_ID=""
fi

# The same Tier-2 verb again, now with content: this proves the barrier poll
# above was answering about a real vault rather than an empty one. The
# requirement is at least one card AND at least one card of `kind == "board"`
# — see `wait_index` for why the second is not implied by the first.
if wait_index "$INDEX_TIMEOUT" 1 1; then
    pass "(c) penpot boards now lists $BOARDS_COUNT card(s), $BOARDS_REAL of them real indexed boards — the imported file reached the index"
else
    fail "(c) the imported file never reached the boards listing with a real board card"
fi

# `search` shares the index — and the 503. Query a term taken from a REAL
# indexed board name (not a hardcoded fixture string), so the assertion tracks
# whatever the template actually contains.
SEARCH_TERM="$(python3 -c '
import json, re, sys
listing = json.load(sys.stdin)
for card in listing.get("boards", []):
    if card.get("kind") != "board":
        continue
    for word in re.findall(r"[A-Za-z]{4,}", card.get("name", "")):
        print(word)
        sys.exit(0)
sys.exit(1)
' <"$WORK_DIR/boards.json" 2>/dev/null || true)"
if [ -n "$SEARCH_TERM" ]; then
    if run_verb search search "$SEARCH_TERM" --limit 50; then
        HITS="$(jget count <"$WORK_DIR/search.json" 2>/dev/null || echo 0)"
        if [ "${HITS:-0}" -ge 1 ] 2>/dev/null; then
            pass "(c) penpot search '$SEARCH_TERM' returned $HITS hit(s) from the same index"
        else
            fail "(c) penpot search '$SEARCH_TERM' returned no hits though the board is listed"
        fi
    else
        fail "(c) penpot search failed with the daemon up"; head -3 "$WORK_DIR/search.err" >&2
    fi
else
    fail "(c) no real (kind=board) card in the listing — nothing to search for"
fi

# The ack-then-detach Tier-3 verb. `POST /__api/prefs/reboot` answers 202, and
# 202 is SUCCESS: a leg comparing against a literal 200 would call a working
# reboot broken. The verb returning is not the reboot finishing — the outcome
# lands in `lastOp`, which is what the poll below reads.
if run_verb reboot reboot; then
    pass "(c) penpot reboot (Tier 3) accepted — a 202 ack is success, not failure"
else
    fail "(c) penpot reboot failed"; head -5 "$WORK_DIR/reboot.err" >&2
fi
REBOOT_T0=$(date +%s)
REBOOT_DEADLINE=$((REBOOT_T0 + REBOOT_TIMEOUT))
REBOOT_LANDED=0
POLLS_KILLED=0
POLLS_ISSUED=0
POLLS_DURING=0
POLL_BOUND=30
while [ "$(date +%s)" -lt "$REBOOT_DEADLINE" ]; do
    run_verb_within "$POLL_BOUND" after-reboot prefs-get --live
    rc=$?
    POLLS_ISSUED=$((POLLS_ISSUED + 1))
    [ "$LAST_KILLED" -eq 1 ] && POLLS_KILLED=$((POLLS_KILLED + 1))
    if [ "$rc" -eq 0 ] &&
       [ "$(jget lastOp.op <"$WORK_DIR/after-reboot.json" 2>/dev/null || true)" = "reboot" ]; then
        REBOOT_LANDED=1; break
    fi
    # This poll did NOT see the reboot reported complete, so it sampled the
    # teardown/boot window itself — either refused (proxy down), or answered
    # off a stack whose reboot had not finished. That is the overlap the
    # deadlock pin below needs in order to mean anything.
    POLLS_DURING=$((POLLS_DURING + 1))
    kill -0 "$DAEMON_PID" 2>/dev/null || break
    sleep 2
done
REBOOT_SECS=$(( $(date +%s) - REBOOT_T0 ))
if [ "$REBOOT_LANDED" -eq 1 ] && [ "$(jget lastOp.ok <"$WORK_DIR/after-reboot.json")" = "true" ]; then
    pass "(c) the detached reboot landed: lastOp={op:reboot, ok:true} — and the CLI reattached over the SAME record"
else
    fail "(c) the detached reboot never reported success in lastOp"; cat "$WORK_DIR/after-reboot.json" >&2 2>/dev/null
fi

# THE regression pin for the teardown deadlock, and the reason the assertion
# above is not it. A `GET /__api/prefs` in flight when the teardown starts used
# to block on the `app` mutex that `VaultRunner::shutdown` held across the
# proxy's graceful shutdown — proxy waits for the handler, handler waits for
# the lock, lock is held by the code waiting for the proxy. Nothing times out;
# only SIGKILL recovers.
#
# The outcome assertion above CANNOT catch that, and this gate proved it: with
# the bug present, the poll wedged, this script's own watchdog killed it after
# ${POLL_BOUND}s, closing the connection — which released the cycle, let the
# reboot finish, and left `lastOp` reporting a clean success. The leg went
# GREEN against a deadlocked teardown. What distinguishes the two worlds is not
# whether the reboot eventually landed, but whether anything had to be KILLED
# for it to. A healthy reboot answers every poll in well under a second (the
# proxy is either up, or down and refusing instantly).
#
# It is only a pin if a poll actually OVERLAPPED the teardown, and that is a
# timing property, not something `POLLS_KILLED == 0` establishes. If a future
# change made attach slower or the 202 later, every poll could land after the
# reboot had already finished; `POLLS_KILLED == 0` would then be true for the
# wrong reason and this pin would go silently, permanently green. So the
# overlap is asserted directly: at least one poll must have run while the
# reboot was still in flight (refused, or answering off a stack that had not
# finished). Counting merely-issued polls would not do — the loop always runs
# at least once, so that assertion could never fail.
if [ "$POLLS_DURING" -ge 1 ]; then
    pass "(c) the teardown window was really sampled: $POLLS_DURING of $POLLS_ISSUED poll(s) saw the reboot still in flight, over ${REBOOT_SECS}s — the pin below has something to be a pin about"
else
    fail "(c) every poll landed AFTER the reboot had already completed ($POLLS_ISSUED issued in ${REBOOT_SECS}s) — the deadlock pin below never overlapped the teardown and is vacuous"
fi
if [ "$POLLS_KILLED" -eq 0 ]; then
    pass "(c) none of those $POLLS_ISSUED poll(s) had to be killed — the teardown released the app lock instead of deadlocking on the proxy drain"
else
    fail "(c) $POLLS_KILLED of $POLLS_ISSUED poll(s) hung the full ${POLL_BOUND}s during the reboot and had to be killed — THE TEARDOWN DEADLOCKED: VaultRunner::shutdown is holding the app lock across the proxy's graceful shutdown, and only this gate's watchdog broke the cycle"
fi

if wait_index "$INDEX_TIMEOUT" 1 1; then
    pass "(c) boards still lists $BOARDS_REAL real board(s) after the reboot — reboot-in-place kept the vault's state"
else
    fail "(c) boards did not come back after the reboot"
fi

# ---------------------------------------------------------------------------
# (e) the record is removed on clean shutdown
# ---------------------------------------------------------------------------
if stop_daemon; then
    pass "(e) SIGTERM: \`penpot daemon\` shut down cleanly"
else
    fail "(e) \`penpot daemon\` did not exit within ${STOP_TIMEOUT}s of SIGTERM"
fi
grep -q '^STOPPED' "$LOG" &&
    pass "(e) daemon printed STOPPED (the shutdown path ran to the end)" ||
    fail "(e) no STOPPED line — the shutdown path did not complete"
[ ! -f "$RECORD" ] &&
    pass "(e) runtime.json REMOVED on clean shutdown — no stale record pointing at a dead port" ||
    fail "(e) runtime.json survived a clean shutdown: $(cat "$RECORD")"

# And the consequence that matters: the CLI is back to leg (b)'s behaviour
# rather than dialing a port nobody is listening on.
run_verb after-stop boards
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'penpot daemon' "$WORK_DIR/after-stop.err"; then
    pass "(e) after shutdown, penpot boards fails naming \`penpot daemon\` again (exit $rc)"
else
    fail "(e) after shutdown, penpot boards exited $rc without naming \`penpot daemon\`"
fi
sleep 1
ports_all_free &&
    pass "(e) all 5 ports freed" ||
    fail "(e) ports still busy after shutdown"

echo
echo "headline: Tier-1 verbs ran with no stack (ports free before AND after) ; Tier-2/3 with no daemon exit non-zero naming \`penpot daemon\` ; with a daemon they succeed past the index barrier ; the control port refuses tokenless/wrong-token/browser-shaped ; runtime.json removed on clean shutdown"
if [ "$FAILURES" -eq 0 ]; then
    echo "C2 CLI: ALL PASS"
    exit 0
else
    echo "C2 CLI: $FAILURES FAILURE(S)"
    exit 1
fi
