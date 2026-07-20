# D5a SPIKE — Finder document association for `.penpot` directory bundles

**Verdict: GO — with one load-bearing caveat.** macOS *can* treat a `.penpot`
**directory** as a double-clickable document bundle, launch our app on
double-click, and hand it the path. It works **only from an installed,
code-signed `.app`** — never from a `cargo run` / dev build — which is fine,
because Finder integration is inherently a packaged-app feature.

## The question

A `.penpot` on disk is a **directory** (an unzipped binfile), not a flat file.
The D5 survey flagged that macOS won't treat a directory extension as a
double-clickable document without `LSTypeIsPackage`, which Tauri's config schema
does not expose — so the whole Finder story rested on an assumption. The user
chose to spike it before building D5 around it.

## Method

A minimal throwaway `.app` (a bash launcher that logs its argv) with a
hand-written `Info.plist` declaring:

- `UTExportedTypeDeclarations` — a custom UTI `dev.albyianna.penpot-file`,
  `UTTypeConformsTo` = `com.apple.package`, extension `penpot`.
- `CFBundleDocumentTypes` — role `Editor`, `LSItemContentTypes` = that UTI, and
  crucially **`LSTypeIsPackage = true`**.

Registered with LaunchServices (`lsregister`) and interrogated with a Swift
probe (`URLResourceValues.contentType` / `.isPackage`,
`NSWorkspace.urlForApplication(toOpen:)`) and `open`. All commands and outputs
are reproduced by `scripts/d5a-finder-spike.sh` (self-cleaning).

## Evidence

| Check | Unsigned, in a temp dir | **Ad-hoc signed, in `~/Applications`** |
|---|---|---|
| LaunchServices `contentType` of a `.penpot` dir | `dev.albyianna.penpot-file` ✅ | `dev.albyianna.penpot-file` ✅ |
| `isPackage` | `true` ✅ | `true` ✅ |
| default app for the type | **none** ❌ | **PenpotSpike.app** ✅ |
| `open Sample.penpot` (the double-click path, no `-a`) | `kLSApplicationNotFoundErr` ❌ | **launched** ✅ (rc 0) |

Two subtleties the evidence pinned down:

1. **`mdls` lies here.** Spotlight kept reporting the directory as
   `public.folder`, but Spotlight is a different subsystem from the
   LaunchServices resolution that double-click actually uses — and
   LaunchServices resolved it to our package type correctly. Do not use `mdls`
   to judge this; use the LaunchServices/`URLResourceValues` path.

2. **The path is NOT passed as argv.** Even when the app launched, its argv was
   empty. macOS delivers the document via the **open-documents Apple event**,
   which Tauri 2.11.5 surfaces as `RunEvent::Opened { urls: Vec<url::Url> }`
   (`tauri-2.11.5/src/app.rs:263`, macOS-gated) — dispatched into the exact
   `app.run(|_, event| …)` loop this app already has at
   `apps/desktop/src/main.rs:443`. **D5 must handle `RunEvent::Opened`, not
   argv**, for the Finder path.

## The caveat that shapes D5

Default-handler binding only happened once the app was **ad-hoc code-signed and
in a real Applications location**. An unsigned bundle in a temp dir was flagged
`untrusted` by LaunchServices: its type claim was recognised for *resolution*
but not honoured as the *default handler*, so double-click failed with
`kLSApplicationNotFoundErr`.

The shipped app already ad-hoc signs (`signingIdentity: "-"` in
`tauri.conf.json`) and `build-dmg.sh` already documents removing the quarantine
bit on first open — so the real app clears this bar once installed. But it means:

- **Finder double-click cannot be verified from a dev build**, only from the
  packaged, signed `.app`. D5's automated gate therefore proves the *testable*
  paths (CLI argument, second-launch forwarding, drag-drop, and the
  path→file-id resolver) and treats Finder double-click as verified-by-this-spike
  + a manual packaged-build check, not a headless assertion.
- D5's Info.plist must carry `LSTypeIsPackage` and the `com.apple.package`
  conformance. Tauri's `fileAssociations` config cannot express `LSTypeIsPackage`
  (verified against `tauri-utils-2.9.3`'s schema — no such field), so D5 uses the
  `bundle.macOS.infoPlist` escape hatch with a hand-written plist, and a build
  step must confirm Tauri's generated `CFBundleDocumentTypes` and our custom
  plist merge rather than one clobbering the other.

## What D5 now builds on

1. Register `.penpot` as a package document type via a custom `Info.plist`
   (`LSTypeIsPackage` + `com.apple.package`).
2. Handle `RunEvent::Opened { urls }` in the existing `app.run` loop → resolve
   each `file://` path to a file-id → `open_file_window`.
3. Resolve path→file-id via the manifest's `entry_by_path` for in-vault files;
   for an **external** `.penpot` (dragged in from outside the vault), offer to
   import it (copy into the vault, let the daemon import it, open once it has an
   id) — the product-owner decision for D5.
4. Also wire a first-launch CLI argument and second-launch argv forwarding
   through the same resolver, since those are testable without a packaged build.
