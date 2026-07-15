#!/usr/bin/env bash
# E2 package-home / lockfile / installer gate (PLAN3.md milestone E2, `just e2`).
#
# Chapter 3's spine, on-disk half: `.penpot-packages/` as the in-vault package
# home (blind to BOTH sync directions), a git-diffable `lock.json` per vault,
# and a generalized installer (N6's import-as-new + settle-until-fixpoint) that
# lands a package as an ORDINARY vault `.penpot` file. Install is an EXPLICIT
# verb; fetch is `git clone` (git repos, not a registry).
#
# Exit criteria (PLAN3 E2, verbatim), each a PASS/FAIL block in the house style:
#   (a) a template `.penpot` AND a component-library `.penpot` dropped under
#       `.penpot-packages/` → the daemon NEVER enumerates/hashes/conflict-copies
#       them: editing a file inside leaves the sync manifest byte-unchanged, adds
#       no `.conflict` copy, imports nothing (boards count + lock stay put).
#   (b) an explicit install imports + settles to a FIXPOINT (two equal semantic
#       hashes — the N6 P0, no phantom diff on first rebuild) + writes a lock
#       entry ({version, contentHash, contractHash, sourceGitUrl, fileId}).
#   (c) `git clone <local-bare-repo>` lands a package and it installs OFFLINE
#       (a `file://` repo — no registry, no network); its `sourceGitUrl` is the
#       clone origin.
#   (d) delete-DB + reboot re-applies EVERY locked package deterministically
#       (in-place import preserves file-ids per M2, invariant 1): every locked
#       fileId is live again with the SAME id, lock.json is byte-identical, and
#       NO user-disk write happened outside `.penpot-packages/` (design trees +
#       the package home both byte-untouched; no `.conflict` copy).
#   (e) run-twice = no-op: a second install is `alreadyInstalled` with zero
#       settle cycles and leaves lock.json byte-identical.
#
# Dedicated ports (ledger): proxy 8962, backend 6423, postgres 5497, valkey
# 6440 (control 8963 + exporter 6503 reserved; renders OFF — packages need no
# renders). Fresh mktemp dirs; ANSI-stripped log greps; pg-install cache seeded;
# dirs kept on failure. Run-twice idempotent (fresh dirs each run).
#
# Requirements: rust toolchain, runtime/ artifacts (scripts/fetch-penpot.sh),
# JDK 26, valkey-server, git, python3, curl.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E2_PROXY_PORT:-8962}"
BACKEND_PORT="${E2_BACKEND_PORT:-6423}"
POSTGRES_PORT="${E2_POSTGRES_PORT:-5497}"
VALKEY_PORT="${E2_VALKEY_PORT:-6440}"
FIRST_BOOT_TIMEOUT="${E2_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
MATERIALIZE_TIMEOUT="${E2_MATERIALIZE_TIMEOUT:-180}"
RECONCILE_TIMEOUT=240
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e2-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e2-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e2-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BIN="$ROOT/target/debug/headless"
HELPER="$ROOT/scripts/e2_packages_helper.py"
HEADLESS_PID=""
FAILURES=0

export PENPOT_BACKEND="$BASE"
export PENPOT_FRONTEND="$BASE"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
e2helper() { python3 "$HELPER" "$@"; }
sha() { shasum -a 256 | cut -d' ' -f1; }

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
        for _ in $(seq 1 20); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    pkill -9 -f "$DATA_DIR" 2>/dev/null
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT log=$LOG"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

