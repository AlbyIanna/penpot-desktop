#!/usr/bin/env bash
# D1 offline + config-hardening gate (PLAN4 milestone D1, `just d1`, chained
# into `just e2e` — unlike the D0 spike, D1 ships PRODUCT code).
#
# Proves the offline promise for every Penpot flag that deletes a cloud
# surface, PASS/FAIL like the sibling gates:
#   (a) SERVED — necessary but NOT sufficient: config.js carries the flag
#       tokens `compose_frontend_flags` (apps/desktop/src/lib.rs) appends —
#       disable-registration, disable-dashboard-templates-section,
#       disable-google-fonts-provider, disable-login-with-password. A renamed
#       upstream flag would still be "served" while the surface came back, so
#       this leg alone is never trusted.
#   (b) TOOK EFFECT — the assertion that actually matters (PLAN4 risk 4).
#       `node scripts/d1_surfaces.cjs` drives the real SPA (bootstrap login,
#       #/auth/register, #/auth/login, the dashboard) and reports tri-state
#       gone|present|inconclusive for three surfaces:
#         * templatesSection MUST be "gone" — verified live, this flag
#           genuinely removes the cloud-templates section. "present" fails.
#         * loginForm MUST be "gone" (no password input, no submit control) —
#           verified live (D1 task 4), this flag genuinely strips the login
#           form's fields. "present" fails. This closes what used to be this
#           gate's KNOWN GAP (a manual, one-shot audit with no automated
#           re-check); see `.superpowers/sdd/d1-login-audit.md` for the
#           original manual evidence this behavioural check now re-derives.
#         * registration is EXPECTED to read "present" and that is NOT a
#           failure here — see the (navwatch) leg below, which is what makes
#           that tolerable. Only "inconclusive" (the probe could not look)
#           fails this leg for any of the three surfaces.
#       "inconclusive" always FAILS LOUDLY and is reported as an
#       INFRASTRUCTURE FAILURE, never as a negative finding — a page that
#       never rendered proves nothing about what is or isn't on it, and
#       silently reading that as "gone" is the exact false-pass this gate
#       exists to prevent.
#   (navwatch) THE REGISTRATION CLOSURE — asserted separately, NOT by probing
#       the raw route in a bare browser (product-owner decision, D1 task 3):
#       `disable-registration` only removes the "Create an account" LINK from
#       the login page. `#/auth/register` still renders a working signup form
#       if reached directly, and the backend signup RPC stays live — our own
#       single-user provisioning calls that exact RPC, and that path runs on
#       EVERY DB WIPE (the project's P0 core invariant), so it cannot be
#       disabled backend-side. That surface is closed instead by D0/D1's
#       navigation policy: `apps/desktop/src/navwatch.rs`'s `decide()`
#       cancels the ENTIRE `#/auth/*` family in the webview UNCONDITIONALLY —
#       no env var required — and redirects to `/__home`. (D1 hardening: this
#       used to be gated behind `PENPOT_LOCAL_NAVWATCH_REDIRECT=1`, which
#       nothing in the shipped product ever sets, so the policy this gate
#       cited as the reason "registration present" is tolerable was actually
#       DORMANT by default — see .superpowers/sdd/task-6-report.md finding 1.
#       `#/dashboard`/`#/settings` are UNCHANGED: still measurement-only,
#       still gated behind that env var, because their native replacement
#       doesn't exist until D2/D3.) This leg runs the EXISTING Rust unit
#       tests (`cargo test -p penpot-desktop navwatch::tests::`) and requires
#       the SPECIFIC tests that pin the unconditional `#/auth` contract with
#       the env var OFF (the product's actual default) — not just "the suite
#       passed". If this leg ever goes red, the registration surface is no
#       longer closed by anything and (b)'s "present is fine" tolerance above
#       stops being true.
#   (c) ZERO NON-LOOPBACK EGRESS, both sides, sampled AFTER a realistic
#       session (create a file, edit it, export it via `export-binfile`) so
#       the socket check isn't just watching an idle boot:
#         * (c/spa) the SPA's own request log (from the same browser session
#           that measured (b)) must show zero non-loopback requests. Guarded
#           on the total request count being non-zero first — an observer
#           that captured no traffic at all would make nonLoopbackRequests=[]
#           vacuous, not a real zero-egress finding.
#         * (c/proc) one `lsof -nP -i` SAMPLE of the supervised process tree,
#           parsed by `scripts/d1_egress.py`, must show zero non-loopback
#           peers. THIS IS A SAMPLE, NOT A PROOF OF ABSENCE — a connection
#           that opens and closes between polls could be missed. It is a
#           strong signal, stated here as exactly that. Guarded on the total
#           connection count being non-zero first (same vacuous-pass family
#           as (c/spa)) — an lsof invocation that silently failed and
#           produced nothing must not read as "verified clean".
#
# Dedicated ports: proxy 9046, backend 6508, postgres 5581, valkey 6524
# (control 9047 reserved, not bound — headless.rs only opens a control
# server when PENPOT_LOCAL_CONTROL_PORT is set, which this gate never sets).
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on a
# different port block. We kill ONLY the PID this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D1_PROXY_PORT:-9046}"
BACKEND_PORT="${D1_BACKEND_PORT:-6508}"
POSTGRES_PORT="${D1_POSTGRES_PORT:-5581}"
VALKEY_PORT="${D1_VALKEY_PORT:-6524}"
BASE="http://localhost:${PROXY_PORT}"

