#!/usr/bin/env bash
# D0 navigation-control SPIKE gate (PLAN4 milestone D0, `just d0`).
#
# Answers: can the webview observe + redirect Penpot's SPA HASH navigation
# WITHOUT touching the SPA (invariant 3)? Assembles Tasks 1-5 (all merged):
#   - GET /__navprobe?run=<case> (apps/desktop/src/navprobe.{rs,html})
#   - PENPOT_LOCAL_NAVWATCH_LOG / _REDIRECT (apps/desktop/src/navwatch.rs)
#   - PENPOT_LOCAL_START_URL (apps/desktop/src/main.rs)
#   - scripts/d0_navprobe.py (reads the navwatch JSONL)
#   - scripts/d0_penpot_nav.cjs (read-only DOM inspection of Penpot's SPA)
#
# CONTROLLER DECISION (2026-07-19, recorded in .superpowers/sdd/progress.md):
# the central hash-observation leg asserts a CONCLUSIVE MEASUREMENT, not a
# direction. The control case (full document nav) MUST be observed or the
# harness itself is broken. The hash case passes iff the probe produced a
# definite, parseable true/false — a NO-GO (not observed) is a legitimate
# result that lowers PLAN4 chapter 4's ceiling; it still passes the gate. The
# GO/NO-GO verdict is written to findings.json as DATA, not asserted as a
# direction. This task (6) does NOT compute `redirectWorks`/`vaultIntact`
# — Task 7 (below, legs (e)/(f)) fills them in when hashObserved is True;
# they stay null otherwise (nothing to redirect).
#
# REQUIRES A GUI SESSION — it launches the real Tauri binary (penpot-desktop,
# not headless) so `on_navigation` can be attached to a real window. Not
# CI-headless. Also requires no other Penpot Local GUI instance already
# running: tauri_plugin_single_instance is keyed on the app identifier, not
# our dedicated ports, so a second launch would just focus the existing
# window and exit before ever reaching our probe (pre-flight checked below).
#
# Dedicated ports: proxy 9034, backend 6496, postgres 5569, valkey 6512.
# Control 9037 is reserved for this port block (not bound here — this GUI
# binary has no localhost control server; headless.rs is the one that reads
# PENPOT_LOCAL_CONTROL_PORT) so a sibling gate never lands on it either.
#
# KNOWN RISK: scripts/d0_penpot_nav.cjs waits a fixed 4s before scraping
# anchors. A slow-rendering SPA yields a FALSE NEGATIVE on usesAnchorHref.
# The raw node output is recorded verbatim in findings.json; see the caveat
# line printed next to that result below.
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on
# other port blocks. We kill ONLY the PID this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT="$ROOT"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D0_PROXY_PORT:-9034}"
BACKEND_PORT="${D0_BACKEND_PORT:-6496}"
POSTGRES_PORT="${D0_POSTGRES_PORT:-5569}"
VALKEY_PORT="${D0_VALKEY_PORT:-6512}"
BASE="http://localhost:${PROXY_PORT}"

FIRST_BOOT_TIMEOUT="${D0_FIRST_BOOT_TIMEOUT:-600}"   # fresh data dir, pg-cache seeded
REBOOT_TIMEOUT="${D0_REBOOT_TIMEOUT:-300}"            # data dir already provisioned
SETTLE_SECS=4                                          # covers the probe page's 300ms timer

APP_BIN="$ROOT/target/debug/penpot-desktop"
HEADLESS_BIN="$ROOT/target/debug/headless"
NAVPROBE_PY="$ROOT/scripts/d0_navprobe.py"
PENPOT_NAV_CJS="$ROOT/scripts/d0_penpot_nav.cjs"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d0-work.XXXXXX")"
FINDINGS="$WORK_DIR/findings.json"

PLAYWRIGHT="${D0_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
NODE_BIN="${D0_NODE:-node}"

APP_PID=""
PASS=0; FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

PG_CACHE="${D0_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY. Never pkill/killall by name (another gate may
# run concurrently on a different port block).
cleanup() {
    if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
        kill -TERM "$APP_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$APP_PID" 2>/dev/null
        wait "$APP_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAIL" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT work=$WORK_DIR"
    fi
}
trap cleanup EXIT

