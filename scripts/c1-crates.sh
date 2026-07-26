#!/usr/bin/env bash
# C1 extraction gate (chapter C). A FAST, PURE, STACK-FREE gate proving the
# library extraction is real rather than nominal:
#   (a) NO extracted crate depends on tauri, directly or transitively. Without
#       this the extraction regresses silently at the first convenient
#       `use tauri::`, leaving the old coupling spread over more files.
#   (b) Every extracted crate's tests RUN and pass in their new home, named by
#       crate — so a module that quietly failed to move cannot hide behind a
#       workspace-wide green. "RUN" is asserted, not assumed: `cargo test -p X`
#       exits 0 on `running 0 tests`, so a crate whose tests never arrived would
#       have PASSED this leg while proving nothing (it did — `installer` passed
#       vacuously until the `N passed` count below was parsed and required to be
#       >= 1). The count is printed so a suspiciously small number is visible
#       rather than merely non-zero.
#   (c) apps/desktop keeps only the GUI: its src/ holds no module the crate map
#       assigned elsewhere, and no orphaned HTML page whose module has left.
# No ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

# The six extracted crates, plus `penpot-cli` — created this chapter rather than
# extracted, but it belongs under both legs for the same reasons the six do:
#   (a) `apps/desktop` DEPENDS on `penpot-cli` (`bin/headless` is a shim over
#       `penpot_cli::verbs::daemon`), so it sits on the GUI's dependency path —
#       exactly where a `use tauri::` is most convenient and least noticed. Left
#       out of this list, adding `tauri` to `apps/cli/Cargo.toml` kept `just c1`
#       green.
#   (b) its tests run in no other gate reachable from `just e2e`, including the
#       `tier1.rs` tests that pin the exact `partial`/`missing` arrays
#       `c2-cli.sh` greps for — so those two would have been free to drift apart.
EXTRACTED=(installer app-state packages vault-ops web-ui app-core penpot-cli)

echo "-- (a) no extracted crate depends on tauri"
for c in "${EXTRACTED[@]}"; do
    TREE_OUT="$(cd "$ROOT" && cargo tree -p "$c" -e normal 2>&1)"
    TREE_RC=$?
    if [ "$TREE_RC" -ne 0 ]; then
        fail "(a) $c — cargo tree exited $TREE_RC, dependency graph not inspected: $TREE_OUT"
    elif echo "$TREE_OUT" | grep -q 'tauri v'; then
        fail "(a) $c depends on tauri — the extraction has regressed"
    else
        pass "(a) $c is tauri-free"
    fi
done

echo "-- (b) every extracted crate's tests pass in their new home"
for c in "${EXTRACTED[@]}"; do
    LOG="/tmp/c1-$c.log"
    if (cd "$ROOT" && cargo test -p "$c") >"$LOG" 2>&1; then
        # A green exit is NOT evidence the crate has tests: `cargo test -p X`
        # prints "running 0 tests" and exits 0 for a crate with none. Sum the
        # `N passed` reported by every test binary it ran (lib, integration,
        # doc) and require at least one, so this leg cannot pass vacuously.
        RAN="$(awk '/^test result:/ { s += $4 } END { print s+0 }' "$LOG")"
        if [ "$RAN" -ge 1 ]; then
            pass "(b) cargo test -p $c — $RAN tests ran"
        else
            fail "(b) cargo test -p $c ran 0 tests — exit 0 proves nothing here; the crate has no test that could have failed (see $LOG)"
        fi
    else
        fail "(b) cargo test -p $c failed — see $LOG"
    fi
done

echo "-- (c) apps/desktop holds only GUI modules"
MOVED=(installer prefs recent last_op packages templates vault checkpoint
       gitinit manage docopen reveal home navprobe control layout preflight
       windows status prefs_http)
for m in "${MOVED[@]}"; do
    if [ -f "$ROOT/apps/desktop/src/$m.rs" ]; then
        fail "(c) apps/desktop/src/$m.rs still exists — it belongs in a crate"
    else
        pass "(c) $m.rs is out of apps/desktop"
    fi
done
for p in home palette navprobe preferences templates; do
    if [ -f "$ROOT/apps/desktop/src/$p.html" ]; then
        fail "(c) apps/desktop/src/$p.html is orphaned — its module moved without it"
    else
        pass "(c) $p.html moved with its module"
    fi
done

echo
echo "C1: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