FIRST_BOOT_TIMEOUT="${D1_FIRST_BOOT_TIMEOUT:-900}"   # fresh data dir, pg-cache seeded

BIN="$ROOT/target/debug/headless"
SURFACES_CJS="$ROOT/scripts/d1_surfaces.cjs"
EGRESS_PY="$ROOT/scripts/d1_egress.py"
PLAYWRIGHT="${D1_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
NODE_BIN="${D1_NODE:-node}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d1-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d1-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d1-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"

HEADLESS_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

PG_CACHE="${D1_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY.
cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT work=$WORK_DIR"
        echo "  headless log: $LOG"
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

ports_all_free() {
    local p ok=0
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            ok=1
        fi
    done
    return "$ok"
}

wait_ready() {
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" 2>/dev/null | grep -q "^READY " && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:" >&2; tail -25 "$LOG" >&2; return 1; }
        sleep 2
    done
    echo "timed out waiting for READY ($1s)" >&2; return 1
}

stop_headless() {
    [ -n "$HEADLESS_PID" ] || return 0
    kill -TERM "$HEADLESS_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || { HEADLESS_PID=""; return 0; }; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""
}

read_token() {
    PENPOT_TOKEN="$(json_field access_token <"$DATA_DIR/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

echo "== D1 offline + config-hardening gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT"

# --- pre-flight --------------------------------------------------------------
[ -f "$SURFACES_CJS" ] || { fail "surfaces observer missing: $SURFACES_CJS"; exit 1; }
[ -f "$EGRESS_PY" ] || { fail "egress parser missing: $EGRESS_PY"; exit 1; }
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing: $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }
[ -d "$BROWSERS" ] || { fail "bundled browsers missing: $BROWSERS"; exit 1; }
ports_free || { fail "one of the D1 ports is busy"; exit 1; }
pass "pre-flight: ports free, observer + egress-parser + playwright + browsers present"

# Cheap preflight selftests — pure logic, no stack, no browser.
if python3 "$EGRESS_PY" selftest >"$WORK_DIR/egress-selftest.log" 2>&1; then
    pass "preflight: d1_egress.py selftest"
else
    fail "preflight: d1_egress.py selftest failed"
    cat "$WORK_DIR/egress-selftest.log" >&2
