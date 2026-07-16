#!/usr/bin/env bash
# E5 cross-package token-resolver SPIKE gate (PLAN3.md milestone E5, `just e5`).
#
# E5 is a SPIKE, not a build: it ships an EXECUTED GO/NO-GO verdict
# (docs/ecosystem-spikes/token-resolver.md) plus this gate — no UI and no
# product Rust changes. It live-proves the mirror-and-surface token-package
# model: a token package is a git repo of DTCG set files under
# `.penpot-packages/`; "install" MIRRORS the package's sets into the consumer
# file's tokens.json by EXPLICIT tooling (edit the exported tree + in-place
# re-import — never the SPA, ecosystem invariant 3), each mirrored set stamped
# with a provenance theme whose "id" string lands in tokens_lib's :external-id
# and survives the round trip verbatim; drift of a mirrored set = the exact
# conflict rule (`.conflict-<ts>` copy, overwrite neither side).
#
# Exit criteria (PLAN3 E5, verbatim), against a fresh re-provisioned 2.16.2:
#   (a) mirrors a package's sets into a consumer and asserts the merged file
#       round-trips A=B per roundtrip.py semantics (export -> normalize ->
#       in-place re-import -> re-export -> equal SEMANTIC tree hashes), with
#       the mirrored sets staying FLATTEN-equal ({path:{type,value,description}})
#       to the package source (flatten-level equality = drift-checkable; the
#       canonical content is the settled export, not raw repo bytes).
#   (b) a scripted collision (same bare path in two active sets) resolves to
#       the tokenSetOrder-winner AND flipping ONLY the order flips the
#       resolved value (order-is-contract); the flipped order survives export
#       and is RPC-observable via get-file data.tokensLib. NOTE: what is
#       RPC-observable is the SERIALIZATION (tokenSetOrder/activeThemes) — a
#       future Penpot changing the client-side merge DIRECTION would be caught
#       as serialization drift only if the bytes change; a pure resolution-
#       SEMANTICS change (same bytes, different merge) is NOT caught by this
#       gate. That is the standing caveat (token-resolver.md caveat 5).
#   (c) the static resolver runs HEADLESS over the starter-kit dump (offline
#       files in, JSON report out — never injected) and (c1) lists each
#       token's free-variable deps, (c2) reproduces the starter kit's
#       PRE-EXISTING dangling paths as already-open baseline noise, not new
#       breakage, (c3) flags a synthetic dropped token as MAJOR with every
#       baseline path excluded from the breakage set.
#   Documented extras proven by the same probe run: the same bare path
#   (layerBase.text) resolving differently by active theme WITHIN one file
#   (theme-only change = MAJOR-BEHAVIORAL), shadowing-add =
#   READS-MINOR-BEHAVES-MAJOR (the token analogue of E1 caveat 2), the
#   provenance-stamp survival matrix (theme id + selectedTokenSets survive;
#   theme description blanked, $metadata extras dropped, set-root
#   $description REJECTED at import), and drift detection over the real
#   mirror prescribing the conflict copy.
#
# NOT chained into `just e2e` — recorded decision: spike precedent (the
# contract-extractability spike's scripts were never chained either); E5
# lands no product code, so the e2e ladder has nothing of E5's to regress.
# Re-run it when the token-package build (PLAN3 ch.4) starts or after any
# Penpot version bump (the resolver semantics + pinned baselines are 2.16.2).
#
# Gate helpers (cleaned spike scripts): scripts/ecosystem-spike/e5_probe.py
# (the live probe; uses scripts/roundtrip.py as RPC client + normalizer) and
# scripts/ecosystem-spike/e5_resolver.py (the static resolver: report/bump/
# drift subcommands, stdlib-only python).
#
# Dedicated ports (ledger): proxy 8998, backend 6459, postgres 5533, valkey
# 6476, control 8999 (exporter 6539 reserved; renders OFF — tokens need no
# renders). Fresh mktemp dirs; pg-install cache seeded; dirs kept on failure.
# Run-twice idempotent (fresh dirs + fresh DB each run; nothing persisted).
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E5_PROXY_PORT:-8998}"
BACKEND_PORT="${E5_BACKEND_PORT:-6459}"
POSTGRES_PORT="${E5_POSTGRES_PORT:-5533}"
VALKEY_PORT="${E5_VALKEY_PORT:-6476}"
CONTROL_PORT="${E5_CONTROL_PORT:-8999}"
FIRST_BOOT_TIMEOUT="${E5_TIMEOUT:-900}"
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e5-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e5-vault.XXXXXX")"
if [ -n "${E5_WORK_DIR:-}" ]; then
    WORK_DIR="$E5_WORK_DIR"; KEEP_WORK=1   # caller-owned: never delete
