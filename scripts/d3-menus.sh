#!/usr/bin/env bash
# D3 menu-bar gate (PLAN4 milestone D3, `just d3`, chained into `just e2e` —
# D3 shipped a native menu bar + made the app window-per-file).
#
# THE CENTRAL PROBLEM: a native menu cannot be clicked in CI. So instead of
# driving clicks, this gate asserts three things that together make "every
# menu item does something real and nothing is dead" provable without one:
#
#   (a) THE MENU MODEL'S SHAPE. All the branching (which items exist, which
#       are enabled, which carry a command) lives in the pure, Tauri-free
#       `menubar/model.rs` (mirrors the tray/model.rs precedent) — so it is
#       unit-tested, and this gate requires the SPECIFIC test names that pin
#       the contract, not merely "the suite passed" (a renamed or weakened
#       test would otherwise leave this green). Run via
#       `cargo test -p penpot-desktop -- menubar:: windows:: recent:: reveal::`
#       — cargo only accepts one positional TESTNAME, so the multi-module
#       filter goes after `--`, straight to the test harness (libtest ORs
#       multiple filters). Every name below is verified present in that
#       output (`grep`ped from the real run, not assumed) AND asserted `ok`;
#       the leg also fails on the overall `cargo test` exit code, so a
#       failure in a module test NOT individually pinned still fails the
#       gate rather than being silently absorbed by the named checks alone.
#
#   (b) EVERY COMMAND BEHIND THE MENU ACTUALLY WORKS, HEADLESSLY, against a
#       live stack — or is explicitly, individually marked NOT COVERED with
#       a reason (a silent gap here is the "no dead menu items" rule quietly
#       going unenforced):
#         * New File / New Project: POST /__api/vault/manage/{file,project}
#           — `menubar/mod.rs`'s own doc comments say these are "the SAME
#           RPC" the menu's create_new_file/create_new_project call, so
#           driving the manage route exercises the identical backend call.
#           Verified on disk too (scripts/d3_menus_helper.py polls the
#           manifest + folder tree, never trusting the first read — the
#           sync daemon's own ~2s poll cadence, CLAUDE.md gotcha).
#         * Home / Search / Palette / Packages / Templates (the five View
#           items): GET /__home, /__search, /__palette, /__packages,
#           /__templates and assert each RENDERS — HTTP 200 AND a
#           non-trivial body containing a page-specific marker (each page's
#           own <title>). A bare 200 from a blank or wrong page must not
#           read as a pass (the D1 tri-state/proof-of-render discipline).
#         * Reveal in Finder: `reveal.rs`'s pure `reveal_command` builder is
#           what `Command::RevealInFinder` ultimately spawns; proven via its
#           own pinned unit test in the SAME cargo-test leg as (a) — the
#           exact `open -R <path>` argv this OS would get.
#         * Open Vault: POST to the localhost control server's `/open` —
#           the EXACT `VaultRunner::switch_to` the GUI dialog's
#           `on_open_vault` callback calls (control.rs; same technique
#           scripts/n5-vaults.sh already established for driving vault
#           switches headlessly).
#         * Open Recent (the STORE half): `record_open`/`list_recent`
#           (recent.rs) are pure and Tauri-free, but every PRODUCTION
#           caller of `record_open` sits behind an `AppHandle`-coupled menu
#           dispatch function (`open_file_window` + friends in
#           `menubar/mod.rs`) that only exists inside a real Tauri runtime
#           — there is no HTTP or headless-reachable trigger for it, the
#           same "a menu cannot be clicked in CI" wall this whole gate
#           exists to work around. So, exactly like (a) proves menu WIRING
#           without literal clicks, this gate proves the recent-store
#           CONTRACT (open a file -> appears; open a second -> ordering;
#           reopen the first -> moves to front, no duplicate) via recent.rs's
#           own pinned unit tests in the SAME cargo-test leg — which really
#           do read the store back off disk (`std::fs::read` inside
#           `list_recent`), just inside the Rust test process's own tempdir
#           rather than this script's. Documented here rather than silently
#           left out, per the "print what's NOT covered" rule below.
#         * OpenFile / Import / Export: native OS file/folder pickers
#           (`choose_folder`/`choose_file`/`save_file`) — no headless
#           equivalent exists (the product's own Known Limits text says so:
#           "Open…, Import… and Export… use native pickers on macOS only").
#           NOT COVERED, printed as such.
#         * FocusWindow / About / KnownLimits: need a live second window or
#           a native modal dialog — NOT COVERED headlessly; FocusWindow's
#           underlying registry decision IS unit-tested (leg (a)'s
#           `windows::tests::opening_an_already_open_file_reuses_its_window`).
#       The full command-by-command table is printed near the end of every
#       run — covered AND not-covered, so a gap is a printed line, not a
#       silent absence.
#
#   (c) NO ORPHANED ITEMS, EITHER DIRECTION. Every enabled model item
#       resolves to a command (`every_enabled_item_carries_a_command`,
#       `command_for_id_round_trips_every_enabled_item`,
#       `menubar::tests::every_item_in_the_model_has_a_non_empty_id`) AND no
#       `Command` variant exists that no menu item ever reaches
#       (`every_command_variant_is_produced_by_some_model_build`) — all four
#       pinned by name in leg (a)'s cargo-test run. An unreachable command is
#       dead code; a command-less enabled item is a dead menu entry.
#
#   (d) WINDOW-PER-FILE, ASSERTED WHERE IT CAN BE. The registry's decision
#       logic (`windows.rs`) is unit-tested and pinned above
#       (`opening_an_already_open_file_reuses_its_window`). The GUI half —
#       two real windows, two titles, ⌘Q — needs to DRIVE native menu items
#       (File > New File / Open Recent) and READ window titles from outside
#       the process; neither has any automation surface available to this
#       gate (no Tauri IPC command is registered for either — this was
#       checked, not assumed — and no window-enumeration tool exists here).
#       That is a permanent, documented tooling gap, not a flaky omission:
#       this leg boots the REAL GUI binary (D0/D2's own navwatch precedent —
#       proves the new window-per-file / menu-bar wiring doesn't break basic
#       boot) and then prints an explicit, clearly-distinct SKIPPED line for
#       the two-window/title assertion specifically, with its reason — never
#       silently omitted, never counted as a PASS.
#
# Dedicated ports: proxy 9050, backend 6512, postgres 5585, valkey 6528. A
# fifth, D3-local port (control 9051 — the proxy+1 convention scripts/n5-
# vaults.sh / e3 / e4 / e6 already use) is added for the Open Vault control
# server; not part of the brief's four but needed to drive (b)'s Open Vault
# check the same headless way n5 drives vault switches.
#
# CRITICAL: teardown is strictly PID-scoped — another live gate may run on a
# different port block. We kill ONLY the PIDs this script recorded; never
# pkill/killall by name.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export REPO_ROOT="$ROOT"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

