#!/usr/bin/env bash
# C1 extraction gate (chapter C). A FAST, PURE, STACK-FREE gate proving the
# library extraction is real rather than nominal:
#   (a) NO extracted crate depends on tauri, directly or transitively. Without
#       this the extraction regresses silently at the first convenient
#       `use tauri::`, leaving the old coupling spread over more files.
#   (b) Every extracted crate's tests RUN and pass in their new home, named by
#       crate — so a module that quietly failed to move cannot hide behind a
#       workspace-wide green.
#   (c) apps/desktop keeps only the GUI: its src/ holds no module the crate map
#       assigned elsewhere, and no orphaned HTML page whose module has left.
# No ports, no stack. Chained into `just e2e`.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0; FAIL=0
pass() { echo "  PASS  $*"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }

EXTRACTED=(installer app-state packages vault-ops web-ui app-core)

echo "-- (a) no extracted crate depends on tauri"
for c in "${EXTRACTED[@]}"; do
    if (cd "$ROOT" && cargo tree -p "$c" -e normal 2>/dev/null | grep -q 'tauri v'); then
        fail "(a) $c depends on tauri — the extraction has regressed"
    else
        pass "(a) $c is tauri-free"
    fi
done

echo "-- (b) every extracted crate's tests pass in their new home"
for c in "${EXTRACTED[@]}"; do
    if (cd "$ROOT" && cargo test -p "$c") >"/tmp/c1-$c.log" 2>&1; then
        pass "(b) cargo test -p $c"
    else
        fail "(b) cargo test -p $c failed — see /tmp/c1-$c.log"
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
