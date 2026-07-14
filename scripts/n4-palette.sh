#!/usr/bin/env bash
# N4 quick-open palette + Peek + Checkpoint-now gate (PLAN2.md milestone N4,
# `just n4`). Exit criteria implemented VERBATIM, each a PASS/FAIL block in the
# house style of n3-home.sh / m5-features.sh:
#
#   (a) torture fixture: the shared N1 seeder plants 100 files / ~1000 boards
#       across 4 projects, mirrored to disk + indexed (the scale at which the
#       grid-scroll fix matters).
#   (b) PALETTE RANKING over HTTP: a board is renamed to a distinctive needle;
#       a fuzzy /__api/vault/palette?q= query ranks THAT board first, kind=board,
#       and the Enter payload is its EXACT /#/workspace deep link. Ranking
#       latency reported.
#   (c) headless-browser PALETTE nav: /__palette, type the needle, Enter →
#       lands the exact deep link (bundled chromium, offline).
#   (d) headless-browser GRID FIX: /__home scrolled + a card focused; after a
#       periodic refresh the scroll position AND the focused card are unchanged
#       (the N3-debt diff/patch fix — no more innerHTML="" on the interval).
#   (e) PEEK: the planted board's .exports render is served (HTTP 200 +
#       content hash equals the on-disk bytes); a board with no render stays
#       degraded (proven by n3's thumb path, reused).
#   (f) REVEAL resolves the right dir (200 for an in-vault path; 400 for
#       traversal).
#   (g) CHECKPOINT NOW: fresh vault → exactly 1 commit (manifest + .penpot
#       dirs); no change → clean no-op; a new change → exactly 1 more commit
#       rewriting NO history; a dirty/in-progress repo (detached HEAD) → LOUD
#       refusal (409) touching nothing — all asserted via git log/git status.
#
# The OS pieces (global-shortcut firing, overlay focus-steal) are NOT asserted
# here — tauri-driver has no macOS support — they ride the manual-QA checklist
# in docs/milestones/n4.md. Everything they REACH (ranked palette API, the
# /__palette page, the viewer route) is gated here + in routes-gate.sh.
#
# Dedicated ports (ledger): proxy 8944, backend 6405, postgres 5479, valkey
# 6422 (exporter 6485 reserved; renders stay OFF — a planted N2-shape render
# proves the Peek path, keeping the 1000-board fixture fast). Fresh mktemp
# dirs; ANSI-stripped log greps; pg-install cache seeded; dirs kept on failure.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK, valkey-server, python3, curl, git, node + the bundled playwright/chromium
# (scripts/fetch-penpot.sh --with-browsers) for the browser legs.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${N4_PROXY_PORT:-8944}"
BACKEND_PORT="${N4_BACKEND_PORT:-6405}"
POSTGRES_PORT="${N4_POSTGRES_PORT:-5479}"
VALKEY_PORT="${N4_VALKEY_PORT:-6422}"
FIRST_BOOT_TIMEOUT="${N4_TIMEOUT:-900}"
FIXTURE_SYNC_TIMEOUT="${N4_FIXTURE_SYNC_TIMEOUT:-600}"
EDIT_TIMEOUT="${N4_EDIT_TIMEOUT:-120}"
BASE="http://localhost:${PROXY_PORT}"

export N1_FILES="${N1_FILES:-100}"
export N1_BOARDS="${N1_BOARDS:-10}"

# The distinctive needle we rename a board to (must fuzzy-rank first for "checkout").
NEEDLE="Checkout-Needle-ZZ"
FUZZY="checkout"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n4-data.XXXXXX")"
DESIGNS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n4-designs.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-n4-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
N1HELPER="$ROOT/scripts/n1_index_helper.py"
N3HELPER="$ROOT/scripts/n3_home_helper.py"
N4HELPER="$ROOT/scripts/n4_palette_helper.py"
NAV_DRIVER="$ROOT/scripts/n4_palette_nav.cjs"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
PLAYWRIGHT="${ROUTES_GATE_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
NODE_BIN="${ROUTES_GATE_NODE:-node}"
HEADLESS_PID=""
FAILURES=0

export N1_DESIGNS_DIR="$DESIGNS_DIR"
export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
warn() { echo "WARNING: $1"; }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
n1helper() { python3 "$N1HELPER" "$@"; }
n3helper() { python3 "$N3HELPER" "$@"; }
n4helper() { python3 "$N4HELPER" "$@"; }
git_v() { git -C "$DESIGNS_DIR" "$@"; }
commits() { git_v rev-list --count HEAD 2>/dev/null || echo 0; }

PG_CACHE="${N4_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
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
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$DESIGNS_DIR" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR designs=$DESIGNS_DIR log=$LOG"
    fi
}
trap cleanup EXIT

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$DESIGNS_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        "$BIN" >>"$LOG" 2>&1 &
    HEADLESS_PID=$!
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -q "^READY " "$LOG" 2>/dev/null && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:"; tail -20 "$LOG"; return 1; }
        sleep 2
    done
    echo "timed out waiting for READY ($1s)"; return 1
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

