# Known limits

Penpot Local wraps Penpot's own web frontend (`runtime/frontend/`) unmodified — invariant 3
forbids patching the SPA (see [PLAN.md](../PLAN.md) and [PLAN4.md](../PLAN4.md)). Every surface
described below is a consequence of that constraint: we can stop *navigating* somewhere, and we
can set the configuration flags Penpot's own frontend already understands, but we cannot delete
an in-canvas affordance without forking the bundle. This document is the honest catalogue of
what that leaves behind, written the way [PLAN4.md](../PLAN4.md) demands: "the app never
navigates you there," never "Penpot's account UI was removed" — because it wasn't, and claiming
otherwise would be false the moment someone typed the URL fragment by hand.

This is chapter 4's closer (milestone D6). It accounts for **every** reachable web route, not
just the ones our own UI links to — see the "Kept deliberately" section below for why that
distinction mattered.

## What the app removed or closed

Two different mechanisms, both configuration-only — no byte of `runtime/frontend/` changed:

**Frontend flags** (`D1_CLOUD_SURFACE_FLAGS`, [`apps/desktop/src/lib.rs`](../apps/desktop/src/lib.rs)),
appended to the `penpotFlags` string Penpot's own `config.js` bootstrap reads — landed in
[D1](milestones/d1/README.md):

| Flag | Removes |
|---|---|
| `disable-registration` | The "Create an account" link on the login page |
| `disable-login-with-password` | The password fields on the login form |
| `disable-dashboard-templates-section` | The "Libraries & Templates" carousel, which advertises content that lives on penpot.app |
| `disable-google-fonts-provider` | The workspace font picker's live network dependency |