fi
if "$NODE_BIN" "$SURFACES_CJS" selftest >"$WORK_DIR/surfaces-selftest.log" 2>&1; then
    pass "preflight: node d1_surfaces.cjs selftest"
else
    fail "preflight: node d1_surfaces.cjs selftest failed"
    cat "$WORK_DIR/surfaces-selftest.log" >&2
fi

# --- build ---------------------------------------------------------------
echo "-- build headless"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless); then
    fail "cargo build -p penpot-desktop --bin headless"; exit 1
fi
[ -x "$BIN" ] || { fail "built binary missing: $BIN"; exit 1; }
pass "cargo build -p penpot-desktop --bin headless"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "   (seeded postgres binaries from $PG_CACHE)"
fi

# --- boot ------------------------------------------------------------------
echo "-- boot (product defaults — no D1 env overrides; the flags under test"
echo "   are baked into compose_frontend_flags, not opted into by this gate)"
env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
    "$BIN" >"$LOG" 2>&1 &
HEADLESS_PID=$!
if ! wait_ready "$FIRST_BOOT_TIMEOUT"; then fail "boot reaches READY"; exit 1; fi
pass "boot reaches READY (product-default flags)"
read_token || { fail "no access token in $DATA_DIR/credentials.json"; exit 1; }

# --- (a) FLAGS SERVED — necessary but NOT sufficient on its own ------------
CONFIG="$(curl -fsS "$BASE/js/config.js" 2>/dev/null || true)"
for f in disable-registration disable-dashboard-templates-section \
         disable-google-fonts-provider disable-login-with-password; do
    if echo "$CONFIG" | grep -q -- "$f"; then
        pass "(a/served) config.js carries $f"
    else
        fail "(a/served) config.js is MISSING $f"
    fi
done

# --- (b) FLAGS TOOK EFFECT — the assertion that actually matters -----------
echo "-- (b) behavioural surface check: node d1_surfaces.cjs"
SURF="$(BASE="$BASE" REPO_ROOT="$ROOT" PLAYWRIGHT_MODULE="$PLAYWRIGHT" \
    PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" "$NODE_BIN" "$SURFACES_CJS" 2>"$WORK_DIR/surfaces.err")"
echo "     surfaces: $SURF"
SURF_OK="$(echo "$SURF" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))' 2>/dev/null || echo False)"
if [ "$SURF_OK" != "True" ]; then
    fail "(b/effect) d1_surfaces.cjs did not complete successfully — INFRASTRUCTURE FAILURE, no surface measurement is trustworthy: $(cat "$WORK_DIR/surfaces.err")"
    # Tri-state fallback uses the contract's own "inconclusive" string, not
    # JSON null — a null here would print as Python's "None" downstream and
    # contradict the documented gone|present|inconclusive contract even
    # though the case statements below already fail correctly on it.
    SURF='{"requests":0,"nonLoopbackRequests":[],"registration":"inconclusive","templatesSection":"inconclusive","loginForm":"inconclusive"}'
fi

# `or 'inconclusive'` (not just `.get(key, 'inconclusive')`) so a JSON null —
# not just a missing key — also renders as the contract value, never as
# Python's bare "None" (finding 5).
REG_V="$(echo "$SURF" | python3 -c "import json,sys;print(json.load(sys.stdin).get('registration') or 'inconclusive')")"
case "$REG_V" in
    gone)
        pass "(b/effect) registration — the surface is actually gone" ;;
    present)
        pass "(b/effect) registration is PRESENT — this is the ACCEPTED, DOCUMENTED outcome, NOT a failure."
        echo "     WHY: disable-registration only removes the \"Create an account\" LINK from the"
        echo "     login page (verified live, D1 task 3). #/auth/register still renders a working"
        echo "     signup form if reached directly, and the backend signup RPC stays live — our own"
        echo "     single-user provisioning calls that exact RPC, and that path runs on EVERY DB WIPE"
        echo "     (the project's P0 core invariant), so it cannot be disabled backend-side. This"
        echo "     surface is closed instead by D0/D1's navigation policy (navwatch::decide cancels"
        echo "     #/auth/* in the webview UNCONDITIONALLY) — asserted separately below, not by this"
        echo "     DOM probe."
        ;;
    *)
        # Never let "we could not look" read as "it is gone" — that is the
        # false-pass this whole leg exists to prevent.
        fail "(b/effect) registration is INCONCLUSIVE ($REG_V) — the page did not render, so absence proves nothing; this is an INFRASTRUCTURE FAILURE, not a real negative finding"
        ;;