PROXY_PORT="${D3_PROXY_PORT:-9050}"
BACKEND_PORT="${D3_BACKEND_PORT:-6512}"
POSTGRES_PORT="${D3_POSTGRES_PORT:-5585}"
VALKEY_PORT="${D3_VALKEY_PORT:-6528}"
CONTROL_PORT="${D3_CONTROL_PORT:-9051}"
BASE="http://localhost:${PROXY_PORT}"
CONTROL="http://127.0.0.1:${CONTROL_PORT}"

FIRST_BOOT_TIMEOUT="${D3_FIRST_BOOT_TIMEOUT:-900}"   # fresh data dir, pg-cache seeded
DISK_POLL_TIMEOUT="${D3_DISK_POLL_TIMEOUT:-90}"       # per-step: daemon poll (~2s) + index lag
SWITCH_TIMEOUT="${D3_SWITCH_TIMEOUT:-300}"            # Open Vault: a full teardown+reboot
GUI_BOOT_TIMEOUT="${D3_GUI_BOOT_TIMEOUT:-300}"
GUI_SETTLE_SECS="${D3_GUI_SETTLE_SECS:-10}"           # generous — a short settle silently lies (D1 finding)
PAGE_MIN_BYTES="${D3_PAGE_MIN_BYTES:-200}"            # "non-trivial body", not just a 200

