#!/usr/bin/env bash
# D6 residue gate (PLAN4 milestone D6, `just d6`) — chapter 4's closer. Ties
# together the route-accounting audit, the honest known-limits doc, and the
# packaged-artifact proof (Task 3, scripts/m4-artifact-test.sh) into one
# gate, without re-running or duplicating any of them.
#
# Model: scripts/d1-offline.sh — header block, pass()/fail(), PID-scoped
# cleanup, totals, non-zero exit; SKIPPED is counted SEPARATELY from PASS
# (D0/D2/D3/D5 precedent) and must never read as a silent pass.
#
# Legs:
#   (a) ROUTE ACCOUNTING IS COMPLETE AND PINNED. `cargo test -p penpot-desktop
#       navwatch::` must both PASS and report every one of the specific tests
#       that pin each route-family verdict AS HAVING RUN AND PASSED, by name
#       — "the suite passed" is not enough (the D1/D2 precedent): a renamed
#       or weakened test would still leave the suite green while the
#       accounting silently regressed. Every required name is first grepped
#       directly out of apps/desktop/src/navwatch.rs, so this leg fails
#       loudly if a name doesn't actually exist in source rather than trust
#       a stale copy-paste in this script. Required: debug_routes_are_cancelled,
#       internal_render_routes_and_the_viewer_stay_allowed,
#       dashboard_is_cancelled_even_with_redirect_disabled,
#       settings_is_present_and_cancelled_in_d4,
#       auth_family_is_cancelled_even_with_redirect_disabled,
#       workspace_and_our_surfaces_are_never_redirected.
#   (b) A LIVE REDIRECT LANDS ON /__home. SKIPPED here — see the skip() call
#       below for the full reason. The redirect POLICY itself (both
#       #/dashboard and #/debug/playground) is already REQUIRED by name in
#       leg (a); what a live GUI leg would add on top is proof that Tauri's
#       real on_navigation callback (not just the pure decide() function)
#       wires the policy up end-to-end — D0 already established that
#       mechanism works for the #/dashboard case, and D1-D5 have exercised it
#       continuously since. SKIPPED is counted separately from PASS and never
#       silently folded into it.
#   (c) THE KNOWN-LIMITS DOC EXISTS AND IS HONEST. A grep-level structural
#       check — cheap, and it stops docs/known-limits.md silently
#       disappearing or being gutted — that the doc exists and carries all
#       five required sections (what was removed/closed; what stays
#       web-shaped inside the canvas and why invariant 3 keeps it; what is
#       kept deliberately; deferred threads; testing honesty).
#   (d) THE PACKAGED PROOF IS REFERENCED, NOT DUPLICATED. Greps
#       scripts/m4-artifact-test.sh for the navwatch-log wiring and
#       assertion Task 3 added, so `just d6` and `just m4` cannot drift
#       apart silently. The actual packaged run (needs a built dmg, many
#       minutes) stays in `just m4`, never re-executed here.
#
# Dedicated ports for this gate (proxy 9058, backend 6520, postgres 5593,
# valkey 6536) — reserved per the milestone plan; NOT bound today because leg
# (b) is SKIPPED, so nothing in this script boots a live stack. Kept as
# env-overridable variables (same shape as the sibling gates) so a future
# live leg (b) can pick them up without renumbering anything.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D6_PROXY_PORT:-9058}"
BACKEND_PORT="${D6_BACKEND_PORT:-6520}"
POSTGRES_PORT="${D6_POSTGRES_PORT:-5593}"
VALKEY_PORT="${D6_VALKEY_PORT:-6536}"

NAVWATCH_RS="$ROOT/apps/desktop/src/navwatch.rs"
KNOWN_LIMITS="$ROOT/docs/known-limits.md"
M4_SCRIPT="$ROOT/scripts/m4-artifact-test.sh"

FAILURES=0
SKIPPED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
skip() { echo "SKIPPED: $1"; SKIPPED=$((SKIPPED + 1)); }