start_headless() {
    env PENPOT_LOCAL_DATA_DIR="$DATA_DIR" \
        PENPOT_LOCAL_DESIGNS_DIR="$VAULT" \
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

ports_all_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

# Byte hashes proving "no user-disk write outside .penpot-packages" on re-apply.
design_hash() { # all design-tree files EXCEPT the package home
    find "$VAULT" -type f -path '*.penpot/*' -not -path '*/.penpot-packages/*' -print0 2>/dev/null \
        | LC_ALL=C sort -z | xargs -0 shasum -a 256 2>/dev/null | sha
}
pkg_hash() { # everything under the package home
    find "$VAULT/.penpot-packages" -type f -print0 2>/dev/null \
        | LC_ALL=C sort -z | xargs -0 shasum -a 256 2>/dev/null | sha
}
no_conflicts() { [ -z "$(find "$VAULT" -name '*.conflict-*' -print -quit 2>/dev/null)" ]; }

wait_packages_live() { # wait until GET /__api/packages has >=N live, no live:false
    local want="$1" deadline=$(($(date +%s) + RECONCILE_TIMEOUT)) body
    while [ "$(date +%s)" -lt "$deadline" ]; do
        body="$(curl -fsS "$BASE/__api/packages" 2>/dev/null || true)"
        if [ -n "$body" ] && ! echo "$body" | grep -q '"live": *false' && ! echo "$body" | grep -q '"live":false'; then
            local n; n="$(echo "$body" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("count",0))' 2>/dev/null || echo 0)"
            [ "${n:-0}" -ge "$want" ] && return 0
        fi
        sleep 3
    done
    return 1
}

author_package() { # author_package <pkg_id> <kind> <version> <src_rel>
    local id="$1" kind="$2" version="$3" src_rel="$4"
    local dest="$VAULT/.penpot-packages/$id"
    mkdir -p "$dest"
    cp -R "$VAULT/$src_rel" "$dest/$id.penpot"
    cat >"$dest/package.json" <<EOF
{
  "id": "$id",
  "version": "$version",
  "kind": "$kind",
  "name": "$id"
}
EOF
}

echo "== E2 packages gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT"
echo "   vault (empty): $VAULT"

# --- build -----------------------------------------------------------------
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "build (headless + penpot-watchdog)"; exit 1
fi
pass "build (headless + penpot-watchdog)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "     (seeded postgres binaries from $PG_CACHE)"
fi

# ---------------------------------------------------------------------------
# Boot + author two packages from a builtin template (a package is a folder).
# ---------------------------------------------------------------------------
start_headless
if wait_ready "$FIRST_BOOT_TIMEOUT"; then pass "boot READY on an empty vault"; else fail "boot"; exit 1; fi
read_token || { fail "no access token"; exit 1; }

echo "== authoring package trees from a builtin template =="
if OUT="$(e2helper new_template "$BASE" "$VAULT" welcome "$MATERIALIZE_TIMEOUT" 2>&1)"; then
    echo "$OUT" | grep '^PASS' || true
    SRC_REL="$(echo "$OUT" | tail -1 | json_field relPath)"
    pass "materialized a source template tree ($SRC_REL)"
else
    fail "authoring source template"; echo "$OUT" >&2; exit 1
fi
# Drop a "template" package AND a "component-library" package under the home.
author_package welcome-template template  1.0.0 "$SRC_REL"
author_package button-library   component-library 2.0.0 "$SRC_REL"
pass "dropped a template .penpot AND a component-library .penpot under .penpot-packages/"
sleep 8  # let the source template finish syncing before the blindness snapshot

# ---------------------------------------------------------------------------
# (a) daemon blindness: edit inside the packages → zero sync activity.
# ---------------------------------------------------------------------------
echo "== (a) daemon is blind to .penpot-packages/ =="
if e2helper blind_check "$BASE" "$VAULT" 14; then :; else fail "(a) daemon blindness"; fi

# ---------------------------------------------------------------------------
# (b) explicit install (dropped-in package) → import + settle + lock entry.
# ---------------------------------------------------------------------------
echo "== (b) explicit install imports + settles to a fixpoint + writes lock =="
if e2helper install_verify "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$VAULT" welcome-template "$MATERIALIZE_TIMEOUT" \
    >"$WORK_DIR/install-wt.out" 2>&1; then
    grep '^PASS' "$WORK_DIR/install-wt.out"; tail -1 "$WORK_DIR/install-wt.out"
else
    fail "(b) install welcome-template"; cat "$WORK_DIR/install-wt.out" >&2
fi

# ---------------------------------------------------------------------------
# (c) git clone a local bare repo → offline install.
# ---------------------------------------------------------------------------
echo "== (c) git clone <local bare repo> lands a package + installs offline =="
BARE="$WORK_DIR/button-library.git"
(
    cd "$VAULT/.penpot-packages/button-library" &&
    git init -q && git config user.email e2@local && git config user.name e2 &&
    git add -A && git commit -q -m "button library package" &&
    git clone -q --bare . "$BARE"
) >>"$LOG" 2>&1 || { fail "(c) build local bare repo"; }
# Remove the working copy so fetch has to clone it back from the bare repo.
rm -rf "$VAULT/.penpot-packages/button-library"
FETCH_OUT="$(curl -fsS -X POST "$BASE/__api/packages/fetch" \
    -H 'Content-Type: application/json' \
    -d "{\"url\": \"file://$BARE\", \"id\": \"button-library\"}" 2>/dev/null || true)"
if echo "$FETCH_OUT" | grep -q '"ok": *true' && [ -d "$VAULT/.penpot-packages/button-library/button-library.penpot" ]; then
    pass "(c) git clone landed button-library under .penpot-packages/ (offline file:// repo)"
else
    fail "(c) fetch/clone: $FETCH_OUT"
fi
if e2helper install_verify "$BASE" "$BACKEND" "$PENPOT_TOKEN" "$VAULT" button-library "$MATERIALIZE_TIMEOUT" \
    >"$WORK_DIR/install-bl.out" 2>&1; then
    grep '^PASS' "$WORK_DIR/install-bl.out"; tail -1 "$WORK_DIR/install-bl.out"
    SRC_URL="$(tail -1 "$WORK_DIR/install-bl.out" | json_field sourceGitUrl 2>/dev/null || true)"
    case "$SRC_URL" in
        file://*) pass "(c) lock records the clone origin as sourceGitUrl ($SRC_URL) — git repo, not a registry" ;;
        *) fail "(c) sourceGitUrl not the file:// clone origin: '$SRC_URL'" ;;
    esac