HEADLESS_BIN="$ROOT/target/debug/headless"
GUI_BIN="$ROOT/target/debug/penpot-desktop"
HELPER="$ROOT/scripts/d3_menus_helper.py"

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d3-data.XXXXXX")"
VAULT="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d3-vault.XXXXXX")"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/penpot-d3-work.XXXXXX")"
LOG1="$WORK_DIR/headless.log"
LOG_GUI="$WORK_DIR/gui-navwatch.jsonl"
CARGO_TEST_LOG="$WORK_DIR/cargo-test.log"

HEADLESS_PID=""
GUI_PID=""
FAILURES=0
SKIPPED=0

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
skip() { echo "SKIPPED: $1"; SKIPPED=$((SKIPPED + 1)); }
strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

PG_CACHE="${D3_PG_CACHE:-$HOME/.cache/penpot-local/pg-install}"
save_pg_cache() {
    if [ ! -d "$PG_CACHE" ] && [ -d "$DATA_DIR/postgres/install" ]; then
        mkdir -p "$(dirname "$PG_CACHE")"
        cp -R "$DATA_DIR/postgres/install" "$PG_CACHE.tmp-$$" &&
            mv "$PG_CACHE.tmp-$$" "$PG_CACHE" &&
            echo "     (cached postgres binaries at $PG_CACHE)"
    fi
}

# PID-scoped teardown ONLY — never pkill/killall by name.
cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill -TERM "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    if [ -n "$GUI_PID" ] && kill -0 "$GUI_PID" 2>/dev/null; then
        kill -TERM "$GUI_PID" 2>/dev/null
        for _ in $(seq 1 25); do kill -0 "$GUI_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$GUI_PID" 2>/dev/null
    fi
    save_pg_cache
    if [ "$FAILURES" -eq 0 ]; then
        rm -rf "$DATA_DIR" "$VAULT" "$WORK_DIR"
    else
        echo "kept for debugging: data=$DATA_DIR vault=$VAULT work=$WORK_DIR"
    fi
}
trap cleanup EXIT

ports_free() {
    local p
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; do
        lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && { echo "port $p busy" >&2; return 1; }
    done
    return 0
}

ports_all_free() {
    local p ok=0
    for p in "$PROXY_PORT" "$BACKEND_PORT" "$POSTGRES_PORT" "$VALKEY_PORT" "$CONTROL_PORT"; do
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "port $p still has a listener:" >&2
            lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2 || true
            ok=1
        fi
    done
    return "$ok"
}

wait_ready() {
    local log="$1" timeout="$2"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        strip_ansi <"$log" 2>/dev/null | grep -q "^READY " && return 0
        kill -0 "$HEADLESS_PID" 2>/dev/null || { echo "headless died:" >&2; tail -25 "$log" >&2; return 1; }
        sleep 2
    done
    echo "timed out waiting for READY in $log (${timeout}s)" >&2; return 1
}

stop_headless() {
    [ -n "$HEADLESS_PID" ] || return 0
    kill -TERM "$HEADLESS_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$HEADLESS_PID" 2>/dev/null || { HEADLESS_PID=""; return 0; }; sleep 1; done
    kill -9 "$HEADLESS_PID" 2>/dev/null; HEADLESS_PID=""
}

# Mirrors d0-navigation-spike.sh / d2-home.sh's helper: the app reaching
# $needle is the readiness signal (main.rs only navigates once the full
# stack is up, and on_navigation records it immediately).
wait_for_log_line() {
    local log="$1" needle="$2" timeout="$3"
    local deadline=$(($(date +%s) + timeout))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        grep -qF "$needle" "$log" 2>/dev/null && return 0
        kill -0 "$GUI_PID" 2>/dev/null || { echo "GUI app exited before reaching '$needle'" >&2; return 1; }
        sleep 1
    done
    echo "timed out waiting for '$needle' in $log (${timeout}s)" >&2
    return 1
}

