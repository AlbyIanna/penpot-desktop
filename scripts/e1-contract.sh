#!/usr/bin/env bash
# E1 contract-extractor gate (PLAN3.md milestone E1, `just contract`).
#
# A FAST, PURE, STACK-FREE static gate: E1 extracts and diffs contracts over
# on-disk normalized `.penpot` JSON (the same bytes the ledger hashes), so its
# whole exit surface is verifiable without a Penpot backend. Exit criteria
# implemented verbatim, each a PASS/FAIL block in the house style of
# n1-index.sh / m5-features.sh:
#
#   (a) extract emits the EXPECTED contract for the authored combined fixture:
#       per-set {name/path, exposedProperties[], tokensUsed[]} for BOTH the
#       first-class and the legacy variant model, plus the library-level
#       exported color/typography/token surface.
#   (b) extract(A) == extract(A') where A' is A after a uuid churn (the cheap,
#       stack-free simulation of import-as-new's per-DB id remap): the contract
#       body is byte-identical and `diff` reads PATCH — keyed by name/path,
#       never the remapped variantId (caveat 2). The spike oracle, which keys
#       first-class sets on variantId, reads the SAME churn as MAJOR noise —
#       the gate asserts that divergence to prove the caveat is real.
#   (c) the curated delta matrix classifies impl-only $value -> patch,
#       added -> minor, removed/renamed -> major EXACTLY matching the python
#       oracle (scripts/ecosystem-spike). Two E1 extensions beyond the oracle
#       are asserted against the Rust classifier and documented: a token
#       $type-change -> major, and the migration case below.
#   (d) the legacy->first-class migration does NOT read as a spurious minor
#       (caveat 3): the Rust classifier labels it `migration`, never `minor`.
#   (e) invariant 1: contracts are a PURE function of disk (no persisted index
#       in E1) — re-extracting rebuilds a byte-identical contract from disk
#       alone; the delete-index-db+reindex property is trivially satisfied.
#
# Run-twice idempotent: each run regenerates the fixture into a fresh mktemp
# workdir and cleans up on success. ANSI-stripped greps. Requires: rust
# toolchain, python3. No Penpot stack, no ports, no network.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e1-work.XXXXXX")"
FIXTURE="$ROOT/scripts/e1-fixture.py"
SPIKE="$ROOT/scripts/ecosystem-spike"
BIN="$ROOT/target/debug/contract"
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }

cleanup() {
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$WORK_DIR"
    else
        echo "-- kept workdir for inspection: $WORK_DIR"
    fi
}
trap cleanup EXIT

# Rust `contract diff` -> PATCH|MINOR|MAJOR|MIGRATION (grep line).
rust_bump() { "$BIN" diff "$1" "$2" | strip_ansi | sed -n 's/^OVERALL BUMP: //p' | head -1; }

# Python oracle bump on the same two trees.
oracle_bump() {
    python3 "$SPIKE/extract_contract.py" "$1" --json "$WORK_DIR/_ob.json" >/dev/null || return 1
    python3 "$SPIKE/extract_contract.py" "$2" --json "$WORK_DIR/_oa.json" >/dev/null || return 1
    python3 "$SPIKE/diff_contracts.py" "$WORK_DIR/_ob.json" "$WORK_DIR/_oa.json" \
        | strip_ansi | sed -n 's/^OVERALL BUMP: //p' | head -1
}

echo "== E1 contract gate =="

# ---------------------------------------------------------------------------
# Build the contract CLI (the lint surface).
# ---------------------------------------------------------------------------
echo "-- building contract CLI"
if cargo build -p vault-index --bin contract >/dev/null 2>&1; then
    pass "contract CLI builds"
else
    fail "contract CLI build"
    exit 1
fi

# ---------------------------------------------------------------------------
# Author the combined fixture + delta matrix (fresh each run -> idempotent).
# ---------------------------------------------------------------------------
echo "-- authoring fixture"
if python3 "$FIXTURE" "$WORK_DIR" >/dev/null; then
    pass "authored combined fixture + delta matrix"
else
    fail "fixture generation"
    exit 1
fi
BASE="$WORK_DIR/baseline"

# ---------------------------------------------------------------------------
# (a) extract emits the expected contract for the fixture.
# ---------------------------------------------------------------------------
echo "== (a) extract emits expected contract =="
"$BIN" extract "$BASE" > "$WORK_DIR/base.json"
python3 - "$WORK_DIR/base.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
ok = True
def check(cond, msg):
    global ok
    print(("PASS: " if cond else "FAIL: ") + msg)
    ok = ok and cond

byset = {c["set"]: c for c in d["contracts"]}
check(len(d["contracts"]) == 2, "two variant sets extracted")

fc = byset.get("Controls / Button", {})
check(fc.get("setKind") == "first-class-variant", "first-class set keyed by PATH not variantId")
check(fc.get("variantNames") == ["Default", "Large"], "first-class variantNames")
check(fc.get("exposedProperties") == ["Size", "State"], "first-class exposedProperties (variantProperties[].name)")
check(fc.get("tokensUsed") == ["layerBase.text", "spacing.sm"], "first-class tokensUsed = set-union over subtree")

lg = byset.get("Legacy / Combobox", {})
check(lg.get("setKind") == "path-convention", "legacy set recovered by shared path")
check(lg.get("variantNames") == ["Active", "Default", "Disabled"], "legacy variantNames = component names")
check(lg.get("exposedProperties") == [], "legacy exposedProperties undeclared (empty)")

