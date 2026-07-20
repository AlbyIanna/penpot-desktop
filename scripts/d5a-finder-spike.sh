#!/usr/bin/env bash
# D5a SPIKE — can macOS treat a `.penpot` DIRECTORY as a double-clickable
# document bundle, and route a double-click to our app?
#
# GUI/macOS-ONLY, and NOT chained into `just e2e` (a pure-verdict spike lands
# no product code the ladder can regress — same rule D0/E5/E6 followed). It
# touches the per-user LaunchServices database and ~/Applications, and CLEANS
# UP after itself (an EXIT trap unregisters and removes the throwaway app).
#
# Verdict lives in docs/spikes/finder-document-association.md. This script is
# the reproducible evidence behind it.
#
# The core finding it proves: a directory extension declared with
# `LSTypeIsPackage` + `com.apple.package` conformance is recognised by
# LaunchServices as a package (isPackage=true) and, ONCE THE APP IS AD-HOC
# SIGNED AND IN ~/Applications, becomes the default handler so `open <dir>`
# (the double-click path) launches it. Unsigned/temp bundles resolve the type
# but are `untrusted` and never bind as default.
set -u

if [ "$(uname)" != "Darwin" ]; then
    echo "SKIPPED: macOS only (this probes LaunchServices)."; exit 0
fi

LSR=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
WORK="$(mktemp -d "${TMPDIR:-/tmp}/d5a-spike.XXXXXX")"
APPDIR="$HOME/Applications/PenpotSpike.app"
SAMPLE="$WORK/Sample.penpot"
FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

cleanup() {
    "$LSR" -u "$APPDIR" 2>/dev/null || true
    rm -rf "$APPDIR" "$WORK"
}
trap cleanup EXIT

# --- build a minimal package-type .app ------------------------------------
C="$WORK/PenpotSpike.app/Contents"
mkdir -p "$C/MacOS"
cat > "$C/MacOS/PenpotSpike" <<'SH'
#!/bin/bash
echo "launched" >> "$PENPOT_SPIKE_LOG"
SH
chmod +x "$C/MacOS/PenpotSpike"
cat > "$C/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>PenpotSpike</string>
  <key>CFBundleIdentifier</key><string>dev.albyianna.penpot-spike</string>
  <key>CFBundleExecutable</key><string>PenpotSpike</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>UTExportedTypeDeclarations</key><array><dict>
    <key>UTTypeIdentifier</key><string>dev.albyianna.penpot-file</string>
    <key>UTTypeDescription</key><string>Penpot File</string>
    <key>UTTypeConformsTo</key><array><string>com.apple.package</string></array>
    <key>UTTypeTagSpecification</key><dict>
      <key>public.filename-extension</key><array><string>penpot</string></array></dict>
  </dict></array>
  <key>CFBundleDocumentTypes</key><array><dict>
    <key>CFBundleTypeName</key><string>Penpot File</string>
    <key>CFBundleTypeRole</key><string>Editor</string>
    <key>LSTypeIsPackage</key><true/>
    <key>LSItemContentTypes</key><array><string>dev.albyianna.penpot-file</string></array>
  </dict></array>
</dict></plist>
PLIST

mkdir -p "$SAMPLE"; echo '{}' > "$SAMPLE/file.json"

# The probe: LaunchServices' own view (NOT Spotlight/mdls, which is a separate
# subsystem that reports .penpot as public.folder and would mislead here).
PROBE="$WORK/probe.swift"
cat > "$PROBE" <<'SWIFT'
import Foundation
import AppKit
import UniformTypeIdentifiers
let url = URL(fileURLWithPath: CommandLine.arguments[1])
let v = try? url.resourceValues(forKeys: [.contentTypeKey, .isPackageKey])
print("contentType=\(v?.contentType?.identifier ?? "nil")")
print("isPackage=\(v?.isPackage ?? false)")
print("defaultApp=\(NSWorkspace.shared.urlForApplication(toOpen: url)?.lastPathComponent ?? "none")")
SWIFT
SETDEF="$WORK/setdefault.swift"
cat > "$SETDEF" <<'SWIFT'
import Foundation
import AppKit
import UniformTypeIdentifiers
let app = URL(fileURLWithPath: CommandLine.arguments[1])
guard let ut = UTType("dev.albyianna.penpot-file") else { exit(1) }
let sem = DispatchSemaphore(value: 0)
NSWorkspace.shared.setDefaultApplication(at: app, toOpen: ut) { _ in sem.signal() }
sem.wait()
SWIFT

# --- install signed into ~/Applications (the trusted case) ----------------
mkdir -p "$HOME/Applications"
rm -rf "$APPDIR"
cp -R "$WORK/PenpotSpike.app" "$APPDIR"
codesign --force --deep --sign - "$APPDIR" >/dev/null 2>&1
"$LSR" -f "$APPDIR"
swift "$SETDEF" "$APPDIR" >/dev/null 2>&1

OUT="$(swift "$PROBE" "$SAMPLE" 2>/dev/null)"
echo "$OUT" | sed 's/^/   /'

case "$OUT" in
  *"contentType=dev.albyianna.penpot-file"*) pass "LaunchServices resolves the .penpot DIRECTORY to our package UTI (mdls/Spotlight would wrongly say public.folder)";;
  *) fail "type not resolved to our UTI — the extension claim did not register";;
esac
case "$OUT" in
  *"isPackage=true"*) pass "the .penpot directory is treated as an opaque PACKAGE (double-click launches the app instead of browsing in)";;
  *) fail "isPackage=false — LSTypeIsPackage/com.apple.package did not take; Finder would browse into the folder";;
esac
case "$OUT" in
  *"defaultApp=PenpotSpike.app"*) pass "the signed, installed app is the DEFAULT handler (an unsigned temp bundle is 'untrusted' and never binds)";;
  *) fail "no default handler bound — double-click would fail with kLSApplicationNotFoundErr";;
esac

# The actual double-click path: `open <dir>` with no -a.
export PENPOT_SPIKE_LOG="$WORK/launch.log"; : > "$PENPOT_SPIKE_LOG"
open "$SAMPLE" >/dev/null 2>&1
for _ in $(seq 1 10); do [ -s "$PENPOT_SPIKE_LOG" ] && break; sleep 0.5; done
if [ -s "$PENPOT_SPIKE_LOG" ]; then
    pass "the double-click path (open <dir>, no -a) LAUNCHED the app"
else
    fail "double-click did not launch the app"
fi

# The event-delivery finding, asserted against the pinned dependency: the path
# arrives via RunEvent::Opened, NOT argv (the launcher above logs no argv).
TAURI="$(find "$HOME/.cargo/registry/src" -maxdepth 2 -name 'tauri-2.11.*' -type d 2>/dev/null | head -1)"
if [ -n "$TAURI" ] && grep -q "Opened {" "$TAURI/src/app.rs"; then
    pass "Tauri 2.11.x exposes RunEvent::Opened { urls } — the macOS open-document event D5 must handle (the doc path is NOT argv)"
else
    fail "could not confirm RunEvent::Opened in the pinned Tauri source"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "D5a FINDER SPIKE: GO — .penpot is a double-clickable package from a signed, installed app"
    exit 0
else
    echo "D5a FINDER SPIKE: $FAILURES CHECK(S) FAILED"
    exit 1
fi
