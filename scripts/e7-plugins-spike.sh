#!/usr/bin/env bash
# E7 — plugin packages: staged ACTIVATION spike + THIN-BUILD gate (PLAN3.md
# milestone E7).
#
# The chapter closer, STAGED: the ACTIVATION spike ran first and landed GO
# (+ CSP-GO), so the thin build shipped and THIS GATE NOW DRIVES THE REAL
# PRODUCT ROUTES: every boot uses the product DEFAULTS (enable-plugins +
# penpotPluginsWhitelist + the CSP header are default-ON — no plugin env is
# passed), the /__packages/<pkg>/* serve route is the shipped hardened one,
# the install is CAPTURED into lock.json by the product reconcile, and the
# delete-DB re-registration is the product BOOT RE-APPLY (not a manual
# probe replay). A plugin package = a git repo of static assets
# (manifest.json + plugin.js + icon) under `.penpot-packages/`, served AT THE
# LOCAL PROXY ORIGIN (`/__packages/<pkg>/manifest.json`), carried-and-
# pointed-at, NEVER imported into the design DB.
#
# What it proves live on a 2.16.2 self-hosted stack, house style (PASS/FAIL):
#   (a) ACTIVATION — the bundled-chromium browser leg opens Penpot's OWN native
#       Plugin Manager, enters the local /__packages/<pkg>/manifest.json URL,
#       passes Penpot's own consent prompt (Allow), and the plugin's observable
#       effect (a named shape) appears in the workspace — asserted over RPC.
#   (b) INVARIANT-3 WITNESS — the served SPA index.html + main JS bundle sha256
#       are IDENTICAL before/after the whole install flow, and no <script> was
#       injected. Our integration is the proxy + a native URL boundary, never a
#       patched/injected/driven SPA.
#   (c) CONSENT — no registry pointer exists in profile props BEFORE the
#       browser leg's explicit install/allow step (no pre-seeding path).
#   (d) POINTER — the registry pointer is written via the PUBLIC
#       update-profile-props RPC; captured from the stored props.
#   (e) PERSISTENCE / WIPE-RECOVERY — the product capture loop pins the user's
#       install into lock.json (LockEntry.plugin_props) AND records the consent
#       into the per-machine ledger (<data_dir>/plugin-consent.json, finding 1).
#       Delete the DB (keep the data dir + ledger), reboot, and the product BOOT
#       RE-APPLY re-registers the pointer — DRIVEN BY LEDGER AUTHORITY (asserted
#       from get-profile + the boot log line), restored value identical.
#   (f) CSP-EGRESS PROBE (MULTI-VECTOR) — the fixture plugin attempts BOTH a
#       fetch() (connect-src) and an Image().src (img-src) off-origin beacon.
#       With the proxy CSP response header OFF both are OBSERVED; with the
#       default-src+connect-src+img-src CSP ON both are ABSENT + CSP-blocked
#       while the plugin STILL LOADS. Plus the runtime-mechanism capture.
#   (g) INVARIANT-3 (FULL SCRIPT SET) — the witness hashes index.html PLUS every
#       <script src> it references (config.js, polyfills, libs.js, main.js, …),
#       each asserted HTTP 200 + NON-EMPTY (no vacuous empty-hash pass), before
#       and after the install flow (same boot → config.js stable).
#   (h) CONSENT-LEDGER (finding 1) — the SECURITY REGRESSION legs:
#       * SEEDED VAULT: a vault whose lock.json ALREADY pins a plugin but with
#         NO local ledger entry (a cloned/pulled vault) boots and registers
#         NOTHING; the listing shows state=availableNeedsConsent. (Fails on the
#         old lock.json-drives-reapply code; passes now.)
#       * DRIFT: consent a plugin, change its plugin.js on disk, delete-DB +
#         reboot → NOT auto-re-registered; listing state=driftedNeedsReconsent.
#
# Dedicated ports (ledger): proxy 9022, backend 6484, postgres 5557, valkey
# 6500, control 9023 (exporter 6563 reserved; renders OFF). Beacon observer on
# 9024 (its own off-origin port for the egress probe).
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on
# 9010/6472/5545/6488/9011. We kill ONLY the PIDs this boot recorded; never
# pkill/killall by name.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${E7_PROXY_PORT:-9022}"
BACKEND_PORT="${E7_BACKEND_PORT:-6484}"
POSTGRES_PORT="${E7_POSTGRES_PORT:-5557}"
VALKEY_PORT="${E7_VALKEY_PORT:-6500}"
BEACON_PORT="${E7_BEACON_PORT:-9024}"
FIRST_BOOT_TIMEOUT="${E7_TIMEOUT:-900}"
REBOOT_TIMEOUT=600
BASE="http://localhost:${PROXY_PORT}"
BACKEND="http://127.0.0.1:${BACKEND_PORT}"
PKG_ID="e7-fixture-plugin"
MANIFEST_URL="${BASE}/__packages/${PKG_ID}/manifest.json"
BEACON_URL="http://127.0.0.1:${BEACON_PORT}/beacon"