else
    WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e5-work.XXXXXX")"; KEEP_WORK=0
fi
mkdir -p "$WORK_DIR"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
PROBE="$ROOT/scripts/ecosystem-spike/e5_probe.py"
RESOLVER="$ROOT/scripts/ecosystem-spike/e5_resolver.py"
HEADLESS_PID=""
FAILURES=0

export PENPOT_BACKEND="$BACKEND"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

PG_CACHE="${E2_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    # kill any stragglers (java backend / valkey / postgres) tied to this run,
    # then free every E5 port even on failure.
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    sleep 1
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; do
        lsof -nP -tiTCP:"$p" -sTCP:LISTEN 2>/dev/null | xargs kill -9 2>/dev/null
    done
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT"
        [ "$KEEP_WORK" -eq 1 ] && echo "kept (E5_WORK_DIR): $WORK_DIR" || rm -rf "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT work=$WORK_DIR log=$LOG"
    fi
}
trap cleanup EXIT

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
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

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

# probe_has <check-name>: the tee'd probe output must contain PASS: <name>.
probe_has() {
    if grep -q "^PASS: $1" "$WORK_DIR/probe.out"; then
        pass "$2"
    else
        fail "$2 (probe check '$1' did not PASS)"
    fi
}

echo "== E5 token-resolver spike gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   work: $WORK_DIR"

ports_all_free || { fail "E5 ports busy before boot"; exit 1; }

# --- build -------------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ------------------------------------------------------------------------------
# Boot a fresh re-provisioned 2.16.2 stack on an empty vault.
# ------------------------------------------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then pass "boot READY on an empty vault (fresh DB)"; else fail "boot"; exit 1; fi
read_token || { fail "no access token"; exit 1; }

# ------------------------------------------------------------------------------
# The live probe: mirror + A=B, collision/order-flip, theme flip, resolver
# over the dump, drift. Every probe check prints its own PASS/FAIL line.
# ------------------------------------------------------------------------------
echo "== live probe (e5_probe.py) =="
python3 "$PROBE" run "$WORK_DIR" 2>&1 | tee "$WORK_DIR/probe.out"
PROBE_RC="${PIPESTATUS[0]}"   # tee must not swallow the probe's exit code
if [ "$PROBE_RC" -eq 0 ]; then
    pass "probe exit 0 ($(grep -c '^PASS: ' "$WORK_DIR/probe.out") checks)"
else
    fail "probe exited $PROBE_RC ($(grep -c '^FAIL: ' "$WORK_DIR/probe.out") failing checks)"
fi

# ------------------------------------------------------------------------------
# Map the PLAN3 E5 exit criteria onto the probe's checks, explicitly.
# ------------------------------------------------------------------------------
echo "== (a) mirror package sets into a consumer -> merged file round-trips A=B =="
probe_has "B.merged-consumer-roundtrips-A=B" \
    "(a) merged consumer round-trips A=B per roundtrip.py semantic hashes"
probe_has "B.mirrored-sets-flatten-equal-to-package" \
    "(a) mirrored sets flatten-equal ({path:{type,value,description}}) to the package source"
probe_has "B.provenance-theme-id-survives" \
    "(a) provenance stamp (theme id -> tokens_lib :external-id) survives verbatim"

echo "== (b) collision resolves to the tokenSetOrder-winner; order flip flips the value =="
probe_has "C.later-set-in-tokenSetOrder-wins" \
    "(b) scripted collision resolves to the LATER set in tokenSetOrder"
probe_has "C.order-flip-flips-resolved-value" \
    "(b) flipping ONLY tokenSetOrder flips the resolved value (order-is-contract)"
probe_has "C.rpc-observes-flipped-order" \
    "(b) flipped order is RPC-observable (get-file data.tokensLib == exported order)"
probe_has "C.classifier-order-flip=MAJOR" \
    "(b) classifier: order-flip-on-collision = MAJOR"