wait_indexed() {
    local deadline=$(($(date +%s) + $1)) got=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        got="$(curl -fsS "$BASE/__api/vault/status" 2>/dev/null | json_field filesIndexed 2>/dev/null || echo '?')"
        [ "$got" = "$2" ] && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died"; tail -20 "$LOG"; return 1; }
        sleep 2
    done
    echo "timed out: filesIndexed=$got want $2"; return 1
}

echo "== N4 palette: data=$DATA_DIR designs=$DESIGNS_DIR proxy=$BASE fixture=${N1_FILES}x${N1_BOARDS}"

# --- pre-flight ------------------------------------------------------------
for f in "$N1HELPER" "$N3HELPER" "$N4HELPER" "$NAV_DRIVER"; do
    [ -f "$f" ] || { fail "missing helper: $f"; exit 1; }
done
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing at $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- boot ------------------------------------------------------------------
if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
fi
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT" && strip_ansi <"$LOG" | grep -q "vault-index service started"; then
    pass "boot READY with the vault-index service"
else
    fail "boot"; exit 1
fi
read_token || { fail "no access token"; exit 1; }

# --- (a) torture fixture ---------------------------------------------------
SEED_OUT="$(n1helper seed "$WORK_DIR" 2>"$WORK_DIR/seed.err")"
[ -n "$SEED_OUT" ] && pass "(a) torture fixture seeded: $SEED_OUT" || { fail "(a) seed failed"; cat "$WORK_DIR/seed.err" >&2; exit 1; }
if wait_indexed "$FIXTURE_SYNC_TIMEOUT" "$N1_FILES"; then
    pass "(a) all $N1_FILES files mirrored + indexed"
else
    fail "(a) fixture did not finish sync+index"; exit 1
fi

# --- rename one board to a distinctive needle so the palette has a target --
if n3helper edit_board "$WORK_DIR" "$NEEDLE" >/dev/null 2>"$WORK_DIR/edit.err"; then
    if n3helper wait_card "$WORK_DIR" "$NEEDLE" "$EDIT_TIMEOUT" >/dev/null 2>&1; then
        pass "renamed a board to '$NEEDLE' (indexed)"
    else
        fail "renamed board never appeared in the index"; cat "$WORK_DIR/edit.err" >&2
    fi
else
    fail "edit_board failed"; cat "$WORK_DIR/edit.err" >&2
fi

# --- (b) palette ranking over HTTP -----------------------------------------
if PAL_OUT="$(n4helper assert_palette "$WORK_DIR" "$FUZZY" 2>"$WORK_DIR/pal.err")"; then
    PAL_MS="$(echo "$PAL_OUT" | awk '{print $1}')"
    PAL_LINK="$(echo "$PAL_OUT" | awk '{print $2}')"
    pass "(b) palette ranks '$NEEDLE' first for q='$FUZZY'; Enter payload = exact deep link"
    echo "     palette ranking latency: ${PAL_MS}ms  deepLink=$PAL_LINK"
else
    fail "(b) palette ranking wrong"; cat "$WORK_DIR/pal.err" >&2
fi

# --- (e) Peek preview from .exports (plant a render, assert 200 + hash) -----
PLANT_OUT="$(n3helper plant_thumb "$WORK_DIR" 2>"$WORK_DIR/plant.err")"
[ -n "$PLANT_OUT" ] && pass "(e) planted an N2-shape render: $PLANT_OUT" || { fail "(e) plant_thumb failed"; cat "$WORK_DIR/plant.err" >&2; }
if PEEK_OUT="$(n4helper assert_peek "$WORK_DIR" 2>"$WORK_DIR/peek.err")"; then
    pass "(e) Peek preview served from .exports (HTTP $(echo "$PEEK_OUT" | awk '{print $1}'), sha256 $(echo "$PEEK_OUT" | awk '{print $2}'), $(echo "$PEEK_OUT" | awk '{print $3}') bytes)"
else
    fail "(e) Peek preview path broken"; cat "$WORK_DIR/peek.err" >&2
fi

# --- (f) reveal resolves the right dir / rejects traversal -----------------
REL="$(python3 -c "import json;print(json.load(open('$WORK_DIR/planted.json'))['relPath'])")"
REVEAL_OK="$(n3helper reveal "$WORK_DIR" "$REL" 2>/dev/null)"
REVEAL_BAD="$(n3helper reveal "$WORK_DIR" "../etc/passwd" 2>/dev/null)"
if echo "$REVEAL_OK" | grep -q "^200 True" && echo "$REVEAL_BAD" | grep -q "^400"; then
    pass "(f) reveal: in-vault path → 200 True; traversal → 400"