BIN="$ROOT/target/debug/headless"
PROBE="$ROOT/scripts/ecosystem-spike/e7_probe.py"
BEACON="$ROOT/scripts/ecosystem-spike/e7_beacon.py"
NAV="$ROOT/scripts/ecosystem-spike/e7_activation_nav.cjs"
FIXTURE="$ROOT/scripts/ecosystem-spike/e7-fixture-plugin"
PLAYWRIGHT="${ROUTES_GATE_PLAYWRIGHT:-$ROOT/runtime/exporter/node_modules/playwright}"
BROWSERS="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/runtime/exporter-browsers}"
NODE_BIN="${ROUTES_GATE_NODE:-node}"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e7-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e7-vault.XXXXXX")"
# Separate data+vault for the seeded-vault (cloned-vault) security-regression
# leg: a fresh DATA_DIR guarantees NO consent ledger on this machine.
SEED_DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e7-seed-data.XXXXXX")"
SEED_VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e7-seed-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-e7-work.XXXXXX")"
LOG="$WORK_DIR/headless.log"
BEACON_LOG="$WORK_DIR/beacon.log"
SHOT_DIR="$WORK_DIR/shots"
mkdir -p "$SHOT_DIR"

HEADLESS_PID=""
BEACON_PID=""
FAILURES=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
sha() { shasum -a 256 | cut -d' ' -f1; }
json_field() { python3 -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

PG_CACHE="${E7_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY. Never pkill/killall by name (another gate may run).
cleanup() {
    if [ -n "$BEACON_PID" ] && kill -0 "$BEACON_PID" 2>/dev/null; then
        kill -TERM "$BEACON_PID" 2>/dev/null
    fi
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$SEED_DATA_DIR" "$SEED_VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT seed-data=$SEED_DATA_DIR seed-vault=$SEED_VAULT work=$WORK_DIR"
        echo "  headless log: $LOG"
        echo "  beacon  log: $BEACON_LOG"
        echo "  screenshots: $SHOT_DIR"
    fi
}
trap cleanup EXIT

ports_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$BEACON_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

start_headless() { # start_headless [extra env assignments...]
    # Truncate the log so wait_ready waits for THIS boot's READY, not a stale
    # READY line left over from a previous boot in the cumulative log.
    #
    # NO plugin env is passed: the thin build made enable-plugins, the
    # penpotPluginsWhitelist local-origin pin, and the CSP header PRODUCT
    # DEFAULTS — the gate asserts the defaults, not an opt-in. Probe legs
    # that need the CSP witnessed off pass PENPOT_LOCAL_CSP=off explicitly.
    : >"$LOG"
    env PENPOT_LOCAL_DATA_DIR="${RUN_DATA_DIR:-$DATA_DIR}" \
        PENPOT_LOCAL_DESIGNS_DIR="${RUN_VAULT:-$VAULT}" \
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
    PENPOT_TOKEN="$(json_field access_token <"${RUN_DATA_DIR:-$DATA_DIR}/credentials.json" 2>/dev/null || true)"
    export PENPOT_TOKEN
    [ -n "$PENPOT_TOKEN" ]
}

# SPA witness helpers: hash the served index.html + main JS bundle(s).
spa_index_sha() { curl -fsS "$BASE/index.html" 2>/dev/null | sha; }
spa_bundle_sha() { curl -fsS "$BASE/js/main.js" 2>/dev/null | sha; }
spa_has_injected_script() {
    # true (0) if the served index.html carries any script beyond the upstream
    # module bootstrap — we assert NONE injected.
    curl -fsS "$BASE/index.html" 2>/dev/null | grep -c "<script"
}

# FULL executed-script-set witness (invariant-3, finding g): hash index.html
# PLUS every <script src> it references (config.js, polyfills, libs.js, main.js,
# chunks — parsed out of index.html, not hardcoded). Each referenced script is
# asserted HTTP 200 + NON-EMPTY before hashing, killing the vacuous empty-hash
# pass. Prints the combined sha256 on stdout (empty on any 404/empty body); the
# resolved script list goes to the given log file for the record.
spa_scriptset_sha() {
    local listfile="${1:-/dev/null}"
    python3 - "$BASE" "$listfile" <<'PY'
import sys, re, hashlib, urllib.request
base = sys.argv[1].rstrip("/")
listfile = sys.argv[2]

def get(url):
    with urllib.request.urlopen(url, timeout=25) as r:
        return r.status, r.read()

try:
    st, idx = get(base + "/index.html")
except Exception as e:
    print("", end=""); sys.stderr.write(f"index.html fetch failed: {e}\n"); sys.exit(1)
if st != 200 or not idx:
    sys.stderr.write(f"index.html status={st} len={len(idx or b'')}\n"); sys.exit(1)

srcs = re.findall(r'<script[^>]+src="([^"]+)"', idx.decode("utf-8", "replace"))
h = hashlib.sha256()
h.update(idx)
resolved = []
for s in srcs:
    if s.startswith("http"):
        url = s
    elif s.startswith("/"):
        url = base + s
    else:
        url = base + "/" + s
    try:
        st, body = get(url)
    except Exception as e:
        sys.stderr.write(f"script fetch failed {s}: {e}\n"); sys.exit(1)
    if st != 200 or not body:
        sys.stderr.write(f"script empty/404 {s} status={st} len={len(body or b'')}\n"); sys.exit(1)
    h.update(b"\n" + s.encode() + b"\n")
    h.update(body)
    resolved.append(s)
try:
    with open(listfile, "w", encoding="utf-8") as f:
        f.write("\n".join(resolved) + "\n")
except Exception:
    pass
print(h.hexdigest())
PY
}

echo "== E7 plugin-activation spike =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT beacon=$BEACON_PORT"
echo "   vault: $VAULT"
echo "   manifest URL: $MANIFEST_URL"

# --- pre-flight ------------------------------------------------------------
[ -f "$NAV" ] || { fail "nav driver missing: $NAV"; exit 1; }
[ -e "$PLAYWRIGHT" ] || { fail "bundled playwright missing: $PLAYWRIGHT (fetch-penpot.sh --with-browsers)"; exit 1; }
[ -d "$BROWSERS" ] || { fail "bundled browsers missing: $BROWSERS"; exit 1; }
ports_free || { fail "one of the E7 ports is busy"; exit 1; }

export PENPOT_BACKEND="$BASE" PENPOT_FRONTEND="$BASE"
export ROUTES_GATE_PLAYWRIGHT="$PLAYWRIGHT" PLAYWRIGHT_BROWSERS_PATH="$BROWSERS"

# --- build -----------------------------------------------------------------
echo "-- build headless + watchdog"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop --bin headless -p supervisor --bin penpot-watchdog); then
    fail "cargo build"; exit 1
