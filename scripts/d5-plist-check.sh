#!/usr/bin/env bash
# D5a follow-up: assert the PACKAGED .app's Info.plist actually carries the
# `.penpot` package-document-type declaration, and that it MERGED with
# Tauri's own generated Info.plist keys rather than one clobbering the other.
#
# Background (docs/spikes/finder-document-association.md,
# .superpowers/sdd/task-7-brief.md): Tauri's `fileAssociations` config cannot
# express `LSTypeIsPackage`, so apps/desktop/tauri.bundle.conf.json points
# `bundle.macOS.infoPlist` at apps/desktop/Info.plist, a hand-written
# fragment Tauri merges into the generated Info.plist
# (tauri-bundler-2.9.4/src/bundle/macos/app.rs:create_info_plist — each
# top-level key of the user plist is inserted into the dict Tauri already
# built from tauri.conf.json). The merge risk: if this script only checked
# our custom keys, a completely broken/replaced Info.plist could still pass
# vacuously — so it also asserts Tauri's own baseline keys survived.
#
# GUI/macOS-ONLY, NOT chained into `just e2e`: it needs a real packaged
# `.app` (scripts/build-dmg.sh), which the milestone doc governs separately
# (see docs/spikes/finder-document-association.md "What D5 now builds on").
#
# Usage: scripts/d5-plist-check.sh [path/to/Penpot Local.app]
#   Default app path: target/release/bundle/macos/Penpot Local.app
set -u

if [ "$(uname)" != "Darwin" ]; then
    echo "SKIPPED: macOS only (Info.plist / PlistBuddy are macOS-specific)."
    exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${1:-$ROOT/target/release/bundle/macos/Penpot Local.app}"
PLIST="$APP/Contents/Info.plist"
PLISTBUDDY=/usr/libexec/PlistBuddy
UTI="dev.albyianna.penpot-file"
BUNDLE_ID="dev.albyianna.penpot-local"

FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

if [ ! -d "$APP" ]; then
    echo "ERROR: no packaged app at: $APP" >&2
    echo "Build the app first (scripts/build-dmg.sh), then re-run this script" \
        "(optionally pointing it at the built .app path as \$1)." >&2
    exit 1
fi

if [ ! -f "$PLIST" ]; then
    echo "ERROR: $APP exists but has no Contents/Info.plist — not a valid .app bundle." >&2
    exit 1
fi

# Sanity: the merged file must still be a well-formed plist.
if plutil -lint -s "$PLIST" >/dev/null 2>&1; then
    pass "Contents/Info.plist is well-formed XML (plutil -lint)"
else
    fail "Contents/Info.plist failed plutil -lint — merge produced invalid XML"
fi

DUMP="$("$PLISTBUDDY" -c "Print" "$PLIST" 2>/dev/null)"
if [ -z "$DUMP" ]; then
    echo "ERROR: PlistBuddy could not read $PLIST" >&2
    exit 1
fi

# --- Tauri's own baseline keys must have survived the merge -----------------
# (proves our custom plist did not wholesale-replace the generated dict)
if "$PLISTBUDDY" -c "Print :CFBundleIdentifier" "$PLIST" 2>/dev/null | grep -qx "$BUNDLE_ID"; then
    pass "CFBundleIdentifier ($BUNDLE_ID) — Tauri's generated key survived the merge"
else
    fail "CFBundleIdentifier missing or wrong — merge may have clobbered Tauri's generated Info.plist"
fi

if "$PLISTBUDDY" -c "Print :CFBundleExecutable" "$PLIST" >/dev/null 2>&1; then
    pass "CFBundleExecutable present — Tauri's generated key survived the merge"
else
    fail "CFBundleExecutable missing — merge may have clobbered Tauri's generated Info.plist"
fi

# --- our custom UTI export must be present -----------------------------------
UTI_FOUND="$(echo "$DUMP" | grep -c "$UTI")"
if [ "$UTI_FOUND" -ge 2 ]; then
    # expect it to appear at least twice: once in UTExportedTypeDeclarations'
    # UTTypeIdentifier, once in CFBundleDocumentTypes' LSItemContentTypes
    pass "UTI '$UTI' is declared (UTExportedTypeDeclarations) and referenced (LSItemContentTypes)"
else
    fail "UTI '$UTI' not found (or found only once) in Contents/Info.plist"
fi

if echo "$DUMP" | grep -q "UTTypeConformsTo"; then
    pass "UTExportedTypeDeclarations present (UTTypeConformsTo key found)"
else
    fail "UTExportedTypeDeclarations / UTTypeConformsTo missing — .penpot won't conform to com.apple.package"
fi

# --- the load-bearing key: LSTypeIsPackage = true ----------------------------
if echo "$DUMP" | grep -q "LSTypeIsPackage = true"; then
    pass "LSTypeIsPackage = true is present — Finder will treat .penpot as an opaque package, not a folder"
else
    fail "LSTypeIsPackage not found (or not true) — Finder would browse INTO .penpot instead of opening it"
fi

if echo "$DUMP" | grep -q "CFBundleTypeRole = Editor"; then
    pass "CFBundleTypeRole = Editor is present"
else
    fail "CFBundleTypeRole = Editor not found"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D5 PLIST CHECK: GO — custom document-type plist merged cleanly into the packaged Info.plist"
    exit 0
else
    echo "D5 PLIST CHECK: $FAILURES CHECK(S) FAILED"
    exit 1
fi