# This gate spawns no live process/stack today (leg (b) is SKIPPED — see
# below), so there is nothing PID-scoped to tear down. Kept as a named no-op
# trap, rather than omitted, so a future live leg (b) has an obvious place to
# add PID-scoped teardown without restructuring the script — same discipline
# every sibling gate uses ("teardown is strictly PID-scoped ... never
# pkill/killall by name").
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d6-work.XXXXXX")"
cleanup() {
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$WORK_DIR"
    else
        echo "kept for debugging: $WORK_DIR"
    fi
}
trap cleanup EXIT

echo "== D6 residue audit gate =="
echo "   (ports reserved for a future live leg (b): proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT)"

# --- (a) route accounting is complete and pinned ----------------------------
echo "-- (a) cargo test -p penpot-desktop navwatch:: (route accounting)"

REQUIRED_TESTS=(
    "debug_routes_are_cancelled"
    "internal_render_routes_and_the_viewer_stay_allowed"
    "dashboard_is_cancelled_even_with_redirect_disabled"
    "settings_is_present_and_cancelled_in_d4"
    "auth_family_is_cancelled_even_with_redirect_disabled"
    "workspace_and_our_surfaces_are_never_redirected"
)

if [ ! -f "$NAVWATCH_RS" ]; then
    fail "(a/source) navwatch.rs present (missing: $NAVWATCH_RS)"
else
    MISSING_IN_SRC=()
    for t in "${REQUIRED_TESTS[@]}"; do
        grep -q "fn $t" "$NAVWATCH_RS" 2>/dev/null || MISSING_IN_SRC+=("$t")
    done
    if [ "${#MISSING_IN_SRC[@]}" -eq 0 ]; then
        pass "(a/source) all ${#REQUIRED_TESTS[@]} required navwatch tests exist by name in navwatch.rs: ${REQUIRED_TESTS[*]}"
    else
        fail "(a/source) required test(s) missing from navwatch.rs source (renamed or deleted?): ${MISSING_IN_SRC[*]}"
    fi
fi

NAV_LOG="$WORK_DIR/navwatch-test.log"
if (cd "$ROOT" && cargo test -p penpot-desktop navwatch::) >"$NAV_LOG" 2>&1; then
    NAV_RC=0
else
    NAV_RC=$?
fi
if [ "$NAV_RC" -eq 0 ]; then
    NAMED_MISSING=()
    for t in "${REQUIRED_TESTS[@]}"; do
        grep -q "test navwatch::tests::$t ... ok" "$NAV_LOG" || NAMED_MISSING+=("$t")
    done
    if [ "${#NAMED_MISSING[@]}" -eq 0 ]; then
        pass "(a/run) cargo test -p penpot-desktop navwatch:: passed AND every required test reported ok BY NAME (not just \"the suite passed\"): ${REQUIRED_TESTS[*]}"
    else
        fail "(a/run) cargo test -p penpot-desktop navwatch:: passed overall, but these required tests did NOT report ok by name (renamed/weakened?): ${NAMED_MISSING[*]} — see $NAV_LOG"
    fi
else
    fail "(a/run) cargo test -p penpot-desktop navwatch:: failed (rc=$NAV_RC) — see $NAV_LOG"
fi