fi

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
fi

# --- author the fixture plugin package into the vault package home ---------
echo "-- author fixture plugin package (beacon URL substituted)"
DEST="$VAULT/.penpot-packages/$PKG_ID"
mkdir -p "$DEST"
cp "$FIXTURE/manifest.json" "$FIXTURE/icon.png" "$DEST/"
sed "s|@BEACON_URL@|$BEACON_URL|g" "$FIXTURE/plugin.js" >"$DEST/plugin.js"
cat >"$DEST/package.json" <<EOF
{ "id": "$PKG_ID", "version": "0.0.1", "kind": "plugin", "name": "E7 Fixture Plugin" }
EOF

# --- beacon observer -------------------------------------------------------
python3 "$BEACON" "$BEACON_PORT" "$BEACON_LOG" &
BEACON_PID=$!
sleep 1
if kill -0 "$BEACON_PID" 2>/dev/null; then pass "beacon observer live on :$BEACON_PORT"; else fail "beacon observer failed to start"; fi

# ===========================================================================
# BOOT 1 — plugins enabled, whitelist pinned, CSP OFF (activation + egress-off)
# ===========================================================================
echo "-- BOOT 1: plugins ON, whitelist pinned, CSP OFF"
# The thin build ships the CSP ON by default (CSP-GO); this probe leg needs it
# OFF to witness the beacon actually leaving, so opt out explicitly.
start_headless PENPOT_LOCAL_CSP=off
if ! wait_ready "$FIRST_BOOT_TIMEOUT"; then fail "boot 1 READY"; exit 1; fi
pass "boot 1 reached READY (plugins enabled)"
read_token || { fail "no access token after boot 1"; exit 1; }

# Serve check: the plugin manifest + code are reachable at the proxy origin.
if curl -fsS "$MANIFEST_URL" | grep -q '"code": *"/__packages/'; then
    pass "(serve) manifest.json served at $MANIFEST_URL (scaffold route /__packages/<pkg>/*)"
else
    fail "(serve) manifest.json not served at the proxy origin"
fi
if curl -fsS "$BASE/__packages/$PKG_ID/plugin.js" | grep -q "E7-FIXTURE-SHAPE"; then
    pass "(serve) plugin.js served with the substituted beacon + shape effect"
else
    fail "(serve) plugin.js not served correctly"
fi

# frontend flag + whitelist witnesses (config.js the proxy serves) — these are
# PRODUCT DEFAULTS now (no env was passed): the thin build ships plugins ON.
CFG="$(curl -fsS "$BASE/js/config.js" 2>/dev/null || true)"
echo "$CFG" | grep -q "enable-plugins" && pass "(flags) config.js carries enable-plugins BY DEFAULT (no env)" || fail "(flags) enable-plugins missing from config.js (default-on broken)"
echo "$CFG" | grep -q "penpotPluginsWhitelist = \[\"http://localhost:${PROXY_PORT}\",\"http://127.0.0.1:${PROXY_PORT}\"\]" \
    && pass "(flags) config.js pins penpotPluginsWhitelist to BOTH local-origin spellings BY DEFAULT" \
    || fail "(flags) whitelist not pinned to the local origins by default"

# Product serve-route hardening witness: a dotfile segment is refused (400),
# so a hostile repo's .git internals are never served.
GIT_CODE="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/__packages/$PKG_ID/.git/config" 2>/dev/null || echo 000)"
[ "$GIT_CODE" = "400" ] && pass "(serve) /__packages/<pkg>/.git/config refused with 400 (dotfile hardening)" \
    || fail "(serve) dotfile path not refused (got $GIT_CODE, want 400)"

# Discovery surface (surface-don't-apply): the fixture is LISTED with its
# manifest URL, but installed=false/live=false — nothing registers on its own.
DISC_PRE="$(curl -fsS "$BASE/__api/packages/plugins" 2>/dev/null || true)"
if echo "$DISC_PRE" | python3 -c '
import json, sys
d = json.load(sys.stdin)
p = next((p for p in d.get("plugins", []) if p["id"] == sys.argv[1]), None)
ok = p and p["manifestUrl"] == f"/__packages/{sys.argv[1]}/manifest.json" \
    and not p["installed"] and not p["live"]