esac

TMPL_V="$(echo "$SURF" | python3 -c "import json,sys;print(json.load(sys.stdin).get('templatesSection') or 'inconclusive')")"
case "$TMPL_V" in
    gone)
        pass "(b/effect) templatesSection — the surface is actually gone, not just flagged" ;;
    present)
        fail "(b/effect) templatesSection is PRESENT — the flag was SET but did NOT take effect (upstream may have renamed or dropped it)" ;;
    *)
        fail "(b/effect) templatesSection is INCONCLUSIVE ($TMPL_V) — the page did not render, so absence proves nothing; this is an INFRASTRUCTURE FAILURE, not a real negative finding" ;;
esac

# `disable-login-with-password` behavioural re-check (finding 3 closes the
# gate's former KNOWN GAP: task 4's live audit was manual and one-shot; this
# is the automated, repeatable re-check via the SAME observer + tri-state
# idiom used for registration/templatesSection above).
LOGIN_V="$(echo "$SURF" | python3 -c "import json,sys;print(json.load(sys.stdin).get('loginForm') or 'inconclusive')")"
case "$LOGIN_V" in
    gone)
        pass "(b/effect) loginForm — disable-login-with-password actually removed the password field and submit control, not just flagged" ;;
    present)
        fail "(b/effect) loginForm is PRESENT — disable-login-with-password was SET but did NOT take effect" ;;
    *)
        fail "(b/effect) loginForm is INCONCLUSIVE ($LOGIN_V) — the page did not render, so absence proves nothing; this is an INFRASTRUCTURE FAILURE, not a real negative finding" ;;
esac

# --- (navwatch) THE REGISTRATION CLOSURE — proven by policy, not DOM-probed -
# registration=present above is only tolerable if navwatch::decide() really
# does cancel+redirect the #/auth family in the webview — UNCONDITIONALLY,
# with no env var required (D1 hardening: the policy used to be dormant by
# default, since only PENPOT_LOCAL_NAVWATCH_REDIRECT=1 enabled it and nothing
# in the shipped product ever sets that var — see .superpowers/sdd/task-6-report.md
# finding 1). Run the EXISTING Rust unit tests that exercise that policy (no
# live stack needed for this leg — pure function, per
# apps/desktop/src/navwatch.rs), and require the SPECIFIC tests that pin the
# unconditional contract, not just "the suite passed" (a renamed/weakened
# test could still make the suite green while the policy regressed).
echo "-- (navwatch) cargo test -p penpot-desktop navwatch::tests:: (D0/D1 navigation policy)"
NAVWATCH_LOG="$WORK_DIR/navwatch-test.log"
if (cd "$ROOT" && cargo test -p penpot-desktop navwatch::tests::) >"$NAVWATCH_LOG" 2>&1; then
    NAVWATCH_RC=0
else
    NAVWATCH_RC=$?