stop_gui() {
    [ -n "$GUI_PID" ] || return 0
    kill -TERM "$GUI_PID" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$GUI_PID" 2>/dev/null || { GUI_PID=""; return 0; }; sleep 1; done
    kill -9 "$GUI_PID" 2>/dev/null; GUI_PID=""
}

# check_page <path> <expected-title-marker>: GET $BASE<path>, require HTTP
# 200 AND a non-trivial body (>= PAGE_MIN_BYTES — a bare 200 from a blank or
# error page must not read as a pass) AND the page's own page-specific
# <title> marker present (never a generic string every page would contain).
check_page() {
    local path="$1" want="$2"
    local body_file="$WORK_DIR/page$(echo "$path" | tr -c 'a-zA-Z0-9' '_').html"
    local code
    code="$(curl -sS -o "$body_file" -w '%{http_code}' --max-time 30 "$BASE$path" 2>/dev/null)" || code="000"
    if [ "$code" != "200" ]; then
        fail "(b/view) GET $path -> HTTP $code (expected 200)"
        return
    fi
    local size
    size=$(wc -c <"$body_file" | tr -d ' ')
    if [ "$size" -lt "$PAGE_MIN_BYTES" ]; then
        fail "(b/view) GET $path returned only $size bytes (< $PAGE_MIN_BYTES) — reads like a blank/error page, not a real render"
        return
    fi
    if ! grep -qF "$want" "$body_file"; then
        fail "(b/view) GET $path is missing its page-specific marker '$want' (HTTP 200, ${size} bytes) — a generically-200 wrong page must not pass"
        return
    fi
    pass "(b/view) GET $path renders: HTTP 200, ${size} bytes, contains '$want'"
}

echo "== D3 menu-bar gate =="
echo "   ports: proxy=$PROXY_PORT backend=$BACKEND_PORT pg=$POSTGRES_PORT valkey=$VALKEY_PORT control=$CONTROL_PORT"
echo "   data:  $DATA_DIR"
echo "   vault: $VAULT"

# --- pre-flight --------------------------------------------------------------
[ -f "$HELPER" ] || { fail "manage helper missing: $HELPER"; exit 1; }
if ! python3 -m py_compile "$HELPER"; then
    fail "preflight: d3_menus_helper.py does not compile"
else
    pass "preflight: python3 -m py_compile d3_menus_helper.py"
fi
ports_free || { fail "one of the D3 ports is busy"; exit 1; }
if pgrep -f "$GUI_BIN" >/dev/null 2>&1; then
    fail "a penpot-desktop GUI process is already running (single-instance guard would swallow our launch) — quit it first"
    exit 1
fi
pass "pre-flight: ports free, no existing GUI instance, helper present"

# =========================================================================
# LEG (a) + (c) — the menu model's shape, no orphaned items either
# direction, Open Recent's store contract, and Reveal's pure command
# builder: all pure/Tauri-free unit tests, no live stack needed. Run first
# so a broken contract fails fast before paying for a stack boot.
# =========================================================================
echo "-- (a)/(c) cargo test -p penpot-desktop -- menubar:: windows:: recent:: reveal::"
(cd "$ROOT" && cargo test -p penpot-desktop -- menubar:: windows:: recent:: reveal::) >"$CARGO_TEST_LOG" 2>&1
CARGO_TEST_EXIT=$?
CARGO_TEST_STRIPPED="$WORK_DIR/cargo-test.stripped.log"
strip_ansi <"$CARGO_TEST_LOG" >"$CARGO_TEST_STRIPPED"