check(d["exportedColors"] == ["Accent", "Brand Teal"], "library exported colors")
check(d["exportedTypographies"] == ["Heading XL"], "library exported typographies")
paths = [t["path"] for t in d["exportedTokens"]]
check(paths == ["layerBase.text", "layerOne.text", "radius.lg", "spacing.md", "spacing.sm"],
      "library exported token paths from tokens.json")
sys.exit(0 if ok else 1)
PY
[ $? -eq 0 ] || fail "expected-contract assertions"

# ---------------------------------------------------------------------------
# (b) extract(A) == extract(A') under uuid churn (caveat 2, uuid-invariance).
# ---------------------------------------------------------------------------
echo "== (b) extract(A) == extract(A') after uuid churn =="
"$BIN" churn "$BASE" "$WORK_DIR/churned" 2>/dev/null
"$BIN" extract "$WORK_DIR/churned" > "$WORK_DIR/churned.json"

# fileId churns; the contract body must not.
if python3 - "$WORK_DIR/base.json" "$WORK_DIR/churned.json" <<'PY'
import json, sys
a = json.load(open(sys.argv[1])); b = json.load(open(sys.argv[2]))
assert a["fileId"] != b["fileId"], "fileId did NOT churn — churn was a no-op"
del a["fileId"]; del b["fileId"]
sys.exit(0 if a == b else 2)
PY
then
    pass "contract body byte-identical across uuid churn (fileId excluded)"
else
    fail "contract not uuid-invariant"
fi

CHURN_RUST="$(rust_bump "$BASE" "$WORK_DIR/churned")"
if [ "$CHURN_RUST" = "PATCH" ]; then
    pass "Rust diff(A, A') = PATCH (id-free, keyed by name/path)"
else
    fail "Rust diff(A, A') = $CHURN_RUST (expected PATCH)"
fi

# The oracle keys first-class sets on variantId -> the SAME churn is all-major
# noise. Asserting the divergence proves caveat 2 is a real hazard E1 fixes.
CHURN_ORACLE="$(oracle_bump "$BASE" "$WORK_DIR/churned")"
if [ "$CHURN_ORACLE" = "MAJOR" ]; then
    pass "spike oracle mis-reads the churn as MAJOR (variantId-keyed) — E1 fixes this"
else
    fail "expected oracle churn=MAJOR (variantId noise), got $CHURN_ORACLE"
fi

# ---------------------------------------------------------------------------
# (c) delta matrix classification == oracle exactly (spike matrix).
# ---------------------------------------------------------------------------
echo "== (c) delta matrix classification matches the oracle =="
check_parity() {
    local delta="$1" want="$2"
    local r o
    r="$(rust_bump "$BASE" "$WORK_DIR/$delta")"
    o="$(oracle_bump "$BASE" "$WORK_DIR/$delta")"
    if [ "$r" = "$want" ] && [ "$o" = "$want" ]; then
        pass "$delta: rust=$r oracle=$o (== $want)"
    else
        fail "$delta: rust=$r oracle=$o (expected both $want)"
    fi
}
check_parity delta-patch          PATCH
check_parity delta-minor          MINOR
check_parity delta-major-removed  MAJOR
check_parity delta-major-renamed  MAJOR

# E1 extension #1: token $type change -> major (oracle can't see exportedTokens).
R="$(rust_bump "$BASE" "$WORK_DIR/delta-major-typechanged")"
O="$(oracle_bump "$BASE" "$WORK_DIR/delta-major-typechanged")"
if [ "$R" = "MAJOR" ] && [ "$O" = "PATCH" ]; then
    pass "delta-major-typechanged: rust=MAJOR (E1 \$type-sensitive), oracle blind (PATCH)"
else
    fail "delta-major-typechanged: rust=$R oracle=$O (expected rust=MAJOR oracle=PATCH)"
fi

# ---------------------------------------------------------------------------
# (d) legacy->first-class migration is NOT a spurious minor (caveat 3).
# ---------------------------------------------------------------------------
echo "== (d) legacy->first-class migration special-case =="
MIG_RUST="$(rust_bump "$BASE" "$WORK_DIR/delta-migration")"
if [ "$MIG_RUST" = "MIGRATION" ]; then
    pass "migration classified as MIGRATION"
else
    fail "migration classified as $MIG_RUST (expected MIGRATION)"
fi
if [ "$MIG_RUST" != "MINOR" ]; then
    pass "migration does NOT read as a spurious minor (caveat 3)"
else
    fail "migration read as spurious minor"
fi
# The naive oracle keying reads it as noise (major); E1's name/path keying
# recovers the true model-switch semantics.
MIG_ORACLE="$(oracle_bump "$BASE" "$WORK_DIR/delta-migration")"
if [ "$MIG_ORACLE" != "MIGRATION" ]; then
    pass "spike oracle has no migration concept (got $MIG_ORACLE) — E1 special-cases it"
else
    fail "unexpected: oracle produced MIGRATION"
fi

# ---------------------------------------------------------------------------
# (e) invariant 1: contract is a pure function of disk, rebuilt identically.
# ---------------------------------------------------------------------------
echo "== (e) invariant 1: rebuilt identically from disk alone =="
"$BIN" extract "$BASE" > "$WORK_DIR/base2.json"
if diff -q "$WORK_DIR/base.json" "$WORK_DIR/base2.json" >/dev/null; then
    pass "re-extraction byte-identical (no hidden state; delete-index+reindex safe)"
else
    fail "re-extraction not deterministic"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "E1 GATE: ALL PASS"
    exit 0
else
    echo "E1 GATE: $FAILURES FAILURE(S)"
    exit 1
fi
