#!/usr/bin/env bash
# N6 template-gallery gate (PLAN2.md milestone N6, `just n6`).
#
# Pillar 7: an OFFLINE template gallery (the "Arduino Examples menu" move) seeded
# from the 15 builtin-template binfiles shipped in the runtime bundle
# (`runtime/backend/builtin-templates/`), and a "New file from template" verb
# that imports one as a REAL working `.penpot` file into the active vault's
# default project — the sync daemon then materializes the tree on disk (folder =
# source of truth; "surface, don't apply").
#
# Exit criteria (PLAN2.md N6), each a PASS/FAIL block in the house style:
#   (a) GET /__api/templates lists EXACTLY the shippable set (15 builtin
#       binfiles: 4 v3-zip + 11 legacy-v1, both import cleanly per the spike),
#       each with a display name, format and size.
#   (b) /__templates serves OFFLINE — 200, no external URL anywhere in the page.
#   (c) for a REPRESENTATIVE template of EACH import path — a CLEAN v3-zip, an
#       ASSET-HEAVY v3-zip (cached thumbnails + orphaned media GC'd on the first
#       in-place re-import: tokens-starter-kit), and three legacy (migration
#       settle path): POST /__api/templates/new → a real `.penpot` dir appears on
#       disk in the vault, its page/board count + text content match the
#       template, and the materialized tree round-trips A=B per roundtrip.py
#       semantics (import-as-new remaps ids, so no hash equality with the source
#       — the NEW tree must itself be a settle-until-fixpoint tree).
#   (d) an invalid templateId → a clean 4xx, no crash (stack still serves).
#
# Bounded on purpose: the default set imports SMALL representatives so it stays
# fast, but it now INCLUDES tokens-starter-kit — the fast asset-heavy v3 case
# whose first export is not a fixpoint — so the "v3 origins are always clean"
# blind spot cannot recur silently. The one genuinely heavy template,
# penpot-design-system (37 MB / 41 pages / 11338 boards), is gated behind
# N6_INCLUDE_HEAVY=1 (opt-in) because a single settle+round-trip on it takes
# minutes; it is verified live at least once and its cost documented in
# docs/milestones/n6.md, but kept out of the default run so the gate stays
# reasonable. The full packaged ladder (dmg + m4 + n1..n5 + just e2e) is the
# orchestrator's job.
#
# Dedicated ports (ledger): proxy 8954, backend 6415, postgres 5489, valkey
# 6432 (control 8955 + exporter 6495 reserved; renders OFF — templates need no
# renders, keeps the gate fast). Fresh mktemp dirs; ANSI-stripped log greps;
# pg-install cache seeded; dirs kept on failure. Run-twice idempotent (fresh
# dirs each run).
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N6_PROXY_PORT:-8954}"
BACKEND_PORT="${N6_BACKEND_PORT:-6415}"
POSTGRES_PORT="${N6_POSTGRES_PORT:-5489}"
VALKEY_PORT="${N6_VALKEY_PORT:-6432}"
FIRST_BOOT_TIMEOUT="${N6_TIMEOUT:-900}"
MATERIALIZE_TIMEOUT="${N6_MATERIALIZE_TIMEOUT:-180}"
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

# Representative templates: a clean v3-zip + an asset-heavy v3-zip
# (tokens-starter-kit: cached thumbnails + orphaned media GC'd on first in-place
# re-import, so it needs a settle cycle to reach a fixpoint) + three legacy
# (migration settle path). Kept small so the gate stays bounded — but the
# asset-heavy v3 case is always exercised so the settle gating can't blind again.
REP_TEMPLATES=(black-white-mobile-templates tokens-starter-kit welcome ux-notes open-color-scheme)
# Opt-in heavy leg: penpot-design-system is 37 MB / 11338 boards; one
# settle+round-trip on it takes minutes. Verified live once (see n6.md); off by
# default so the gate stays fast. Set N6_INCLUDE_HEAVY=1 to include it.
if [ "${N6_INCLUDE_HEAVY:-0}" = "1" ]; then
    REP_TEMPLATES+=(penpot-design-system)
    # The heavy leg needs a longer materialize/settle window.
    MATERIALIZE_TIMEOUT="${N6_MATERIALIZE_TIMEOUT:-600}"
fi

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n6-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n6-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n6-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/n6_templates_helper.py"
HEADLESS_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

PG_CACHE="${N6_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
    pkill -9 -f "$DATA_DIR" 2>/dev/null
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
n6helper() { python3 "$HELPER" "$@"; }

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$@" \
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

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    [ -n "$PENPOT_TOKEN" ]
}

# ---------------------------------------------------------------------------
echo "== N6 template gallery gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   vault (empty): $DESIGNS_DIR"

if [ ! -x "$BIN" ]; then
    echo "building headless binary…"
    cargo build -p penpot-desktop --bin headless >>"$LOG" 2>&1 ||
        { echo "cargo build failed; see $LOG" >&2; exit 1; }
fi

echo "booting headless stack (renders OFF)…"
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then
    pass "boot READY on an empty vault"
else
    fail "boot never reached READY"
    exit 1
fi

if ! read_token; then
    fail "no access token in credentials.json (cannot drive RPC)"
    exit 1
fi

# (a) catalog lists exactly the shippable set.
if n6helper assert_catalog "$BASE"; then :; else fail "(a) catalog"; fi

# (b) gallery offline.
if n6helper assert_gallery_offline "$BASE"; then :; else fail "(b) gallery offline"; fi

# (c) new-from-template for each representative → on disk, content match, A=B.
for tpl in "${REP_TEMPLATES[@]}"; do
    if n6helper new_and_verify "$BASE" "$BACKEND" "$PENPOT_TOKEN" \
        "$DESIGNS_DIR" "$tpl" "$MATERIALIZE_TIMEOUT" \
        >"$WORK_DIR/verify-$tpl.out" 2>&1; then
        tail -2 "$WORK_DIR/verify-$tpl.out" | head -1
    else
        fail "(c) new-from-template $tpl"
        cat "$WORK_DIR/verify-$tpl.out" >&2
    fi
done

# (d) invalid templateId → clean 4xx, no crash.
if n6helper assert_invalid "$BASE"; then :; else fail "(d) invalid templateId"; fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "== N6 gate: ALL PASS =="
else
    echo "== N6 gate: $FAILURES FAILURE(S) =="
fi
exit "$FAILURES"