# --- (b) a live redirect lands on /__home -----------------------------------
# SKIPPED, not asserted live, by the explicit fallback the milestone plan
# names for this leg. Reasoning:
#   1. The redirect POLICY for both routes under test (#/dashboard AND
#      #/debug/playground) is already REQUIRED BY NAME in leg (a) above
#      (dashboard_is_cancelled_even_with_redirect_disabled,
#      debug_routes_are_cancelled) — a regression in the policy itself
#      already fails this gate through that leg.
#   2. Driving a genuinely new live probe for #/debug/playground (as opposed
#      to reusing D0's existing /__navprobe page, which is hardcoded to the
#      #/dashboard case only — apps/desktop/src/navprobe.html) would need a
#      product-code change to navprobe.{rs,html} to parameterize the target
#      fragment. That is out of this task's declared scope (this task creates
#      scripts/d6-residue.sh and modifies the justfile only) and risks
#      touching invariant-3-adjacent surface without the same scrutiny D0
#      gave it.
#   3. Unlike D0/D2/D3/D5's GUI legs (which unconditionally attempt a real
#      launch and let a missing display fail loudly), no mechanism exists in
#      this codebase to detect "no GUI session available" ahead of time and
#      skip gracefully — every precedent script just launches and lets a
#      timeout/crash report the failure. Adding that detection here, un-
#      verified in this environment, would be new untested infrastructure
#      layered on an already-skipped leg.
# Both routes are covered end-to-end elsewhere: D0's own navprobe leg proved
# the on_navigation -> decide() -> redirect wiring works live for a
# same-document hash change to #/dashboard, and D1-D5 have exercised that
# exact live-redirect mechanism continuously since (D1's `(navwatch)` leg,
# D2's `(d/navwatch)` leg). This leg's SKIP therefore narrows to "no NEW live
# probe was written for #/debug/playground specifically" — not "the redirect
# mechanism is unverified live".
skip "(b) live GUI redirect probe for #/dashboard and #/debug/playground — not wired (see the comment above this leg for the full reasoning); the redirect policy for both routes is already REQUIRED BY NAME in leg (a), and the live on_navigation->decide()->redirect MECHANISM (as opposed to the pure policy function) has been exercised continuously since D0/D1/D2's own live GUI legs"

# --- (c) known-limits doc exists and is honest ------------------------------
echo "-- (c) docs/known-limits.md exists and is honest (structural check)"
if [ ! -f "$KNOWN_LIMITS" ]; then
    fail "(c) docs/known-limits.md exists (missing: $KNOWN_LIMITS)"
else
    REQUIRED_SECTIONS=(
        "What the app removed or closed"
        "stays web-shaped inside the canvas"
        "What is kept deliberately"
        "Deferred threads"
        "Testing honesty"
    )
    MISSING_SECTIONS=()
    for s in "${REQUIRED_SECTIONS[@]}"; do
        grep -qF "$s" "$KNOWN_LIMITS" || MISSING_SECTIONS+=("$s")
    done
    if [ "${#MISSING_SECTIONS[@]}" -eq 0 ]; then
        pass "(c) docs/known-limits.md exists and carries all ${#REQUIRED_SECTIONS[@]} required sections"
    else
        fail "(c) docs/known-limits.md is missing required section(s): ${MISSING_SECTIONS[*]}"
    fi
fi

# --- (d) the packaged proof is referenced, not duplicated ------------------
echo "-- (d) scripts/m4-artifact-test.sh carries the D6 navwatch-log leg (not re-run here)"
if [ ! -f "$M4_SCRIPT" ]; then
    fail "(d) scripts/m4-artifact-test.sh present (missing: $M4_SCRIPT)"
else
    M4_OK=1
    if ! grep -qF 'PENPOT_LOCAL_NAVWATCH_LOG="$NAVWATCH_LOG"' "$M4_SCRIPT"; then
        M4_OK=0
        fail "(d) scripts/m4-artifact-test.sh no longer wires PENPOT_LOCAL_NAVWATCH_LOG through the packaged env -i launch"
    fi
    if ! grep -qF '/__bootstrap -> /__home' "$M4_SCRIPT"; then
        M4_OK=0
        fail "(d) scripts/m4-artifact-test.sh no longer carries the /__bootstrap -> /__home navwatch-log assertion"
    fi
    if [ "$M4_OK" -eq 1 ]; then
        pass "(d) scripts/m4-artifact-test.sh still carries the D6 navwatch-log wiring + assertion (just d6 and just m4 have not drifted apart); the actual packaged run stays in just m4"
    fi
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D6 RESIDUE: ALL PASS ($SKIPPED SKIPPED)"
else
    echo "D6 RESIDUE: $FAILURES FAILURE(S) ($SKIPPED SKIPPED)"
fi
exit "$FAILURES"