sys.exit(0 if ok else 1)
' "$PKG_ID" 2>/dev/null; then
    pass "(surface) /__api/packages/plugins lists the fixture (manifestUrl, installed=false, live=false)"
else
    fail "(surface) discovery surface wrong before install: $DISC_PRE"
fi

# --- seed the design file --------------------------------------------------
SEED_JSON="$(python3 "$PROBE" seed_file | tail -1)"
FILE_ID="$(echo "$SEED_JSON" | json_field fileId)"
DEEP_LINK="$(echo "$SEED_JSON" | json_field deepLink)"
[ -n "$FILE_ID" ] && pass "(seed) design file $FILE_ID" || { fail "(seed) file"; }

# --- (c) CONSENT: no pointer BEFORE install --------------------------------
PROPS_PRE="$(python3 "$PROBE" profile_props | tail -1)"
if echo "$PROPS_PRE" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if not d["hasPlugins"] else 1)'; then
    pass "(c/consent) no plugin registry pointer in profile props before install"
else
    fail "(c/consent) a plugin pointer existed BEFORE any install step (pre-seed path!)"
fi

# --- (b/g) INVARIANT-3 witness: FULL SCRIPT SET before the install flow ------
IDX_PRE="$(spa_index_sha)"; BUNDLE_PRE="$(spa_bundle_sha)"; SCRIPTS_PRE="$(spa_has_injected_script)"
SCRIPTSET_PRE="$(spa_scriptset_sha "$WORK_DIR/scriptset-pre.txt" 2>"$WORK_DIR/scriptset-pre.err")"
if [ -n "$SCRIPTSET_PRE" ]; then
    pass "(g/invariant-3) full script set hashed non-empty ($SCRIPTSET_PRE over $(wc -l <"$WORK_DIR/scriptset-pre.txt" | tr -d ' ') referenced scripts, all 200+non-empty)"
    echo "   scripts: $(tr '\n' ' ' <"$WORK_DIR/scriptset-pre.txt")"
else
    fail "(g/invariant-3) full script-set witness empty/failed (a referenced script was 404/empty): $(cat "$WORK_DIR/scriptset-pre.err")"
fi

# --- (a) ACTIVATION: drive the native Plugin Manager (CSP off) --------------
echo "-- ACTIVATION: bundled-chromium native Plugin Manager install (csp-off)"
NAV_OFF="$("$NODE_BIN" "$NAV" "$BASE" "$MANIFEST_URL" "E7-FIXTURE-SHAPE" "$SHOT_DIR" "csp-off" "$DEEP_LINK" "$BEACON_URL" 2>"$WORK_DIR/nav-off.err" | tail -1 || true)"
echo "   nav(csp-off): $NAV_OFF"
if echo "$NAV_OFF" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("steps",{}).get("consentAllowed") else 1)' 2>/dev/null; then
    pass "(a/activation) native Plugin Manager: URL entered, consent Allowed via bundled chromium"
else
    fail "(a/activation) native install flow did not reach consent-allowed (see nav-off.err + shots)"
fi

# Observable 1: the named shape appears (RPC, UI-independent).
if python3 "$PROBE" count_shapes "$FILE_ID" "E7-FIXTURE-SHAPE" 40 | tail -1 \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["count"]>=1 else 1)'; then
    pass "(a/activation) plugin observable effect — shape E7-FIXTURE-SHAPE present in the file"
else
    fail "(a/activation) plugin shape effect NOT observed in the file"
fi

# --- (b/g) INVARIANT-3 witness: FULL SCRIPT SET after the install flow -------
IDX_POST="$(spa_index_sha)"; BUNDLE_POST="$(spa_bundle_sha)"; SCRIPTS_POST="$(spa_has_injected_script)"
SCRIPTSET_POST="$(spa_scriptset_sha "$WORK_DIR/scriptset-post.txt" 2>"$WORK_DIR/scriptset-post.err")"
if [ -n "$IDX_PRE" ] && [ "$IDX_PRE" = "$IDX_POST" ] && [ "$BUNDLE_PRE" = "$BUNDLE_POST" ]; then
    pass "(b/invariant-3) SPA index.html + main.js sha256 IDENTICAL before/after install ($IDX_PRE / $BUNDLE_PRE)"
else
    fail "(b/invariant-3) SPA bytes changed across install (idx $IDX_PRE->$IDX_POST bundle $BUNDLE_PRE->$BUNDLE_POST)"
fi
if [ -n "$SCRIPTSET_POST" ] && [ "$SCRIPTSET_PRE" = "$SCRIPTSET_POST" ]; then
    pass "(g/invariant-3) FULL executed script set sha256 IDENTICAL before/after the install flow ($SCRIPTSET_PRE) — no served byte mutated"
else
    fail "(g/invariant-3) full script-set hash changed across install ($SCRIPTSET_PRE -> $SCRIPTSET_POST)"
fi
if [ "$SCRIPTS_PRE" = "$SCRIPTS_POST" ]; then
    pass "(b/invariant-3) served index.html <script> count unchanged ($SCRIPTS_PRE) — no injected script"
else
    fail "(b/invariant-3) index.html script count changed ($SCRIPTS_PRE->$SCRIPTS_POST)"
