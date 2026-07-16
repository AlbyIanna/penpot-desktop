#!/usr/bin/env bash
# E6 SPIKE gate — cross-vault component-library id-remap resolver (PLAN3 E6).
#
# De-risks the biggest wall before any portable-library build: import-as-new is
# per-DB non-deterministic, so the SAME package installed into two vaults mints
# a DIFFERENT :component-file id AND different ids for everything inside the
# library file. A consumer carried from vault A to vault B therefore dangles
# every instance. This gate proves the whole remedy live, on ONE app instance
# switching vaults through the control endpoint (the N5 mechanism):
#
#   (1) install the SAME component-library package into vaults A and B and
#       assert the minted file ids DIFFER — and that the ids INSIDE differ too
#       (componentId, mainInstanceId, every main-instance subtree shape id).
#   (2) build the identity map by STATIC extraction over the package source
#       tree and each vault's materialized library tree, joined on E1's
#       uuid-stable keys (path + name + variantProperties) — capture example
#       entries.
#   (0) OFFLINE SELFTEST first (`e6_rewrite.py selftest`, no stack): synthetic
#       library trees with genuinely different ids prove the lockstep pairing
#       DISCRIMINATES, the duplicate-key + subtree-size refusals fire, and the
#       asset-ref rewrite (fill/stroke color + typography refs) is correct —
#       on 2.16.2 the live legs can't distinguish a correct mapping from a
#       no-op (internals are preserved), so this leg is the mapping evidence.
#   (3) author in A a consumer that links the library and places BOTH an
#       E3-style root-only instance AND a nested-subtree instance (copied
#       frame with per-shape shapeRefs into the library) PLUS two
#       library-STYLED shapes: a rect whose fill+stroke carry the library
#       color (fillColorRefId/RefFile, strokeColorRefId/RefFile) and a text
#       shape whose content nodes carry the library typography
#       (typographyRefId/RefFile) — the ASSET-ref classes, live.
#   (4) carry the consumer's on-disk tree to B, prove the naive carry DANGLES
#       (static + live), run the offline REWRITE TOOL
#       (scripts/ecosystem-spike/e6_rewrite.py), let the sync daemon import
#       the rewritten tree (import-as-new — fresh consumer id is fine), pin a
#       lock.json entry + link-file-to-library, and assert ZERO dangling refs
#       in B over RPC and on disk.
#   (5) invariant 1: delete the postgres cluster + reboot on B (re-assert the
#       on-disk tree statically too — the disk half is asserted, not
#       inferred), and switch back to A — id-stability and the rewrite hold on
#       BOTH vaults, with file_library_rel re-derived from lock.json by the
#       boot re-link.
#
# Dedicated ports (ledger): proxy 9010, backend 6472, postgres 5545, valkey
# 6488, control 9011 (exporter 6551 reserved; renders OFF). NOTE: the E6 plan
# sketch reserved backend 6471, but that is N2's exporter port — 6472 is the
# first free neighbor (grep scripts/ confirms). Fresh mktemp dirs; ANSI-
# stripped log greps; pg-install cache seeded; dirs kept on failure; clean
# teardown frees every port even on failure. Run-twice idempotent.
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, git, python3, curl.
#
# DECISION: deliberately NOT chained into `just e2e` — spike precedent (E5,
# contract-extractability): E6 lands no product code, so the ladder has nothing
# of E6's to regress. Re-run this gate on every Penpot version bump — the
# preserved-internal-ids behavior it captures is UNDOCUMENTED upstream (see
# docs/ecosystem-spikes/library-portability.md, caveat 1).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E6_PROXY_PORT:-9010}"
BACKEND_PORT="${E6_BACKEND_PORT:-6472}"
POSTGRES_PORT="${E6_POSTGRES_PORT:-5545}"
VALKEY_PORT="${E6_VALKEY_PORT:-6488}"
CONTROL_PORT="${E6_CONTROL_PORT:-9011}"
FIRST_BOOT_TIMEOUT="${E6_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
SWITCH_TIMEOUT="${E6_SWITCH_TIMEOUT:-600}"
SYNC_TIMEOUT="${E6_SYNC_TIMEOUT:-300}"
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"
CONTROL="http://127.0.0.1:${CONTROL_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e6-data.XXXXXX")"
VAULT_S="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e6-vaultS.XXXXXX")"  # scratch: source authoring
VAULT_A="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e6-vaultA.XXXXXX")"
VAULT_B="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e6-vaultB.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e6-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
PROBE="$ROOT/scripts/ecosystem-spike/e6_probe.py"
REWRITE="$ROOT/scripts/ecosystem-spike/e6_rewrite.py"
HEADLESS_PID=""
FAILURES=0

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
probe() { python3 "$PROBE" "$@"; }
rewrite_tool() { python3 "$REWRITE" "$@"; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
last_json_field() { tail -1 | json_field "$1"; }

PG_CACHE="${E6_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"

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
        rm -rf "$DATA_DIR" "$VAULT_S" "$VAULT_A" "$VAULT_B" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR S=$VAULT_S A=$VAULT_A B=$VAULT_B work=$WORK_DIR log=$LOG"
    fi
}
trap cleanup EXIT