echo "== (c) static resolver headless over the starter-kit dump =="
probe_has "E.deps-listing-over-dump" \
    "(c1) per-token free-variable deps listed for every ref-bearing token"
probe_has "E.pre-existing-dangling-baseline-pinned" \
    "(c2) pre-existing dangling paths pinned as already-open baseline"
probe_has "E.synthetic-dropped-token=MAJOR" \
    "(c3) synthetic dropped token = MAJOR; baseline noise excluded from breakage"

echo "== documented extras (theme flip, shadowing-add, drift=conflict-copy) =="
probe_has "D.same-path-resolves-differently-by-theme" \
    "layerBase.text resolves differently by active theme WITHIN one file"
probe_has "D.classifier-theme-only=MAJOR-BEHAVIORAL" \
    "classifier: theme-only change = MAJOR-BEHAVIORAL"
probe_has "E.shadowing-add=READS-MINOR-BEHAVES-MAJOR" \
    "classifier: shadowing-add = READS-MINOR-BEHAVES-MAJOR (E1-caveat-2 analogue)"
probe_has "F.drift-detector-clean-mirror" \
    "drift detector: real mirror all-clean against the package source"
probe_has "F.drift-detector-flags-mutated-set" \
    "drift detector: mutated mirrored set -> DRIFTED, prescribes conflict copy"

echo "== offline classifier adversarial pairs (multi-axis; resolved-view refound) =="
probe_has "G.value-edit-on-applied-token-not-PATCH" \
    "classifier: in-place \$value edit on an applied token = behavioral breakage (NOT PATCH)"
probe_has "G.winning-definition-drop-not-PATCH" \
    "classifier: dropping the winning colliding definition = MAJOR (NOT PATCH)"
probe_has "G.theme-flip-plus-add-not-MINOR" \
    "classifier: theme flip + harmless token add = MAJOR-BEHAVIORAL (NOT MINOR)"
probe_has "G.baseline-identical=PATCH-and-pure-add=MINOR" \
    "classifier baseline: identical trees = PATCH, pure add = MINOR"
probe_has "G.hostile-math-bounded-and-composite-no-phantom-refs" \
    "resolver hardening: '99**999999' bounded (no eval hang), composite \$values mint no phantom refs"

# ------------------------------------------------------------------------------
# Shut the stack down FIRST, then re-run the resolver over the on-disk dump:
# static means static — offline files in, JSON report out, no server anywhere.
# ------------------------------------------------------------------------------
if stop_headless; then pass "clean shutdown"; else fail "shutdown hung"; fi
sleep 1
ports_all_free && pass "all 5 E5 ports freed" || fail "ports still busy after shutdown"

echo "== (c) resolver re-run HEADLESS with the stack down (never injected) =="
if [ -d "$WORK_DIR/pkg.penpot" ] &&
    python3 "$RESOLVER" report "$WORK_DIR/pkg.penpot" >"$WORK_DIR/offline-report.json" 2>&1; then
    python3 - "$WORK_DIR/offline-report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
ok = True
def check(cond, msg):
    global ok
    print(("PASS: " if cond else "FAIL: ") + msg)
    ok = ok and cond
# pinned to the 2.16.2 starter kit: the template bytes carry 495 tokens but
# import silently DROPS 5 unknown-type strokeWidth tokens (line.*) -> 490.
check(r["tokenCount"] == 490,
      f"offline report: 490 tokens resolved (got {r['tokenCount']}; template ships 495, "
      "2.16.2 import drops 5 unknown-type strokeWidth tokens)")
check(r["freeVariables"] == [],
      "offline report: zero free variables (every value-ref resolves in active sets)")
check(len(r["danglingApplied"]) == 16,
      f"offline report: the pre-existing dangling baseline is exactly 16 applied paths "
      f"(got {len(r['danglingApplied'])})")
check(r["tokens"]["modular.xl"]["refs"] == ["density", "modular.lg"],
      "offline report: token math deps listed (modular.xl -> [density, modular.lg])")
sys.exit(0 if ok else 1)
PY
    [ $? -eq 0 ] || fail "(c) offline resolver report assertions"
else
    fail "(c) resolver failed to run offline over the dump"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "E5 TOKENS SPIKE: ALL PASS"
    exit 0
else
    echo "E5 TOKENS SPIKE: $FAILURES FAILURE(S)"
    exit 1
fi