# name|why — "why" is printed alongside PASS/FAIL so a reader never has to
# cross-reference this script against model.rs to know what a name proves.
REQUIRED_TESTS=(
    "menubar::model::tests::app_section_is_first_and_carries_quit|the application submenu (About/Services/Hide/Quit) must be FIRST or macOS has no ⌘Q"
    "menubar::model::tests::preferences_is_absent_in_d3|no dead Preferences item before D4 owns it"
    "menubar::model::tests::every_enabled_item_carries_a_command|no-dead-items direction 1: every enabled row dispatches something"
    "menubar::model::tests::every_command_variant_is_produced_by_some_model_build|no-dead-items direction 2: every Command is reachable from some model build"
    "menubar::model::tests::item_ids_are_unique|duplicate ids would make command_for_id silently dispatch the wrong action"
    "menubar::model::tests::the_promised_accelerators_are_present_and_correct|⌘N/⌘O/⌘F are pinned, not just present-by-accident"
    "menubar::model::tests::the_key_window_is_marked_and_others_are_not|the Window menu must mark which window is actually key"
    "menubar::model::tests::command_for_id_round_trips_every_enabled_item|id->Command resolution actually works for every enabled row, not just exists"
    "menubar::tests::every_item_in_the_model_has_a_non_empty_id|the half of no-dead-items the compiler can't check: every item id is real"
    "windows::tests::opening_an_already_open_file_reuses_its_window|window-per-file: a second Open on the same file focuses, not duplicates"
    "recent::tests::most_recent_first|Open Recent is real (1/3): opening a file puts it at the front"
    "recent::tests::reopening_moves_to_front_without_duplicating|Open Recent is real (2/3): reopening moves to front, never duplicates"
    "recent::tests::the_list_is_capped|Open Recent is real (3/3): the store never grows past RECENT_LIMIT"
    "recent::tests::a_missing_or_corrupt_store_is_an_empty_list_not_an_error|a broken store degrades to empty, never panics the menu build"
    "reveal::tests::macos_reveal_uses_open_dash_r_on_the_item_itself|Reveal in Finder: the exact 'open -R <path>' OsCommand this OS would spawn"
)

for entry in "${REQUIRED_TESTS[@]}"; do
    name="${entry%%|*}"
    why="${entry#*|}"
    line="$(grep -F "test ${name} " "$CARGO_TEST_STRIPPED" || true)"
    if [ -z "$line" ]; then
        fail "(a) required test NOT FOUND in the run: $name — $why (a rename or removal would hide behind this gate)"
    elif echo "$line" | grep -q '\.\.\. ok$'; then
        pass "(a) $name — $why"
    else
        fail "(a) $name FAILED — $why : $line"
    fi
done

if [ "$CARGO_TEST_EXIT" -eq 0 ]; then
    pass "(a) cargo test -p penpot-desktop -- menubar:: windows:: recent:: reveal:: exits 0 (no unlisted regression hiding behind the named checks alone)"
else
    fail "(a) cargo test -p penpot-desktop -- menubar:: windows:: recent:: reveal:: exited $CARGO_TEST_EXIT — see $CARGO_TEST_LOG"
fi

# =========================================================================
# BUILD
# =========================================================================
echo "-- build (penpot-desktop package builds both the headless and GUI bins)"
if ! (cd "$ROOT" && cargo build -q -p penpot-desktop); then
    fail "cargo build -p penpot-desktop"; exit 1
fi
[ -x "$HEADLESS_BIN" ] || { fail "built binary missing: $HEADLESS_BIN"; exit 1; }
[ -x "$GUI_BIN" ] || { fail "built binary missing: $GUI_BIN"; exit 1; }
pass "cargo build -p penpot-desktop (headless + penpot-desktop GUI)"

if [ -d "$PG_CACHE" ]; then
    mkdir -p "$DATA_DIR/postgres"
    cp -R "$PG_CACHE" "$DATA_DIR/postgres/install"
    echo "   (seeded postgres binaries from $PG_CACHE)"
fi

BOOT_ENV=(
    PENPOT_LOCAL_DATA_DIR="$DATA_DIR"
    PENPOT_LOCAL_DESIGNS_DIR="$VAULT"
    PENPOT_LOCAL_PROXY_PORT="$PROXY_PORT"
    PENPOT_LOCAL_BACKEND_PORT="$BACKEND_PORT"
    PENPOT_LOCAL_POSTGRES_PORT="$POSTGRES_PORT"
    PENPOT_LOCAL_VALKEY_PORT="$VALKEY_PORT"
)