ports_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

echo "== D0 navigation-control spike =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT"
echo "   work:  $WORK_DIR"

# --- Task 7 setup: seed a vault canary up front -------------------------------
# Deterministic recursive hash over every *.penpot dir under VAULT: dir names
# (sorted) plus every contained file's path+content hash (sorted), folded
# into one final digest. Same file set, same traversal order, both times —
# this is what "byte-identical on disk" is checked against in leg (f) below.
vault_hash() {
    (
        cd "$VAULT" || exit 1
        {
            find . -type d -name '*.penpot' | LC_ALL=C sort
            find . -type f -path '*.penpot/*' -print0 \
                | LC_ALL=C sort -z \
                | xargs -0 shasum -a 256 2>/dev/null
        } | shasum -a 256 | cut -d' ' -f1
    )
}

# Seed a canary design directory into the vault up front, before ANY app
# boot. This D0 gate never creates real Penpot content through the normal
# RPC/UI path (no control surface is wired to the GUI binary — see the
# reality-check comment below), so without a seeded file leg (f)'s integrity
# check would only ever compare "nothing" to "nothing". Seeding it here,
# ahead of the (a)-(d) legs' boots, gives the sync daemon's ordinary
# reconciliation (every boot reconciles from the tree — control.rs) room to
# touch/normalize it once and converge *before* the redirect leg's own boot.
# HASH_BEFORE is taken later, right before leg (e), i.e. AFTER that
# stabilization — so a back-to-back HASH_BEFORE/HASH_AFTER around the single
# redirect-leg boot isolates just that boot's effect and can't conflate it
# with the ordinary first-time reconciliation of a newly-seeded file.
SEED_DIR="$VAULT/d0-integrity-canary.penpot"
mkdir -p "$SEED_DIR/pages"
printf '%s\n' '{"seed":"d0-integrity-canary","note":"must be byte-identical after the mid-session redirect"}' >"$SEED_DIR/manifest.json"
printf '%s\n' '{"objects":{}}' >"$SEED_DIR/pages/page1.json"

# --- pre-flight --------------------------------------------------------------
[ -f "$NAVPROBE_PY" ] || { fail "probe runner missing: $NAVPROBE_PY"; exit 1; }
[ -f "$PENPOT_NAV_CJS" ] || { fail "penpot nav inspector missing: $PENPOT_NAV_CJS"; exit 1; }
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing: $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }
[ -d "$BROWSERS" ] || { fail "bundled browsers missing: $BROWSERS"; exit 1; }
ports_free || { fail "one of the D0 ports is busy"; exit 1; }
# tauri_plugin_single_instance is keyed on the app identifier, not our ports:
# a second GUI launch would silently focus that window and exit immediately,
# never reaching our probe. Refuse to run into that trap.
if pgrep -f "$APP_BIN" >/dev/null 2>&1; then
    fail "a penpot-desktop GUI process is already running (single-instance guard would swallow our launch) — quit it first"
    exit 1
fi
pass "pre-flight: ports free, no existing GUI instance, playwright + browsers present"

# --- build ---------------------------------------------------------------
echo "-- build the GUI binary + headless (penpot-desktop package builds both bins)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop); then
    fail "cargo build -p penpot-desktop"; exit 1
fi
[ -x "$APP_BIN" ] || { fail "built binary missing: $APP_BIN"; exit 1; }
[ -x "$HEADLESS_BIN" ] || { fail "built binary missing: $HEADLESS_BIN"; exit 1; }
pass "cargo build -p penpot-desktop (penpot-desktop + headless)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "   (seeded postgres binaries from $PG_CACHE)"
fi

# --- launch helper -------------------------------------------------------
# Waits for a line containing $needle to appear in $log (the app reaching
# $needle is our readiness signal: main.rs only calls window.navigate(url)
# AFTER the full stack -- postgres/backend/valkey/proxy -- is up, and
# on_navigation records that navigation immediately). Returns 0 on success.
wait_for_log_line() {
    local log="$1" needle="$2" timeout="$3"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -qF "$needle" "$log" 2>/dev/null && return 0
        kill -0 "$APP_PID" 2>/dev/null || { echo "app exited before reaching '$needle'" >&2; return 1; }
        sleep 1
    done
    echo "timed out waiting for '$needle' in $log (${timeout}s)" >&2
    return 1
}