fi
if [ "$NAVWATCH_RC" -eq 0 ] &&
    grep -q "test navwatch::tests::auth_family_is_cancelled_even_with_redirect_disabled ... ok" "$NAVWATCH_LOG" &&
    grep -q "test navwatch::tests::auth_family_is_cancelled_with_redirect_enabled_too ... ok" "$NAVWATCH_LOG" &&
    grep -q "test navwatch::tests::dashboard_and_settings_are_allowed_with_redirect_disabled ... ok" "$NAVWATCH_LOG" &&
    grep -q "test navwatch::tests::prefix_match_still_redirects_exact_and_subpath ... ok" "$NAVWATCH_LOG"; then
    pass "(navwatch) navwatch::decide cancels-and-redirects the #/auth family UNCONDITIONALLY (no env var), while #/dashboard and #/settings stay open by default as measurement-only"
    echo "     NOTE: no test is literally named \"register\" — decide()'s boundary-checked prefix"
    echo "     match on the #/auth family (exact match OR a \"/\" subpath) is the SAME code path for"
    echo "     #/auth/login and #/auth/register, and auth_family_is_cancelled_even_with_redirect_disabled"
    echo "     exercises that family with the env var OFF — the shipped product's actual default. That"
    echo "     is what makes registration=present (asserted above) a tolerated outcome rather than a"
    echo "     silent hole, and it no longer depends on an env var the product never sets."
else
    fail "(navwatch) cargo test -p penpot-desktop navwatch::tests:: did not confirm the unconditional #/auth redirect policy (rc=$NAVWATCH_RC) — see $NAVWATCH_LOG"
fi

# --- (c/spa) ZERO NON-LOOPBACK EGRESS — SPA side (same session as (b)) -----
# Guarded on SURF_OK: an empty nonLoopbackRequests array from the FALLBACK
# JSON (observer crashed) must NOT read as "zero requests observed" — that
# would be a vacuous pass reporting compliance for a session we never
# actually watched. A crashed observer already failed (b/effect) above; this
# leg must fail again here rather than silently pass on the dummy fallback.
if [ "$SURF_OK" != "True" ]; then
    fail "(c/spa) SKIPPED — d1_surfaces.cjs did not complete, so there is no real request log to check (INFRASTRUCTURE FAILURE, already reported above; NOT a zero-egress finding)"
else
    # Sanity check BEFORE trusting nonLoopbackRequests==[] as exhaustive
    # (finding 6, same vacuous-pass family as (c/proc) below): a browser
    # session that made zero requests of ANY kind never actually observed
    # network traffic, so an empty non-loopback list would prove nothing.
    REQ_TOTAL="$(echo "$SURF" | python3 -c "import json,sys;print(json.load(sys.stdin).get('requests', 0))")"
    if [ "$REQ_TOTAL" = "0" ]; then
        fail "(c/spa) INFRASTRUCTURE FAILURE — the browser observer recorded ZERO requests total (not even the page's own loopback traffic); nonLoopbackRequests=[] is vacuous here, not evidence of zero egress"
    else
        SPA_BAD="$(echo "$SURF" | python3 -c "import json,sys;print(len(json.load(sys.stdin).get('nonLoopbackRequests',[])))")"
        if [ "$SPA_BAD" = "0" ]; then
            pass "(c/spa) the SPA made ZERO non-loopback requests ($REQ_TOTAL total request(s) observed, confirming the session was actually captured)"
        else
            fail "(c/spa) the SPA attempted $SPA_BAD non-loopback request(s): $(echo "$SURF" | python3 -c "import json,sys;print(json.load(sys.stdin)['nonLoopbackRequests'])")"
        fi
    fi
fi

# --- realistic session BEFORE sampling: create a file, edit it, export it --
# The socket check below is a SAMPLE (see header caveat); sampling right
# after real activity, rather than against an idle boot, is what makes it a
# meaningful signal.
echo "-- realistic session: create/edit/export via RPC (scripts/roundtrip.py)"
EXERCISE_OUT="$(PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE" PENPOT_TOKEN="$PENPOT_TOKEN" \
    python3 - "$ROOT/scripts" <<'PY' 2>"$WORK_DIR/exercise.err"
import json, sys, uuid
sys.path.insert(0, sys.argv[1])
import roundtrip as rt