else
    fail "(c) install button-library (offline)"; cat "$WORK_DIR/install-bl.out" >&2
fi

# ---------------------------------------------------------------------------
# (d) delete-DB + reboot re-applies every locked package deterministically.
# ---------------------------------------------------------------------------
echo "== (d) delete-DB + reboot re-applies every locked package =="
cp "$VAULT/lock.json" "$WORK_DIR/lock-before.json"
DESIGN_BEFORE="$(design_hash)"; PKG_BEFORE="$(pkg_hash)"
if stop_headless; then pass "(d) clean shutdown before DB wipe"; else fail "(d) shutdown hung"; fi
rm -rf "$DATA_DIR/postgres"     # wipe ONLY the disposable DB (M2)
pass "(d) deleted the Penpot database (rm -rf <data>/postgres)"
: >"$LOG"
start_headless
if wait_ready "$REBOOT_TIMEOUT" && wait_log "$RECONCILE_TIMEOUT" "startup reconciliation done"; then
    pass "(d) reboot + startup reconciliation from disk"
else
    fail "(d) reboot/reconcile"; tail -25 "$LOG" >&2; exit 1
fi
read_token || { fail "(d) no token after reboot"; exit 1; }
if wait_packages_live 2; then
    pass "(d) every locked package is live again in the DB after the wipe"
else
    fail "(d) not all packages became live after reboot"; curl -fsS "$BASE/__api/packages" >&2 || true
fi
if e2helper assert_reapply "$BASE" "$VAULT" "$WORK_DIR/lock-before.json" >"$WORK_DIR/reapply.out" 2>&1; then
    grep '^PASS' "$WORK_DIR/reapply.out"
else
    fail "(d) re-apply determinism"; cat "$WORK_DIR/reapply.out" >&2
fi
[ "$(design_hash)" = "$DESIGN_BEFORE" ] &&
    pass "(d) design trees byte-identical across the wipe (no user-disk write outside .penpot-packages)" ||
    fail "(d) a design tree changed across the wipe"
[ "$(pkg_hash)" = "$PKG_BEFORE" ] &&
    pass "(d) .penpot-packages/ byte-untouched across the wipe" ||
    fail "(d) .penpot-packages changed across the wipe"
no_conflicts && pass "(d) no .conflict-* copies after re-apply" || fail "(d) conflict copy after re-apply"

# ---------------------------------------------------------------------------
# (e) run-twice = no-op (idempotent, no phantom diff).
# ---------------------------------------------------------------------------
echo "== (e) run-twice install is a no-op =="
cp "$VAULT/lock.json" "$WORK_DIR/lock-pre-reinstall.json"
for pkg in welcome-template button-library; do
    if e2helper idempotent "$BASE" "$VAULT" "$pkg" >"$WORK_DIR/idem-$pkg.out" 2>&1; then
        grep '^PASS' "$WORK_DIR/idem-$pkg.out"
    else
        fail "(e) idempotent re-install $pkg"; cat "$WORK_DIR/idem-$pkg.out" >&2
    fi
done
if cmp -s "$VAULT/lock.json" "$WORK_DIR/lock-pre-reinstall.json"; then
    pass "(e) lock.json byte-identical after the no-op re-installs"
else
    fail "(e) lock.json changed on a no-op re-install"
fi

# --- shutdown --------------------------------------------------------------
if stop_headless; then pass "final clean shutdown"; else fail "final shutdown hung"; fi
sleep 1
ports_all_free && pass "all 4 ports freed" || fail "ports still busy after shutdown"

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "E2 PACKAGES: ALL PASS"
    exit 0
else
    echo "E2 PACKAGES: $FAILURES FAILURE(S)"
    exit 1
fi