stop_app() {
    [ -n "$APP_PID" ] || return 0
    kill -TERM "$APP_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$APP_PID" 2>/dev/null || { wait "$APP_PID" 2>/dev/null; APP_PID=""; return 0; }; sleep 1; done
    kill -9 "$APP_PID" 2>/dev/null
    wait "$APP_PID" 2>/dev/null
    APP_PID=""
}

# --- probe one navigation case ------------------------------------------------
# Launches the app pointed at /__navprobe?run=<case> against the SHARED
# DATA_DIR/VAULT (so only the very first boot pays full provisioning cost;
# every later boot in this run starts against an already-initialized
# Postgres cluster, same pattern as the sibling gates' second-boot legs).
probe_case() {
    local case="$1" boot_timeout="$2"
    local log="$WORK_DIR/nav-$case.jsonl" applog="$WORK_DIR/app-$case.log"
    : >"$log"
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_NAVWATCH_LOG="$log" \
        PENPOT_LOCAL_START_URL="${BASE}/__navprobe?run=${case}" \
        "$APP_BIN" >"$applog" 2>&1 &
    APP_PID=$!
    if wait_for_log_line "$log" "/__navprobe?run=${case}" "$boot_timeout"; then
        sleep "$SETTLE_SECS"
    else
        echo "   -- boot/nav log tail --" >&2; tail -25 "$applog" >&2 || true
    fi
    stop_app
    python3 "$NAVPROBE_PY" observe "$log" "$case"
}

# Proof-of-life gate shared by all three probe_case legs below. Every
# successful launch records an initial-load observation line before anything
# else happens (verified live: {"source":"on_navigation","url":"tauri://
# localhost"}). If that baseline is absent, the probe never ran — a crash or
# a timed-out boot — and the "observed" field for that case is NOT a real
# measurement (d0_navprobe.py's observe treats a missing/empty log the same
# as a genuine "not observed", which is exactly the ambiguity this check
# exists to break). A case that didn't run must fail LOUDLY, distinctly from
# a genuine negative finding, and must not contribute a verdict.
require_probe_ran() {
    local label="$1" json="$2"
    local ran
    ran=$(echo "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ran"))' 2>/dev/null || echo "ERROR")
    if [ "$ran" = "True" ]; then
        return 0
    fi
    fail "($label) PROBE DID NOT RUN — no baseline observation in the navwatch log (app crash or boot timeout, not a measurement); this is an infra failure, not a NO-GO finding"
    return 1
}

# (a) CONTROL: a full document navigation MUST be observed. If this fails the
#     harness itself is broken and every other result is meaningless.
echo "-- case: full (control)"
FULL=$(probe_case full "$FIRST_BOOT_TIMEOUT")
echo "     full: $FULL"
FULL_RAN=$(echo "$FULL" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ran"))' 2>/dev/null || echo "ERROR")
if require_probe_ran "control" "$FULL"; then
    FULL_OBSERVED=$(echo "$FULL" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")
    if [ "$FULL_OBSERVED" = "True" ]; then
        pass "(control) a full document navigation is observed by on_navigation"
    else
        fail "(control) full navigation NOT observed ($FULL_OBSERVED) — harness is broken, results below are meaningless"
    fi
else
    FULL_OBSERVED="ERROR"
fi

# (b) THE CENTRAL QUESTION: is a same-document HASH change observed? Both
#     outcomes measure conclusively; only an inconclusive reading (probe page
#     never loaded / unparseable output) fails this leg.
echo "-- case: hash (central)"
HASH=$(probe_case hash "$REBOOT_TIMEOUT")
echo "     hash: $HASH"
HASH_RAN=$(echo "$HASH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ran"))' 2>/dev/null || echo "ERROR")
if require_probe_ran "central" "$HASH"; then
    HASH_OBSERVED=$(echo "$HASH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")
    if [ "$HASH_OBSERVED" = "True" ] || [ "$HASH_OBSERVED" = "False" ]; then
        pass "(central) conclusive measurement obtained: same-document hash change observed=$HASH_OBSERVED"
        if [ "$HASH_OBSERVED" = "True" ]; then
            echo "     -> DATA: hash change IS observed; redirect would be possible (Task 7)"
        else
            echo "     -> DATA: hash change is NOT observed; ceiling is 'not the default' (NO-GO is legitimate)"
        fi
    else
        fail "(central) INCONCLUSIVE — no definite true/false reading ($HASH_OBSERVED) despite proof-of-life; unexpected d0_navprobe.py output"
    fi
else
    HASH_OBSERVED="ERROR"
fi

# (c) pushState (expected NOT observed on most engines — recorded, not
#     asserted; the brief only wants this echoed as data). Proof-of-life is
#     still required: a crashed/timed-out boot must not be recorded as data.
echo "-- case: pushstate (recorded, not asserted)"
PUSH=$(probe_case pushstate "$REBOOT_TIMEOUT")
echo "     pushstate: $PUSH"
PUSH_RAN=$(echo "$PUSH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ran"))' 2>/dev/null || echo "ERROR")
if require_probe_ran "pushstate" "$PUSH"; then
    PUSH_OBSERVED=$(echo "$PUSH" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("observed"))' 2>/dev/null || echo "ERROR")
