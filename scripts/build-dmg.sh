#!/usr/bin/env bash
# M4: build the macOS single artifact (Penpot Local.app + .dmg).
#
#   1. scripts/build-runtime-bundle.sh  — produces/refreshes dist/penpot-runtime
#      (idempotent: skips the build on a fingerprint match and only re-runs its
#      proofs, ~13 s).
#   2. cargo tauri build                — release build + .app + .dmg bundling.
#
# The penpot-runtime resources are injected via the OVERLAY config
# apps/desktop/tauri.bundle.conf.json (--config), NOT the base tauri.conf.json.
# Rationale: tauri-build copies `bundle.resources` next to the compiled binary
# so `tauri dev` can resolve them — with the resources in the base config every
# debug build would drop a penpot-runtime/ into target/debug/, and the
# executable-adjacent bundle discovery in apps/desktop/src/layout.rs would
# silently flip dev runs (cargo run, m1-smoke, m2/m3 scripts) into packaged
# mode. Dev mode must stay byte-identical to pre-M4; only this script opts in.
#
# Signing: ad-hoc (no Developer ID, no notarization in M4). Consequence: on
# another machine Gatekeeper blocks double-click-open of the downloaded app —
# right-click > Open (or `xattr -d com.apple.quarantine`) is required on first
# launch. Documented in docs/milestones/m4.md.
#
# Output: target/release/bundle/macos/Penpot Local.app
#         target/release/bundle/dmg/Penpot Local_<version>_aarch64.dmg
#
# Flags: --skip-bundle   don't (re)run build-runtime-bundle.sh (dist/ must exist)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

SKIP_BUNDLE=0
for arg in "$@"; do
    case "$arg" in
        --skip-bundle) SKIP_BUNDLE=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

if [ "$(uname -s)/$(uname -m)" != "Darwin/arm64" ]; then
    echo "ERROR: macOS arm64 only (Linux/AppImage is a separate implementation)" >&2
    exit 1
fi

# --- 1. runtime bundle -------------------------------------------------------
if [ "$SKIP_BUNDLE" -eq 0 ]; then
    bash "$ROOT/scripts/build-runtime-bundle.sh"
else
    echo "== skipping build-runtime-bundle.sh (--skip-bundle)"
fi
if [ ! -f "$ROOT/dist/penpot-runtime/backend/penpot.jar" ]; then
    echo "ERROR: dist/penpot-runtime is not a valid bundle (missing backend/penpot.jar)" >&2
    exit 1
fi

# --- 2. tauri build (app + dmg) ----------------------------------------------
echo "== cargo tauri build (release + app + dmg)"
# tauri-build copies bundle.resources next to the release binary
# (target/release/penpot-runtime). The bundle contains read-only files (jre
# legal docs, license texts, mode 444) and std::fs::copy cannot overwrite a
# read-only destination -> "Permission denied" on the SECOND build. Clear the
# stale copy first; tauri-build recreates it.
rm -rf "$ROOT/target/release/penpot-runtime"
# CI=true makes the dmg step pass --skip-jenkins to bundle_dmg.sh: no
# AppleScript/Finder window-layout automation, which fails (and would prompt
# for Automation permission) in headless/scripted runs.
(cd "$ROOT/apps/desktop" && CI=true cargo tauri build --config tauri.bundle.conf.json)

# --- report -------------------------------------------------------------------
APP="$ROOT/target/release/bundle/macos/Penpot Local.app"
DMG="$(ls -t "$ROOT/target/release/bundle/dmg/"*.dmg 2>/dev/null | head -1 || true)"
echo
echo "== artifacts"
[ -d "$APP" ] && du -sh "$APP" || { echo "ERROR: .app missing" >&2; exit 1; }
[ -n "$DMG" ] && du -sh "$DMG" || { echo "ERROR: .dmg missing" >&2; exit 1; }
codesign -dv "$APP" 2>&1 | sed -n 's/^\(Signature\|Authority\|Identifier\)/  &/p' || true