c = rt.Client()
c.login()
profile = c.rpc("get-profile", {})
team_id = profile["defaultTeamId"]
projects = c.rpc("get-projects", {"teamId": team_id})
project = next((p for p in projects if p.get("isDefault")), projects[0])

file_id, _created = rt.ensure_test_file(c, project["id"])          # create
g = c.rpc("get-file", {"id": file_id})
page_id = g["data"]["pages"][0]
sid = str(uuid.uuid4())
shape = rt.rect_shape(sid, "D1 Exercise Rect", 700, 120, 200, 150, "#22C55E")
c.rpc("update-file", {                                              # edit
    "id": file_id, "sessionId": str(uuid.uuid4()),
    "revn": g["revn"], "vern": g["vern"],
    "changes": [{"type": "add-obj", "id": sid, "pageId": page_id,
                 "frameId": rt.ROOT_FRAME, "parentId": rt.ROOT_FRAME,
                 "obj": shape}],
})
rt.export_binfile(c, file_id)                                       # export
print(json.dumps({"ok": True, "fileId": file_id}))
PY
)"
if echo "$EXERCISE_OUT" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("ok") else 1)' 2>/dev/null; then
    pass "(exercise) created a file, edited it (added a shape), exported it via export-binfile"
else
    fail "(exercise) create/edit/export exercise failed: $(cat "$WORK_DIR/exercise.err")"
fi

# --- (c/proc) ZERO NON-LOOPBACK EGRESS — process side, ONE SAMPLE ----------
# `lsof` errors are swallowed above (`|| true`) because a transient lsof
# failure must not abort the whole gate — but that also means an lsof that
# silently produced NOTHING (permissions hiccup, process already gone, wrong
# PID set, etc.) would otherwise flow straight through d1_egress.py parse
# (which returns nonLoopback:[] on empty input BY DESIGN) and read as a clean
# pass with zero evidence a sample ever actually happened (finding 2). Guard
# against that vacuous pass: require at least one CONNECTION of any kind
# (loopback included — this stack always holds several, e.g. the proxy<->
# backend/postgres/valkey sockets) before trusting the zero-non-loopback
# verdict.
lsof -nP -i -a -p "$(pgrep -P "$HEADLESS_PID" | tr '\n' ',' | sed 's/,$//'),$HEADLESS_PID" \
    >"$WORK_DIR/lsof.txt" 2>/dev/null || true
EG="$(python3 "$EGRESS_PY" parse "$WORK_DIR/lsof.txt")"
CONN_TOTAL="$(echo "$EG" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['connections']))")"
if [ "$CONN_TOTAL" = "0" ]; then
    fail "(c/proc) INFRASTRUCTURE FAILURE — the lsof sample captured ZERO connections of any kind (not even our own loopback sockets); nonLoopback=[] is vacuous here, not evidence of zero egress — see $WORK_DIR/lsof.txt"
else
    PROC_BAD="$(echo "$EG" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['nonLoopback']))")"
    if [ "$PROC_BAD" = "0" ]; then
        pass "(c/proc) no supervised process holds a non-loopback connection ($CONN_TOTAL loopback connection(s) observed, confirming the sample was real — single lsof sample, not a proof of absence: a connection that opens and closes between polls could be missed)"
    else
        fail "(c/proc) non-loopback connection(s): $(echo "$EG" | python3 -c "import json,sys;print(json.load(sys.stdin)['nonLoopback'])")"
    fi
fi

# --- shutdown ----------------------------------------------------------------
stop_headless
sleep 1
if ports_all_free; then
    pass "clean shutdown, all 4 D1 ports freed ($PROXY_PORT/$BACKEND_PORT/$POSTGRES_PORT/$VALKEY_PORT)"
else
    fail "ports still busy after clean shutdown"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D1 OFFLINE: ALL PASS"
else
    echo "D1 OFFLINE: $FAILURES FAILURE(S)"
fi
exit "$FAILURES"