else
    PUSH_OBSERVED="ERROR"
fi

# (d) REALITY CHECK: does Penpot actually navigate via anchor hrefs? This leg
#     only needs a normally reachable stack for the bundled offline chromium
#     to auto-login and inspect the real dashboard DOM — it never touches
#     on_navigation, so it uses the HEADLESS binary (same one every sibling
#     gate boots) rather than the GUI one: no window overhead, and no risk of
#     tripping the single-instance guard.
echo "-- reality check: does Penpot's SPA use anchor hrefs for its routes?"
BOOTLOG="$WORK_DIR/headless-bootstrap.log"
env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
    "$HEADLESS_BIN" >"$BOOTLOG" 2>&1 &
APP_PID=$!
READY=0
DEADLINE=$(($(date +%s) + REBOOT_TIMEOUT))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    strip_ansi <"$BOOTLOG" 2>/dev/null | grep -q "^READY " && { READY=1; break; }
    kill -0 "$APP_PID" 2>/dev/null || break
    sleep 1
done
if [ "$READY" = "1" ]; then
    pass "(reality-check pre-flight) headless stack reached READY at $BASE"
    PENPOT="$(PLAYWRIGHT_MODULE="$PLAYWRIGHT" PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" BASE="$BASE" REPO_ROOT="$ROOT" "$NODE_BIN" "$PENPOT_NAV_CJS" 2>"$WORK_DIR/penpot-nav.err" || true)"
else
    fail "(reality-check pre-flight) headless stack never reached READY within ${REBOOT_TIMEOUT}s"
    tail -25 "$BOOTLOG" >&2 || true
    PENPOT='{"ok":false,"error":"stack not reachable"}'