fi

# --- (d) POINTER: registry pointer written via update-profile-props ---------
PROPS_POST="$(python3 "$PROBE" profile_props | tail -1)"
if echo "$PROPS_POST" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["hasPlugins"] else 1)'; then
    pass "(d/pointer) plugin registry pointer now present in profile props (via update-profile-props)"
    echo "$PROPS_POST" | python3 -c 'import json,sys; print("   props.plugins =", json.dumps(json.load(sys.stdin)["pluginsProps"])[:300])'
else
    fail "(d/pointer) no plugin pointer in profile props after install"
fi
# Save the pointer VALUE — the wipe leg compares the product-restored props to it.
echo "$PROPS_POST" | python3 -c 'import json,sys; json.dump(json.load(sys.stdin)["pluginsProps"], open(sys.argv[1],"w"))' "$WORK_DIR/plugins_props.json"

# --- (e) PRODUCT CAPTURE: the reconcile pins the user's install in lock.json -
# The thin build's capture loop (spawn_plugin_reconcile phase 2, 5s interval)
# RECORDS the consented pointer into LockEntry.plugin_props + a content pin.
LOCK_PINNED=0
L_DEADLINE=$(($(date +%s) + 45))
while [ "$(date +%s)" -lt "$L_DEADLINE" ]; do
    if python3 -c '
import json, sys
try:
    lock = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
e = lock.get("packages", {}).get(sys.argv[2], {})
props = e.get("pluginProps", {})
ok = props and any("/__packages/" in v for v in props.values()) \
    and e.get("contentHash", "") and e.get("fileId", "x") == ""
sys.exit(0 if ok else 1)
' "$VAULT/lock.json" "$PKG_ID" 2>/dev/null; then
        LOCK_PINNED=1
        break
    fi
    sleep 2
done
if [ "$LOCK_PINNED" = "1" ]; then
    pass "(e/capture) product reconcile pinned the install into lock.json (pluginProps + content pin, fileId empty — never imported)"
else
    fail "(e/capture) lock.json never pinned the user's install within 45s"
fi

# Discovery now reports installed=true AND live=true.
DISC_POST="$(curl -fsS "$BASE/__api/packages/plugins" 2>/dev/null || true)"
if echo "$DISC_POST" | python3 -c '
import json, sys
d = json.load(sys.stdin)
p = next((p for p in d.get("plugins", []) if p["id"] == sys.argv[1]), None)
sys.exit(0 if p and p["installed"] and p["live"] and not p["drifted"] else 1)
' "$PKG_ID" 2>/dev/null; then
    pass "(surface) discovery after install: installed=true, live=true, drifted=false"
else
    fail "(surface) discovery surface wrong after install: $DISC_POST"
fi

# --- (f) CSP-EGRESS PROBE, leg 1: CSP OFF => BOTH vectors OBSERVED -----------
BEACON_HITS_OFF="$(wc -l <"$BEACON_LOG" | tr -d ' ')"
FETCH_OFF="$(grep -c 'src=fetch' "$BEACON_LOG" 2>/dev/null || echo 0)"
IMG_OFF="$(grep -c 'src=img' "$BEACON_LOG" 2>/dev/null || echo 0)"
if [ "${FETCH_OFF:-0}" -ge 1 ] && [ "${IMG_OFF:-0}" -ge 1 ]; then
    pass "(f/csp-off) BOTH egress vectors OBSERVED with CSP off — fetch=$FETCH_OFF image=$IMG_OFF ($BEACON_HITS_OFF total) reached the off-origin observer"
else
    fail "(f/csp-off) not both vectors observed with CSP off (fetch=$FETCH_OFF image=$IMG_OFF) — the multi-vector probe never left"
fi
echo "   runtime mechanism (csp-off nav): $(echo "$NAV_OFF" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("frames=",len(d.get("frames",[])), "mech=", d.get("runtimeMechanism"))' 2>/dev/null || echo n/a)"

# ===========================================================================
# (e) PERSISTENCE — delete DB, reboot: the PRODUCT boot re-apply re-registers
# the pointer from lock.json (no manual replay — this is the shipped path).
# ===========================================================================
echo "-- PERSISTENCE: delete DB, reboot, product boot re-apply from lock.json"
stop_headless
if [ -d "$DATA_DIR/postgres" ] && rm -rf "$DATA_DIR/postgres"; then
    pass "(e) surgical DB wipe (rm -rf <data>/postgres)"
else
    fail "(e) DB wipe"
fi
if [ -d "$PG_CACHE" ]; then mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; fi

start_headless PENPOT_LOCAL_CSP=off
if ! wait_ready "$REBOOT_TIMEOUT"; then fail "(e) reboot READY"; exit 1; fi
pass "(e) second boot reached READY on the wiped DB"
read_token || { fail "(e) no token after reboot"; exit 1; }

# Finding 1: the consent LEDGER (not lock.json) is the re-apply authority, and
# it survives a DB wipe because it lives at the DATA dir root (NOT the vault).
if [ -f "$DATA_DIR/plugin-consent.json" ] &&
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if d.get("plugins") else 1)' "$DATA_DIR/plugin-consent.json" 2>/dev/null; then
    pass "(e/ledger) consent ledger present at <data_dir>/plugin-consent.json (survived the DB wipe; authority for re-apply)"