# =========================================================================
# PHASE 1 — headless boot: drive (b)'s live-stack legs
# =========================================================================
echo "-- boot (headless, control server on :$CONTROL_PORT)"
env "${BOOT_ENV[@]}" PENPOT_LOCAL_CONTROL_PORT="$CONTROL_PORT" "$HEADLESS_BIN" >"$LOG1" 2>&1 &
HEADLESS_PID=$!
if ! wait_ready "$LOG1" "$FIRST_BOOT_TIMEOUT"; then fail "boot reaches READY"; exit 1; fi
pass "boot reaches READY"
export PENPOT_BACKEND="$BASE"

echo "-- (b) New File + New Project via /__api/vault/manage/{file,project}"
NEWFILE_OUT="$(python3 "$HELPER" new-file-and-project "$WORK_DIR" "$VAULT" "$DISK_POLL_TIMEOUT" 2>"$WORK_DIR/new-file.err")"
if echo "$NEWFILE_OUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("steps") else 1)' 2>/dev/null; then
    while IFS=$'\t' read -r verdict name detail; do
        if [ "$verdict" = "PASS" ]; then
            pass "(b/new-file+project) $name: $detail"
        else
            fail "(b/new-file+project) $name: $detail"
        fi
    done < <(echo "$NEWFILE_OUT" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception as e:
    print("FAIL\t(unparseable)\t" + str(e)); sys.exit(0)
for s in data.get("steps", []):
    verdict = "PASS" if s.get("ok") else "FAIL"
    name = s.get("name", "?")
    detail = s.get("detail", "")
    print(f"{verdict}\t{name}\t{detail}")
')
else
    fail "(b/new-file+project) d3_menus_helper.py produced no parseable step list — INFRASTRUCTURE FAILURE: $(cat "$WORK_DIR/new-file.err" 2>/dev/null)"
fi

echo "-- (b) View pages: Home / Search / Palette / Packages / Templates"
check_page "/__home" "Penpot Local — Vault"
check_page "/__search" "Vault search"
check_page "/__palette" "Quick open"
check_page "/__packages" "Package gallery"
check_page "/__templates" "New from template"

echo "-- (b) Open Vault via the localhost control server"
if curl -fsS --max-time 10 "$CONTROL/health" >/dev/null 2>&1; then
    pass "(b/open-vault) proof of looking: control server is up on :$CONTROL_PORT before we assert anything about /open"
    SWITCH_OUT="$(curl -fsS --max-time "$SWITCH_TIMEOUT" -X POST "$CONTROL/open" \
        -H 'Content-Type: application/json' -d "{\"path\": \"$VAULT\"}" 2>"$WORK_DIR/open-vault.err")"
    if echo "$SWITCH_OUT" | grep -qE '"ok":[[:space:]]*true'; then
        pass "(b/open-vault) POST $CONTROL/open -> ok:true — the exact VaultRunner::switch_to the GUI's Open Vault callback invokes: $SWITCH_OUT"
    else
        fail "(b/open-vault) POST $CONTROL/open did not return ok:true: $SWITCH_OUT $(cat "$WORK_DIR/open-vault.err" 2>/dev/null)"
    fi
else
    fail "(b/open-vault) control server never came up on :$CONTROL_PORT — cannot exercise Open Vault at all"
fi

echo "-- clean shutdown of headless"
stop_headless
sleep 1
if ports_all_free; then
    pass "boot: clean shutdown, all 5 D3 ports freed"
else
    fail "boot: ports still busy after shutdown"
fi

# =========================================================================
# PHASE 2 — GUI boot: proves the window-per-file / menu-bar wiring doesn't
# break basic boot; the two-window/title assertion itself is SKIPPED (d).
# =========================================================================
echo "-- GUI boot (penpot-desktop — REQUIRES A GUI SESSION, same operational constraint as D0/D2)"
if pgrep -f "$GUI_BIN" >/dev/null 2>&1; then
    fail "(d) a penpot-desktop GUI process is already running — refusing to launch a second one"
else
    : >"$LOG_GUI"
    env "${BOOT_ENV[@]}" PENPOT_LOCAL_NAVWATCH_LOG="$LOG_GUI" "$GUI_BIN" >"$WORK_DIR/gui.log" 2>&1 &
    GUI_PID=$!
    if wait_for_log_line "$LOG_GUI" "/__bootstrap" "$GUI_BOOT_TIMEOUT"; then
        pass "(d) GUI boot reached the default entry point (/__bootstrap) — the home window opens under the new window-per-file/menu-bar wiring without crashing"
        sleep "$GUI_SETTLE_SECS"
    else
        fail "(d) GUI never reached /__bootstrap within ${GUI_BOOT_TIMEOUT}s"
        echo "   -- app log tail --" >&2; tail -25 "$WORK_DIR/gui.log" >&2 || true
    fi
    stop_gui
    sleep 1
    if ports_all_free; then
        pass "(d) GUI boot: clean shutdown, all 5 D3 ports freed"
    else
        fail "(d) GUI boot: ports still busy after shutdown"
    fi
fi

skip "(d) two-window / two-title / ⌘Q live verification — this would require driving File > New File (or ⌘N) a second time and then reading BOTH windows' titles from outside the process. Neither has automation available to this gate: no Tauri IPC command is registered anywhere in this crate for opening a file window or listing windows (checked via grep, not assumed), and no window-enumeration/accessibility tool is available here. This is a PERMANENT tooling gap, not a flaky omission — the registry decision this would exercise is already unit-tested and REQUIRED above (windows::tests::opening_an_already_open_file_reuses_its_window)."

# =========================================================================
# COMMAND COVERAGE TABLE — every Command variant, covered or not, printed
# so a gap is a line in the log, never a silent absence.
# =========================================================================
echo
echo "== (b) command coverage =="
COVERAGE=(
    "NewFile|COVERED|POST /__api/vault/manage/file — the same create-file RPC create_new_file() calls"
    "NewProject|COVERED|POST /__api/vault/manage/project — the same create-project RPC create_new_project() calls"
    "OpenFile|NOT COVERED|native folder picker (choose_folder) — no headless equivalent; product's own Known Limits text documents picker-only Open"
    "OpenRecent|PARTIAL|store ordering/dedup/cap proven live via recent::tests (leg a above); the GUI dispatch (open_file_window+record_open) needs a live AppHandle, out of scope headlessly"
    "OpenVault|COVERED|POST $CONTROL_PORT/open — the exact VaultRunner::switch_to the GUI dialog's on_open_vault callback invokes"
    "Import|NOT COVERED|native file picker (choose_file) — no headless equivalent"
    "Export|NOT COVERED|native save picker (save_file) — no headless equivalent"
    "RevealInFinder|COVERED|reveal::tests::macos_reveal_uses_open_dash_r_on_the_item_itself (leg a) pins the exact OsCommand this OS spawns"
    "ShowHome|COVERED|GET /__home renders"
    "ShowSearch|COVERED|GET /__search renders"
    "ShowPalette|COVERED|GET /__palette renders"
    "ShowPackages|COVERED|GET /__packages renders"
    "ShowTemplates|COVERED|GET /__templates renders"
    "FocusWindow|NOT COVERED|needs a second live window; registry decision is unit-tested (leg a), live focus needs the SKIPPED GUI leg (d)"
    "About|NOT COVERED|native modal dialog — no headless observation possible"
    "KnownLimits|NOT COVERED|native modal dialog — no headless observation possible"
)
COVERED_N=0
NOT_COVERED_N=0
for row in "${COVERAGE[@]}"; do
    cmd="${row%%|*}"; rest="${row#*|}"; status="${rest%%|*}"; why="${rest#*|}"
    printf '   %-16s %-12s %s\n' "$cmd" "$status" "$why"
    if [ "$status" = "NOT COVERED" ]; then
        NOT_COVERED_N=$((NOT_COVERED_N + 1))
    else
        COVERED_N=$((COVERED_N + 1))
    fi
done
echo "   -> ${#COVERAGE[@]} Command variants: $COVERED_N covered/partial, $NOT_COVERED_N not covered (every gap documented above, none silent)"

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D3 MENUS: ALL PASS ($SKIPPED SKIPPED)"
else
    echo "D3 MENUS: $FAILURES FAILURE(S) ($SKIPPED SKIPPED)"
fi
exit "$FAILURES"