**Backend single-user-mode flags** (`DEFAULT_PENPOT_FLAGS`,
[`crates/supervisor/src/lib.rs`](../crates/supervisor/src/lib.rs)), part of the supervisor's
baseline `PENPOT_FLAGS` since the app first booted a stack (see [PLAN.md](../PLAN.md), "Single-user
mode"):

| Flag | Removes |
|---|---|
| `disable-onboarding` | The multi-step onboarding wizard a fresh account would otherwise see |
| `disable-email-verification` | The email-verification gate, meaningless with no mail server |

**Redirected routes** — closed in the webview by
[`apps/desktop/src/navwatch.rs`](../apps/desktop/src/navwatch.rs), which observes Tauri's
`on_navigation` callback (our code watching our own window — invariant 3 holds) and cancels a
navigation before the SPA renders it, sending the webview to `/__home` instead:

| Route | Closed in | Why it could close |
|---|---|---|
| `#/auth/*` | [D1](milestones/d1/README.md) | No second account to log into or register; the real login path (`/__bootstrap`) never goes through this fragment |
| `#/dashboard/*` | [D2](milestones/d2/README.md) | `/__home` (create/rename/duplicate/move/delete) shipped as its replacement |
| `#/settings/*` | [D4](milestones/d4/README.md) | The native Preferences window (`/__preferences`) shipped as its replacement |
| `#/debug/*` | D6 (this milestone) | Inert dev tooling (`#/debug/icons-preview`, `#/debug/playground`) the shipped bundle does not elide and the app never navigates to; no replacement needed because nothing depended on it |

The `#/debug` closure is the gap this milestone's audit found: the shipped bundle does not elide
`*assert*`, so those two dev-tooling routes were compiled into the route tree and reachable by
URL the whole time — five milestones of "no web route reaches the canvas" never accounted for
them. `navwatch.rs`'s module doc comment is now the authoritative route-accounting table; this
document summarises it in prose.

## What stays web-shaped inside the canvas — and why invariant 3 keeps it

Everything below lives inside the SPA bundle Penpot ships. We do not navigate the user to any of
it from our own UI, but once they are inside `#/workspace` (which is, correctly, always allowed
— it's the product), it is still Penpot's own JavaScript rendering Penpot's own menus. Removing
any of it would mean patching `runtime/frontend/`, which invariant 3 forbids. The only lever this
project has is not-navigating-there, and that lever is already pulled everywhere it leads *out*
of the canvas.

- **The workspace Share/publish control.** Present in the file menu and toolbar. Inert offline:
  there is no server for a share link to register against or resolve, so clicking it fails
  quietly rather than doing anything harmful — but the control itself cannot be hidden without
  patching the SPA.
- **Comments.** Fully functional in-canvas, because they need no server round-trip beyond the
  one this project's own backend already serves locally. Left as-is deliberately — they are
  usable as notes-to-self on a single-user document, which is a real feature, not residue.
- **The subscription "Power up" menu item**, plus version-history subscription warnings. Penpot's
  paid-tier upsell surface. It renders because the frontend renders it unconditionally; there is
  no subscription backend behind it in this stack, so clicking through leads nowhere, but the
  nag itself is not removable without a frontend patch.
- **Help-center and feedback links in the file menu.** Point at Penpot's own hosted help site.
  Inert offline in the sense that following them requires network access this app does not
  provide by default, but the links remain in the menu.
- **The viewer's go-to-dashboard icon.** Visible in `#/view` (present mode), but neutralised: it
  fires a navigation to `#/dashboard`, and `navwatch.rs`'s unconditional cancellation (see above)
  redirects that to `/__home` before the dashboard ever renders. The icon is not hidden — it is
  defused at the point it tries to leave the canvas.

**No avatar/profile menu and no invite affordance were found reachable from inside
`#/workspace`.** Both live on `#/dashboard`, which is redirected away — so there is no in-canvas
path to either, not because they were removed, but because their only entry point is a route this
app never lets the user land on.

For every item above, the same three facts hold: it lives in the SPA bundle Penpot ships;
invariant 3 forbids removing it without patching that bundle; the only lever available is
not-navigating-there, which has already been applied everywhere the item would otherwise lead out
of the canvas.

## What is kept deliberately

Two route families are reachable and were deliberately **left allowed**, not overlooked:

- **`#/view/*` (present mode).** Kept because our own Present buttons navigate here, and a
  share-link pointing at it is inert offline anyway (no server to resolve it against). This was a
  considered decision, not a gap — see the route-accounting table in
  [`navwatch.rs`](../apps/desktop/src/navwatch.rs)'s module doc comment for the reasoning.
- **`#/frame-preview` and `#/render-sprite/:id`.** Internal render utilities the SPA navigates to
  **itself** as part of rendering thumbnails and frames. Cancelling either risks breaking
  rendering — the same lesson `#/view` already taught during earlier route-accounting work.
  `navwatch::tests::internal_render_routes_and_the_viewer_stay_allowed` pins that these three
  routes (plus `#/workspace`) stay allowed, specifically so a future tidy-up pass can't silently
  cancel them.

`#/subscribe-nitrate` is unreachable in this build — its route literal ships in the bundle, but
a runtime flag guard (the app does not set the `nitrate` flag) leaves it unregistered in the
route tree, so no navigation can reach it. Recorded for completeness; no policy needed.

## Deferred threads

**Updates.** This app never checks for updates and never phones home to find out if one exists.
Releases are manual downloads (see [RELEASES.md](RELEASES.md)). This was raised and deferred as
early as D4 ("that deserves its own decision; D6's residue audit is the place for it" —
[D4's known limits](milestones/d4/README.md#known-limits--stated-not-buried)) and this audit
defers it again, explicitly, rather than letting it drift: an update check is a network call by
definition, and this chapter's offline guarantee is zero non-loopback connection attempts in a
normal session. Building an updater is a decision for a later chapter, not an oversight in this
one.

**D1's egress caveat.** [D1's offline gate](milestones/d1/README.md) samples `lsof -nP -i` over
the supervised stack's pids once per run — a single sample, which D1's own known-limits section
names as not a proof of absence (a connection that opens and closes between polls could be
missed). D6's packaged-artifact leg (`scripts/m4-artifact-test.sh`) strengthens this: it boots
the packaged `.app` under `env -i` with poisoned proxies — an environment where a non-loopback
connection cannot succeed even in principle — and samples `lsof` across the full packaged stack,
reusing `d1_egress.py`'s loopback predicate rather than a second implementation. That leg is
tracked as its own task in this milestone's plan; this document records the strengthening it is
meant to provide so the caveat's status is visible regardless of exactly when that task lands.

## Testing honesty

Native chrome — the menu bar, native dialogs, window titles, and Finder integration — is OS-level
UI outside any browser and is not CI-testable: nothing can click a macOS menu or an `NSOpenPanel`
headlessly. Per [PLAN4.md](../PLAN4.md), these are captured manually on a real GUI session,
guided by a per-milestone checklist, rather than faked as automated coverage. The automated gates
instead test the **command layer beneath** each native surface — the HTTP routes and Rust
functions a menu item or dialog action calls into — which catches a dead menu item's wiring but
not "the menu didn't render." That gap is accepted explicitly rather than covered with a test that
would prove nothing.

**Finder double-click** specifically needs a signed, installed app to exercise the real macOS
"Open With" path — [D5's known limits](milestones/d5/README.md#known-limits--stated-not-buried)
records that it is covered by the D5a spike, a packaged-plist check, and a manual double-click on
an installed build, not by the headless gate.