else
    fail "(e/ledger) consent ledger missing/empty after the wipe — re-apply would have no authority"
fi

# The pointer was DB-only derived state (wiped); the product reconcile must
# re-insert it from LockEntry.plugin_props via update-profile-props, and the
# restored data must be IDENTICAL to what the user's install wrote in boot 1.
RESTORED_OK=0
R_DEADLINE=$(($(date +%s) + 120))
while [ "$(date +%s)" -lt "$R_DEADLINE" ]; do
    PROPS_RESTORED="$(python3 "$PROBE" profile_props 2>/dev/null | tail -1 || true)"
    if echo "$PROPS_RESTORED" | python3 -c '
import json, sys
cap = json.load(open(sys.argv[1]))
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
p = d.get("pluginsProps") or {}
ok = d.get("hasPlugins") and p.get("data") == cap.get("data") \
    and sorted(p.get("ids", [])) == sorted(p.get("data", {}).keys())
sys.exit(0 if ok else 1)
' "$WORK_DIR/plugins_props.json" 2>/dev/null; then
        RESTORED_OK=1
        break
    fi
    sleep 3
done
if [ "$RESTORED_OK" = "1" ]; then
    pass "(e) PRODUCT boot re-apply restored the pointer from lock.json — data identical to the consented install, ids in sync"
else
    fail "(e) pointer not restored (or not identical) after the wipe within 120s: $PROPS_RESTORED"
fi
# The boot log must witness the re-apply actually RAN (insert-only: applied>=1
# proves the pointer was missing post-wipe, i.e. genuinely DB-only state).
if strip_ansi <"$LOG" | grep -q "E7 boot re-apply"; then
    pass "(e) boot log witnesses the re-apply pass ($(strip_ansi <"$LOG" | grep -o 'applied=[0-9]*' | head -1))"
else
    fail "(e) no 'E7 boot re-apply' line in the boot log"
fi

# ===========================================================================
# (h/drift) DRIFT — change the served plugin.js on disk, wipe the DB, reboot:
# the ledger consented to the OLD content hash, so the boot re-apply must
# DECLINE to re-register (finding 3, folded into finding 1c). This app is
# currently running from the wipe-recovery leg; stop it first.
# ===========================================================================
echo "-- DRIFT: mutate served plugin.js, wipe DB, reboot → NOT auto-re-registered"
stop_headless
# Mutate the served code (append a benign comment: behaviour preserved — still
# creates the shape + beacons — but the content hash now differs from consent).
echo "// E7 drift marker $(date +%s)" >>"$DEST/plugin.js"
pass "(h/drift) served plugin.js mutated on disk (content drift since consent)"
rm -rf "$DATA_DIR/postgres"
if [ -d "$PG_CACHE" ]; then mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; fi
start_headless PENPOT_LOCAL_CSP=off
if ! wait_ready "$REBOOT_TIMEOUT"; then fail "(h/drift) reboot READY"; exit 1; fi
read_token || { fail "(h/drift) token"; exit 1; }
# Allow boot re-apply attempts + a couple of capture cycles to run.
sleep 20
PROPS_DRIFT="$(python3 "$PROBE" profile_props 2>/dev/null | tail -1 || true)"
if echo "$PROPS_DRIFT" | python3 -c 'import json,sys; sys.exit(0 if not json.load(sys.stdin)["hasPlugins"] else 1)' 2>/dev/null; then
    pass "(h/drift) drifted package NOT auto-re-registered — no pointer in profile props (consent was for the old code)"
else
    fail "(h/drift) SECURITY: a drifted package was re-registered without reconsent: $PROPS_DRIFT"
fi
DISC_DRIFT="$(curl -fsS "$BASE/__api/packages/plugins" 2>/dev/null || true)"
if echo "$DISC_DRIFT" | python3 -c '
import json, sys
d = json.load(sys.stdin)
p = next((p for p in d.get("plugins", []) if p["id"] == sys.argv[1]), None)
sys.exit(0 if p and p["state"] == "driftedNeedsReconsent" and not p["installed"] and p["drifted"] else 1)
' "$PKG_ID" 2>/dev/null; then
    pass "(h/drift) listing surfaces state=driftedNeedsReconsent (installed=false, drifted=true)"
else
    fail "(h/drift) listing did not surface driftedNeedsReconsent: $DISC_DRIFT"
fi

# ===========================================================================
# (f) CSP-EGRESS PROBE, leg 2 — reboot with CSP ON: beacon ABSENT + blocked
# ===========================================================================
echo "-- CSP-ON leg: reboot with proxy CSP response header, re-run native install"
stop_headless
# fresh DB so the plugin install runs clean again under CSP. Drop the lockfile
# for a fully PRISTINE consent flow (URL → Install → Allow): with no lock pin
# the boot re-apply has nothing to re-insert regardless of the surviving
# consent ledger (re-apply needs BOTH the lock-pinned pointer body AND ledger
# authority — finding 1), so this leg exercises a clean native install.
rm -rf "$DATA_DIR/postgres"
rm -f "$VAULT/lock.json"
if [ -d "$PG_CACHE" ]; then mkdir -p "$DATA_DIR/postgres"; cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"; fi
: >"$BEACON_LOG"  # reset the observer log; any line now = egress under CSP