else
    fail "(f) reveal wrong (ok='$REVEAL_OK' bad='$REVEAL_BAD')"
fi

# --- (c)+(d) headless-browser: palette Enter nav + grid scroll/focus hold ---
NAV_OUT="$(PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" ROUTES_GATE_PLAYWRIGHT="$PLAYWRIGHT" \
    "$NODE_BIN" "$NAV_DRIVER" "$BASE" "$FUZZY" 2>"$WORK_DIR/nav.err")"
if echo "$NAV_OUT" | grep -q '"ok":true'; then
    pass "(c) headless: palette Enter landed on the exact /#/workspace deep link"
    pass "(d) headless: grid refresh PRESERVED scroll position + focused card (grid fix)"
    echo "   nav: $NAV_OUT"
else
    fail "(c/d) browser legs failed: $NAV_OUT"; cat "$WORK_DIR/nav.err" >&2
fi

# --- (g) Checkpoint now: fresh → 1 commit; no-op; +1 no-rewrite; dirty refuse
checkpoint() { curl -fsS -X POST "$BASE/__api/vault/checkpoint" 2>/dev/null; }
checkpoint_code() { curl -s -o "$WORK_DIR/cp.json" -w '%{http_code}' -X POST "$BASE/__api/vault/checkpoint" 2>/dev/null; }
cp_field() { python3 -c "import json,sys;print(json.load(open('$WORK_DIR/cp.json')).get(sys.argv[1]))" "$1"; }

# Fresh vault (no .git yet): exactly one commit containing manifest + .penpot.
if [ -e "$DESIGNS_DIR/.git" ]; then
    warn "(g) vault already had a .git before checkpoint (unexpected)"
fi
CODE="$(checkpoint_code)"; DEC="$(cp_field decision)"
if [ "$CODE" = "200" ] && [ "$DEC" = "init" ] && [ "$(commits)" = "1" ]; then
    LS="$(git_v ls-files)"
    if echo "$LS" | grep -q '\.penpot/' && echo "$LS" | grep -q '\.penpot-sync.json'; then
        pass "(g) fresh vault → exactly 1 commit containing manifest + .penpot dirs"
    else
        fail "(g) fresh commit missing manifest/.penpot dirs"
    fi
else
    fail "(g) fresh checkpoint wrong (code=$CODE decision=$DEC commits=$(commits))"
fi

# No change since the last checkpoint → clean no-op. Absorb any daemon churn
# first so the tree is genuinely unchanged, then assert the no-op.
for _ in $(seq 1 10); do
    [ -z "$(git_v status --porcelain 2>/dev/null)" ] && break
    checkpoint >/dev/null; sleep 1
done
BEFORE="$(commits)"; CODE="$(checkpoint_code)"; DEC="$(cp_field decision)"
if [ "$CODE" = "200" ] && [ "$DEC" = "noop" ] && [ "$(commits)" = "$BEFORE" ]; then
    pass "(g) no change → clean no-op (no new commit)"
else
    fail "(g) no-op wrong (code=$CODE decision=$DEC commits $BEFORE→$(commits))"
fi

# A new change → exactly one more commit, rewriting NO history.
echo "a user note" > "$DESIGNS_DIR/NOTES.md"
BEFORE="$(commits)"; OLD_HEAD="$(git_v rev-parse --short HEAD)"
CODE="$(checkpoint_code)"; DEC="$(cp_field decision)"
if [ "$CODE" = "200" ] && [ "$DEC" = "commit" ] && [ "$(commits)" = "$((BEFORE + 1))" ] \
    && [ "$(git_v rev-parse --short HEAD~1)" = "$OLD_HEAD" ]; then
    pass "(g) new change → exactly 1 more commit, previous HEAD preserved (no history rewrite)"
else
    fail "(g) commit case wrong (code=$CODE decision=$DEC commits $BEFORE→$(commits))"
fi

# Dirty / in-progress (detached HEAD) → LOUD refusal touching nothing.
git_v checkout --detach --quiet 2>/dev/null
BEFORE="$(commits)"
CODE="$(checkpoint_code)"; DEC="$(cp_field decision)"; MSG="$(cp_field message)"
if [ "$CODE" = "409" ] && [ "$DEC" = "refused" ] && [ "$(commits)" = "$BEFORE" ] \
    && echo "$MSG" | grep -qi "refused"; then
    pass "(g) dirty repo (detached HEAD) → 409 loud refusal, nothing committed"
else
    fail "(g) dirty refusal wrong (code=$CODE decision=$DEC commits=$(commits) msg='$MSG')"
fi
git_v checkout --quiet - 2>/dev/null || true

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "N4-PALETTE: ALL PASS"
    exit 0
else
    echo "N4-PALETTE: $FAILURES FAILURE(S)"
    exit 1
fi