# start_headless honors HL_DESIGNS (omit env when empty -> registry active).
start_headless() {
    local extra=()
    [ -n "${HL_DESIGNS:-}" ] && extra+=(PENPOT_LOCAL_DESIGNS_DIR="$HL_DESIGNS")
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT" \
        PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT" \
        PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT" \
        PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT" \
        PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" \
        "${extra[@]+"${extra[@]}"}" \
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

wait_log() { # wait_log <timeout> <pattern>
    local deadline=$(($(date +%s) + $1))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$LOG" | grep -q "$2" && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died waiting for '$2'" >&2; return 1; }
        sleep 2
    done
    echo "timed out waiting for '$2' ($1s)" >&2; return 1
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

switch_vault() { # switch_vault <abs-path>
    local out
    out="$(curl -fsS --max-time "$SWITCH_TIMEOUT" -X POST "$CONTROL/open" \
        -H 'Content-Type: application/json' -d "{\"path\": \"$1\"}" 2>/dev/null)" || return 1
    echo "$out" | grep -q '"ok":true' || echo "$out" | grep -q '"ok": true'
}

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

post_json() { curl -fsS -X POST "$BASE$1" -H 'Content-Type: application/json' -d "$2" 2>/dev/null; }

make_pkg_repo() { # make_pkg_repo <repo_dir> <id> <kind> <version> <tree_src>
    local repo="$1" id="$2" kind="$3" version="$4" tree="$5"
    mkdir -p "$repo"
    cp -R "$tree" "$repo/$id.penpot"
    cat >"$repo/package.json" <<EOF
{ "id": "$id", "version": "$version", "kind": "$kind", "name": "$id" }
EOF
    git -C "$repo" init -q &&
        git -C "$repo" -c user.email=e6@spike -c user.name=e6 add -A &&
        git -C "$repo" -c user.email=e6@spike -c user.name=e6 commit -qm "e6 package $id@$version"
}

echo "== E6 library-portability spike =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   vaults: S=$VAULT_S A=$VAULT_A B=$VAULT_B"

# --- build -------------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

# --- offline selftest (no stack): the mapping/refusal evidence ----------------
if OUT="$(rewrite_tool selftest 2>&1)"; then
    pass "(S) offline selftest: pairing discriminates on genuinely different ids; dup-key + subtree-size refusals fire; asset refs (fill/stroke color + typography) rewritten"
else
    fail "(S) offline selftest"; echo "$OUT" >&2; exit 1
fi

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ------------------------------------------------------------------------------
# Phase 0 — scratch vault: author + export the package SOURCE trees, build repos.
# ------------------------------------------------------------------------------
echo "== phase 0: author the package source (scratch vault S) =="
HL_DESIGNS="$VAULT_S" start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then pass "boot READY on scratch vault S"; else fail "boot"; exit 1; fi
read_token || { fail "no access token"; exit 1; }

SRC_DIR="$WORK_DIR/src"
if OUT="$(probe author_sources "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$SRC_DIR" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    pass "(0) authored + exported the package source trees"
else
    fail "(0) author_sources"; echo "$OUT" >&2; exit 1
fi
SRC_LIB_FID="$(tail -1 <<<"$OUT" | json_field sourceLibFileId)"
echo "     source lib file id: $SRC_LIB_FID"

make_pkg_repo "$WORK_DIR/repos/e6-button-kit" e6-button-kit component-library 1.0.0 "$SRC_DIR/e6-button-kit.penpot" &&
    make_pkg_repo "$WORK_DIR/repos/e6-app" e6-app design 1.0.0 "$SRC_DIR/e6-app.penpot" &&
    pass "(0) built the two git package repos (file:// fetchable)" ||
    { fail "(0) building package repos"; exit 1; }

# ------------------------------------------------------------------------------
# Phase A — vault A: install+publish the library, author the linked consumer.
# ------------------------------------------------------------------------------
echo "== phase A: vault A — install library + author linked consumer =="
if switch_vault "$VAULT_A"; then pass "(A) switched to vault A via control /open"; else fail "(A) switch to A"; exit 1; fi
read_token || { fail "(A) no token"; exit 1; }

FETCH="$(post_json /__api/packages/fetch "{\"url\":\"file://$WORK_DIR/repos/e6-button-kit\"}")"
echo "$FETCH" | grep -q '"ok":true' && pass "(A) fetched e6-button-kit" || { fail "(A) fetch: $FETCH"; exit 1; }
PUB_A="$(post_json /__api/packages/publish '{"id":"e6-button-kit"}')"
LIB_A="$(echo "$PUB_A" | json_field fileId 2>/dev/null || true)"
[ -n "$LIB_A" ] && pass "(A) installed+published e6-button-kit (fileId=$LIB_A)" ||
    { fail "(A) publish: $PUB_A"; exit 1; }

if OUT="$(probe capture_lib "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$VAULT_A" "$LIB_A" "$WORK_DIR/libA.penpot" "$SYNC_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    tail -1 <<<"$OUT" >"$WORK_DIR/libA-cap.json"
    pass "(A) captured vault-A materialized library identity"
else
    fail "(A) capture_lib A"; echo "$OUT" >&2; exit 1
fi

FETCH="$(post_json /__api/packages/fetch "{\"url\":\"file://$WORK_DIR/repos/e6-app\"}")"
echo "$FETCH" | grep -q '"ok":true' && pass "(A) fetched e6-app" || { fail "(A) fetch e6-app: $FETCH"; exit 1; }
LINK_A="$(post_json /__api/packages/link '{"consumerId":"e6-app","libraryId":"e6-button-kit"}')"
CONS_A="$(echo "$LINK_A" | json_field consumerFileId 2>/dev/null || true)"
[ -n "$CONS_A" ] && pass "(A) linked e6-app -> e6-button-kit (consumerFileId=$CONS_A)" ||
    { fail "(A) link: $LINK_A"; exit 1; }

if OUT="$(probe place_instances "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_A" "$WORK_DIR/libA-cap.json" 2>&1)"; then
    echo "$OUT" | grep -E '^(PASS|FAIL)' || true
    NEST_A="$(tail -1 <<<"$OUT" | json_field nestedInstanceId)"
    ROOT_A="$(tail -1 <<<"$OUT" | json_field rootInstanceId)"
    pass "(A) placed root-only + nested-subtree instances (nested=$NEST_A)"
else
    fail "(A) place_instances"; echo "$OUT" >&2; exit 1
fi

if OUT="$(probe wait_ondisk "$BASE" "$VAULT_A" "$CONS_A" "$LIB_A" "$NEST_A" "$SYNC_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    CONS_A_REL="$(tail -1 <<<"$OUT" | json_field relPath)"
    pass "(A) surgical export landed on disk ($CONS_A_REL)"
else
    fail "(A) consumer never on disk with componentFile=libA"; echo "$OUT" >&2; exit 1
fi

probe verify_live "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_A" "$LIB_A" 30 >"$WORK_DIR/verifyA-baseline.json" 2>&1 &&
    pass "(A) baseline: zero dangling refs in A (RPC)" ||
    { fail "(A) baseline verify"; cat "$WORK_DIR/verifyA-baseline.json" >&2; }

cp -R "$VAULT_A/$CONS_A_REL" "$WORK_DIR/carried.penpot" &&
    pass "(A) carried the consumer's on-disk tree out of vault A" ||
    { fail "(A) carry copy"; exit 1; }

# the library pin the carried consumer's B-side lock entry will reference
curl -fsS "$BASE/__api/packages" >"$WORK_DIR/packagesA.json" 2>/dev/null || true
LIB_VERSION="$(python3 -c "
import json,sys
d=json.load(open('$WORK_DIR/packagesA.json'))
p={x['id']:x for x in d['packages']}['e6-button-kit']
print(p['version'])")"
LIB_CONTRACT_A="$(python3 -c "
import json,sys
d=json.load(open('$WORK_DIR/packagesA.json'))
p={x['id']:x for x in d['packages']}['e6-button-kit']
print(p['contractHash'])")"

# ------------------------------------------------------------------------------
# Phase B — vault B: install the SAME package; capture that ALL ids differ.
# ------------------------------------------------------------------------------
echo "== phase B: vault B — same package, different minted ids =="
if switch_vault "$VAULT_B"; then pass "(B) switched to vault B via control /open"; else fail "(B) switch to B"; exit 1; fi
read_token || { fail "(B) no token"; exit 1; }

FETCH="$(post_json /__api/packages/fetch "{\"url\":\"file://$WORK_DIR/repos/e6-button-kit\"}")"
echo "$FETCH" | grep -q '"ok":true' && pass "(B) fetched e6-button-kit" || { fail "(B) fetch: $FETCH"; exit 1; }
PUB_B="$(post_json /__api/packages/publish '{"id":"e6-button-kit"}')"
LIB_B="$(echo "$PUB_B" | json_field fileId 2>/dev/null || true)"
[ -n "$LIB_B" ] && pass "(B) installed+published e6-button-kit (fileId=$LIB_B)" ||
    { fail "(B) publish: $PUB_B"; exit 1; }

if OUT="$(probe capture_lib "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$VAULT_B" "$LIB_B" "$WORK_DIR/libB.penpot" "$SYNC_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    tail -1 <<<"$OUT" >"$WORK_DIR/libB-cap.json"
    pass "(B) captured vault-B materialized library identity"
else
    fail "(B) capture_lib B"; echo "$OUT" >&2; exit 1
fi

# (1) THE NON-DETERMINISM CAPTURE. The load-bearing wall: the minted
#     :component-file ids DIFFER per vault (hard assert). The internal-id
#     behavior is CAPTURED, not presumed: on 2.16.2, binfile-v3 import-as-new
#     builds its remap index over FILE ids (+ media/thumbnail object ids) only
#     — `bfc/update-index` in app/binfile/v3.clj `import-file` — so component/
#     shape ids inside the library are PRESERVED from the package source bytes.
#     The gate records which of the two worlds (preserved vs remapped) this
#     Penpot lives in; the rewrite tool handles BOTH (the E1-keyed map is an
#     identity map for preserved internals). Either way the map must join 1:1.
if python3 - "$WORK_DIR" "$SRC_LIB_FID" <<'PY'
import json, sys
work, src_fid = sys.argv[1], sys.argv[2]
A = json.load(open(f"{work}/libA-cap.json"))
B = json.load(open(f"{work}/libB-cap.json"))
S = json.load(open(f"{work}/src/source-ids.json"))
ok = True
def check(c, m):
    global ok
    print(("PASS: " if c else "FAIL: ") + m); ok = ok and c
check(len({A["fileId"], B["fileId"], src_fid}) == 3,
      f"minted :component-file ids DIFFER (src={src_fid} A={A['fileId']} B={B['fileId']})")
ka = {json.dumps(c["key"], sort_keys=True): c for c in A["components"]}
kb = {json.dumps(c["key"], sort_keys=True): c for c in B["components"]}
ks = {json.dumps(c["key"], sort_keys=True): c for c in S["components"]}
check(set(ka) == set(kb) == set(ks) and len(ka) == 2,
      f"uuid-stable E1 keys join source/A/B 1:1 ({len(ka)} components)")
same = diff = 0
for k in sorted(ka):
    a, b, s = ka[k], kb[k], ks[k]
    check(len(a["subtreeShapeIds"]) == len(b["subtreeShapeIds"]) == len(s["subtreeShapeIds"]),
          f"subtree size equal source/A/B for {a['key']['name']} ({len(a['subtreeShapeIds'])} shapes)")
    ids_a = [a["componentId"], a["mainInstanceId"]] + a["subtreeShapeIds"]
    ids_b = [b["componentId"], b["mainInstanceId"]] + b["subtreeShapeIds"]
    ids_s = [s["componentId"], s["mainInstanceId"]] + s["subtreeShapeIds"]
    if ids_a == ids_b == ids_s:
        same += 1
    elif all(x != y for x, y in zip(ids_a, ids_b)):
        diff += 1
if same == len(ka):
    print("PASS: CAPTURED: internal componentId/mainInstanceId/shape ids are "
          "PRESERVED from the package source (2.16.2 v3 import remaps only "
          "file ids) — the FILE-id fields (componentFile + the asset "
          "*RefFile trio) are the ONLY dangling ref class today")
elif diff == len(ka):
    print("PASS: CAPTURED: internal ids are REMAPPED per vault — the full "
          "component-level map is load-bearing")
else:
    check(False, "internal-id behavior is MIXED across components (unexpected)")
sys.exit(0 if ok else 1)
PY
then pass "(1) non-determinism captured: :component-file differs per vault; internal-id behavior recorded"
else fail "(1) ids capture"; fi

# ------------------------------------------------------------------------------
# Phase C — naive carry DANGLES; the rewrite tool fixes it (offline, static).
# ------------------------------------------------------------------------------
echo "== phase C: naive-carry dangle witness + offline rewrite =="
if rewrite_tool verify "$WORK_DIR/carried.penpot" "$WORK_DIR/libB.penpot" \
    -o "$WORK_DIR/naive-verify.json" >/dev/null 2>&1; then
    fail "(C) naive carried tree unexpectedly resolves in B"
elif [ -f "$WORK_DIR/naive-verify.json" ]; then
    DANGLING_N="$(python3 -c "import json;print(len(json.load(open('$WORK_DIR/naive-verify.json'))['dangling']))")"
    ASSET_DANGLING_N="$(python3 -c "
import json
d = json.load(open('$WORK_DIR/naive-verify.json'))['dangling']
print(sum(1 for x in d if x.get('asset')))")"
    [ "${DANGLING_N:-0}" -gt 0 ] &&
        pass "(C) NAIVE carry dangles in B ($DANGLING_N dangling refs — the wall is real)" ||
        fail "(C) naive verify reported no dangling yet failed"
    [ "${ASSET_DANGLING_N:-0}" -gt 0 ] &&
        pass "(C) the naive dangle set includes ASSET refs ($ASSET_DANGLING_N of them — the verifier enumerates fill/stroke/typography refs)" ||
        fail "(C) naive verify saw no dangling asset refs (verifier blind to them?)"
else
    fail "(C) naive verify crashed (no report)"
fi
# live half of the witness: the componentFile the carried tree references (libA)
# does not exist in vault B's DB at all.
if curl -fsS -X POST "$BASE/api/rpc/command/get-file" \
    -H "Authorization: Token $PENPOT_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"id\":\"$LIB_A\"}" >/dev/null 2>&1; then
    fail "(C) vault-A library id unexpectedly live in vault B"
else
    pass "(C) vault-A library file id ($LIB_A) is NOT live in vault B (get-file fails)"
fi

if rewrite_tool rewrite "$WORK_DIR/carried.penpot" "$WORK_DIR/libA.penpot" "$WORK_DIR/libB.penpot" \
    "$WORK_DIR/rewritten.penpot" -o "$WORK_DIR/rewrite-report.json" >/dev/null 2>&1; then
    pass "(C) REWRITE TOOL: map derived (E1 keys) + consumer rewritten + post-verify zero dangling (static)"
    python3 -c "
import json
r = json.load(open('$WORK_DIR/rewrite-report.json'))
s = r['rewrite']; m = r['map']
print(f\"     rewritten: componentFile={s['componentFileRewritten']} componentId={s['componentIdRewritten']} shapeRef={s['shapeRefRewritten']} assetRefFile={s['assetRefFileRewritten']} assetRefId={s['assetRefIdRewritten']} (files touched: {s['filesTouched']})\")
print(f\"     map: fileId {m['oldFileId']} -> {m['newFileId']}; {len(m['componentIdMap'])} components; {len(m['shapeIdMap'])} subtree shapes; {len(m['colorIdMap'])} colors; {len(m['typographyIdMap'])} typographies; problems={len(m['problems'])} duplicates={len(m['duplicates'])}\")
for e in m['components']:
    print(f\"     entry {e['key']['path']}/{e['key']['name']}: depth={e['subtreeDepth']} structuralMatch={e['structuralMatch']}\")"
else
    fail "(C) rewrite tool failed"; cat "$WORK_DIR/rewrite-report.json" >&2 2>/dev/null; exit 1
fi
# the ASSET-ref classes must have been exercised: fill + stroke color refs on
# the styled rect, typography refs on the text paragraph+span, span fill ref
if python3 -c "
import json, sys
s = json.load(open('$WORK_DIR/rewrite-report.json'))['rewrite']
sys.exit(0 if s['assetRefFileRewritten'] == 5 and s['assetRefIdRewritten'] == 5 else 1)"; then
    pass "(C) all 5 library ASSET refs rewritten (fill+stroke color, paragraph+span typography, span fill)"
else
    fail "(C) asset refs not fully rewritten (expected 5+5)"
fi

# ------------------------------------------------------------------------------
# Phase D — carry into B while the stack is DOWN; import; pin; link; verify.
# ------------------------------------------------------------------------------
echo "== phase D: carry the rewritten tree into vault B =="
if stop_headless; then pass "(D) stack down before touching vault B's tree"; else fail "(D) shutdown hung"; fi
mkdir -p "$VAULT_B/Carried"
cp -R "$WORK_DIR/rewritten.penpot" "$VAULT_B/Carried/e6-app.penpot"
pass "(D) rewritten consumer tree copied into vault B (stack down — no sync race)"

: >"$LOG"
HL_DESIGNS="" start_headless   # registry active vault = B
if wait_ready "$REBOOT_TIMEOUT" && wait_log "$SYNC_TIMEOUT" "startup reconciliation done"; then
    pass "(D) reboot on B + startup reconciliation (daemon imports the carried tree)"
else
    fail "(D) reboot/reconcile"; tail -25 "$LOG" >&2; exit 1
fi
read_token || { fail "(D) no token after reboot"; exit 1; }

if OUT="$(probe find_file_by_rel "$VAULT_B" "Carried/e6-app.penpot" "$SYNC_TIMEOUT" 2>&1)"; then
    CONS_B="$(tail -1 <<<"$OUT" | json_field fileId)"
    pass "(D) carried consumer imported as-new in B (fresh fileId=$CONS_B)"
else
    fail "(D) carried consumer never imported"; echo "$OUT" >&2; exit 1
fi
[ "$CONS_B" != "$CONS_A" ] &&
    pass "(D) import-as-new minted a FRESH consumer id in B ($CONS_A -> $CONS_B) — expected" ||
    fail "(D) consumer id unexpectedly preserved across vaults"

probe add_lock_entry "$VAULT_B" e6-app-carried "$CONS_B" "e6-app (carried)" \
    e6-button-kit "$LIB_B" "$LIB_VERSION" "$LIB_CONTRACT_A" &&
    pass "(D) lock.json pins the carried consumer + its library link (re-derivable)" ||
    fail "(D) add_lock_entry"

probe link "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_B" "$LIB_B" &&
    pass "(D) link-file-to-library re-established file_library_rel in B" ||
    fail "(D) link in B"

# force one export so the on-disk tree is fully B-minted AND surgical
probe nudge "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_B" auto 41 >/dev/null &&
    pass "(D) nudged the consumer to force a (surgical) re-export" || fail "(D) nudge"
if OUT="$(probe wait_ondisk "$BASE" "$VAULT_B" "$CONS_B" "$LIB_B" any "$SYNC_TIMEOUT" "$CONS_B" 2>&1)"; then
    CONS_B_REL="$(tail -1 <<<"$OUT" | json_field relPath)"
    pass "(D) DAEMON re-exported the consumer surgically: componentFile=<libB> kept, embedded id now B-minted ($CONS_B_REL)"
else
    fail "(D) surgical export in B"; echo "$OUT" >&2
fi

probe verify_live "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_B" "$LIB_B" 60 >"$WORK_DIR/verifyB.json" 2>&1 &&
    { pass "(D) ZERO dangling refs in B over RPC (every componentFile/componentId/shapeRef resolves)"; grep '^PASS' "$WORK_DIR/verifyB.json" || true; } ||
    { fail "(D) zero-dangling in B"; cat "$WORK_DIR/verifyB.json" >&2; }

rewrite_tool verify "$VAULT_B/${CONS_B_REL:-Carried/e6-app.penpot}" "$WORK_DIR/libB.penpot" \
    -o "$WORK_DIR/verifyB-disk.json" >/dev/null 2>&1 &&
    pass "(D) ZERO dangling refs in B's ON-DISK exported tree (static verify)" ||
    { fail "(D) on-disk zero-dangling in B"; cat "$WORK_DIR/verifyB-disk.json" >&2; }

# ------------------------------------------------------------------------------
# Phase E — invariant 1: delete-DB + reboot on B; then switch back to A.
# ------------------------------------------------------------------------------
echo "== phase E: invariant 1 on BOTH vaults =="
cp "$VAULT_B/lock.json" "$WORK_DIR/lockB-before.json"
if stop_headless; then pass "(E) clean shutdown before DB wipe"; else fail "(E) shutdown hung"; fi
rm -rf "$DATA_DIR/postgres"
pass "(E) deleted the Penpot database (rm -rf <data>/postgres)"
: >"$LOG"
HL_DESIGNS="" start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log "$SYNC_TIMEOUT" "startup reconciliation done"; then
    pass "(E) reboot on B + reconciliation from disk"
else
    fail "(E) reboot/reconcile on B"; tail -25 "$LOG" >&2; exit 1
fi
read_token || { fail "(E) no token"; exit 1; }

if probe verify_live "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_B" "$LIB_B" "$SYNC_TIMEOUT" --expect-relink >"$WORK_DIR/verifyB-wipe.json" 2>&1; then
    pass "(E) B after delete-DB+reboot: SAME ids resurrected, zero dangling, file_library_rel re-derived from lock.json"
    grep '^PASS' "$WORK_DIR/verifyB-wipe.json" || true
else
    fail "(E) B post-wipe verify"; cat "$WORK_DIR/verifyB-wipe.json" >&2
fi
cmp -s "$VAULT_B/lock.json" "$WORK_DIR/lockB-before.json" &&
    pass "(E) vault-B lock.json byte-identical across the wipe" ||
    fail "(E) vault-B lock.json changed across the wipe"

# the ON-DISK half of "invariant 1 preserves the rewrite": static zero-dangling
# over the carried consumer's exported tree AFTER the wipe+reboot (asserted,
# not inferred from the RPC leg)
rewrite_tool verify "$VAULT_B/${CONS_B_REL:-Carried/e6-app.penpot}" "$WORK_DIR/libB.penpot" \
    -o "$WORK_DIR/verifyB-wipe-disk.json" >/dev/null 2>&1 &&
    pass "(E) post-wipe ON-DISK zero-dangling over the carried consumer's tree (static verify)" ||
    { fail "(E) post-wipe on-disk verify"; cat "$WORK_DIR/verifyB-wipe-disk.json" >&2 2>/dev/null; }

if switch_vault "$VAULT_A"; then pass "(E) switched back to vault A"; else fail "(E) switch back to A"; exit 1; fi
read_token || { fail "(E) no token after switch"; exit 1; }
if probe verify_live "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$CONS_A" "$LIB_A" "$SYNC_TIMEOUT" --expect-relink >"$WORK_DIR/verifyA-back.json" 2>&1; then
    pass "(E) A after the switch-back (its own wipe+rebuild): SAME ids, zero dangling, relinked"
    grep '^PASS' "$WORK_DIR/verifyA-back.json" || true
else
    fail "(E) A post-switch verify"; cat "$WORK_DIR/verifyA-back.json" >&2
fi

# --- shutdown -----------------------------------------------------------------
if stop_headless; then pass "final clean shutdown"; else fail "final shutdown hung"; fi
sleep 1
ports_all_free && pass "all 4 ports freed" || fail "ports still busy after shutdown"

echo
echo "headline: same package -> different ids in A/B ; naive carry dangles (instances AND asset refs) ; offline rewrite -> zero dangling in B ; invariant 1 holds on BOTH vaults ; selftest proves the mapping discriminates"
if [ "$FAILURES" -eq 0 ]; then
    echo "E6 PORTABILITY: ALL PASS"
    exit 0
else
    echo "E6 PORTABILITY: $FAILURES FAILURE(S)"
    exit 1
fi