fi
stop_app
echo "     penpot: $PENPOT"
echo "     CAVEAT: d0_penpot_nav.cjs waits a fixed 4s before scraping anchors —"
echo "             a slower SPA render would read as a FALSE NEGATIVE here."
PENPOT_OK=$(echo "$PENPOT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))' 2>/dev/null || echo "False")
USES_ANCHOR=$(echo "$PENPOT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("usesAnchorHref"))' 2>/dev/null || echo "null")
if [ "$PENPOT_OK" = "True" ]; then
    pass "(reality-check) inspector ran successfully — usesAnchorHref=$USES_ANCHOR (see caveat above)"
else
    fail "(reality-check) d0_penpot_nav.cjs did not complete successfully: $PENPOT"
fi

# (e) REDIRECT + (f) INTEGRITY (Task 7). Gated on hashObserved=True — with no
# same-document hash change observed there is nothing to redirect, and
# testing it would be meaningless (see CONTROLLER DECISION above). Mirrors
# Task 6's own null-writing for these two findings.json fields.
#
# NOTE on naming: this measures a hash over a hand-seeded canary `.penpot`
# dir under VAULT, exercised from /__navprobe — no workspace was ever open
# during this gate. That is narrower than PLAN4's D0 exit criterion (evidence
# that redirecting mid-session leaves the live workspace undisturbed), so the
# findings.json field is named `vaultIntact`: it names what was actually
# measured (the on-disk vault tree), not a live workspace session. D2 must
# re-assert this with a real file actually open.
REDIRECT_WORKS="None"
VAULT_INTACT="None"
REDIR_RAN="None"
if [ "$HASH_OBSERVED" = "True" ]; then
    # HASH_BEFORE is taken now — after the (a)-(d) boots above have already
    # run against this same VAULT and had their chance to reconcile/stabilize
    # the seeded canary — so it reflects steady state, not a first-touch.
    HASH_BEFORE="$(vault_hash)"

    # (e) REDIRECT: with the policy enabled, a #/dashboard navigation must be
    #     cancelled and land on /__home instead. Proof-of-life first — an app
    #     that never booted must FAIL LOUDLY as an infra failure, not be
    #     recorded as "redirect didn't work" (same discipline as (a)-(c)).
    echo "-- case: redirect (policy enabled)"
    REDIR_LOG="$WORK_DIR/nav-redirect.jsonl"
    REDIR_APPLOG="$WORK_DIR/app-redirect.log"
    : >"$REDIR_LOG"
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_NAVWATCH_LOG="$REDIR_LOG" \
        PENPOT_LOCAL_NAVWATCH_REDIRECT=1 \
        PENPOT_LOCAL_START_URL="${BASE}/__navprobe?run=hash" \
        "$APP_BIN" >"$REDIR_APPLOG" 2>&1 &
    APP_PID=$!
    if wait_for_log_line "$REDIR_LOG" "/__navprobe?run=hash" "$REBOOT_TIMEOUT"; then
        sleep "$SETTLE_SECS"
    else
        echo "   -- boot/nav log tail --" >&2; tail -25 "$REDIR_APPLOG" >&2 || true
    fi
    stop_app
    REDIRECT_JSON=$(python3 "$NAVPROBE_PY" observe "$REDIR_LOG" hash)
    echo "     redirect: $REDIRECT_JSON"
    if require_probe_ran "redirect" "$REDIRECT_JSON"; then
        REDIR_RAN="True"
        if grep -q '__home' "$REDIR_LOG"; then
            REDIRECT_WORKS="True"
            pass "(e/redirect) a #/dashboard navigation was cancelled by the policy and landed on /__home"
        else
            REDIRECT_WORKS="False"
            fail "(e/redirect) redirect policy enabled but /__home was never reached"
        fi
    else
        REDIR_RAN="False"
    fi

    # (f) INTEGRITY: the vault must be byte-identical across the redirect
    #     boot — folder-is-truth is this project's P0 (CLAUDE.md) and a
    #     navigation trick must not dent it. Gated on the redirect leg's own
    #     proof-of-life: if that app never booted, the vault was never
    #     touched either way and HASH_AFTER == HASH_BEFORE would be a
    #     vacuous pass, not a real measurement — so this must fail loudly
    #     too rather than silently record a (meaningless) true.
    if [ "$REDIR_RAN" = "True" ]; then
        HASH_AFTER="$(vault_hash)"
        if [ "$HASH_AFTER" = "$HASH_BEFORE" ]; then
            VAULT_INTACT="True"
            pass "(f/integrity) vault tree byte-identical across the redirect boot (boot+reconcile included, not isolated; no workspace was open — see notes.vaultIntactCaveat)"
        else
            VAULT_INTACT="False"
            fail "(f/integrity) vault tree CHANGED across the redirect (before=$HASH_BEFORE after=$HASH_AFTER)"
        fi
    else
        VAULT_INTACT="None"
        fail "(f/integrity) SKIPPED — the redirect leg (e) never ran (no proof-of-life), so before/after is not a real measurement; this is an infra failure carried over from (e), not a passing (or failing) result"
    fi
else
    echo "-- skipping redirect + integrity legs: hashObserved != True (nothing to redirect, see CONTROLLER DECISION above)"
fi

# --- findings.json -----------------------------------------------------------
# redirectWorks / vaultIntact (Task 7, legs (e)/(f) above) are real
# measurements when hashObserved is True; they stay null when there was
# nothing to redirect, or when the redirect leg itself never got proof of
# life (an infra failure, not a "false" measurement — see (f) above).
#
# probeRan.{full,hash,pushstate,redirect} records proof-of-life (baseline
# navwatch observation seen) separately from the observed measurement
# itself, so a reader of findings.json can tell "the probe ran and measured
# X" apart from "the probe never ran" — the latter must never be read as a
# measurement.
python3 - "$FINDINGS" "$FULL_OBSERVED" "$HASH_OBSERVED" "$PUSH_OBSERVED" "$USES_ANCHOR" "$PENPOT" "$FULL_RAN" "$HASH_RAN" "$PUSH_RAN" "$REDIRECT_WORKS" "$VAULT_INTACT" "$REDIR_RAN" <<'PY'
import json, sys

def to_bool_or_none(s):
    if s == "True":
        return True
    if s == "False":
        return False
    return None

path, full_o, hash_o, push_o, anchor_o, penpot_raw, full_r, hash_r, push_r, redirect_o, vault_o, redirect_r = sys.argv[1:13]
try:
    penpot_json = json.loads(penpot_raw)
except Exception:
    penpot_json = {"ok": False, "raw": penpot_raw}

hash_observed = to_bool_or_none(hash_o)
verdict = "NO-VERDICT (inconclusive)"
if hash_observed is True:
    verdict = "GO (hash change IS observed by on_navigation)"
elif hash_observed is False:
    verdict = "NO-GO (hash change is NOT observed by on_navigation)"

findings = {
    "fullObserved": to_bool_or_none(full_o),
    "hashObserved": hash_observed,
    "pushstateObserved": to_bool_or_none(push_o),
    "usesAnchorHref": to_bool_or_none(anchor_o),
    "probeRan": {
        "full": to_bool_or_none(full_r),
        "hash": to_bool_or_none(hash_r),
        "pushstate": to_bool_or_none(push_r),
        "redirect": to_bool_or_none(redirect_r),
    },
    "redirectWorks": to_bool_or_none(redirect_o),
    "vaultIntact": to_bool_or_none(vault_o),
    "verdict": verdict,
    "notes": {
        "vaultIntactCaveat": "measures a hash over a hand-seeded canary .penpot dir under VAULT, exercised from /__navprobe; no workspace was ever open during this gate — narrower than 'the workspace stays intact'. D2 must re-assert this with a real file actually open.",
        "usesAnchorHrefCaveat": "d0_penpot_nav.cjs uses a fixed 4s post-load wait; a slower SPA render reads as a false negative here",
        "probeRan": "proof-of-life per case (baseline navwatch observation seen); if false for a case, that case's *Observed field is null (infra failure, not a measurement) and its FAIL was already reported above",
    },
    "penpotNavRaw": penpot_json,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(findings, f, indent=2, sort_keys=True)
    f.write("\n")
print(json.dumps(findings, indent=2, sort_keys=True))
PY

# The spike's own deliverable must survive a successful run: cleanup() below
# rm -rf's WORK_DIR (where $FINDINGS lives) whenever FAIL==0, so copy it to a
# durable location NOW, before that happens, and report that surviving path
# rather than the one that's about to be deleted. Not committed to git —
# this is a run artifact, not a repo file.
DURABLE_FINDINGS_DIR="${D0_FINDINGS_DIR:-$HOME/.cache/penpot-local/d0-findings}"
mkdir -p "$DURABLE_FINDINGS_DIR"
DURABLE_FINDINGS="$DURABLE_FINDINGS_DIR/findings-$(date +%Y%m%dT%H%M%S).json"
cp "$FINDINGS" "$DURABLE_FINDINGS"

echo
echo "== findings written to $DURABLE_FINDINGS (copied out of $WORK_DIR before teardown) =="
echo "D0 VERDICT (data, not an assertion of direction): hashObserved=$HASH_OBSERVED"

echo
if [ "$FAIL" -eq 0 ]; then
    echo "D0 NAVIGATION: ALL PASS ($PASS passed)"
else
    echo "D0 NAVIGATION: $FAIL FAILURE(S) ($PASS passed)"
fi
exit "$FAIL"