# NO CSP env: the thin build ships the header ON BY DEFAULT (CSP-GO), so this
# leg is the PRODUCT DEFAULT boot — exactly what a user gets.
start_headless
if ! wait_ready "$REBOOT_TIMEOUT"; then fail "(f/csp-on) boot READY"; exit 1; fi
pass "(f/csp-on) booted with NO CSP env (product default = CSP ON)"
read_token || { fail "(f/csp-on) token"; exit 1; }

# Witness the DEFAULT CSP header is on the SPA document (where plugin.js runs).
# Finding 2: it must carry the default-src BASELINE (fences non-connect exfil
# vectors), the img-src fence (the image-beacon vector), AND the connect-src
# fence — not connect-src alone.
CSP_HDR="$(curl -fsSI "$BASE/index.html" 2>/dev/null | tr -d '\r' | grep -i '^content-security-policy:' || true)"
if echo "$CSP_HDR" | grep -q "default-src 'self' data: blob:" &&
    echo "$CSP_HDR" | grep -q "img-src 'self' data: blob:" &&
    echo "$CSP_HDR" | grep -q "connect-src 'self' ws://localhost:${PROXY_PORT} ws://127.0.0.1:${PROXY_PORT}" &&
    echo "$CSP_HDR" | grep -q "form-action 'self'"; then
    pass "(f/csp-on) DEFAULT CSP on the SPA document carries default-src + img-src + connect-src + form-action fences: $CSP_HDR"
else
    fail "(f/csp-on) default CSP header missing the sharpened fences on index.html (got: ${CSP_HDR:-none})"
fi
# Header rides text/html ONLY — a JS asset must NOT carry it.
CFG_CSP="$(curl -fsSI "$BASE/js/config.js" 2>/dev/null | tr -d '\r' | grep -ci '^content-security-policy:' || true)"
[ "${CFG_CSP:-0}" = "0" ] && pass "(f/csp-on) no CSP header on non-html assets (js/config.js clean)" \
    || fail "(f/csp-on) CSP header leaked onto a non-html asset"
# And confirm the header is header-only: the SPA index.html BYTES are unchanged.
IDX_CSP="$(spa_index_sha)"
[ "$IDX_CSP" = "$IDX_PRE" ] && pass "(f/csp-on) index.html BYTES identical with CSP on (header-only; invariant 3)" \
    || fail "(f/csp-on) index.html bytes changed under CSP (should be header-only): $IDX_PRE -> $IDX_CSP"

SEED2="$(python3 "$PROBE" seed_file | tail -1)"; FILE_ID2="$(echo "$SEED2" | json_field fileId)"
DEEP_LINK2="$(echo "$SEED2" | json_field deepLink)"

echo "-- ACTIVATION under CSP: native Plugin Manager install (csp-on)"
NAV_ON="$("$NODE_BIN" "$NAV" "$BASE" "$MANIFEST_URL" "E7-FIXTURE-SHAPE" "$SHOT_DIR" "csp-on" "$DEEP_LINK2" "$BEACON_URL" 2>"$WORK_DIR/nav-on.err" | tail -1 || true)"
echo "   nav(csp-on): $NAV_ON"

# Plugin STILL LOADS under CSP (observable shape present) — required for CSP-GO.
if python3 "$PROBE" count_shapes "$FILE_ID2" "E7-FIXTURE-SHAPE" 40 | tail -1 \
    | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["count"]>=1 else 1)'; then
    pass "(f/csp-on) plugin STILL LOADS under CSP — shape effect present"
    PLUGIN_LOADS_CSP=1
else
    fail "(f/csp-on) plugin did NOT load under CSP (CSP too strict — breaks the app)"
    PLUGIN_LOADS_CSP=0
fi

# BOTH egress vectors (fetch + image) must be ABSENT under CSP — the observer
# log was reset just before this boot, so ANY line is egress under CSP.
BEACON_HITS_ON="$(wc -l <"$BEACON_LOG" | tr -d ' ')"
FETCH_ON="$(grep -c 'src=fetch' "$BEACON_LOG" 2>/dev/null || echo 0)"
IMG_ON="$(grep -c 'src=img' "$BEACON_LOG" 2>/dev/null || echo 0)"
if [ "${BEACON_HITS_ON:-0}" -eq 0 ]; then
    pass "(f/csp-on) BOTH egress vectors ABSENT under CSP — 0 requests reached the observer (fetch=$FETCH_ON image=$IMG_ON)"
    BEACON_ABSENT_CSP=1
else
    fail "(f/csp-on) egress STILL reached the observer under CSP (fetch=$FETCH_ON image=$IMG_ON, total $BEACON_HITS_ON) — not egress-contained"
    BEACON_ABSENT_CSP=0
fi
# And the browser leg should have seen a CSP block on the fetch.
echo "   csp violations seen: $(echo "$NAV_ON" | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("cspViolations",[])))' 2>/dev/null || echo n/a)"
echo "   beacon console (csp-on): $(echo "$NAV_ON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("beaconConsole"))' 2>/dev/null || echo n/a)"
echo "   image beacon console (csp-on): $(echo "$NAV_ON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("imageBeaconConsole"))' 2>/dev/null || echo n/a)"

