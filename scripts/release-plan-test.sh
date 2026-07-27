#!/usr/bin/env bash
# Gate for scripts/release-plan.sh: the conventional-commit release decision.
# Builds throwaway git repos (empty commits — no files needed) and pins:
#   bump rules   feat->minor, fix/perf->patch, breaking->major, rest->none
#   version math X.Y.Z arithmetic; bump=none -> next-patch + "-dev.<shortsha>"
#   anchoring    only bare vX.Y.Z tags anchor the range (rc tags never);
#                a bare tag on HEAD forces bump=none
#   changelog    grouped, merge commits excluded, prefix stripped
# Pure and fast: no network, no ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/release-plan.sh"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- fixture helpers ---------------------------------------------------------
G() { git -c user.email=t@test -c user.name=t -C "$1" "${@:2}"; }
mk_repo() {  # mk_repo <name> -> path; starts with one chore commit on main
  local r="$TMP/$1"
  git init -q -b main "$r"
  G "$r" commit -q --allow-empty -m "chore: root"
  echo "$r"
}
cm() { G "$1" commit -q --allow-empty -m "$2"; }          # cm <repo> <subject>
cmb() { G "$1" commit -q --allow-empty -m "$2" -m "$3"; } # with body
run_plan() {  # run_plan <repo> -> stdout of the script; changelog at <repo>/ch.md
  (cd "$1" && bash "$SCRIPT" ch.md)
}
val() { printf '%s\n' "$1" | sed -n "s/^$2=//p"; }  # val <output> <key>
expect() {  # expect <label> <output> <key> <want>
  local got; got="$(val "$2" "$3")"
  if [ "$got" = "$4" ]; then pass "$1: $3=$4"; else fail "$1: $3 got '$got' want '$4'"; fi
}

# --- 1. no tags + a feat -> first release 0.1.0 ------------------------------
R="$(mk_repo t1)"; cm "$R" "feat: first feature"
OUT="$(run_plan "$R")"
expect "no-tag+feat" "$OUT" bump minor
expect "no-tag+feat" "$OUT" version 0.1.0
expect "no-tag+feat" "$OUT" release_tag v0.1.0
expect "no-tag+feat" "$OUT" last_tag ""

# --- 2. no tags + docs only -> none, 0.0.1-dev.<sha> -------------------------
R="$(mk_repo t2)"; cm "$R" "docs: readme"
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "no-tag+docs" "$OUT" bump none
expect "no-tag+docs" "$OUT" version "0.0.1-dev.$SHORT"
expect "no-tag+docs" "$OUT" release_tag ""

# --- 3. tag v1.2.3 + docs/refactor/chore only -> none, 1.2.4-dev -------------
R="$(mk_repo t3)"; G "$R" tag v1.2.3
cm "$R" "docs: notes"; cm "$R" "refactor(core): shuffle"; cm "$R" "chore: deps"
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "quiet" "$OUT" bump none
expect "quiet" "$OUT" last_tag v1.2.3
expect "quiet" "$OUT" version "1.2.4-dev.$SHORT"
expect "quiet" "$OUT" release_tag ""

# --- 4. tag v1.2.3 + fix -> patch 1.2.4 --------------------------------------
R="$(mk_repo t4)"; G "$R" tag v1.2.3; cm "$R" "fix(sync): off-by-one"
OUT="$(run_plan "$R")"
expect "fix" "$OUT" bump patch
expect "fix" "$OUT" version 1.2.4
expect "fix" "$OUT" release_tag v1.2.4

# --- 5. tag v1.2.3 + feat + fix -> minor 1.3.0 -------------------------------
R="$(mk_repo t5)"; G "$R" tag v1.2.3
cm "$R" "fix: small"; cm "$R" "feat(cli): new verb"; cm "$R" "perf: faster"
OUT="$(run_plan "$R")"
expect "feat+fix" "$OUT" bump minor
expect "feat+fix" "$OUT" version 1.3.0
expect "feat+fix" "$OUT" release_tag v1.3.0

# --- 6. changelog: grouped, prefix stripped, scope bolded --------------------
CH="$(cat "$TMP/t5/ch.md")"
echo "$CH" | grep -q '^### Features$'        && pass "changelog: Features group"   || fail "changelog: Features group missing"
echo "$CH" | grep -q '^### Bug fixes$'       && pass "changelog: Bug fixes group"  || fail "changelog: Bug fixes group missing"
echo "$CH" | grep -q '\*\*cli:\*\* new verb' && pass "changelog: scoped subject"   || fail "changelog: scoped subject missing"
echo "$CH" | grep -q 'feat(cli):'            && fail "changelog: prefix not stripped" || pass "changelog: prefix stripped"

# --- 7. breaking via "!" -> major --------------------------------------------
R="$(mk_repo t7)"; G "$R" tag v1.2.3; cm "$R" "feat(api)!: drop legacy verbs"
OUT="$(run_plan "$R")"
expect "bang" "$OUT" bump major
expect "bang" "$OUT" version 2.0.0

# --- 8. breaking via body footer -> major ------------------------------------
R="$(mk_repo t8)"; G "$R" tag v1.2.3
cmb "$R" "fix: change default port" "BREAKING CHANGE: the control port moved"
OUT="$(run_plan "$R")"
expect "body-breaking" "$OUT" bump major
expect "body-breaking" "$OUT" version 2.0.0

# --- 9. rc tags never anchor the range ---------------------------------------
R="$(mk_repo t9)"; G "$R" tag v1.2.3
cm "$R" "feat: one"; G "$R" tag v1.3.0-rc1; cm "$R" "feat: two"
OUT="$(run_plan "$R")"
expect "rc-ignored" "$OUT" last_tag v1.2.3
expect "rc-ignored" "$OUT" version 1.3.0
# BOTH feats (before and after the rc tag) are in the changelog:
grep -q 'one' "$TMP/t9/ch.md" && pass "rc-ignored: pre-rc commit in changelog" \
                              || fail "rc-ignored: pre-rc commit missing"

# --- 10. bare tag on HEAD -> bump=none, rolling derives from it --------------
R="$(mk_repo t10)"; G "$R" tag v1.2.3; cm "$R" "feat: shipped"; G "$R" tag v1.3.0
OUT="$(run_plan "$R")"
SHORT="$(G "$R" rev-parse --short=7 HEAD)"
expect "head-tagged" "$OUT" bump none
expect "head-tagged" "$OUT" head_tag v1.3.0
expect "head-tagged" "$OUT" version "1.3.1-dev.$SHORT"
expect "head-tagged" "$OUT" release_tag ""

# --- 11. merge commits are excluded ------------------------------------------
R="$(mk_repo t11)"; G "$R" tag v1.2.3
G "$R" checkout -q -b feature
cm "$R" "fix: real work"
G "$R" checkout -q main
cm "$R" "docs: mainline noise"
G "$R" merge -q --no-ff feature -m "feat: LYING merge subject"
OUT="$(run_plan "$R")"
expect "merge-excluded" "$OUT" bump patch   # the fix counts; the merge's fake feat does not
grep -q 'LYING' "$TMP/t11/ch.md" && fail "merge-excluded: merge subject leaked" \
                                 || pass "merge-excluded: merge subject absent"

# --- 12. non-conforming subjects -> none, never crash ------------------------
R="$(mk_repo t12)"; G "$R" tag v1.2.3
cm "$R" "just a plain sentence with (parens) and: colons"
OUT="$(run_plan "$R")"
expect "non-conforming" "$OUT" bump none

echo
echo "release-plan-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