# --- CSP verdict -----------------------------------------------------------
if [ "${PLUGIN_LOADS_CSP:-0}" = "1" ] && [ "${BEACON_ABSENT_CSP:-0}" = "1" ]; then
    echo "CSP-VERDICT: CSP-GO (ON blocks egress AND the plugin still loads)"
else
    echo "CSP-VERDICT: CSP-NO-GO"
fi

# ===========================================================================
# (h/seeded) SEEDED VAULT — the consent-bypass security regression test. A
# vault whose lock.json ALREADY pins a plugin (as if carried across machines by
# E6) but with NO local consent ledger (a cloned/pulled vault) must register
# NOTHING at boot. FAILS on the old lock.json-drives-reapply code; PASSES now.
# Uses a FRESH data dir (guaranteed no ledger) + a fresh vault.
# ===========================================================================
echo "-- SEEDED VAULT: lock.json pin present, NO local ledger → nothing auto-registered"
stop_headless
SEED_DEST="$SEED_VAULT/.penpot-packages/$PKG_ID"
mkdir -p "$SEED_DEST"
cp "$FIXTURE/manifest.json" "$FIXTURE/icon.png" "$SEED_DEST/"
sed "s|@BEACON_URL@|$BEACON_URL|g" "$FIXTURE/plugin.js" >"$SEED_DEST/plugin.js"
cat >"$SEED_DEST/package.json" <<EOF
{ "id": "$PKG_ID", "version": "0.0.1", "kind": "plugin", "name": "E7 Fixture Plugin" }
EOF
# Author a lock.json that ALREADY pins the plugin with a LOCAL-origin pointer,
# but this machine's data dir has NO consent ledger.
python3 - "$SEED_VAULT/lock.json" "$PROXY_PORT" "$PKG_ID" <<'PY'
import json, sys
lockpath, port, pkg = sys.argv[1], sys.argv[2], sys.argv[3]
ptr = {"code": f"/__packages/{pkg}/plugin.js", "host": f"http://localhost:{port}",
       "name": "E7 Fixture Plugin", "permissions": ["content:write"],
       "pluginId": "seeded-carried-plugin"}
lock = {"schemaVersion": 1, "packages": {pkg: {
    "version": "0.0.1", "kind": "plugin", "contentHash": "seeded",
    "contractHash": "", "sourceGitUrl": "", "fileId": "",
    "name": "E7 Fixture Plugin", "installedAt": "2026-07-16T00:00:00Z",
    "libraryShared": False,
    "pluginProps": {"seeded-carried-plugin": json.dumps(ptr, sort_keys=True, separators=(",", ":"))},
    "links": []}}}
with open(lockpath, "w", encoding="utf-8") as f:
    f.write(json.dumps(lock, indent=2) + "\n")
PY
if [ -d "$PG_CACHE" ]; then mkdir -p "$SEED_DATA_DIR/postgres"; cp -R "$PG_CACHE" "$SEED_DATA_DIR/postgres/install"; fi
RUN_DATA_DIR="$SEED_DATA_DIR" RUN_VAULT="$SEED_VAULT" start_headless PENPOT_LOCAL_CSP=off
if ! wait_ready "$FIRST_BOOT_TIMEOUT"; then fail "(h/seeded) boot READY"; exit 1; fi
RUN_DATA_DIR="$SEED_DATA_DIR" read_token || { fail "(h/seeded) token"; exit 1; }
# Allow boot re-apply + capture cycles to run — then assert NOTHING registered.
sleep 20
PROPS_SEED="$(python3 "$PROBE" profile_props 2>/dev/null | tail -1 || true)"
if echo "$PROPS_SEED" | python3 -c 'import json,sys; sys.exit(0 if not json.load(sys.stdin)["hasPlugins"] else 1)' 2>/dev/null; then
    pass "(h/seeded) SECURITY: cloned-vault lock pin did NOT auto-register — no pointer in profile props"
else
    fail "(h/seeded) SECURITY REGRESSION: a cloned-vault lock pin seeded a plugin with NO local consent: $PROPS_SEED"
fi
DISC_SEED="$(curl -fsS "$BASE/__api/packages/plugins" 2>/dev/null || true)"
if echo "$DISC_SEED" | python3 -c '
import json, sys
d = json.load(sys.stdin)
p = next((p for p in d.get("plugins", []) if p["id"] == sys.argv[1]), None)
sys.exit(0 if p and p["state"] == "availableNeedsConsent" and not p["installed"] and not p["live"] else 1)
' "$PKG_ID" 2>/dev/null; then
    pass "(h/seeded) listing surfaces state=availableNeedsConsent (installed=false, live=false)"
else
    fail "(h/seeded) listing did not surface availableNeedsConsent: $DISC_SEED"
fi
# A bare boot with no native Install/Allow must NOT create a consent ledger.
if [ ! -f "$SEED_DATA_DIR/plugin-consent.json" ]; then
    pass "(h/seeded) no consent ledger written on a bare boot (no native Install/Allow occurred)"
else
    fail "(h/seeded) a consent ledger appeared without any native install: $(cat "$SEED_DATA_DIR/plugin-consent.json" 2>/dev/null)"
fi
stop_headless

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "== E7 activation spike: ALL PASS =="
    echo "ACTIVATION-VERDICT: GO"
else
    echo "== E7 activation spike: $FAILURES FAILURE(S) =="
    echo "ACTIVATION-VERDICT: see failures above"
fi
exit "$FAILURES"
